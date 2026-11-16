//! Hermes Agent session-row normalization adapted from the pinned Tokscale
//! `hermes.rs` reader. MIT © 2025 Junho Yeo; see
//! docs/migration/parser-provenance.md.
//!
//! Hermes records usage only as per-session aggregates. `session_model_usage`
//! breaks a session down per model and provider; builds that predate that table
//! carry the totals on `sessions` alone. The native reader keeps the two result
//! sets disjoint by session: a session with any per-model row is read from
//! those rows, and a session without them falls back to its session totals.
//! That is what prevents double counting without inventing a reconciliation.
//!
//! The pinned reader additionally splits each row across the calendar days its
//! logged API calls fall in, reading `logs/agent.log*` beside the database.
//! That split is deliberately not implemented here: it re-attributes an
//! aggregate across days using a second, unvalidated input, so the native
//! reader attributes the whole session to its recorded `started_at` and leaves
//! the cross-day distribution as visible unsupported coverage.
//!
//! `reasoning_tokens` is a subset of `output_tokens` in Hermes, so the additive
//! output bucket subtracts it exactly as the pinned reader does.

use super::{CostStatus, ReaderError, ResponseUsage};
use crate::accounting::{SessionKey, TokenCounts};

pub const MAX_RESPONSES: usize = 16_384;
const MAX_IDENTITY_BYTES: usize = 4_096;
const MAX_RETAINED_IDENTITY_BYTES: usize = 8 * 1024 * 1024;

/// One joined usage row, decoded from either the per-model query or the
/// session-totals query. Both project the same columns.
pub struct HermesRow<'a> {
    pub session_id: &'a str,
    pub model: &'a str,
    pub billing_provider: Option<&'a str>,
    /// Unix seconds, as stored by Hermes.
    pub started_at: f64,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cache_write_tokens: Option<i64>,
    pub reasoning_tokens: Option<i64>,
}

pub struct HermesReader {
    fallback_timestamp_ms: Option<i64>,
    responses: Vec<ResponseUsage>,
    retained_identity_bytes: usize,
}

impl HermesReader {
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

    /// `dedup_key` distinguishes the per-model row from the session-totals row
    /// so the two paths can never collide. `provider_key` is the already
    /// resolved identity token: `None` renders as the NULL sentinel, which
    /// keeps SQLite's separate NULL and empty-string groups apart.
    pub fn ingest(&mut self, row: &HermesRow<'_>) -> Result<(), ReaderError> {
        if !valid_identity(row.session_id) {
            return Err(ReaderError::InvalidRecord);
        }
        let model = row.model.trim();
        if !valid_identity(model) {
            return Err(ReaderError::InvalidRecord);
        }
        let input = non_negative(row.input_tokens.unwrap_or(0))?;
        let output = non_negative(row.output_tokens.unwrap_or(0))?;
        let cache_read = non_negative(row.cache_read_tokens.unwrap_or(0))?;
        let cache_write = non_negative(row.cache_write_tokens.unwrap_or(0))?;
        let reasoning = non_negative(row.reasoning_tokens.unwrap_or(0))?;
        if reasoning > output {
            return Err(ReaderError::Declined("Hermes reasoning exceeds output"));
        }
        if input == 0 && output == 0 && cache_read == 0 && cache_write == 0 {
            return Ok(());
        }
        let timestamp = timestamp_secs_to_ms(row.started_at)
            .or(self.fallback_timestamp_ms)
            .ok_or(ReaderError::Declined("Hermes session timestamp"))?;
        // SQLite groups a NULL provider separately from an empty-string one, so
        // the dedup identity keeps them apart: the NULL sentinel stands in for a
        // missing value while an empty string stays an empty key. Folding one
        // into the other would make the aggregation pass drop a real row.
        let provider_key = row.billing_provider.unwrap_or("<null>");
        if provider_key.len() > MAX_IDENTITY_BYTES || provider_key.chars().any(char::is_control) {
            return Err(ReaderError::InvalidRecord);
        }
        let billing_provider_hint = row
            .billing_provider
            .map(str::trim)
            .filter(|value| !value.is_empty() && valid_identity(value))
            .map(str::to_owned);
        let identity = format!("hermes:{}:{}:{}", row.session_id, model, provider_key);
        if identity.len() > MAX_IDENTITY_BYTES {
            return Err(ReaderError::Declined("Hermes session identity"));
        }
        self.retained_identity_bytes = self
            .retained_identity_bytes
            .checked_add(identity.len())
            .filter(|bytes| *bytes <= MAX_RETAINED_IDENTITY_BYTES)
            .ok_or(ReaderError::Capacity("Hermes identities"))?;
        if self.responses.len() == MAX_RESPONSES {
            return Err(ReaderError::Capacity("Hermes responses"));
        }
        self.responses.push(ResponseUsage {
            key: SessionKey::new("hermes", row.session_id),
            model: model.to_owned(),
            tokens: TokenCounts {
                input,
                // Hermes stores inclusive output; reasoning is a subset.
                output: output - reasoning,
                cache_read,
                cache_write,
                reasoning,
            },
            cache_write_1h: 0,
            timestamp_ms: timestamp,
            dedup_key: Some(identity),
            billing_provider_hint,
            service_tier: None,
            agent: Some("Hermes Agent".to_owned()),
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

/// Hermes persists `started_at` as Unix seconds in a REAL column. A value that
/// is not a finite non-negative second count has no usable instant.
fn timestamp_secs_to_ms(seconds: f64) -> Option<i64> {
    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }
    let millis = seconds * 1000.0;
    if millis > i64::MAX as f64 {
        return None;
    }
    Some(millis as i64)
}

fn non_negative(value: i64) -> Result<u64, ReaderError> {
    u64::try_from(value).map_err(|_| ReaderError::InvalidRecord)
}

fn valid_identity(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_IDENTITY_BYTES && !value.chars().any(char::is_control)
}

impl Default for HermesReader {
    fn default() -> Self {
        Self::new()
    }
}
