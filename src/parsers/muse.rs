//! Muse Code event-sourced session JSONL, adapted from the pinned Tokscale
//! `muse.rs` reader. MIT © 2025 Junho Yeo; see docs/migration/parser-provenance.md.
//!
//! Only `runtime.session` `model_completed` events carry usage. Input includes
//! cached reads and output includes reasoning, so both subsets are split out
//! once. Metadata-only records, workflow child aggregates and other events are
//! ignored; workspace and pricing-model hints are not projected.

use super::timestamps::{counter, tokens_total};
use super::{CostStatus, ReaderError, ResponseUsage};
use crate::accounting::{SessionKey, TokenCounts};
use serde_json::Value;

const MAX_RESPONSES: usize = 16_384;

pub struct MuseReader {
    fallback_session: String,
    fallback_timestamp_ms: Option<i64>,
    rows: Vec<ResponseUsage>,
    ordinal: usize,
}

impl MuseReader {
    pub fn new(session: impl Into<String>) -> Self {
        Self {
            fallback_session: session.into(),
            fallback_timestamp_ms: None,
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
        let event = value
            .pointer("/payload/event")
            .and_then(Value::as_object)
            .filter(|event| event.get("kind").and_then(Value::as_str) == Some("model_completed"));
        let Some(event) = event else {
            return Ok(());
        };
        let Some(usage) = event.get("usage") else {
            return Ok(());
        };
        let model = event
            .get("model")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .ok_or(ReaderError::Declined("Muse event model"))?;
        let reported_input = counter(usage, "input_tokens")?;
        let reported_output = counter(usage, "output_tokens")?;
        let cache_read = counter(usage, "cache_read_tokens")?
            .max(counter(usage, "cached_tokens")?)
            .min(reported_input);
        let cache_write = counter(usage, "cache_write_tokens")?;
        let reasoning = counter(usage, "reasoning_tokens")?.min(reported_output);
        let tokens = TokenCounts {
            input: reported_input - cache_read,
            output: reported_output - reasoning,
            cache_read,
            cache_write,
            reasoning,
        };
        if tokens_total(tokens)? == 0 {
            return Ok(());
        }
        let session = value
            .pointer("/stream/id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|session| !session.is_empty())
            .unwrap_or(self.fallback_session.as_str());
        if !super::tencent_buddy::valid_identity(session) {
            return Err(ReaderError::InvalidRecord);
        }
        let recorded_at = value.get("recorded_at").and_then(Value::as_i64);
        let timestamp = recorded_at
            .and_then(recorded_at_to_ms)
            .or(self.fallback_timestamp_ms)
            .ok_or(ReaderError::InvalidTimestamp)?;
        let sequence = value
            .get("sequence")
            .and_then(Value::as_i64)
            .filter(|sequence| *sequence >= 0)
            .map(|sequence| sequence.to_string())
            .unwrap_or_else(|| self.ordinal.to_string());
        let identity = format!(
            "muse:{session}:{sequence}:{model}:{}:{}:{}:{}:{}",
            tokens.input, tokens.output, tokens.cache_read, tokens.cache_write, tokens.reasoning
        );
        self.ordinal += 1;
        if self.rows.len() == MAX_RESPONSES {
            return Err(ReaderError::Capacity("Muse responses"));
        }
        self.rows.push(ResponseUsage {
            key: SessionKey::new("muse", session),
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

fn recorded_at_to_ms(raw: i64) -> Option<i64> {
    if raw >= 1_000_000_000_000_000 {
        Some(raw / 1000)
    } else if raw >= 1_000_000_000_000 {
        Some(raw)
    } else if raw >= 1_000_000_000 {
        raw.checked_mul(1000)
    } else {
        None
    }
}
