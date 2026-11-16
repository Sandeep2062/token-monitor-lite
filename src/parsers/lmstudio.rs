//! LM Studio server-log usage, adapted from the pinned reader
//! `crates/tokscale-core/src/sessions/lmstudio.rs`. See
//! docs/migration/parser-provenance.md.
//!
//! The OpenAI-compatible local server writes pretty-printed responses beneath
//! `~/.lmstudio/server-logs/`. There is no usage ledger: the only usage is the
//! `"usage": { … }` object printed for each completed response, and because the
//! log is pretty-printed that object spans several lines. The log is therefore
//! scanned as a document for balanced `usage` objects rather than parsed as
//! line-delimited JSON.
//!
//! # Identity
//!
//! A record's identity lives in the bytes before its `usage` object: the
//! response id (`chatcmpl-…`, `cmpl-…`, `resp_…`), the model, and the log
//! header's timestamp. The last one before the marker wins, because that is the
//! one belonging to this response. Prompt and completion bodies are never
//! deserialized or retained — only the usage object and that identity are read.
//!
//! # Deviations from the pinned reader, and why
//!
//! The pinned reader streams an 8 MiB window and carries identity across
//! evictions, because `server-logs/` grows without bound and its stated
//! regression is that refusing an oversized log "loses every record in that
//! file, past and future, with no diagnostic". This codebase's document input
//! delivers one whole source revision at a time, so the scan here sees the
//! complete byte slice and needs no carry: every record's metadata is still
//! ahead of it. The trade is a **16 MiB document limit**. A log past it is a
//! visible source failure that retains the prior committed generation, rather
//! than the silent total loss the pinned reader was written to avoid — but it
//! does mean a very large log stops refreshing until it is rotated.
//!
//! The log header is naive, so it is the writing host's wall clock and is
//! resolved in the local zone, exactly as the pinned reader does. See the note
//! on `naive_to_epoch_ms` in `timestamps`: reading it as UTC would shift every
//! record by the host's offset and move records across midnight.
//!
//! Not implemented: `is_turn_start` (this codebase has no such field). Not
//! needed: the provider-reported-cost marker, because LM Studio runs the model
//! locally — the pinned reader marks the zero cost as authoritative and this
//! reader reports `Reported(0)` for the same reason.

use super::timestamps::parse_timestamp_str;
use super::{CostStatus, ReaderError, ResponseUsage, jsonl::MAX_RECORD_BYTES};
use crate::accounting::{SessionKey, TokenCounts};
use serde_json::Value;

pub const MAX_RESPONSES: usize = 16_384;
const MAX_IDENTITY_BYTES: usize = 4_096;
const MAX_RETAINED_IDENTITY_BYTES: usize = 8 * 1024 * 1024;

pub const CLIENT: &str = "lmstudio";
/// LM Studio serves models locally, so there is no billed provider.
const PROVIDER: &str = "lmstudio";

const USAGE_MARKER: &[u8] = b"\"usage\"";

/// Response-id prefixes LM Studio emits. A bare `"id"` field is not enough: the
/// log is full of request ids and model-file ids that are not responses.
const RESPONSE_ID_PREFIXES: [&str; 3] = ["chatcmpl-", "cmpl-", "resp_"];

/// One LM Studio server log.
pub struct LmstudioReader {
    key: SessionKey,
    fallback_timestamp_ms: i64,
    responses: Vec<ResponseUsage>,
    retained_identity_bytes: usize,
}

impl LmstudioReader {
    pub fn new(session: &str) -> Self {
        Self {
            key: SessionKey::new(CLIENT, session),
            fallback_timestamp_ms: 0,
            responses: Vec::new(),
            retained_identity_bytes: 0,
        }
    }

    pub fn with_fallback_timestamp(mut self, timestamp_ms: Option<i64>) -> Self {
        self.fallback_timestamp_ms = timestamp_ms.filter(|value| *value >= 0).unwrap_or(0);
        self
    }

    /// Ingest a whole log revision.
    ///
    /// The document input re-delivers the file when it changes, so this starts
    /// from a clean slate rather than appending to a previous revision's
    /// records.
    pub fn ingest(&mut self, document: &[u8]) -> Result<(), ReaderError> {
        if document.len() > MAX_RECORD_BYTES {
            return Err(ReaderError::Capacity("LM Studio server log"));
        }
        self.responses.clear();
        self.retained_identity_bytes = 0;

        let mut cursor = 0usize;
        while let Some((marker, object_start)) = scan_usage_object_start(document, cursor) {
            let Some(object_end) = balanced_object_end(document, object_start) else {
                // The object never closes: it is a partial final write, so stop
                // rather than charging an incomplete record.
                break;
            };
            cursor = object_end;
            self.push_record(&document[..marker], &document[object_start..object_end])?;
        }
        Ok(())
    }

    fn push_record(&mut self, metadata: &[u8], object: &[u8]) -> Result<(), ReaderError> {
        let usage: Value =
            serde_json::from_slice(object).map_err(|_| ReaderError::InvalidRecord)?;
        let Some(tokens) = tokens(&usage)? else {
            return Ok(());
        };
        let response_id = response_id_in(metadata);
        let model = model_in(metadata).unwrap_or_else(|| "unknown".to_owned());
        if model.len() > MAX_IDENTITY_BYTES {
            return Err(ReaderError::Declined("LM Studio model identity"));
        }
        let timestamp_ms = last_log_timestamp(metadata).unwrap_or(self.fallback_timestamp_ms);
        if timestamp_ms <= 0 {
            return Err(ReaderError::InvalidTimestamp);
        }
        let dedup_key = match response_id {
            Some(id) => format!("lmstudio:{id}"),
            // No response id: fall back to the record's own content. A byte
            // offset is not usable here because a whole-revision scan restarts
            // from zero, so it would not stay stable across revisions.
            None => format!(
                "lmstudio:{}:{}:{}:{}:{}:{}:{}",
                self.key.session,
                model,
                timestamp_ms,
                tokens.input,
                tokens.output,
                tokens.cache_read,
                tokens.cache_write
            ),
        };
        if dedup_key.len() > MAX_IDENTITY_BYTES {
            return Err(ReaderError::Declined("LM Studio response identity"));
        }
        let usage = ResponseUsage {
            key: self.key.clone(),
            model,
            tokens,
            cache_write_1h: 0,
            timestamp_ms,
            dedup_key: Some(dedup_key.clone()),
            billing_provider_hint: Some(PROVIDER.to_owned()),
            service_tier: None,
            agent: None,
            workspace_key: None,
            workspace_label: None,
            title: None,
            // The model runs on this machine, so the pinned reader marks the
            // zero cost authoritative; an explicit zero says "do not price
            // this", which is exactly right.
            cost: CostStatus::Reported(0),
        };
        if self.responses.len() == MAX_RESPONSES {
            return Err(ReaderError::Capacity("LM Studio responses"));
        }
        self.retained_identity_bytes = self
            .retained_identity_bytes
            .checked_add(dedup_key.len())
            .filter(|bytes| *bytes <= MAX_RETAINED_IDENTITY_BYTES)
            .ok_or(ReaderError::Capacity("LM Studio identities"))?;
        self.responses.push(usage);
        Ok(())
    }

    pub fn snapshot(&self) -> Vec<ResponseUsage> {
        self.responses.clone()
    }

    pub fn has_supported_usage(&self) -> bool {
        !self.responses.is_empty()
    }
}

/// The cache and reasoning shares are moved out of the inclusive counts.
///
/// `prompt_tokens` already contains both cache buckets and `completion_tokens`
/// already contains reasoning, so the shares are subtracted rather than added
/// on top; adding them would count every cache token twice.
fn tokens(usage: &Value) -> Result<Option<TokenCounts>, ReaderError> {
    let prompt = counter(usage, "prompt_tokens")?;
    let completion = counter(usage, "completion_tokens")?;
    let prompt_details = usage.get("prompt_tokens_details");
    let output_details = usage.get("output_tokens_details");
    // Both the nested detail block and the flat spelling are read, whichever is
    // larger, because builds have shipped either.
    let cache_read = detail(prompt_details, "cached_tokens")?
        .max(counter(usage, "cached_tokens")?)
        .min(prompt);
    let cache_write = detail(prompt_details, "cache_creation_input_tokens")?
        .max(counter(usage, "cache_creation_input_tokens")?)
        .min(prompt.saturating_sub(cache_read));
    let reasoning = detail(output_details, "reasoning_tokens")?
        .max(counter(usage, "reasoning_tokens")?)
        .min(completion);
    // The reported total is trusted when it is larger, because a server that
    // counts differently must not have its usage shortened.
    let total = counter(usage, "total_tokens")?.max(prompt.saturating_add(completion));
    if total == 0 {
        return Ok(None);
    }
    let input = total
        .saturating_sub(completion)
        .saturating_sub(cache_read)
        .saturating_sub(cache_write);
    Ok(Some(TokenCounts {
        input,
        output: completion.saturating_sub(reasoning),
        cache_read,
        cache_write,
        reasoning,
    }))
}

fn counter(usage: &Value, key: &str) -> Result<u64, ReaderError> {
    match usage.get(key) {
        None | Some(Value::Null) => Ok(0),
        Some(value) => value.as_u64().ok_or(ReaderError::InvalidRecord),
    }
}

fn detail(details: Option<&Value>, key: &str) -> Result<u64, ReaderError> {
    match details {
        None | Some(Value::Null) => Ok(0),
        Some(details) => counter(details, key),
    }
}

/// The next `"usage": {`, as `(marker, object_start)`.
fn scan_usage_object_start(bytes: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut cursor = from;
    while let Some(marker) = find(bytes, USAGE_MARKER, cursor) {
        cursor = marker + USAGE_MARKER.len();
        // An escaped marker is part of a string value, not a key.
        if marker > 0 && bytes[marker - 1] == b'\\' {
            continue;
        }
        let value = skip_whitespace(bytes, cursor);
        if bytes.get(value) != Some(&b':') {
            continue;
        }
        let brace = skip_whitespace(bytes, value + 1);
        if bytes.get(brace) == Some(&b'{') {
            return Some((marker, brace));
        }
    }
    None
}

fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from > haystack.len().saturating_sub(needle.len()) {
        return None;
    }
    haystack[from..]
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|offset| from + offset)
}

fn skip_whitespace(bytes: &[u8], mut index: usize) -> usize {
    while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
        index += 1;
    }
    index
}

/// The index just past the closing brace matching the one at `start`.
fn balanced_object_end(bytes: &[u8], start: usize) -> Option<usize> {
    if bytes.get(start) != Some(&b'{') {
        return None;
    }
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (index, byte) in bytes.iter().copied().enumerate().skip(start) {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(index + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// The last response id in `bytes`, if it carries a response-id prefix.
fn response_id_in(bytes: &[u8]) -> Option<String> {
    last_json_string_field(bytes, b"id").filter(|value| {
        RESPONSE_ID_PREFIXES
            .iter()
            .any(|prefix| value.starts_with(prefix))
    })
}

fn model_in(bytes: &[u8]) -> Option<String> {
    last_json_string_field(bytes, b"model").filter(|value| !value.trim().is_empty())
}

fn last_json_string_field(bytes: &[u8], field: &[u8]) -> Option<String> {
    let mut marker = Vec::with_capacity(field.len() + 2);
    marker.push(b'"');
    marker.extend_from_slice(field);
    marker.push(b'"');
    let mut cursor = 0usize;
    let mut found = None;
    while let Some(index) = find(bytes, &marker, cursor) {
        cursor = index + marker.len();
        if index > 0 && bytes[index - 1] == b'\\' {
            continue;
        }
        let mut value = skip_whitespace(bytes, cursor);
        if bytes.get(value) != Some(&b':') {
            continue;
        }
        value = skip_whitespace(bytes, value + 1);
        let Some(end) = json_string_end(bytes, value) else {
            continue;
        };
        if let Ok(parsed) = serde_json::from_slice::<String>(&bytes[value..end]) {
            found = Some(parsed);
        }
    }
    found
}

fn json_string_end(bytes: &[u8], start: usize) -> Option<usize> {
    if bytes.get(start) != Some(&b'"') {
        return None;
    }
    let mut escaped = false;
    for (index, byte) in bytes.iter().copied().enumerate().skip(start + 1) {
        if escaped {
            escaped = false;
        } else if byte == b'\\' {
            escaped = true;
        } else if byte == b'"' {
            return Some(index + 1);
        }
    }
    None
}

/// The timestamp of the last LM Studio log header in `bytes`.
///
/// Anchored to the header's shape — `[<ts>]` at the start of a line and
/// immediately followed by another `[` — rather than to any bracketed date. A
/// model that prints `[2020-01-01 00:00:00]` in its answer lands *after* the
/// real header, so a bare-bracket last-match scan would date the usage from the
/// model's own output.
///
/// The header is naive and is resolved in the host's local zone; see the module
/// note and `timestamps::naive_to_epoch_ms`.
fn last_log_timestamp(bytes: &[u8]) -> Option<i64> {
    let text = String::from_utf8_lossy(bytes);
    let mut parsed = None;
    for line in text.split('\n') {
        let line = line.strip_prefix('\r').unwrap_or(line);
        let Some(rest) = line.strip_prefix('[') else {
            continue;
        };
        let Some(end) = rest.find(']') else {
            continue;
        };
        // The header always continues into its next bracketed field.
        if !rest[end + 1..].starts_with('[') {
            continue;
        }
        if let Some(timestamp) = parse_timestamp_str(&rest[..end]) {
            parsed = Some(timestamp);
        }
    }
    parsed
}
