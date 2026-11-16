//! Amp thread normalization adapted from the pinned Tokscale `amp.rs` reader.
//! MIT © 2025 Junho Yeo; see docs/migration/parser-provenance.md.
//!
//! Supported input is Amp thread JSON from `~/.local/share/amp/threads` or the
//! tokscale `amp-cache/threads` sync. The usage ledger is authoritative when it
//! yields token rows; otherwise assistant per-message usage is used. Ledger and
//! message reconciliation, credits/cost, reasoning, and provider inference are
//! not implemented and stay unsupported rather than approximated.

use super::{CostStatus, ReaderError, ResponseUsage};
use crate::accounting::{SessionKey, TokenCounts};
use chrono::DateTime;
use serde_json::{Map, Value};

pub const MAX_RESPONSES: usize = 16_384;
const MAX_IDENTITY_BYTES: usize = 4_096;
const MAX_RETAINED_IDENTITY_BYTES: usize = 8 * 1024 * 1024;

struct AmpRecord {
    usage: ResponseUsage,
}

pub struct AmpReader {
    session: String,
    fallback_timestamp_ms: Option<i64>,
    responses: Vec<ResponseUsage>,
    retained_identity_bytes: usize,
}

impl AmpReader {
    pub fn new(session: impl Into<String>) -> Self {
        Self {
            session: session.into(),
            fallback_timestamp_ms: None,
            responses: Vec::new(),
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
        let object = value.as_object().ok_or(ReaderError::InvalidRecord)?;
        let session = match object.get("id") {
            None | Some(Value::Null) => self.session.clone(),
            Some(Value::String(value)) if valid_identity(value) => value.clone(),
            Some(_) => return Err(ReaderError::InvalidRecord),
        };
        if !valid_identity(&session) {
            return Err(ReaderError::InvalidRecord);
        }
        self.session = session.clone();
        let created_ms = timestamp_value(object.get("created"))?.unwrap_or(0);
        let mut ledger = Vec::new();
        if let Some(events) = object
            .get("usageLedger")
            .and_then(Value::as_object)
            .and_then(|ledger| ledger.get("events"))
            .and_then(Value::as_array)
        {
            for (index, event) in events.iter().enumerate() {
                if let Some(row) = self.ledger_record(&session, created_ms, index, event)? {
                    ledger.push(row);
                }
            }
        }
        if !ledger.is_empty() {
            for record in ledger {
                self.push(record.usage)?;
            }
            return Ok(());
        }
        if let Some(messages) = object.get("messages").and_then(Value::as_array) {
            let mut rows = Vec::new();
            for (index, message) in messages.iter().enumerate() {
                if let Some(row) = self.message_record(&session, created_ms, index, message)? {
                    rows.push(row);
                }
            }
            rows.sort_by_key(|row| row.usage.timestamp_ms);
            for row in rows {
                self.push(row.usage)?;
            }
        }
        Ok(())
    }

    pub fn snapshot(&self) -> Vec<ResponseUsage> {
        self.responses.clone()
    }

    pub fn has_supported_usage(&self) -> bool {
        !self.responses.is_empty()
    }

    fn ledger_record(
        &self,
        session: &str,
        created_ms: i64,
        index: usize,
        event: &Value,
    ) -> Result<Option<AmpRecord>, ReaderError> {
        let object = event.as_object().ok_or(ReaderError::InvalidRecord)?;
        let Some(model) = object
            .get("model")
            .and_then(Value::as_str)
            .filter(|value| valid_identity(value))
        else {
            return Ok(None);
        };
        let tokens = match object.get("tokens") {
            None | Some(Value::Null) => None,
            Some(Value::Object(tokens)) => Some(parse_ledger_tokens(tokens)?),
            Some(_) => return Err(ReaderError::InvalidRecord),
        };
        let Some(tokens) = tokens.filter(|tokens| !is_zero(*tokens)) else {
            return Ok(None);
        };
        let timestamp = timestamp_value(object.get("timestamp"))?
            .or_else(|| (created_ms > 0).then_some(created_ms))
            .or(self.fallback_timestamp_ms)
            .ok_or(ReaderError::Declined("Amp ledger timestamp"))?;
        Ok(Some(AmpRecord {
            usage: ResponseUsage {
                key: SessionKey::new("amp", session),
                model: model.to_owned(),
                tokens,
                cache_write_1h: 0,
                timestamp_ms: timestamp,
                dedup_key: Some(format!("amp:{session}:ledger:{index}")),
                billing_provider_hint: None,
                service_tier: None,
                agent: None,
                workspace_key: None,
                workspace_label: None,
                title: None,
                cost: CostStatus::Unavailable,
            },
        }))
    }

    fn message_record(
        &self,
        session: &str,
        created_ms: i64,
        index: usize,
        message: &Value,
    ) -> Result<Option<AmpRecord>, ReaderError> {
        let object = message.as_object().ok_or(ReaderError::InvalidRecord)?;
        if object.get("role").and_then(Value::as_str) != Some("assistant") {
            return Ok(None);
        }
        let Some(usage) = object.get("usage") else {
            return Ok(None);
        };
        let usage = usage.as_object().ok_or(ReaderError::InvalidRecord)?;
        let Some(model) = usage
            .get("model")
            .and_then(Value::as_str)
            .filter(|value| valid_identity(value))
        else {
            return Ok(None);
        };
        let tokens = parse_message_tokens(usage)?;
        if is_zero(tokens) {
            return Ok(None);
        }
        let message_id = as_i64(object.get("messageId")).unwrap_or(0).max(0);
        let timestamp = timestamp_value(usage.get("timestamp"))?
            .or_else(|| {
                (created_ms > 0)
                    .then_some(created_ms.saturating_add(message_id.saturating_mul(1_000)))
            })
            .or(self.fallback_timestamp_ms)
            .ok_or(ReaderError::Declined("Amp message timestamp"))?;
        let identity = if message_id > 0 {
            format!("amp:{session}:message:{message_id}")
        } else {
            format!("amp:{session}:message:{index}")
        };
        Ok(Some(AmpRecord {
            usage: ResponseUsage {
                key: SessionKey::new("amp", session),
                model: model.to_owned(),
                tokens,
                cache_write_1h: 0,
                timestamp_ms: timestamp,
                dedup_key: Some(identity),
                billing_provider_hint: None,
                service_tier: None,
                agent: None,
                workspace_key: None,
                workspace_label: None,
                title: None,
                cost: CostStatus::Unavailable,
            },
        }))
    }

    fn push(&mut self, usage: ResponseUsage) -> Result<(), ReaderError> {
        if self.responses.len() == MAX_RESPONSES {
            return Err(ReaderError::Capacity("Amp responses"));
        }
        let identity_bytes = usage.dedup_key.as_ref().map_or(0, String::len);
        if identity_bytes > MAX_IDENTITY_BYTES {
            return Err(ReaderError::Declined("Amp message identity"));
        }
        self.retained_identity_bytes = self
            .retained_identity_bytes
            .checked_add(identity_bytes)
            .filter(|bytes| *bytes <= MAX_RETAINED_IDENTITY_BYTES)
            .ok_or(ReaderError::Capacity("Amp identities"))?;
        self.responses.push(usage);
        Ok(())
    }
}

fn parse_ledger_tokens(object: &Map<String, Value>) -> Result<TokenCounts, ReaderError> {
    Ok(TokenCounts {
        input: optional_u64(object, "input")?,
        output: optional_u64(object, "output")?,
        cache_read: optional_u64(object, "cacheReadInputTokens")?,
        cache_write: optional_u64(object, "cacheCreationInputTokens")?,
        reasoning: 0,
    })
}

fn parse_message_tokens(object: &Map<String, Value>) -> Result<TokenCounts, ReaderError> {
    Ok(TokenCounts {
        input: optional_u64(object, "inputTokens")?,
        output: optional_u64(object, "outputTokens")?,
        cache_read: optional_u64(object, "cacheReadInputTokens")?,
        cache_write: optional_u64(object, "cacheCreationInputTokens")?,
        reasoning: 0,
    })
}

fn is_zero(tokens: TokenCounts) -> bool {
    tokens == TokenCounts::default()
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

fn as_i64(value: Option<&Value>) -> Option<i64> {
    match value {
        Some(Value::Number(number)) => number.as_i64(),
        Some(Value::String(text)) => text.trim().parse::<i64>().ok(),
        _ => None,
    }
}

fn timestamp_value(value: Option<&Value>) -> Result<Option<i64>, ReaderError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(DateTime::parse_from_rfc3339(text.trim())
            .ok()
            .map(|value| value.timestamp_millis())
            .filter(|value| *value != 0)),
        Some(Value::Number(number)) => {
            if let Some(value) = number.as_i64() {
                return Ok((value != 0).then_some(value));
            }
            let value = number.as_f64().ok_or(ReaderError::InvalidTimestamp)?;
            if !value.is_finite() || value <= 0.0 || value > i64::MAX as f64 || value.fract() != 0.0
            {
                return Err(ReaderError::InvalidTimestamp);
            }
            Ok(Some(value as i64))
        }
        Some(_) => Err(ReaderError::InvalidTimestamp),
    }
}

fn valid_identity(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_IDENTITY_BYTES && !value.chars().any(char::is_control)
}
