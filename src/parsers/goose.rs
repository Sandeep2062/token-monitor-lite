//! Goose session-row normalization adapted from the pinned Tokscale
//! `goose.rs` reader. MIT © 2025 Junho Yeo; see
//! docs/migration/parser-provenance.md.
//!
//! Goose stores one cumulative row per session. The accumulated counters win
//! independently over the matching non-accumulated counter, an accumulated
//! zero is authoritative, and the model name comes from `model_config_json`.
//! Goose has no reasoning column; the pinned reader infers a remainder from the
//! total. The native reader refuses a non-zero unclassified remainder instead
//! of inventing reasoning tokens.

use super::{CostStatus, ReaderError, ResponseUsage};
use crate::accounting::{SessionKey, TokenCounts};
use chrono::{DateTime, NaiveDate, NaiveDateTime};

pub const MAX_RESPONSES: usize = 16_384;
const MAX_IDENTITY_BYTES: usize = 4_096;
const MAX_RETAINED_IDENTITY_BYTES: usize = 8 * 1024 * 1024;

pub struct GooseRow<'a> {
    pub id: &'a str,
    pub model_config_json: &'a str,
    pub provider_name: Option<&'a str>,
    pub created_at: &'a str,
    pub total_tokens: Option<i64>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub accumulated_total_tokens: Option<i64>,
    pub accumulated_input_tokens: Option<i64>,
    pub accumulated_output_tokens: Option<i64>,
}

pub struct GooseReader {
    fallback_timestamp_ms: Option<i64>,
    responses: Vec<ResponseUsage>,
    retained_identity_bytes: usize,
}

impl GooseReader {
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

    pub fn ingest(&mut self, row: &GooseRow<'_>) -> Result<(), ReaderError> {
        if !valid_identity(row.id) {
            return Err(ReaderError::InvalidRecord);
        }
        let Some(model) = parse_model_config(row.model_config_json) else {
            return Ok(());
        };
        let input = non_negative(
            row.accumulated_input_tokens
                .or(row.input_tokens)
                .unwrap_or(0),
        )?;
        let output = non_negative(
            row.accumulated_output_tokens
                .or(row.output_tokens)
                .unwrap_or(0),
        )?;
        let total = non_negative(
            row.accumulated_total_tokens
                .or(row.total_tokens)
                .unwrap_or(0),
        )?;
        if input == 0 && output == 0 && total == 0 {
            return Ok(());
        }
        let summed = input.checked_add(output).ok_or(ReaderError::Overflow)?;
        if total != 0 && total != summed {
            return Err(ReaderError::Declined("Goose unclassified remainder"));
        }
        let timestamp = parse_created_at(row.created_at)
            .or(self.fallback_timestamp_ms)
            .ok_or(ReaderError::Declined("Goose session timestamp"))?;
        let identity = format!("goose:{}", row.id);
        if identity.len() > MAX_IDENTITY_BYTES {
            return Err(ReaderError::Declined("Goose session identity"));
        }
        self.retained_identity_bytes = self
            .retained_identity_bytes
            .checked_add(identity.len())
            .filter(|bytes| *bytes <= MAX_RETAINED_IDENTITY_BYTES)
            .ok_or(ReaderError::Capacity("Goose identities"))?;
        if self.responses.len() == MAX_RESPONSES {
            return Err(ReaderError::Capacity("Goose responses"));
        }
        self.responses.push(ResponseUsage {
            key: SessionKey::new("goose", row.id),
            model,
            tokens: TokenCounts {
                input,
                output,
                cache_read: 0,
                cache_write: 0,
                reasoning: 0,
            },
            cache_write_1h: 0,
            timestamp_ms: timestamp,
            dedup_key: Some(identity),
            billing_provider_hint: row
                .provider_name
                .filter(|value| valid_identity(value))
                .map(str::to_owned),
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

fn parse_model_config(json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let name = value.get("model_name")?.as_str()?.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

fn non_negative(value: i64) -> Result<u64, ReaderError> {
    u64::try_from(value).map_err(|_| ReaderError::InvalidRecord)
}

fn parse_created_at(value: &str) -> Option<i64> {
    if let Ok(value) = DateTime::parse_from_rfc3339(value.trim()) {
        return Some(value.timestamp_millis());
    }
    if let Ok(value) = NaiveDateTime::parse_from_str(value.trim(), "%Y-%m-%d %H:%M:%S") {
        return Some(value.and_utc().timestamp_millis());
    }
    NaiveDate::parse_from_str(value.trim(), "%Y-%m-%d")
        .ok()
        .and_then(|date| date.and_hms_opt(0, 0, 0))
        .map(|value| value.and_utc().timestamp_millis())
}

fn valid_identity(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_IDENTITY_BYTES && !value.chars().any(char::is_control)
}

impl Default for GooseReader {
    fn default() -> Self {
        Self::new()
    }
}
