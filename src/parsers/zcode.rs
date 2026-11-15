//! ZCode (z.ai) session normalization adapted from the pinned Tokscale
//! `zcode.rs` reader. MIT © 2025 Junho Yeo; see
//! docs/migration/parser-provenance.md.
//!
//! ZCode is Z.ai's Electron ADE for the GLM model family. It keeps usage in
//! two places, and both are supported here:
//!
//! - JSONL transcripts at `~/.zcode/projects/<slug>/<session>.jsonl`, where an
//!   assistant line may carry a `usage`/`token_usage` block;
//! - a SQLite `model_usage` table, which is the authoritative per-request
//!   store and the only one that records turn and duration metadata.
//!
//! Both stores report `input_tokens` and `output_tokens` cache- and
//! reasoning-inclusive: `input_tokens` already contains the cache read/write
//! buckets, and `output_tokens` already contains `reasoning_tokens`. The native
//! model keeps five non-overlapping buckets, so the overlap must be subtracted
//! exactly once or the total double-counts. The pinned reader detects the shape
//! by comparing a reported total against the inclusive and exclusive sums; the
//! same rule is implemented here, with the legacy-schema fallback documented
//! below.
//!
//! Not implemented, and recorded as unsupported coverage rather than guessed:
//!
//! - the ~4-chars-per-token estimation the pinned reader falls back to when a
//!   transcript line records no usage. Fabricated counts are worse than none,
//!   because they are indistinguishable from measured ones downstream;
//! - `is_turn_start`, which is an aggregation concern in this codebase rather
//!   than a property of one observation;
//! - per-request duration and the `agent`/`mode` attribution columns.

use super::{CostStatus, ReaderError, ResponseUsage, jsonl::MAX_RECORD_BYTES};
use crate::accounting::{SessionKey, TokenCounts};
use serde_json::{Map, Value};

pub const MAX_RESPONSES: usize = 16_384;
const MAX_IDENTITY_BYTES: usize = 4_096;
const MAX_RETAINED_IDENTITY_BYTES: usize = 8 * 1024 * 1024;

pub const CLIENT: &str = "zcode";
/// ZCode's own provider identity; usage is Z.ai/GLM spend.
pub const PROVIDER: &str = "zhipu";
/// The pinned reader's documented default when a transcript line names no
/// model, because ZCode's marketing surface always resolves to a GLM model.
pub const DEFAULT_MODEL: &str = "glm-5.2";

/// One assistant line from a ZCode JSONL transcript.
pub struct ZcodeReader {
    session: String,
    fallback_timestamp_ms: Option<i64>,
    model: Option<String>,
    responses: Vec<ResponseUsage>,
    retained_identity_bytes: usize,
}

impl ZcodeReader {
    pub fn new(session: impl Into<String>) -> Self {
        Self {
            session: session.into(),
            fallback_timestamp_ms: None,
            model: None,
            responses: Vec::new(),
            retained_identity_bytes: 0,
        }
    }

    pub fn with_fallback_timestamp(mut self, timestamp_ms: Option<i64>) -> Self {
        self.fallback_timestamp_ms = timestamp_ms.filter(|value| *value >= 0);
        self
    }

    pub fn ingest(&mut self, record: &[u8]) -> Result<(), ReaderError> {
        if record.len() > MAX_RECORD_BYTES {
            return Err(ReaderError::Capacity("ZCode line"));
        }
        let value: Value =
            serde_json::from_slice(record).map_err(|_| ReaderError::InvalidRecord)?;
        let Some(line) = value.as_object() else {
            return Ok(());
        };
        // The transcript interleaves tool and metadata records; only assistant
        // turns can carry usage.
        if line.get("role").and_then(Value::as_str) != Some("assistant") {
            return Ok(());
        }
        // The model in effect is the last one named, so a mid-session switch
        // prices later turns under the new model rather than the first.
        if let Some(model) = line.get("model").and_then(Value::as_str)
            && !model.is_empty()
        {
            self.model = Some(canonicalize_model(model)?);
        }
        let Some(tokens) = resolve_usage(line)? else {
            // No authoritative block. The pinned reader estimates here; the
            // native reader contributes nothing instead of inventing counts.
            return Ok(());
        };
        let session = line
            .get("sessionId")
            .and_then(Value::as_str)
            .filter(|value| valid_identity(value))
            .unwrap_or(&self.session)
            .to_owned();
        let timestamp_ms = match line.get("timestamp").and_then(Value::as_str) {
            Some(timestamp) => parse_rfc3339_ms(timestamp)?,
            None => self
                .fallback_timestamp_ms
                .ok_or(ReaderError::Unsupported("ZCode line timestamp"))?,
        };
        let model = self
            .model
            .clone()
            .unwrap_or_else(|| DEFAULT_MODEL.to_owned());
        let identity = format!("{session}:{}", self.responses.len());
        self.push(ResponseUsage {
            key: SessionKey::new(CLIENT, &session),
            model,
            tokens,
            cache_write_1h: 0,
            timestamp_ms,
            dedup_key: Some(identity),
            billing_provider_hint: Some(PROVIDER.to_owned()),
            service_tier: None,
            agent: None,
            workspace_key: None,
            workspace_label: None,
            title: None,
            cost: CostStatus::Unavailable,
        })
    }

    fn push(&mut self, usage: ResponseUsage) -> Result<(), ReaderError> {
        if self.responses.len() == MAX_RESPONSES {
            return Err(ReaderError::Capacity("ZCode responses"));
        }
        let identity_bytes = usage.dedup_key.as_ref().map_or(0, String::len);
        self.retained_identity_bytes = self
            .retained_identity_bytes
            .checked_add(identity_bytes)
            .filter(|bytes| *bytes <= MAX_RETAINED_IDENTITY_BYTES)
            .ok_or(ReaderError::Capacity("ZCode identities"))?;
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

/// One row of the SQLite `model_usage` table.
///
/// `computed_total_tokens` is `None` both when the column does not exist and
/// when this row's value is NULL, so the caller distinguishes those two cases
/// with `legacy_schema` rather than by re-probing per row.
pub struct ZcodeUsageRow<'a> {
    pub id: &'a str,
    pub session_id: Option<&'a str>,
    pub model_id: Option<&'a str>,
    pub started_at: Option<i64>,
    pub completed_at: Option<i64>,
    pub fallback_timestamp_ms: i64,
    pub input: Option<i64>,
    pub output: Option<i64>,
    pub reasoning: Option<i64>,
    pub cache_read: Option<i64>,
    pub cache_write: Option<i64>,
    pub computed_total: Option<i64>,
    /// True when the database has no `computed_total_tokens` column at all.
    pub legacy_schema: bool,
}

/// Normalize one SQLite usage row, or `None` when it records no tokens.
pub fn normalize_row(row: &ZcodeUsageRow<'_>) -> Result<Option<ResponseUsage>, ReaderError> {
    if !valid_identity(row.id) {
        return Err(ReaderError::InvalidRecord);
    }
    let session = row
        .session_id
        .filter(|value| valid_identity(value))
        .unwrap_or("unknown");
    let input = non_negative(row.input, "ZCode input")?;
    let output = non_negative(row.output, "ZCode output")?;
    let reasoning = non_negative(row.reasoning, "ZCode reasoning")?;
    let cache_read = non_negative(row.cache_read, "ZCode cache read")?;
    let cache_write = non_negative(row.cache_write, "ZCode cache write")?;
    let (net_input, net_output) = match row.computed_total {
        Some(total) => normalize_inclusive(
            input,
            output,
            cache_read,
            cache_write,
            reasoning,
            Some(non_negative(Some(total), "ZCode total")?),
        )?,
        // A legacy database has no total column at all. Every sampled row in a
        // real ZCode store is cache/reasoning-inclusive, so subtract
        // unconditionally. In a modern database a NULL total means the shape
        // cannot be detected, so the raw counters pass through unchanged;
        // subtracting there would undercount already-exclusive rows.
        None if row.legacy_schema => (
            subtract_overlap(input, cache_overlap(cache_read, cache_write)?)?,
            subtract_overlap(output, reasoning)?,
        ),
        None => normalize_inclusive(input, output, cache_read, cache_write, reasoning, None)?,
    };
    let tokens = TokenCounts {
        input: net_input,
        output: net_output,
        cache_read,
        cache_write,
        reasoning,
    };
    if tokens_total(&tokens) == 0 {
        return Ok(None);
    }
    let model = match row.model_id {
        Some(model) if !model.is_empty() => canonicalize_model(model)?,
        _ => DEFAULT_MODEL.to_owned(),
    };
    let identity = format!("zcode-sqlite:{}", row.id);
    if identity.len() > MAX_IDENTITY_BYTES {
        return Err(ReaderError::Unsupported("ZCode message identity"));
    }
    Ok(Some(ResponseUsage {
        key: SessionKey::new(CLIENT, session),
        model,
        tokens,
        cache_write_1h: 0,
        timestamp_ms: resolve_timestamp(
            row.started_at,
            row.completed_at,
            row.fallback_timestamp_ms,
        ),
        dedup_key: Some(identity),
        billing_provider_hint: Some(PROVIDER.to_owned()),
        service_tier: None,
        agent: None,
        workspace_key: None,
        workspace_label: None,
        title: None,
        cost: CostStatus::Unavailable,
    }))
}

/// Resolve a transcript line's authoritative usage block.
///
/// `usage` is preferred, but an empty `usage` object must not shadow a
/// populated `token_usage`, so the first block that actually yields tokens wins.
fn resolve_usage(line: &Map<String, Value>) -> Result<Option<TokenCounts>, ReaderError> {
    for key in ["usage", "token_usage"] {
        let Some(block) = line.get(key) else {
            continue;
        };
        let Some(block) = block.as_object() else {
            return Err(ReaderError::InvalidRecord);
        };
        if let Some(tokens) = usage_tokens(block)? {
            return Ok(Some(tokens));
        }
    }
    Ok(None)
}

fn usage_tokens(block: &Map<String, Value>) -> Result<Option<TokenCounts>, ReaderError> {
    let input = aliased_u64(
        block,
        &["input", "input_tokens", "prompt_tokens", "inputTokens"],
    )?;
    let output = aliased_u64(
        block,
        &[
            "output",
            "output_tokens",
            "completion_tokens",
            "outputTokens",
        ],
    )?;
    let cache_read = aliased_u64(
        block,
        &["input_cache_read", "cache_read_tokens", "cacheReadTokens"],
    )?;
    let cache_write = aliased_u64(
        block,
        &[
            "input_cache_creation",
            "cache_write_tokens",
            "cacheCreationTokens",
        ],
    )?;
    let reasoning = aliased_u64(block, &["reasoning", "reasoningTokens"])?;
    let total = aliased_u64(block, &["total", "totalTokens"])?;
    let input_counter = input.unwrap_or(0);
    let output_counter = output.unwrap_or(0);
    let cache_read_counter = cache_read.unwrap_or(0);
    let cache_write_counter = cache_write.unwrap_or(0);
    let reasoning_counter = reasoning.unwrap_or(0);
    let tokens_total = input_counter
        .checked_add(output_counter)
        .and_then(|value| value.checked_add(cache_read_counter))
        .and_then(|value| value.checked_add(cache_write_counter))
        .and_then(|value| value.checked_add(reasoning_counter))
        .ok_or(ReaderError::Overflow)?;
    if tokens_total == 0 {
        // An all-zero block is not usage; leaving it empty lets a sibling
        // `token_usage` block be considered instead.
        return Ok(None);
    }
    let (net_input, net_output) = normalize_inclusive(
        input_counter,
        output_counter,
        cache_read_counter,
        cache_write_counter,
        reasoning_counter,
        total,
    )?;
    Ok(Some(TokenCounts {
        input: net_input,
        output: net_output,
        cache_read: cache_read_counter,
        cache_write: cache_write_counter,
        reasoning: reasoning_counter,
    }))
}

/// Detect and remove the cache/reasoning overlap exactly once.
///
/// A reported `total` that equals `input + output` while differing from the
/// fully additive sum proves the row is inclusive; anything else (or no total
/// at all) leaves the counters alone.
fn normalize_inclusive(
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    reasoning: u64,
    total: Option<u64>,
) -> Result<(u64, u64), ReaderError> {
    let overlap = cache_overlap(cache_read, cache_write)?;
    let Some(total) = total else {
        return Ok((input, output));
    };
    let inclusive = input.checked_add(output).ok_or(ReaderError::Overflow)?;
    let exclusive = inclusive
        .checked_add(overlap)
        .and_then(|value| value.checked_add(reasoning))
        .ok_or(ReaderError::Overflow)?;
    if (overlap > 0 || reasoning > 0) && total == inclusive && total != exclusive {
        return Ok((
            subtract_overlap(input, overlap)?,
            subtract_overlap(output, reasoning)?,
        ));
    }
    Ok((input, output))
}

fn cache_overlap(cache_read: u64, cache_write: u64) -> Result<u64, ReaderError> {
    cache_read
        .checked_add(cache_write)
        .ok_or(ReaderError::Overflow)
}

/// Remove `overlap` from `value`.
///
/// The pinned reader saturates, which silently hides an internally
/// inconsistent record. An overlap larger than the value it was supposedly
/// contained in means the row cannot be interpreted, so it fails visibly.
fn subtract_overlap(value: u64, overlap: u64) -> Result<u64, ReaderError> {
    value
        .checked_sub(overlap)
        .ok_or(ReaderError::Unsupported("ZCode cache overlap"))
}

fn tokens_total(tokens: &TokenCounts) -> u64 {
    tokens
        .input
        .saturating_add(tokens.output)
        .saturating_add(tokens.cache_read)
        .saturating_add(tokens.cache_write)
        .saturating_add(tokens.reasoning)
}

/// Prefer `started_at`, because it anchors the observation at the call's start
/// and matches `duration_ms`'s own start-to-end span. A missing start falls back
/// to `completed_at`; that is a visible approximation rather than an invented
/// interval, so it is used as-is.
fn resolve_timestamp(started_at: Option<i64>, completed_at: Option<i64>, fallback: i64) -> i64 {
    if let Some(started) = started_at.filter(|value| *value > 0) {
        return started;
    }
    match completed_at.filter(|value| *value > 0) {
        Some(completed) => completed,
        None => fallback,
    }
}

fn aliased_u64(block: &Map<String, Value>, keys: &[&str]) -> Result<Option<u64>, ReaderError> {
    for key in keys {
        let Some(value) = block.get(*key) else {
            continue;
        };
        if value.is_null() {
            continue;
        }
        let Some(number) = value.as_u64() else {
            return Err(ReaderError::InvalidRecord);
        };
        return Ok(Some(number));
    }
    Ok(None)
}

fn non_negative(value: Option<i64>, what: &'static str) -> Result<u64, ReaderError> {
    match value {
        None => Ok(0),
        Some(value) if value < 0 => Err(ReaderError::Unsupported(what)),
        Some(value) => Ok(value as u64),
    }
}

/// ZCode reports GLM names mixed-case (`GLM-5.2`, `glm-5-Turbo`), so the model
/// is lowercased for a stable pricing key.
fn canonicalize_model(model: &str) -> Result<String, ReaderError> {
    if !valid_identity(model) {
        return Err(ReaderError::InvalidRecord);
    }
    Ok(model.to_lowercase())
}

fn parse_rfc3339_ms(timestamp: &str) -> Result<i64, ReaderError> {
    if !valid_identity(timestamp) {
        return Err(ReaderError::InvalidTimestamp);
    }
    crate::parsers::timestamps::parse_timestamp_str(timestamp).ok_or(ReaderError::InvalidTimestamp)
}

fn valid_identity(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_IDENTITY_BYTES && !value.chars().any(char::is_control)
}
