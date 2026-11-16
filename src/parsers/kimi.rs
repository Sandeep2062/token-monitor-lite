//! Kimi CLI session normalization adapted from the pinned Tokscale `kimi.rs`
//! reader. MIT © 2025 Junho Yeo; see docs/migration/parser-provenance.md.
//!
//! Supported input is `kimi-cli`'s `wire.jsonl`
//! (`~/.kimi/sessions/<GROUP>/<SESSION>/wire.jsonl`). Usage lives on
//! `StatusUpdate` messages: `payload.token_usage` carries `input_other`,
//! `output`, `input_cache_read` and `input_cache_creation` (snake_case on the
//! legacy wire, tolerant of the camelCase aliases). A wire timestamp is Unix
//! seconds as a float, converted to milliseconds.
//!
//! Two things the pinned reader derives are not available here and are
//! recorded as unsupported coverage rather than guessed:
//!
//! - `kimi-code`'s `usage.record` format, which pairs each record with the
//!   preceding `llm.request` to derive a response duration.
//! - The session model from the sibling `config.json`; the native reader takes
//!   the caller-supplied session and the documented default model instead of
//!   reading a second file.
//!
//! Kimi wire protocols expose no reasoning tokens, so output is reported whole
//! and reasoning stays zero.

use super::{CostStatus, ReaderError, ResponseUsage};
use crate::accounting::{SessionKey, TokenCounts};
use serde_json::Value;

pub const MAX_RESPONSES: usize = 16_384;
const MAX_IDENTITY_BYTES: usize = 4_096;
const MAX_RETAINED_IDENTITY_BYTES: usize = 8 * 1024 * 1024;

/// The wire protocol carries no model id, so the documented default is used.
pub const DEFAULT_MODEL: &str = "kimi-for-coding";
pub const DEFAULT_PROVIDER: &str = "moonshot";

pub struct KimiReader {
    session: String,
    fallback_timestamp_ms: Option<i64>,
    responses: Vec<ResponseUsage>,
    /// Index of the observation already emitted for a `message_id`, so a
    /// duplicate StatusUpdate can be resolved instead of counted twice.
    keyed: Vec<Option<String>>,
    retained_identity_bytes: usize,
}

impl KimiReader {
    pub fn new(session: impl Into<String>) -> Self {
        Self {
            session: session.into(),
            fallback_timestamp_ms: None,
            responses: Vec::new(),
            keyed: Vec::new(),
            retained_identity_bytes: 0,
        }
    }

    pub fn with_fallback_timestamp(mut self, timestamp_ms: Option<i64>) -> Self {
        self.fallback_timestamp_ms = timestamp_ms.filter(|value| *value >= 0);
        self
    }

    pub fn ingest(&mut self, record: &[u8]) -> Result<(), ReaderError> {
        let value: Value =
            serde_json::from_slice(record).map_err(|_| ReaderError::InvalidRecord)?;
        let Some(line) = value.as_object() else {
            return Ok(());
        };
        // Only `StatusUpdate` carries usage; metadata, TurnBegin, ToolCall and
        // content parts are not usage.
        let Some(message) = line.get("message").and_then(Value::as_object) else {
            return Ok(());
        };
        if message.get("type").and_then(Value::as_str) != Some("StatusUpdate") {
            return Ok(());
        }
        let Some(payload) = message.get("payload").and_then(Value::as_object) else {
            return Ok(());
        };
        let Some(usage) = payload.get("token_usage").and_then(Value::as_object) else {
            return Ok(());
        };

        let input = counter(usage, &["input_other", "inputOther"])?;
        let output = counter(usage, &["output"])?;
        let cache_read = counter(usage, &["input_cache_read", "inputCacheRead"])?;
        let cache_write = counter(usage, &["input_cache_creation", "inputCacheCreation"])?;
        if input == 0 && output == 0 && cache_read == 0 && cache_write == 0 {
            return Ok(());
        }
        let timestamp = line
            .get("timestamp")
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite() && *value > 0.0)
            .map(|seconds| (seconds * 1000.0) as i64)
            .filter(|millis| *millis > 0)
            .or(self.fallback_timestamp_ms)
            .ok_or(ReaderError::Declined("Kimi wire timestamp"))?;

        let message_id = payload
            .get("message_id")
            .or_else(|| payload.get("messageId"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty() && value.len() <= MAX_IDENTITY_BYTES)
            .filter(|value| !value.chars().any(char::is_control));
        let dedup_key = message_id.map(|id| format!("kimi:{id}"));
        if let Some(key) = dedup_key.as_ref() {
            self.retained_identity_bytes = self
                .retained_identity_bytes
                .checked_add(key.len())
                .filter(|bytes| *bytes <= MAX_RETAINED_IDENTITY_BYTES)
                .ok_or(ReaderError::Capacity("Kimi identities"))?;
        }

        let response = ResponseUsage {
            key: SessionKey::new("kimi", self.session.clone()),
            model: DEFAULT_MODEL.to_owned(),
            tokens: TokenCounts {
                input,
                output,
                cache_read,
                cache_write,
                reasoning: 0,
            },
            cache_write_1h: 0,
            timestamp_ms: timestamp,
            dedup_key: dedup_key.clone(),
            billing_provider_hint: Some(DEFAULT_PROVIDER.to_owned()),
            service_tier: None,
            agent: None,
            workspace_key: None,
            workspace_label: None,
            title: None,
            cost: CostStatus::Unavailable,
        };

        // A duplicate `message_id` is the same call reported twice. Keep the
        // larger total, which is the pinned reader's rule: a later StatusUpdate
        // for the same message carries the more complete count.
        if let Some(key) = dedup_key {
            if let Some(index) = self
                .keyed
                .iter()
                .position(|existing| existing.as_deref().is_some_and(|existing| existing == key))
            {
                if total(&response.tokens) > total(&self.responses[index].tokens) {
                    self.responses[index] = response;
                }
                return Ok(());
            }
            if self.responses.len() == MAX_RESPONSES {
                return Err(ReaderError::Capacity("Kimi responses"));
            }
            self.responses.push(response);
            self.keyed.push(Some(key));
            return Ok(());
        }
        if self.responses.len() == MAX_RESPONSES {
            return Err(ReaderError::Capacity("Kimi responses"));
        }
        self.responses.push(response);
        self.keyed.push(None);
        Ok(())
    }

    pub fn snapshot(&self) -> Vec<ResponseUsage> {
        self.responses.clone()
    }

    pub fn has_supported_usage(&self) -> bool {
        !self.responses.is_empty()
    }
}

impl Default for KimiReader {
    fn default() -> Self {
        Self::new("kimi")
    }
}

fn total(tokens: &TokenCounts) -> u64 {
    tokens
        .input
        .saturating_add(tokens.output)
        .saturating_add(tokens.cache_read)
        .saturating_add(tokens.cache_write)
        .saturating_add(tokens.reasoning)
}

/// The first present alias decides; a present but non-integer value is a
/// malformed record, not a zero.
fn counter(usage: &serde_json::Map<String, Value>, names: &[&str]) -> Result<u64, ReaderError> {
    for name in names {
        match usage.get(*name) {
            None | Some(Value::Null) => continue,
            Some(value) => {
                return value
                    .as_i64()
                    .and_then(|value| u64::try_from(value).ok())
                    .ok_or(ReaderError::InvalidRecord);
            }
        }
    }
    Ok(0)
}
