//! Cline CLI session-message normalization adapted from the pinned Tokscale
//! `cline.rs` reader. MIT © 2025 Junho Yeo; see
//! docs/migration/parser-provenance.md.
//!
//! Supported input is Cline CLI/Desktop `~/.cline/data/sessions/<session>/
//! <session>.messages.json` with assistant `metrics`. The VS Code extension's
//! `tasks/` logs (the Roo/Kilo shared text-payload format) are not implemented.
//! Input counters are treated as gross of cache; a row whose cache exceeds its
//! input is visible unsupported instead of silently losing tokens the way the
//! pinned reader's saturating subtraction does.

use super::{CostStatus, ReaderError, ResponseUsage};
use crate::accounting::{SessionKey, TokenCounts};
use chrono::DateTime;
use serde_json::{Map, Value};

pub const MAX_RESPONSES: usize = 16_384;
const MAX_IDENTITY_BYTES: usize = 4_096;
const MAX_RETAINED_IDENTITY_BYTES: usize = 8 * 1024 * 1024;

pub struct ClineReader {
    session: String,
    fallback_timestamp_ms: Option<i64>,
    responses: Vec<ResponseUsage>,
    retained_identity_bytes: usize,
    assistant_ordinal: usize,
    current_model: Option<String>,
    current_provider: Option<String>,
}

impl ClineReader {
    pub fn new(session: impl Into<String>) -> Self {
        Self {
            session: session.into(),
            fallback_timestamp_ms: None,
            responses: Vec::new(),
            retained_identity_bytes: 0,
            assistant_ordinal: 0,
            current_model: None,
            current_provider: None,
        }
    }

    pub fn with_fallback_timestamp(mut self, timestamp_ms: Option<i64>) -> Self {
        self.fallback_timestamp_ms = timestamp_ms.filter(|value| *value >= 0);
        self
    }

    pub fn ingest(&mut self, record: &[u8]) -> Result<(), ReaderError> {
        let value: Value =
            serde_json::from_slice(record).map_err(|_| ReaderError::InvalidRecord)?;
        let object = value.as_object().ok_or(ReaderError::InvalidRecord)?;
        let messages = object
            .get("messages")
            .and_then(Value::as_array)
            .ok_or(ReaderError::Unsupported("Cline messages"))?;
        let session = match object.get("sessionId") {
            None | Some(Value::Null) => self.session.clone(),
            Some(Value::String(value)) if valid_identity(value) => value.clone(),
            Some(_) => return Err(ReaderError::InvalidRecord),
        };
        if !valid_identity(&session) {
            return Err(ReaderError::InvalidRecord);
        }
        self.session = session.clone();
        let agent = object
            .get("agent")
            .and_then(Value::as_str)
            .filter(|value| valid_identity(value));
        for message in messages {
            self.ingest_message(&session, agent, message)?;
        }
        Ok(())
    }

    pub fn snapshot(&self) -> Vec<ResponseUsage> {
        self.responses.clone()
    }

    pub fn has_supported_usage(&self) -> bool {
        !self.responses.is_empty()
    }

    fn ingest_message(
        &mut self,
        session: &str,
        agent: Option<&str>,
        message: &Value,
    ) -> Result<(), ReaderError> {
        let object = message.as_object().ok_or(ReaderError::InvalidRecord)?;
        if object.get("role").and_then(Value::as_str) != Some("assistant") {
            return Ok(());
        }
        let Some(metrics) = object.get("metrics") else {
            return Ok(());
        };
        let metrics = metrics.as_object().ok_or(ReaderError::InvalidRecord)?;
        let model_info = object.get("modelInfo").and_then(Value::as_object);
        if let Some(model) = model_info
            .and_then(|model_info| model_info.get("id"))
            .and_then(Value::as_str)
            .filter(|value| valid_identity(value))
        {
            self.current_model = Some(model.to_owned());
        }
        if let Some(provider) = model_info
            .and_then(|model_info| model_info.get("provider"))
            .and_then(Value::as_str)
            .filter(|value| valid_identity(value))
        {
            self.current_provider = Some(provider.to_owned());
        }
        let Some(model) = self.current_model.as_deref() else {
            return Err(ReaderError::Unsupported("Cline message model"));
        };
        let provider = self.current_provider.clone();
        let input_tokens = optional_u64(metrics, "inputTokens")?;
        let output = optional_u64(metrics, "outputTokens")?;
        let cache_read = optional_u64(metrics, "cacheReadTokens")?;
        let cache_write = optional_u64(metrics, "cacheWriteTokens")?;
        let cached = cache_read
            .checked_add(cache_write)
            .ok_or(ReaderError::Overflow)?;
        let input = input_tokens
            .checked_sub(cached)
            .ok_or(ReaderError::Unsupported("Cline cache/input"))?;
        let total = input_tokens
            .checked_add(output)
            .ok_or(ReaderError::Overflow)?;
        if total == 0 {
            return Ok(());
        }
        let timestamp = object
            .get("ts")
            .and_then(flexible_timestamp)
            .or(self.fallback_timestamp_ms)
            .ok_or(ReaderError::Unsupported("Cline message timestamp"))?;
        let id = object.get("id").and_then(Value::as_str);
        let dedup_key = match id {
            None => Some(format!("cline-cli:{session}:{}", self.assistant_ordinal)),
            Some(id) if valid_identity(id) => Some(format!("cline-cli:{session}:{id}")),
            Some(_) => return Err(ReaderError::InvalidRecord),
        };
        if dedup_key
            .as_ref()
            .is_some_and(|key| key.len() > MAX_IDENTITY_BYTES)
        {
            return Err(ReaderError::Unsupported("Cline message identity"));
        }
        let identity_bytes = dedup_key.as_ref().map_or(0, String::len);
        self.retained_identity_bytes = self
            .retained_identity_bytes
            .checked_add(identity_bytes)
            .filter(|bytes| *bytes <= MAX_RETAINED_IDENTITY_BYTES)
            .ok_or(ReaderError::Capacity("Cline identities"))?;
        if self.responses.len() == MAX_RESPONSES {
            return Err(ReaderError::Capacity("Cline responses"));
        }
        self.responses.push(ResponseUsage {
            key: SessionKey::new("cline", session),
            model: model.to_owned(),
            tokens: TokenCounts {
                input,
                output,
                cache_read,
                cache_write,
                reasoning: 0,
            },
            cache_write_1h: 0,
            timestamp_ms: timestamp,
            dedup_key,
            billing_provider_hint: provider,
            service_tier: None,
            agent: agent.map(str::to_owned),
            cost: CostStatus::Unavailable,
        });
        self.assistant_ordinal += 1;
        Ok(())
    }
}

fn optional_u64(object: &Map<String, Value>, key: &str) -> Result<u64, ReaderError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(0),
        Some(Value::Number(number)) => number.as_u64().ok_or(ReaderError::InvalidRecord),
        Some(Value::String(text)) => text
            .trim()
            .parse::<u64>()
            .map_err(|_| ReaderError::InvalidRecord),
        Some(_) => Err(ReaderError::InvalidRecord),
    }
}

fn flexible_timestamp(value: &Value) -> Option<i64> {
    if let Some(text) = value.as_str() {
        if let Ok(parsed) = text.trim().parse::<i64>() {
            return (parsed > 0).then_some(parsed);
        }
        return DateTime::parse_from_rfc3339(text.trim())
            .ok()
            .map(|value| value.timestamp_millis());
    }
    value.as_i64().filter(|value| *value > 0).or_else(|| {
        value
            .as_f64()
            .filter(|value| {
                value.is_finite()
                    && *value > 0.0
                    && *value <= i64::MAX as f64
                    && value.fract() == 0.0
            })
            .map(|value| value as i64)
    })
}

fn valid_identity(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_IDENTITY_BYTES && !value.chars().any(char::is_control)
}
