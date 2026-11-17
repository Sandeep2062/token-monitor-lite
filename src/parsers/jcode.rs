//! Jcode session usage, adapted from the pinned reader
//! `crates/tokscale-core/src/sessions/jcode.rs`. See
//! docs/migration/parser-provenance.md.
//!
//! Jcode writes one compact JSON session snapshot per session at
//! `~/.jcode/sessions/session_*.json`. Authoritative assistant token usage sits
//! on each message under `token_usage`; user and tool messages carry none and
//! are skipped.
//!
//! # Cache accounting depends on the message, not the session
//!
//! Jcode stores `provider_key` and `model` only at **session** scope, so either
//! may describe a later route after a mid-session switch. The message-local
//! usage shape is therefore what decides the accounting:
//!
//! - an Anthropic-style report preserves `cache_creation_input_tokens` even when
//!   its value is zero, which means the cache buckets are already split out of
//!   `input_tokens`;
//! - an OpenAI/OpenRouter `cached_tokens` report omits that field and instead
//!   counts cache reads as a **subset** of `input_tokens`, so the overlap has to
//!   be removed or the cached reads are charged twice.
//!
//! The second signal is `cache_read > input`, which cannot happen under split
//! accounting and therefore proves the subset shape even when the creation field
//! is absent.
//!
//! # Timestamp
//!
//! An assistant message's `timestamp` is written once the message — including
//! `token_usage` — is finalized, i.e. the turn's **end**. `tool_duration_ms` is
//! that turn's elapsed time, so the start is back-calculated. The subtraction
//! applies **only** when the message carries its own recorded timestamp: when it
//! is absent the value is the session/file fallback, and subtracting a turn's
//! duration from that would move the message into the wrong day.
//!
//! # The sibling journal
//!
//! A session can have a `*.journal.jsonl` beside its snapshot. The journal
//! appends the unsaved message suffix and may repeat a snapshot entry to
//! *correct* it, so the two files are read as one source and messages are keyed
//! by identity with the journal winning. Reading them as separate sources would
//! count a corrected message twice.
//!
//! The journal is JSONL — one append batch per line — while the snapshot is a
//! single object, so `ingest` accepts both. Each append batch re-scopes the
//! billing provider for its own messages, since `provider_key` and `model` are
//! session-scoped and may describe a later route.
//!
//! Not implemented: `is_turn_start` and `message_count` (no such fields on this
//! codebase's observation), and `canonical_provider` — `provider_key` is
//! reported verbatim as the billing hint.

use super::timestamps::parse_timestamp_str;
use super::{CostStatus, ReaderError, ResponseUsage, jsonl::MAX_RECORD_BYTES};
use crate::accounting::{SessionKey, TokenCounts};
use serde_json::Value;
use std::collections::HashMap;

pub const MAX_RESPONSES: usize = 16_384;
const MAX_IDENTITY_BYTES: usize = 4_096;
const MAX_RETAINED_IDENTITY_BYTES: usize = 8 * 1024 * 1024;

pub const CLIENT: &str = "jcode";

/// One Jcode session: the snapshot plus any journal that amends it.
///
/// Both arrive through `ingest`. The journal repeats a snapshot message when it
/// corrects it, so messages are keyed by identity and the last write wins —
/// which makes the journal authoritative without needing to know which file a
/// message came from.
pub struct JcodeReader {
    key: SessionKey,
    fallback_timestamp_ms: i64,
    responses: Vec<ResponseUsage>,
    /// Dedup key to position in `responses`, so a repeat replaces in place
    /// instead of appending a second row for the same message.
    positions: HashMap<String, usize>,
    /// Session-scoped identity, updated by any batch that supplies it. A
    /// journal append after a route change re-scopes later messages without
    /// rewriting the ones already recorded.
    model: Option<String>,
    provider: Option<String>,
    workspace_label: Option<String>,
    retained_identity_bytes: usize,
}

impl JcodeReader {
    pub fn new(session: &str) -> Self {
        Self {
            key: SessionKey::new(CLIENT, session),
            fallback_timestamp_ms: 0,
            responses: Vec::new(),
            positions: HashMap::new(),
            model: None,
            provider: None,
            workspace_label: None,
            retained_identity_bytes: 0,
        }
    }

    pub fn with_fallback_timestamp(mut self, timestamp_ms: Option<i64>) -> Self {
        self.fallback_timestamp_ms = timestamp_ms.filter(|value| *value >= 0).unwrap_or(0);
        self
    }

    /// Ingest a session snapshot, or the journal that amends it.
    ///
    /// A snapshot is a single JSON object. A journal is JSONL, one append batch
    /// per line, so a document that does not parse as one object is split into
    /// lines and each line applied in turn.
    pub fn ingest(&mut self, document: &[u8]) -> Result<(), ReaderError> {
        if document.len() > MAX_RECORD_BYTES {
            return Err(ReaderError::Capacity("Jcode session snapshot"));
        }
        if let Ok(root) = serde_json::from_slice::<Value>(document) {
            return self.apply(&root);
        }
        for line in document.split(|byte| *byte == b'\n') {
            let line = trim_ascii(line);
            if line.is_empty() {
                continue;
            }
            let batch: Value =
                serde_json::from_slice(line).map_err(|_| ReaderError::InvalidRecord)?;
            self.apply(&batch)?;
        }
        Ok(())
    }

    /// Apply one snapshot or one append batch.
    fn apply(&mut self, root: &Value) -> Result<(), ReaderError> {
        // A blank id falls through to the caller's path-derived session.
        let session = root
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty() && id.len() <= MAX_IDENTITY_BYTES);
        if let Some(session) = session {
            self.key = SessionKey::new(CLIENT, session);
        }
        // Each batch re-scopes the identity for its own messages, so a route
        // change late in a session cannot re-authenticate earlier ones. A batch
        // that omits them inherits the last value seen.
        if let Some(model) = identity_opt(root.get("model")) {
            self.model = Some(model);
        }
        if let Some(provider) = identity_opt(root.get("provider_key")) {
            self.provider = Some(provider);
        }
        if let Some(label) = root
            .get("working_dir")
            .and_then(Value::as_str)
            .and_then(workspace_label)
        {
            self.workspace_label = Some(label);
        }
        let model = self.model.clone().unwrap_or_else(|| "unknown".to_owned());
        let provider = self.provider.clone().unwrap_or_else(|| "jcode".to_owned());

        match root.get("messages").and_then(Value::as_array) {
            Some(messages) => {
                for (ordinal, message) in messages.iter().enumerate() {
                    self.record(message, ordinal, &model, &provider)?;
                }
            }
            // A journal written one message per line has no enclosing array.
            None if root.get("token_usage").is_some() => self.record(root, 0, &model, &provider)?,
            None => {}
        }
        Ok(())
    }

    fn record(
        &mut self,
        message: &Value,
        ordinal: usize,
        model: &str,
        provider: &str,
    ) -> Result<(), ReaderError> {
        // A single wrong-typed field must only drop that message, not its
        // valid siblings.
        let Some(usage) = message.get("token_usage") else {
            return Ok(());
        };
        let tokens = tokens(usage)?;
        if zero(&tokens) {
            return Ok(());
        }
        let message_id = message
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty() && id.len() <= MAX_IDENTITY_BYTES)
            .map(str::to_owned)
            // Real messages carry stable ids; this keeps a custom or
            // malformed file parseable without colliding across messages.
            .unwrap_or_else(|| format!("snapshot:{ordinal}"));
        let dedup_key = format!("jcode:{}:{message_id}", self.key.session);
        if dedup_key.len() > MAX_IDENTITY_BYTES {
            return Err(ReaderError::Declined("Jcode identity"));
        }

        let explicit_timestamp = message
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_timestamp_str);
        let duration_ms = message
            .get("tool_duration_ms")
            .and_then(Value::as_i64)
            .filter(|duration| *duration > 0);
        // Back-anchor only against the message's own recorded end time.
        let timestamp_ms = match (explicit_timestamp, duration_ms) {
            (Some(end), Some(duration)) => end.saturating_sub(duration).max(0),
            _ => explicit_timestamp.unwrap_or(self.fallback_timestamp_ms),
        };

        let response = ResponseUsage {
            key: self.key.clone(),
            model: model.to_owned(),
            tokens,
            cache_write_1h: 0,
            timestamp_ms,
            dedup_key: Some(dedup_key.clone()),
            billing_provider_hint: Some(provider.to_owned()),
            service_tier: None,
            agent: None,
            workspace_key: None,
            workspace_label: self.workspace_label.clone(),
            title: None,
            cost: CostStatus::Unavailable,
        };
        self.insert(response, dedup_key)
    }

    /// Keyed by message identity, with the last write winning.
    fn insert(&mut self, response: ResponseUsage, dedup_key: String) -> Result<(), ReaderError> {
        if let Some(&position) = self.positions.get(&dedup_key) {
            // The journal is authoritative for a message it repeats: it is the
            // corrected form of a snapshot entry, not a second turn.
            self.responses[position] = response;
            return Ok(());
        }
        if self.responses.len() == MAX_RESPONSES {
            return Err(ReaderError::Capacity("Jcode responses"));
        }
        self.retained_identity_bytes = self
            .retained_identity_bytes
            .checked_add(dedup_key.len())
            .filter(|bytes| *bytes <= MAX_RETAINED_IDENTITY_BYTES)
            .ok_or(ReaderError::Capacity("Jcode identities"))?;
        self.positions.insert(dedup_key, self.responses.len());
        self.responses.push(response);
        Ok(())
    }

    pub fn snapshot(&self) -> Vec<ResponseUsage> {
        self.responses.clone()
    }

    pub fn has_supported_usage(&self) -> bool {
        !self.responses.is_empty()
    }
}

/// Whether this message's cache counters are already split out of its input.
///
/// Session-scoped `provider_key` and `model` cannot decide it — a mid-session
/// switch makes either describe a later route — so the message-local usage shape
/// is used instead.
fn split_cache_accounting(usage: &Value, input: u64, cache_read: u64) -> bool {
    // Anthropic-style reports keep the creation field even at zero, so the
    // field's presence — not its value — is the signal.
    if usage.get("cache_creation_input_tokens").is_some() {
        return true;
    }
    // Cache reads above the input count are impossible under split accounting,
    // so they prove the subset shape even without the creation field.
    cache_read > input
}

fn tokens(usage: &Value) -> Result<TokenCounts, ReaderError> {
    let reported_input = counter(usage, "input_tokens")?;
    let cache_read = counter(usage, "cache_read_input_tokens")?;
    let cache_write = counter(usage, "cache_creation_input_tokens")?;
    let input = if split_cache_accounting(usage, reported_input, cache_read) {
        reported_input
    } else {
        // OpenAI-style APIs report cached tokens as a subset of `input_tokens`.
        // The buckets are priced independently, so the overlap is removed here
        // rather than charged twice.
        reported_input.saturating_sub(cache_read.min(reported_input))
    };
    Ok(TokenCounts {
        input,
        output: counter(usage, "output_tokens")?,
        cache_read,
        cache_write,
        reasoning: counter(usage, "reasoning_output_tokens")?,
    })
}

fn counter(usage: &Value, key: &str) -> Result<u64, ReaderError> {
    match usage.get(key) {
        None | Some(Value::Null) => Ok(0),
        Some(value) => value.as_u64().ok_or(ReaderError::InvalidRecord),
    }
}

fn zero(tokens: &TokenCounts) -> bool {
    tokens.input == 0
        && tokens.output == 0
        && tokens.cache_read == 0
        && tokens.cache_write == 0
        && tokens.reasoning == 0
}

/// A trimmed session-scoped identity, when the batch supplies a usable one.
fn identity_opt(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty() && text.len() <= MAX_IDENTITY_BYTES)
        .map(str::to_owned)
}

/// Strip leading and trailing ASCII whitespace without allocating.
fn trim_ascii(mut bytes: &[u8]) -> &[u8] {
    while let [first, rest @ ..] = bytes {
        if !first.is_ascii_whitespace() {
            break;
        }
        bytes = rest;
    }
    while let [rest @ .., last] = bytes {
        if !last.is_ascii_whitespace() {
            break;
        }
        bytes = rest;
    }
    bytes
}

/// The final component of a working directory, never the whole path.
fn workspace_label(path: &str) -> Option<String> {
    let trimmed = path.trim().trim_end_matches(['/', '\\']);
    let label = trimmed.rsplit(['/', '\\']).next()?;
    if label.is_empty() || label.len() > MAX_RECORD_BYTES || label.chars().any(char::is_control) {
        return None;
    }
    Some(label.to_owned())
}
