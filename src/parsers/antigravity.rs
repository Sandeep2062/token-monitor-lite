//! Antigravity normalized usage-cache JSONL, adapted from the pinned Tokscale
//! `antigravity.rs` reader. MIT © 2025 Junho Yeo; see
//! docs/migration/parser-provenance.md.
//!
//! Supported rows are `session_meta` (model fallback) and `usage` records with
//! direct counters. Native Antigravity conversation SQLite stores are not read
//! by this cache reader.

use super::timestamps::{counter, tokens_total};
use super::{CostStatus, ReaderError, ResponseUsage};
use crate::accounting::{SessionKey, TokenCounts};
use serde_json::Value;

const MAX_RESPONSES: usize = 16_384;

pub struct AntigravityReader {
    session: String,
    fallback_timestamp_ms: Option<i64>,
    session_model: Option<String>,
    rows: Vec<ResponseUsage>,
    ordinal: usize,
}

impl AntigravityReader {
    pub fn new(session: impl Into<String>) -> Self {
        Self {
            session: session.into(),
            fallback_timestamp_ms: None,
            session_model: None,
            rows: Vec::new(),
            ordinal: 0,
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
            Some("session_meta") => {
                if let Some(model) = value
                    .get("modelId")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|model| !model.is_empty())
                {
                    self.session_model = Some(model.to_owned());
                }
                return Ok(());
            }
            Some("usage") => {}
            _ => return Ok(()),
        }
        let session = value
            .get("sessionId")
            .and_then(Value::as_str)
            .filter(|session| super::tencent_buddy::valid_identity(session))
            .unwrap_or(self.session.as_str());
        let timestamp = value
            .get("timestamp")
            .and_then(|timestamp| {
                timestamp
                    .as_i64()
                    .or_else(|| {
                        timestamp
                            .as_u64()
                            .and_then(|value| i64::try_from(value).ok())
                    })
                    .or_else(|| {
                        timestamp
                            .as_str()
                            .and_then(super::timestamps::parse_timestamp_str)
                    })
            })
            .filter(|timestamp| *timestamp > 0)
            .or(self.fallback_timestamp_ms)
            .ok_or(ReaderError::InvalidTimestamp)?;
        let model = value
            .get("modelId")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .map(str::to_owned)
            .or_else(|| self.session_model.clone())
            .unwrap_or_else(|| "unknown".to_owned());
        let tokens = TokenCounts {
            input: counter(&value, "input")?,
            output: counter(&value, "output")?,
            cache_read: counter(&value, "cacheRead")?,
            cache_write: counter(&value, "cacheWrite")?,
            reasoning: counter(&value, "reasoning")?,
        };
        if tokens_total(tokens)? == 0 {
            return Ok(());
        }
        let identity = value
            .get("responseId")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|identity| !identity.is_empty())
            .map(|identity| format!("antigravity:{session}:{identity}"))
            .unwrap_or_else(|| format!("antigravity:{session}:line:{}", self.ordinal));
        self.ordinal += 1;
        if self.rows.len() == MAX_RESPONSES {
            return Err(ReaderError::Capacity("Antigravity responses"));
        }
        self.rows.push(ResponseUsage {
            key: SessionKey::new("antigravity", session),
            model,
            tokens,
            cache_write_1h: 0,
            timestamp_ms: timestamp,
            dedup_key: Some(identity),
            billing_provider_hint: value
                .get("providerId")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|provider| !provider.is_empty())
                .map(str::to_owned),
            service_tier: None,
            agent: None,
            cost: CostStatus::Unavailable,
        });
        Ok(())
    }

    pub fn snapshot(&self) -> Vec<ResponseUsage> {
        self.rows.clone()
    }

    pub fn has_supported_usage(&self) -> bool {
        !self.rows.is_empty()
    }
}
