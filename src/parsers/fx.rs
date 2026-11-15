//! fx (vercel-labs/fx) session usage, adapted from the pinned reader
//! `crates/tokscale-core/src/sessions/fx.rs`. See
//! docs/migration/parser-provenance.md.
//!
//! fx aggregates per-request token usage into one snapshot per session at
//! `~/.fx/sessions/<sessionId>/usage-v2.json`, so this reader emits one
//! observation per `(session × model)` entry — the same session-level shape as
//! Kilo, Goose and Mux.
//!
//! # The synthetic unknown model
//!
//! A snapshot carries both a per-model breakdown and top-level session
//! aggregates. When the breakdown is empty — or every entry reports zero
//! tokens — the session totals are attributed to `fx-unknown` rather than
//! dropped, so a session's usage is never silently lost. It is emitted only if
//! it carries something: tokens, requests, or a positive cost.
//!
//! # Cost provenance
//!
//! `total_cost` is optional at both levels, and its **presence** is preserved
//! rather than defaulted. A snapshot that never carried a cost must not be
//! submitted as an authoritative $0.00, which would be indistinguishable from
//! a session fx really did bill nothing for. A negative or non-finite total is
//! likewise treated as unreported rather than clamped into an authoritative
//! zero, so it stays eligible for repricing.
//!
//! # Model identity
//!
//! A model id of the form `<provider>/<model>` is split, and the provider names
//! the billing hint. An id with no separator keeps its whole value as the model
//! and carries no hint.
//!
//! # Out of scope
//!
//! The sibling `session.json` supplies the workspace root and the authoritative
//! timestamps, and the shared `index.json` supplies human-readable titles. Both
//! are cross-file reads that this codebase's byte-slice reader contract cannot
//! perform, so the timestamp falls back to the usage file's mtime and no
//! workspace or title is reported. The global `~/.fx/usage.jsonl` stream is
//! also not read, exactly as the pinned reader declines it: it carries no
//! session id or workspace.

use super::{CostStatus, ReaderError, ResponseUsage, jsonl::MAX_RECORD_BYTES};
use crate::accounting::{SessionKey, TokenCounts};
use serde_json::Value;

pub const MAX_RESPONSES: usize = 16_384;
const MAX_IDENTITY_BYTES: usize = 4_096;
const MAX_RETAINED_IDENTITY_BYTES: usize = 8 * 1024 * 1024;

pub const CLIENT: &str = "fx";
/// Follows the `<client>-unknown` convention for session usage whose per-model
/// breakdown is unavailable: the totals are still attributed rather than
/// disappearing into an empty model cell.
pub const UNKNOWN_MODEL: &str = "fx-unknown";

const NANO_USD: f64 = 1_000_000_000.0;

/// One fx `usage-v2.json` snapshot.
pub struct FxReader {
    key: SessionKey,
    fallback_timestamp_ms: i64,
    responses: Vec<ResponseUsage>,
    retained_identity_bytes: usize,
}

impl FxReader {
    pub fn new(session: &str) -> Self {
        Self {
            key: SessionKey::new(CLIENT, session),
            fallback_timestamp_ms: 0,
            responses: Vec::new(),
            retained_identity_bytes: 0,
        }
    }

    pub fn with_fallback_timestamp(mut self, timestamp_ms: Option<i64>) -> Self {
        self.fallback_timestamp_ms = timestamp_ms.filter(|value| *value >= 0).unwrap_or(0);
        self
    }

    /// Ingest a whole snapshot document.
    pub fn ingest(&mut self, document: &[u8]) -> Result<(), ReaderError> {
        if document.len() > MAX_RECORD_BYTES {
            return Err(ReaderError::Capacity("fx usage snapshot"));
        }
        self.responses.clear();
        self.retained_identity_bytes = 0;

        let file: Value =
            serde_json::from_slice(document).map_err(|_| ReaderError::InvalidRecord)?;
        let Some(snapshot) = file.get("snapshot") else {
            return Ok(());
        };
        // The session id is the only identity the snapshot itself carries; the
        // caller derives it from the containing directory otherwise.
        let session = file
            .get("session_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty());
        if let Some(session) = session
            && session.len() <= MAX_IDENTITY_BYTES
        {
            self.key = SessionKey::new(CLIENT, session);
        }
        let timestamp_ms = self.fallback_timestamp_ms;

        let models = snapshot
            .get("models")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        for entry in models {
            let tokens = model_tokens(entry)?;
            if zero(&tokens) {
                continue;
            }
            let raw = entry
                .get("model")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|model| !model.is_empty())
                .unwrap_or(UNKNOWN_MODEL);
            self.push(raw, tokens, entry.get("total_cost"), timestamp_ms)?;
        }

        // The top-level aggregates become one synthetic observation when the
        // breakdown produced nothing, so the session total is not lost.
        if self.responses.is_empty() {
            let tokens = TokenCounts {
                input: counter(snapshot, "input_tokens")?,
                output: counter(snapshot, "output_tokens")?,
                cache_read: counter(snapshot, "cache_read_tokens")?,
                cache_write: counter(snapshot, "cache_write_tokens")?,
                reasoning: counter(snapshot, "reasoning_tokens")?,
            };
            let requests = counter(snapshot, "request_count")?;
            let cost = reported_cost(snapshot.get("total_cost"))?;
            if !zero(&tokens) || requests > 0 || matches!(cost, CostStatus::Reported(n) if n > 0) {
                self.push(
                    UNKNOWN_MODEL,
                    tokens,
                    snapshot.get("total_cost"),
                    timestamp_ms,
                )?;
            }
        }
        Ok(())
    }

    fn push(
        &mut self,
        raw_model: &str,
        tokens: TokenCounts,
        cost: Option<&Value>,
        timestamp_ms: i64,
    ) -> Result<(), ReaderError> {
        let (provider, model) = split_model(raw_model);
        let dedup_key = format!("fx:{}:{model}", self.key.session);
        if dedup_key.len() > MAX_IDENTITY_BYTES || model.len() > MAX_IDENTITY_BYTES {
            return Err(ReaderError::Unsupported("fx identity"));
        }
        let usage = ResponseUsage {
            key: self.key.clone(),
            model,
            tokens,
            cache_write_1h: 0,
            timestamp_ms,
            dedup_key: Some(dedup_key.clone()),
            billing_provider_hint: provider,
            service_tier: None,
            agent: None,
            workspace_key: None,
            workspace_label: None,
            title: None,
            cost: reported_cost(cost)?,
        };
        if self.responses.len() == MAX_RESPONSES {
            return Err(ReaderError::Capacity("fx responses"));
        }
        self.retained_identity_bytes = self
            .retained_identity_bytes
            .checked_add(dedup_key.len())
            .filter(|bytes| *bytes <= MAX_RETAINED_IDENTITY_BYTES)
            .ok_or(ReaderError::Capacity("fx identities"))?;
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

fn model_tokens(entry: &Value) -> Result<TokenCounts, ReaderError> {
    Ok(TokenCounts {
        input: counter(entry, "input_tokens")?,
        output: counter(entry, "output_tokens")?,
        cache_read: counter(entry, "cache_read_tokens")?,
        cache_write: counter(entry, "cache_write_tokens")?,
        reasoning: counter(entry, "reasoning_tokens")?,
    })
}

/// Reuse a recorded USD cost only when it is present, finite and non-negative.
///
/// Defaulting the field would throw its presence away, and a negative or
/// non-finite aggregate cannot be a bill fx meant, so it must not lock the
/// observation out of repricing.
fn reported_cost(cost: Option<&Value>) -> Result<CostStatus, ReaderError> {
    let Some(cost) = cost else {
        return Ok(CostStatus::Unavailable);
    };
    if cost.is_null() {
        return Ok(CostStatus::Unavailable);
    }
    let Some(cost) = cost.as_f64() else {
        return Err(ReaderError::InvalidRecord);
    };
    if !cost.is_finite() || cost < 0.0 {
        return Ok(CostStatus::Unavailable);
    }
    let nanos = cost * NANO_USD;
    if nanos > u64::MAX as f64 {
        return Err(ReaderError::Overflow);
    }
    Ok(CostStatus::Reported(nanos.round() as u64))
}

/// Split `<provider>/<model>`, keeping the whole value when there is no
/// separator or either side is empty.
fn split_model(raw: &str) -> (Option<String>, String) {
    match raw.split_once('/') {
        Some((provider, rest)) if !provider.is_empty() && !rest.is_empty() => {
            (Some(provider.to_owned()), rest.to_owned())
        }
        _ => (None, raw.to_owned()),
    }
}

fn counter(object: &Value, key: &str) -> Result<u64, ReaderError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(0),
        Some(value) => value.as_u64().ok_or(ReaderError::InvalidRecord),
    }
}

fn zero(tokens: &TokenCounts) -> bool {
    tokens.input == 0
        && tokens.output == 0
        && tokens.cache_read == 0
        && tokens.cache_write == 0
        && tokens.reasoning == 0
}
