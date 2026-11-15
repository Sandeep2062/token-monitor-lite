//! Qwen CLI session JSONL (`~/.qwen/projects/<project>/chats/*.jsonl`),
//! adapted from the pinned Tokscale `qwen.rs` reader. MIT © 2025 Junho Yeo; see
//! docs/migration/parser-provenance.md.
//!
//! Only assistant rows with `usageMetadata` are counted. Prompt tokens include
//! cached content and candidates may include thoughts; both subsets are split
//! out once. Workspace attribution and provider inference are not projected.

use super::timestamps::{counter, optional_counter, parse_timestamp_str, tokens_total};
use super::{CostStatus, ReaderError, ResponseUsage};
use crate::accounting::{SessionKey, TokenCounts};
use serde_json::Value;

const MAX_RESPONSES: usize = 16_384;

pub struct QwenReader {
    session: String,
    fallback_timestamp_ms: Option<i64>,
    rows: Vec<ResponseUsage>,
    ordinal: usize,
}

impl QwenReader {
    pub fn new(session: impl Into<String>) -> Self {
        Self {
            session: session.into(),
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
        if value.get("type").and_then(Value::as_str) != Some("assistant") {
            return Ok(());
        }
        let Some(usage) = value.get("usageMetadata") else {
            return Ok(());
        };
        let prompt = counter(usage, "promptTokenCount")?;
        let candidates = counter(usage, "candidatesTokenCount")?;
        let thoughts = counter(usage, "thoughtsTokenCount")?;
        let cache_read = counter(usage, "cachedContentTokenCount")?;
        let total = optional_counter(usage, "totalTokenCount")?;
        let input = if cache_read <= prompt {
            prompt - cache_read
        } else {
            prompt
        };
        let thoughts_in_candidates = thoughts > 0
            && match total {
                Some(0) => candidates > 0,
                Some(total) => total > 0 && total == prompt.saturating_add(candidates),
                None => false,
            };
        let (output, reasoning) = if thoughts_in_candidates {
            let reasoning = thoughts.min(candidates);
            (candidates - reasoning, reasoning)
        } else {
            (candidates, thoughts)
        };
        let tokens = TokenCounts {
            input,
            output,
            cache_read,
            cache_write: 0,
            reasoning,
        };
        if tokens_total(tokens)? == 0 {
            return Ok(());
        }
        let model = value
            .get("model")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .unwrap_or("unknown");
        let session = value
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|session| !session.is_empty())
            .unwrap_or(self.session.as_str());
        let timestamp = value
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_timestamp_str)
            .or(self.fallback_timestamp_ms)
            .ok_or(ReaderError::InvalidTimestamp)?;
        let identity = format!("qwen:{session}:{}", self.ordinal);
        self.ordinal += 1;
        if self.rows.len() == MAX_RESPONSES {
            return Err(ReaderError::Capacity("Qwen responses"));
        }
        self.rows.push(ResponseUsage {
            key: SessionKey::new("qwen", session),
            model: model.to_owned(),
            tokens,
            cache_write_1h: 0,
            timestamp_ms: timestamp,
            dedup_key: Some(identity),
            billing_provider_hint: None,
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
