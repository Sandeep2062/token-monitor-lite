//! Reasonix append-only statistics records (`<state>/stats/YYYY-MM-DD.jsonl`),
//! adapted from the pinned Tokscale `reasonix.rs` reader. MIT © 2025 Junho Yeo;
//! see docs/migration/parser-provenance.md.
//!
//! One JSON object per provider request. Session transcripts are not read: the
//! stats ledger is the authoritative counter source. Turn markers, missing
//! models, malformed records and zero rows are ignored.

use super::timestamps::{counter, optional_counter, parse_timestamp_value, tokens_total};
use super::{CostStatus, ReaderError, ResponseUsage};
use crate::accounting::{SessionKey, TokenCounts};
use serde_json::Value;

const MAX_RESPONSES: usize = 16_384;

pub struct ReasonixReader {
    session: String,
    rows: Vec<ResponseUsage>,
    ordinal: usize,
}

impl ReasonixReader {
    pub fn new(session: impl Into<String>) -> Self {
        Self {
            session: session.into(),
            rows: Vec::new(),
            ordinal: 0,
        }
    }

    pub fn ingest(&mut self, record: &[u8]) -> Result<(), ReaderError> {
        let value: Value =
            serde_json::from_slice(record).map_err(|_| ReaderError::InvalidRecord)?;
        let object = value.as_object().ok_or(ReaderError::InvalidRecord)?;
        if object.get("turn").and_then(Value::as_bool) == Some(true) {
            return Ok(());
        }
        let model_ref = object
            .get("model")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|model| !model.is_empty());
        let Some(model_ref) = model_ref else {
            return Ok(());
        };
        let timestamp = object
            .get("ts")
            .and_then(parse_timestamp_value)
            .ok_or(ReaderError::InvalidTimestamp)?;
        let prompt = counter(&value, "prompt")?;
        let completion = counter(&value, "completion")?;
        let reasoning_reported = counter(&value, "reasoning")?;
        let cache_read = counter(&value, "cache_hit")?;
        let cache_miss = optional_counter(&value, "cache_miss")?;
        let total = counter(&value, "total")?;
        let requests = counter(&value, "requests")?;
        if total == 0 && requests == 0 {
            return Ok(());
        }
        let input = match cache_miss {
            Some(cache_miss) if cache_miss != 0 => cache_miss,
            _ => prompt
                .checked_sub(cache_read)
                .ok_or(ReaderError::Unsupported("Reasonix cache exceeds prompt"))?,
        };
        let reasoning = reasoning_reported.min(completion);
        let tokens = TokenCounts {
            input,
            output: completion - reasoning,
            cache_read,
            cache_write: 0,
            reasoning,
        };
        if tokens_total(tokens)? == 0 {
            return Ok(());
        }
        let (provider, model) = match model_ref.split_once('/') {
            Some((provider, model)) if !provider.trim().is_empty() && !model.trim().is_empty() => {
                (Some(provider.trim().to_owned()), model.trim())
            }
            Some(_) => return Err(ReaderError::Unsupported("Reasonix model reference")),
            None => (None, model_ref),
        };
        if !super::tencent_buddy::valid_identity(model) {
            return Err(ReaderError::InvalidRecord);
        }
        if self.rows.len() == MAX_RESPONSES {
            return Err(ReaderError::Capacity("Reasonix responses"));
        }
        let identity = format!(
            "reasonix:{}:{}:{requests}:{total}",
            self.session, self.ordinal
        );
        self.ordinal += 1;
        self.rows.push(ResponseUsage {
            key: SessionKey::new("reasonix", self.session.as_str()),
            model: model.to_owned(),
            tokens,
            cache_write_1h: 0,
            timestamp_ms: timestamp,
            dedup_key: Some(identity),
            billing_provider_hint: provider,
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
