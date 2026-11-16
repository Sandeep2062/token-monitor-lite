//! Mux (coder/mux) workspace usage, adapted from the pinned reader
//! `crates/tokscale-core/src/sessions/mux.rs`. See
//! docs/migration/parser-provenance.md.
//!
//! Mux keeps one aggregate counter document per workspace at
//! `~/.mux/sessions/<workspaceId>/session-usage.json`, split by model under
//! `byModel`. It is **not** a transcript: there is no per-request record and no
//! message text, so the finest row the format supports is one observation per
//! `(workspace × model)` entry.
//!
//! # The dedup key is workspace-scoped
//!
//! Keying on the model plus a positional index would collide: the index is the
//! `byModel` map's iteration position, which is not stable across re-parses, and
//! two workspaces that used the same model would both produce `mux:<model>:0`
//! and suppress each other. The workspace id (the file's parent directory) is
//! both unique per workspace and stable across reads.
//!
//! # Cost
//!
//! Every bucket carries its own `cost_usd` and the entry's cost is their sum.
//! It is source-reported, so it is carried as an authoritative figure rather
//! than repriced.
//!
//! Not implemented: `canonical_provider`, because no such table exists in this
//! codebase — the `provider:` prefix is reported verbatim as the billing hint,
//! and an unprefixed key carries no hint rather than an empty provider string.

use super::{CostStatus, ReaderError, ResponseUsage, jsonl::MAX_RECORD_BYTES};
use crate::accounting::{SessionKey, TokenCounts};
use serde_json::{Map, Value};

pub const MAX_RESPONSES: usize = 16_384;
const MAX_IDENTITY_BYTES: usize = 4_096;
const MAX_RETAINED_IDENTITY_BYTES: usize = 8 * 1024 * 1024;

pub const CLIENT: &str = "mux";

/// One Mux workspace usage document.
pub struct MuxReader {
    key: SessionKey,
    fallback_timestamp_ms: i64,
    rows: Vec<ResponseUsage>,
    retained_identity_bytes: usize,
}

impl MuxReader {
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

    /// Ingest a whole workspace usage document.
    pub fn ingest(&mut self, document: &[u8]) -> Result<(), ReaderError> {
        if document.len() > MAX_RECORD_BYTES {
            return Err(ReaderError::Capacity("Mux workspace usage"));
        }
        self.rows.clear();
        self.retained_identity_bytes = 0;

        let root: Value =
            serde_json::from_slice(document).map_err(|_| ReaderError::InvalidRecord)?;
        let Some(by_model) = root.get("byModel").and_then(Value::as_object) else {
            return Ok(());
        };
        let timestamp_ms = root
            .pointer("/lastRequest/timestamp")
            .and_then(Value::as_i64)
            .filter(|value| *value > 0)
            .unwrap_or(self.fallback_timestamp_ms);

        // Iterate in key order so a re-read produces the same rows in the same
        // sequence; the map's own order is not stable.
        let mut keys: Vec<&String> = by_model.keys().collect();
        keys.sort_unstable();

        for model_key in keys {
            let Some(entry) = by_model.get(model_key) else {
                continue;
            };
            let tokens = TokenCounts {
                input: bucket_tokens(entry, "input"),
                output: bucket_tokens(entry, "output"),
                cache_read: bucket_tokens(entry, "cached"),
                cache_write: bucket_tokens(entry, "cacheCreate"),
                reasoning: bucket_tokens(entry, "reasoning"),
            };
            let reported = bucket_cost(entry, "input")
                + bucket_cost(entry, "cached")
                + bucket_cost(entry, "cacheCreate")
                + bucket_cost(entry, "output")
                + bucket_cost(entry, "reasoning");
            // An entry that recorded no tokens at all is not an observation.
            if tokens.input == 0
                && tokens.output == 0
                && tokens.cache_read == 0
                && tokens.cache_write == 0
                && tokens.reasoning == 0
            {
                continue;
            }

            // `provider:model`, split once at the first colon so a model whose
            // own name contains colons keeps them.
            let (provider, model) = match model_key.split_once(':') {
                Some((provider, model)) if !provider.is_empty() && !model.is_empty() => {
                    (Some(provider.to_owned()), model.to_owned())
                }
                _ => (None, model_key.clone()),
            };
            if model.len() > MAX_IDENTITY_BYTES {
                return Err(ReaderError::Declined("Mux model identity"));
            }
            let dedup_key = format!("mux:{}:{model_key}", self.key.session);
            if dedup_key.len() > MAX_IDENTITY_BYTES {
                return Err(ReaderError::Declined("Mux identity"));
            }
            self.retained_identity_bytes = self
                .retained_identity_bytes
                .checked_add(dedup_key.len())
                .filter(|bytes| *bytes <= MAX_RETAINED_IDENTITY_BYTES)
                .ok_or(ReaderError::Capacity("Mux identities"))?;
            if self.rows.len() == MAX_RESPONSES {
                return Err(ReaderError::Capacity("Mux responses"));
            }

            self.rows.push(ResponseUsage {
                key: self.key.clone(),
                model,
                tokens,
                cache_write_1h: 0,
                timestamp_ms,
                dedup_key: Some(dedup_key),
                billing_provider_hint: provider,
                service_tier: None,
                agent: None,
                workspace_key: None,
                workspace_label: None,
                title: None,
                // A document that recorded no cost at all reports nothing,
                // which must stay distinct from a genuine zero.
                cost: if reported.is_finite() && reported > 0.0 {
                    CostStatus::Reported((reported * 1e9).round() as u64)
                } else {
                    CostStatus::Unavailable
                },
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

fn bucket<'a>(entry: &'a Value, name: &str) -> Option<&'a Map<String, Value>> {
    entry.get(name).and_then(Value::as_object)
}

fn bucket_tokens(entry: &Value, name: &str) -> u64 {
    bucket(entry, name)
        .and_then(|bucket| bucket.get("tokens"))
        .and_then(Value::as_i64)
        // A contradictory negative counter is clamped rather than rejected:
        // this is an aggregate document, and one bad bucket must not discard a
        // workspace's otherwise valid usage.
        .map_or(0, |tokens| tokens.max(0) as u64)
}

/// A bucket's reported cost, defaulting to zero when it recorded none. The
/// caller distinguishes "no cost anywhere" from "a real zero" by the sum being
/// zero.
fn bucket_cost(entry: &Value, name: &str) -> f64 {
    bucket(entry, name)
        .and_then(|bucket| bucket.get("cost_usd"))
        .and_then(Value::as_f64)
        .filter(|cost| cost.is_finite())
        .unwrap_or(0.0)
}
