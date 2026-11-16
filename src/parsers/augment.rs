//! Augment Code (Auggie CLI) session snapshots, adapted from the pinned reader
//! `crates/tokscale-core/src/sessions/augment.rs`. See
//! docs/migration/parser-provenance.md.
//!
//! Auggie writes one session snapshot per session at
//! `~/.augment/sessions/<sessionId>.json`. Each completed turn exposes an
//! `exchange` whose `responseNodes[]` may carry a `tokenUsage` block, and one
//! observation is emitted per completed turn.
//!
//! # Only completed turns count
//!
//! Snapshots retain in-progress and aborted turns that already carry a partial
//! `tokenUsage`, so a turn is counted only when `completed: true`.
//!
//! # The **last** non-empty usage node wins, never a sum
//!
//! A turn almost never has more than one usage node. When it does, the final
//! streamed totals are the authoritative figure; summing repeated cumulative
//! values would double-count.
//!
//! # Input and cache buckets are independent
//!
//! Auggie reports Anthropic-style split accounting, so `cacheRead` is **not**
//! subtracted from `input`. Treating it as a subset would under-count the input.
//!
//! # End-anchored
//!
//! Auggie records `finishedAt` only — there is no per-turn start time or
//! duration in the on-disk schema — so each observation is anchored at the
//! turn's end. Duration-based metrics stay empty.
//!
//! # Cost
//!
//! Always unavailable. The pinned reader emits `cost = 0` and lets downstream
//! pricing estimate public list prices from the model id; Augment credits and
//! `subAgentCostUsd` are deliberately ignored. A `0` cost here is therefore
//! "not reported", which is exactly [`CostStatus::Unavailable`].

use super::timestamps::parse_timestamp_str;
use super::{CostStatus, ReaderError, ResponseUsage, jsonl::MAX_RECORD_BYTES};
use crate::accounting::{SessionKey, TokenCounts};
use serde_json::Value;

pub const MAX_RESPONSES: usize = 16_384;
const MAX_IDENTITY_BYTES: usize = 4_096;
const MAX_RETAINED_IDENTITY_BYTES: usize = 8 * 1024 * 1024;

pub const CLIENT: &str = "augment";

/// One Auggie session snapshot.
pub struct AugmentReader {
    key: SessionKey,
    fallback_timestamp_ms: i64,
    rows: Vec<ResponseUsage>,
    retained_identity_bytes: usize,
}

impl AugmentReader {
    pub fn new(session: impl Into<String>) -> Self {
        Self {
            key: SessionKey::new(CLIENT, session),
            fallback_timestamp_ms: 0,
            rows: Vec::new(),
            retained_identity_bytes: 0,
        }
    }

    pub fn with_fallback_timestamp(mut self, timestamp_ms: Option<i64>) -> Self {
        self.fallback_timestamp_ms = timestamp_ms.filter(|value| *value >= 0).unwrap_or(0);
        self
    }

    /// Ingest a whole session snapshot.
    pub fn ingest(&mut self, document: &[u8]) -> Result<(), ReaderError> {
        if document.len() > MAX_RECORD_BYTES {
            return Err(ReaderError::Capacity("Augment session"));
        }
        self.rows.clear();
        self.retained_identity_bytes = 0;

        let root: Value =
            serde_json::from_slice(document).map_err(|_| ReaderError::InvalidRecord)?;
        let session = root
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty() && id.len() <= MAX_IDENTITY_BYTES);
        if let Some(session) = session {
            self.key = SessionKey::new(CLIENT, session);
        }
        let default_model = model_of(root.pointer("/agentState/modelId").and_then(Value::as_str));

        let turns = root
            .get("chatHistory")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        for (index, turn) in turns.iter().enumerate() {
            // Aborted and in-progress turns may already carry a partial usage.
            if turn.get("completed").and_then(Value::as_bool) != Some(true) {
                continue;
            }
            let Some(exchange) = turn.get("exchange") else {
                continue;
            };
            let Some(usage) = last_non_empty_usage(exchange) else {
                continue;
            };
            let tokens = tokens(usage)?;
            let model = match exchange
                .get("modelId")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|model| !model.is_empty())
            {
                Some(model) => model.to_owned(),
                None => default_model.clone(),
            };
            if model.len() > MAX_IDENTITY_BYTES {
                return Err(ReaderError::Declined("Augment model identity"));
            }
            let dedup_key = turn_identity(&self.key.session, turn, exchange, index);
            if dedup_key.len() > MAX_IDENTITY_BYTES {
                return Err(ReaderError::Declined("Augment identity"));
            }
            self.retained_identity_bytes = self
                .retained_identity_bytes
                .checked_add(dedup_key.len())
                .filter(|bytes| *bytes <= MAX_RETAINED_IDENTITY_BYTES)
                .ok_or(ReaderError::Capacity("Augment identities"))?;
            if self.rows.len() == MAX_RESPONSES {
                return Err(ReaderError::Capacity("Augment responses"));
            }
            let timestamp_ms = turn
                .get("finishedAt")
                .and_then(Value::as_str)
                .and_then(parse_timestamp_str)
                .unwrap_or(self.fallback_timestamp_ms);
            self.rows.push(ResponseUsage {
                key: self.key.clone(),
                model,
                tokens,
                cache_write_1h: 0,
                timestamp_ms,
                dedup_key: Some(dedup_key),
                // Auggie records no vendor, and inventing one from the model
                // name would be a guess this reader is not entitled to make.
                billing_provider_hint: None,
                service_tier: None,
                agent: None,
                workspace_key: None,
                workspace_label: None,
                title: None,
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

/// The last non-empty usage node, so a later full total wins over an earlier
/// partial rather than being summed with it.
fn last_non_empty_usage(exchange: &Value) -> Option<&Value> {
    exchange
        .get("responseNodes")
        .and_then(Value::as_array)?
        .iter()
        .rev()
        .filter_map(|node| node.get("tokenUsage"))
        .find(|usage| usage.get("inputTokens").is_some() || usage.get("outputTokens").is_some())
}

/// Anthropic-style split accounting: the cache buckets are reported separately
/// and must not be subtracted from the input count.
fn tokens(usage: &Value) -> Result<TokenCounts, ReaderError> {
    Ok(TokenCounts {
        input: counter(usage, "inputTokens")?,
        output: counter(usage, "outputTokens")?,
        cache_read: counter(usage, "cacheReadInputTokens")?,
        cache_write: counter(usage, "cacheCreationInputTokens")?,
        reasoning: 0,
    })
}

fn counter(usage: &Value, key: &str) -> Result<u64, ReaderError> {
    match usage.get(key) {
        None | Some(Value::Null) => Ok(0),
        Some(value) => value
            .as_u64()
            .ok_or(ReaderError::Declined("Augment counter")),
    }
}

fn model_of(value: Option<&str>) -> String {
    value
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| "unknown".to_owned())
}

/// A turn's stable identity: its request id, else its sequence id, else its
/// position. The fallbacks keep a snapshot without either field countable while
/// still collapsing a re-read.
fn turn_identity(session: &str, turn: &Value, exchange: &Value, index: usize) -> String {
    if let Some(request) = exchange
        .get("requestId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        return format!("augment:{session}:req:{request}");
    }
    if let Some(sequence) = turn.get("sequenceId") {
        let sequence = match sequence {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        if !sequence.is_empty() && sequence != "null" {
            return format!("augment:{session}:seq:{sequence}");
        }
    }
    format!("augment:{session}:turn:{index}")
}
