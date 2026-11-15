//! MiniMax Code headless `stream-json` events, adapted from the pinned
//! Tokscale `mcode.rs` reader. MIT © 2025 Junho Yeo; see
//! docs/migration/parser-provenance.md.
//!
//! Legacy assistant `message` usage is buffered by turn and emitted only when a
//! final `exec.result` names the model/provider. Envelope streams carry usage
//! and attribution in `exec.completed.result`. A capture without a model-bearing
//! final result is ignored rather than priced against a guessed model.

use super::timestamps::{counter, tokens_total};
use super::{CostStatus, ReaderError, ResponseUsage};
use crate::accounting::{SessionKey, TokenCounts};
use serde_json::Value;
use std::collections::BTreeMap;

const MAX_RESPONSES: usize = 16_384;

struct PendingUsage {
    timestamp_ms: i64,
    tokens: TokenCounts,
}

pub struct McodeReader {
    fallback_timestamp_ms: Option<i64>,
    pending: BTreeMap<String, Vec<PendingUsage>>,
    rows: Vec<ResponseUsage>,
}

impl McodeReader {
    pub fn new() -> Self {
        Self {
            fallback_timestamp_ms: None,
            pending: BTreeMap::new(),
            rows: Vec::new(),
        }
    }

    pub fn with_fallback_timestamp(mut self, timestamp_ms: Option<i64>) -> Self {
        self.fallback_timestamp_ms = timestamp_ms.filter(|value| *value >= 0);
        self
    }

    pub fn ingest(&mut self, record: &[u8]) -> Result<(), ReaderError> {
        let value: Value =
            serde_json::from_slice(record).map_err(|_| ReaderError::InvalidRecord)?;
        match value.get("type").and_then(Value::as_str) {
            Some("message") => self.ingest_message(&value),
            Some("exec.result") | Some("exec.completed") => self.ingest_result(&value),
            _ => Ok(()),
        }
    }

    fn ingest_message(&mut self, value: &Value) -> Result<(), ReaderError> {
        let Some(message) = value.get("message") else {
            return Ok(());
        };
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            return Ok(());
        }
        let Some(turn_id) = message
            .get("turnId")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|turn| !turn.is_empty())
        else {
            return Ok(());
        };
        let Some(usage) = message.get("usage") else {
            return Ok(());
        };
        let tokens = usage_tokens(usage)?;
        if tokens_total(tokens)? == 0 {
            return Ok(());
        }
        let timestamp_ms = normalize_timestamp(
            message
                .get("timestamp")
                .and_then(Value::as_i64)
                .or(self.fallback_timestamp_ms)
                .ok_or(ReaderError::InvalidTimestamp)?,
        )?;
        self.pending
            .entry(turn_id.to_owned())
            .or_default()
            .push(PendingUsage {
                timestamp_ms,
                tokens,
            });
        Ok(())
    }

    fn ingest_result(&mut self, value: &Value) -> Result<(), ReaderError> {
        let event_type = value.get("type").and_then(Value::as_str).unwrap_or("");
        let Some(turn_id) = value
            .get("turnId")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|turn| !turn.is_empty())
        else {
            return Ok(());
        };
        let Some(session_id) = value
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|session| super::tencent_buddy::valid_identity(session))
        else {
            return Ok(());
        };
        let result = if event_type == "exec.completed" {
            value
                .get("result")
                .ok_or(ReaderError::Unsupported("mcode completed result"))?
        } else {
            value
        };
        let Some(model) = result.get("model") else {
            return Ok(());
        };
        let provider = model
            .get("providerId")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|provider| !provider.is_empty())
            .ok_or(ReaderError::Unsupported("mcode provider"))?;
        let model_id = model
            .get("modelId")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .ok_or(ReaderError::Unsupported("mcode model"))?;
        let usages = if event_type == "exec.completed" {
            let usage = result
                .get("usage")
                .ok_or(ReaderError::Unsupported("mcode completed usage"))?;
            let tokens = usage_tokens(usage)?;
            if tokens_total(tokens)? == 0 {
                return Ok(());
            }
            let timestamp_ms = normalize_timestamp(
                value
                    .get("timestampMs")
                    .and_then(Value::as_i64)
                    .or(self.fallback_timestamp_ms)
                    .ok_or(ReaderError::InvalidTimestamp)?,
            )?;
            vec![PendingUsage {
                timestamp_ms,
                tokens,
            }]
        } else {
            self.pending.remove(turn_id).unwrap_or_default()
        };
        for (index, usage) in usages.into_iter().enumerate() {
            if self.rows.len() == MAX_RESPONSES {
                return Err(ReaderError::Capacity("mcode responses"));
            }
            let identity = format!(
                "mcode:{session_id}:{turn_id}:{index}:{}:{}:{}:{}",
                usage.tokens.input,
                usage.tokens.output,
                usage.tokens.cache_read,
                usage.tokens.cache_write
            );
            self.rows.push(ResponseUsage {
                key: SessionKey::new("mcode", session_id),
                model: model_id.to_owned(),
                tokens: usage.tokens,
                cache_write_1h: 0,
                timestamp_ms: usage.timestamp_ms,
                dedup_key: Some(identity),
                billing_provider_hint: Some(provider.to_owned()),
                service_tier: None,
                agent: Some("headless".to_owned()),
                cost: CostStatus::Unavailable,
            });
        }
        Ok(())
    }

    pub fn snapshot(&self) -> Vec<ResponseUsage> {
        self.rows.clone()
    }

    pub fn has_supported_usage(&self) -> bool {
        !self.rows.is_empty()
    }
}

fn usage_tokens(usage: &Value) -> Result<TokenCounts, ReaderError> {
    Ok(TokenCounts {
        input: counter(usage, "inputTokens")?,
        output: counter(usage, "outputTokens")?,
        cache_read: counter(usage, "cacheReadTokens")?,
        cache_write: counter(usage, "cacheWriteTokens")?,
        reasoning: 0,
    })
}

fn normalize_timestamp(timestamp: i64) -> Result<i64, ReaderError> {
    if timestamp <= 0 {
        return Err(ReaderError::InvalidTimestamp);
    }
    if timestamp < 10_000_000_000 {
        timestamp
            .checked_mul(1000)
            .ok_or(ReaderError::InvalidTimestamp)
    } else {
        Ok(timestamp)
    }
}
