//! MiMo Code session normalization adapted from the pinned Tokscale `micode.rs`
//! reader and its `OpenCodeSchemaConfig::micode()` deltas. MIT © 2025 Junho Yeo;
//! see docs/migration/parser-provenance.md.
//!
//! MiMo Code and Xiaomi MiMo AI share one engine, and that engine stores its
//! assistant turns in OpenCode's `message` schema. The interpretation is
//! therefore OpenCode's, with exactly the departures the pinned reader declares:
//!
//! - `strict_cache: false`. A MiMo assistant message may omit `cache`, or its
//!   `read`/`write` members; requiring them (as OpenCode does) would silently
//!   drop the message. A missing member is zero.
//! - `normalize_epoch_seconds: true`. Some MiMo builds write epoch seconds
//!   where OpenCode writes milliseconds, which landed dates ~1000x in the past.
//! - `cost_provenance: WhenReported`. An explicit `"cost": 0.0` is a real
//!   MiMo-reported price, so it must survive repricing rather than be treated
//!   as absent.
//! - `namespace_rowid_dedup_key: true`. MiMo keeps channel-suffixed databases
//!   whose row ids are only unique per file, so the identity includes the file.
//!
//! Not implemented, and recorded as unsupported coverage rather than guessed:
//! splitting desktop sessions into a separate client via `session.version`
//! (every MiMo session is reported as the single catalog client `mimo`), and
//! the agent/mode attribution and duration the pinned reader also records.

use super::{CostStatus, ReaderError, ResponseUsage, jsonl::MAX_RECORD_BYTES};
use crate::accounting::{SessionKey, TokenCounts};
use serde_json::{Map, Value};

pub const MAX_RESPONSES: usize = 16_384;
const MAX_IDENTITY_BYTES: usize = 4_096;
const MAX_RETAINED_IDENTITY_BYTES: usize = 8 * 1024 * 1024;

/// Timestamps at or below this are epoch seconds rather than milliseconds.
/// One trillion milliseconds is 2001, and one trillion seconds is far beyond
/// any plausible record, so the two ranges do not overlap.
const EPOCH_SECONDS_MAX: f64 = 1e12;

pub const CLIENT: &str = "mimo";

/// One SQLite usage candidate. The adapter supplies the database file's name so
/// the identity can be namespaced across channel-suffixed databases.
pub struct MimoRecord<'a> {
    pub message_id: &'a str,
    pub session_id: &'a str,
    pub data: &'a [u8],
    pub fallback_timestamp_ms: Option<i64>,
    pub database: &'a str,
}

#[derive(Default)]
pub struct MimoReader {
    responses: Vec<ResponseUsage>,
    retained_identity_bytes: usize,
}

impl MimoReader {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn ingest(&mut self, record: MimoRecord<'_>) -> Result<(), ReaderError> {
        let Some(usage) = normalize_record(&record)? else {
            return Ok(());
        };
        if self.responses.len() == MAX_RESPONSES {
            return Err(ReaderError::Capacity("MiMo responses"));
        }
        let identity_bytes = usage.dedup_key.as_ref().map_or(0, String::len);
        self.retained_identity_bytes = self
            .retained_identity_bytes
            .checked_add(identity_bytes)
            .filter(|bytes| *bytes <= MAX_RETAINED_IDENTITY_BYTES)
            .ok_or(ReaderError::Capacity("MiMo identities"))?;
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

pub fn normalize_record(record: &MimoRecord<'_>) -> Result<Option<ResponseUsage>, ReaderError> {
    if record.data.len() > MAX_RECORD_BYTES {
        return Err(ReaderError::Capacity("MiMo message"));
    }
    if !valid_identity(record.session_id)
        || !valid_identity(record.message_id)
        || !valid_identity(record.database)
    {
        return Err(ReaderError::InvalidRecord);
    }
    let value: Value =
        serde_json::from_slice(record.data).map_err(|_| ReaderError::InvalidRecord)?;
    let object = value.as_object().ok_or(ReaderError::InvalidRecord)?;
    // MiMo's query already selects `role = 'assistant'`, but a payload that
    // contradicts it is not assistant usage and is not guessed at.
    match object.get("role") {
        None | Some(Value::Null) => {}
        Some(role) if role.as_str() == Some("assistant") => {}
        Some(_) => return Err(ReaderError::Declined("MiMo assistant role")),
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
        .ok_or(ReaderError::Declined("MiMo reasoning/output"))?;
    // Unlike OpenCode, a missing cache block is zero rather than a rejection.
    let (cache_read, cache_write) = match tokens.get("cache") {
        None | Some(Value::Null) => (0, 0),
        Some(Value::Object(cache)) => (optional_u64(cache, "read")?, optional_u64(cache, "write")?),
        Some(_) => return Err(ReaderError::InvalidRecord),
    };
    let timestamp_ms = resolve_timestamp(object, record.fallback_timestamp_ms)?;
    let cost = resolve_cost(object)?;
    let identity = format!(
        "mimo:{}:{}:{}",
        record.database, record.session_id, record.message_id
    );
    if identity.len() > MAX_IDENTITY_BYTES {
        return Err(ReaderError::Declined("MiMo message identity"));
    }
    Ok(Some(ResponseUsage {
        key: SessionKey::new(CLIENT, record.session_id),
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
        cost,
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
        .ok_or(ReaderError::Declined("MiMo message model"))
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

/// An explicit MiMo-reported price in nano-USD.
///
/// `WhenReported` is the pinned reader's rule for this client: a `cost` that is
/// present is authoritative even when it is `0`, because a free-promo turn
/// genuinely bills nothing and must not be repriced at the paid rate. A
/// negative or non-finite value is rejected rather than clamped.
fn resolve_cost(object: &Map<String, Value>) -> Result<CostStatus, ReaderError> {
    let Some(cost) = object.get("cost") else {
        return Ok(CostStatus::Unavailable);
    };
    if cost.is_null() {
        return Ok(CostStatus::Unavailable);
    }
    let cost = cost.as_f64().ok_or(ReaderError::InvalidRecord)?;
    if !cost.is_finite() || cost < 0.0 || cost > 1_000_000.0 {
        return Err(ReaderError::InvalidRecord);
    }
    let nano = (cost * 1_000_000_000.0).round();
    if !nano.is_finite() || nano < 0.0 || nano > u64::MAX as f64 {
        return Err(ReaderError::InvalidRecord);
    }
    Ok(CostStatus::Reported(nano as u64))
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
        .ok_or(ReaderError::Declined("MiMo message timestamp"))
}

/// MiMo may write epoch seconds where OpenCode writes milliseconds, so a value
/// at or below the seconds ceiling is scaled up rather than taken literally.
fn number_to_ms(value: &Value) -> Result<i64, ReaderError> {
    let Value::Number(number) = value else {
        return Err(ReaderError::InvalidTimestamp);
    };
    if let Some(value) = number.as_i64() {
        if value < 0 {
            return Err(ReaderError::InvalidTimestamp);
        }
        return normalize_epoch(value as f64);
    }
    if let Some(value) = number.as_u64() {
        let value = i64::try_from(value).map_err(|_| ReaderError::InvalidTimestamp)?;
        return normalize_epoch(value as f64);
    }
    let value = number.as_f64().ok_or(ReaderError::InvalidTimestamp)?;
    if !value.is_finite() || value < 0.0 || value > i64::MAX as f64 || value.fract() != 0.0 {
        return Err(ReaderError::InvalidTimestamp);
    }
    normalize_epoch(value)
}

fn normalize_epoch(value: f64) -> Result<i64, ReaderError> {
    let millis = if value <= EPOCH_SECONDS_MAX {
        value * 1000.0
    } else {
        value
    };
    if !millis.is_finite() || millis < 0.0 || millis > i64::MAX as f64 {
        return Err(ReaderError::InvalidTimestamp);
    }
    Ok(millis as i64)
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
