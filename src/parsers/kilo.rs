//! Kilo CLI message normalization adapted from the pinned Tokscale
//! `kilo.rs`/`opencode_schema.rs` readers. MIT © 2025 Junho Yeo; see
//! docs/migration/parser-provenance.md.
//!
//! Supported input is the `message(id, session_id, data)` table in
//! `~/.local/share/kilo/kilo.db`, with assistant payload `role`, top-level
//! `modelID`/`providerID`, strict `tokens.cache.read`/`write`, and
//! `time.created`. Reasoning is split out of inclusive output once. Kilo's
//! IDE task roots, migration stubs, cost, and provider inference stay
//! unsupported.

use super::{CostStatus, ReaderError, ResponseUsage, jsonl::MAX_RECORD_BYTES};
use crate::accounting::{SessionKey, TokenCounts};
use serde_json::{Map, Value};

pub const MAX_RESPONSES: usize = 16_384;
const MAX_IDENTITY_BYTES: usize = 4_096;
const MAX_RETAINED_IDENTITY_BYTES: usize = 8 * 1024 * 1024;

pub struct KiloReader {
    fallback_timestamp_ms: Option<i64>,
    responses: Vec<ResponseUsage>,
    retained_identity_bytes: usize,
}

impl KiloReader {
    pub fn new() -> Self {
        Self {
            fallback_timestamp_ms: None,
            responses: Vec::new(),
            retained_identity_bytes: 0,
        }
    }

    pub fn with_fallback_timestamp(mut self, timestamp_ms: Option<i64>) -> Self {
        self.fallback_timestamp_ms = timestamp_ms.filter(|value| *value >= 0);
        self
    }

    pub fn ingest(
        &mut self,
        row_id: &str,
        row_session_id: &str,
        data: &[u8],
    ) -> Result<(), ReaderError> {
        if data.len() > MAX_RECORD_BYTES {
            return Err(ReaderError::Capacity("Kilo message"));
        }
        let value: Value = serde_json::from_slice(data).map_err(|_| ReaderError::InvalidRecord)?;
        let object = value.as_object().ok_or(ReaderError::InvalidRecord)?;
        if object.get("role").and_then(Value::as_str) != Some("assistant") {
            return Err(ReaderError::InvalidRecord);
        }
        let Some(tokens) = object.get("tokens").and_then(Value::as_object) else {
            return Err(ReaderError::InvalidRecord);
        };
        let Some(model) = object
            .get("modelID")
            .and_then(Value::as_str)
            .filter(|value| valid_identity(value))
        else {
            return Err(ReaderError::Unsupported("Kilo message model"));
        };
        let provider = object
            .get("providerID")
            .and_then(Value::as_str)
            .filter(|value| valid_identity(value));
        if provider.is_none()
            && object
                .get("providerID")
                .is_some_and(|value| !value.is_null())
        {
            return Err(ReaderError::InvalidRecord);
        }
        let input = required_u64(tokens, "input")?;
        let inclusive_output = required_u64(tokens, "output")?;
        let reasoning = optional_u64(tokens, "reasoning")?;
        let output = inclusive_output
            .checked_sub(reasoning)
            .ok_or(ReaderError::Unsupported("Kilo reasoning/output"))?;
        let cache = tokens
            .get("cache")
            .and_then(Value::as_object)
            .ok_or(ReaderError::Unsupported("Kilo token cache"))?;
        let cache_read = required_u64(cache, "read")
            .map_err(|_| ReaderError::Unsupported("Kilo cache read/write"))?;
        let cache_write = required_u64(cache, "write")
            .map_err(|_| ReaderError::Unsupported("Kilo cache read/write"))?;
        if input == 0 && output == 0 && reasoning == 0 && cache_read == 0 && cache_write == 0 {
            return Ok(());
        }
        let session = match object.get("session_id") {
            None | Some(Value::Null) => row_session_id,
            Some(Value::String(value)) if valid_identity(value) => value.as_str(),
            Some(_) => return Err(ReaderError::InvalidRecord),
        };
        if !valid_identity(session) || !valid_identity(row_id) {
            return Err(ReaderError::InvalidRecord);
        }
        let timestamp = object
            .get("time")
            .and_then(Value::as_object)
            .and_then(|time| time.get("created"))
            .and_then(flexible_timestamp)
            .or(self.fallback_timestamp_ms)
            .ok_or(ReaderError::Unsupported("Kilo message timestamp"))?;
        let identity = format!("kilo:{session}:{row_id}");
        if identity.len() > MAX_IDENTITY_BYTES {
            return Err(ReaderError::Unsupported("Kilo message identity"));
        }
        self.retained_identity_bytes = self
            .retained_identity_bytes
            .checked_add(identity.len())
            .filter(|bytes| *bytes <= MAX_RETAINED_IDENTITY_BYTES)
            .ok_or(ReaderError::Capacity("Kilo identities"))?;
        if self.responses.len() == MAX_RESPONSES {
            return Err(ReaderError::Capacity("Kilo responses"));
        }
        self.responses.push(ResponseUsage {
            key: SessionKey::new("kilo", session),
            model: model.to_owned(),
            tokens: TokenCounts {
                input,
                output,
                cache_read,
                cache_write,
                reasoning,
            },
            cache_write_1h: 0,
            timestamp_ms: timestamp,
            dedup_key: Some(identity),
            billing_provider_hint: provider.map(str::to_owned),
            service_tier: None,
            agent: None,
            workspace_key: None,
            workspace_label: None,
            title: None,
            cost: CostStatus::Unavailable,
        });
        Ok(())
    }

    pub fn snapshot(&self) -> Vec<ResponseUsage> {
        self.responses.clone()
    }

    pub fn has_supported_usage(&self) -> bool {
        !self.responses.is_empty()
    }
}

fn required_u64(object: &Map<String, Value>, key: &str) -> Result<u64, ReaderError> {
    object
        .get(key)
        .and_then(Value::as_u64)
        .ok_or(ReaderError::InvalidRecord)
}

fn optional_u64(object: &Map<String, Value>, key: &str) -> Result<u64, ReaderError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(0),
        Some(value) => value.as_u64().ok_or(ReaderError::InvalidRecord),
    }
}

fn flexible_timestamp(value: &Value) -> Option<i64> {
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

impl Default for KiloReader {
    fn default() -> Self {
        Self::new()
    }
}
