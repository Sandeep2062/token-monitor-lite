//! OpenCode SQLite assistant-message normalization adapted from the pinned
//! Tokscale OpenCode readers (`opencode.rs`, `opencode_schema.rs`) at
//! d3df620b2cdd9502c70aececdbc6ece61fe26fc9. MIT © 2025 Junho Yeo; see
//! docs/migration/parser-provenance.md.
//!
//! The adapter in `source_inputs` fences and pages complete SQLite rows; this
//! module only interprets a row's payload and never opens the database.
//!
//! Supported usage is one ordinary/compaction assistant message with explicit
//! `tokens` plus `cache.read`/`cache.write`, a resolved model, and a millisecond
//! timestamp. OpenCode's `input` excludes cache read/write, while `reasoning` is
//! a subset of inclusive `output`; it is split out once so the canonical
//! components never double-count it. Unlike the pinned reader, negative or
//! fractional counters and timestamps are visible unsupported instead of being
//! clamped or truncated. Cost, agent/mode attribution, migrated legacy
//! summary/subtask restoration, and content-fingerprint fork merging are
//! deliberately unsupported.

use super::{CostStatus, ReaderError, ResponseUsage, jsonl::MAX_RECORD_BYTES};
use crate::accounting::{SessionKey, TokenCounts};
use serde_json::{Map, Value};

pub const MAX_RESPONSES: usize = 16_384;
const MAX_IDENTITY_BYTES: usize = 4_096;
const MAX_RETAINED_IDENTITY_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenCodeKind {
    Assistant,
    Compaction,
}

/// One SQLite usage candidate. The adapter decides current-versus-legacy
/// precedence and supplies the row's `time_created` column as a fallback when
/// the payload omits `$.time.created`.
pub struct OpenCodeRecord<'a> {
    pub message_id: &'a str,
    pub session_id: &'a str,
    pub kind: OpenCodeKind,
    pub data: &'a [u8],
    pub fallback_timestamp_ms: Option<i64>,
}

#[derive(Default)]
pub struct OpenCodeReader {
    responses: Vec<ResponseUsage>,
    retained_identity_bytes: usize,
}

impl OpenCodeReader {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn ingest(&mut self, record: OpenCodeRecord<'_>) -> Result<(), ReaderError> {
        let Some(usage) = normalize_record(&record)? else {
            return Ok(());
        };
        if self.responses.len() == MAX_RESPONSES {
            return Err(ReaderError::Capacity("OpenCode responses"));
        }
        let identity_bytes = usage.dedup_key.as_ref().map_or(0, String::len);
        self.retained_identity_bytes = self
            .retained_identity_bytes
            .checked_add(identity_bytes)
            .filter(|bytes| *bytes <= MAX_RETAINED_IDENTITY_BYTES)
            .ok_or(ReaderError::Capacity("OpenCode identities"))?;
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

pub fn normalize_record(record: &OpenCodeRecord<'_>) -> Result<Option<ResponseUsage>, ReaderError> {
    if record.data.len() > MAX_RECORD_BYTES {
        return Err(ReaderError::Capacity("OpenCode message"));
    }
    if !valid_identity(record.session_id) || !valid_identity(record.message_id) {
        return Err(ReaderError::InvalidRecord);
    }
    let value: Value =
        serde_json::from_slice(record.data).map_err(|_| ReaderError::InvalidRecord)?;
    let object = value.as_object().ok_or(ReaderError::InvalidRecord)?;
    match object.get("role") {
        None | Some(Value::Null) => {}
        Some(role) if role.as_str() == Some("assistant") => {}
        Some(_) => return Err(ReaderError::Declined("OpenCode assistant role")),
    }
    let tokens = match object.get("tokens") {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Object(tokens)) => tokens,
        Some(_) => return Err(ReaderError::InvalidRecord),
    };
    let model = resolve_model(object)?;
    let provider = resolve_provider(object)?;
    let input = required_u64(tokens, "input")?;
    let inclusive_output = required_u64(tokens, "output")?;
    let reasoning = optional_u64(tokens, "reasoning")?;
    let output = inclusive_output
        .checked_sub(reasoning)
        .ok_or(ReaderError::Declined("OpenCode reasoning/output"))?;
    let cache = match tokens.get("cache") {
        Some(Value::Object(cache)) => cache,
        Some(Value::Null) | None => {
            return Err(ReaderError::Declined("OpenCode token cache"));
        }
        Some(_) => return Err(ReaderError::InvalidRecord),
    };
    let cache_read = required_u64(cache, "read")
        .map_err(|_| ReaderError::Declined("OpenCode cache read/write"))?;
    let cache_write = required_u64(cache, "write")
        .map_err(|_| ReaderError::Declined("OpenCode cache read/write"))?;
    let timestamp_ms = resolve_timestamp(object, record.fallback_timestamp_ms)?;
    let identity = format!("opencode:{}:{}", record.session_id, record.message_id);
    if identity.len() > MAX_IDENTITY_BYTES {
        return Err(ReaderError::Declined("OpenCode message identity"));
    }
    Ok(Some(ResponseUsage {
        key: SessionKey::new("opencode", record.session_id),
        model: model.to_owned(),
        tokens: TokenCounts {
            input,
            output,
            cache_read,
            cache_write,
            reasoning,
        },
        cache_write_1h: 0,
        timestamp_ms,
        dedup_key: Some(identity),
        billing_provider_hint: provider,
        service_tier: None,
        agent: None,
        workspace_key: None,
        workspace_label: None,
        title: None,
        cost: CostStatus::Unavailable,
    }))
}

fn resolve_model(object: &Map<String, Value>) -> Result<&str, ReaderError> {
    match object.get("modelID") {
        None | Some(Value::Null) => {}
        Some(model) => {
            return model
                .as_str()
                .filter(|value| valid_identity(value))
                .ok_or(ReaderError::InvalidRecord);
        }
    }
    object
        .get("model")
        .and_then(Value::as_object)
        .and_then(|model| model.get("id"))
        .and_then(Value::as_str)
        .filter(|value| valid_identity(value))
        .ok_or(ReaderError::Declined("OpenCode message model"))
}

fn resolve_provider(object: &Map<String, Value>) -> Result<Option<String>, ReaderError> {
    match object.get("providerID") {
        None | Some(Value::Null) => {}
        Some(Value::String(value)) if valid_identity(value) => return Ok(Some(value.clone())),
        Some(_) => return Err(ReaderError::InvalidRecord),
    }
    match object
        .get("model")
        .and_then(Value::as_object)
        .and_then(|model| model.get("providerID"))
    {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if valid_identity(value) => Ok(Some(value.clone())),
        Some(_) => Err(ReaderError::InvalidRecord),
    }
}

fn resolve_timestamp(
    object: &Map<String, Value>,
    fallback: Option<i64>,
) -> Result<i64, ReaderError> {
    if let Some(time) = object.get("time") {
        let time = time.as_object().ok_or(ReaderError::InvalidRecord)?;
        match time.get("created") {
            None | Some(Value::Null) => {}
            Some(created) => return number_to_ms(created),
        }
    }
    fallback
        .filter(|timestamp| *timestamp >= 0)
        .ok_or(ReaderError::Declined("OpenCode message timestamp"))
}

fn number_to_ms(value: &Value) -> Result<i64, ReaderError> {
    let Value::Number(number) = value else {
        return Err(ReaderError::InvalidTimestamp);
    };
    if let Some(value) = number.as_i64() {
        return if value < 0 {
            Err(ReaderError::InvalidTimestamp)
        } else {
            Ok(value)
        };
    }
    if let Some(value) = number.as_u64() {
        return i64::try_from(value).map_err(|_| ReaderError::InvalidTimestamp);
    }
    let value = number.as_f64().ok_or(ReaderError::InvalidTimestamp)?;
    if !value.is_finite() || value < 0.0 || value > i64::MAX as f64 || value.fract() != 0.0 {
        return Err(ReaderError::InvalidTimestamp);
    }
    Ok(value as i64)
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

fn valid_identity(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_IDENTITY_BYTES && !value.chars().any(char::is_control)
}
