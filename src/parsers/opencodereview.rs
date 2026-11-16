//! OpenCodeReview session transcripts, adapted from the pinned reader
//! `crates/tokscale-core/src/sessions/opencodereview.rs`. See
//! docs/migration/parser-provenance.md.
//!
//! Sessions are JSONL at `~/.opencodereview/sessions/<encoded-repo-path>/<session-id>.jsonl`.
//! Two record types matter:
//!
//! - `session_start` — carries `cwd`. Emits nothing; the first one seen names
//!   the workspace.
//! - `llm_response` — carries `usage`, `model`, an optional `timestamp` and an
//!   optional `duration_ms`. One observation each.
//!
//! # The recorded timestamp is the response's end
//!
//! `timestamp` is written when the response is logged, i.e. the call's end, and
//! `duration_ms` is its elapsed time, so the start is back-calculated. That
//! subtraction applies **only** when the record carries its own timestamp: with
//! none, the value is the file mtime, and shifting that by a call duration would
//! move the observation into the wrong day.
//!
//! # Dedup needs a discriminator when the timestamp is missing
//!
//! Two records agreeing on the end timestamp, model and token counts are a
//! replayed write of one call, and collapsing them is deliberate. Without a
//! recorded timestamp, though, the "timestamp" is the file mtime — identical for
//! every record in the file — so that key can no longer separate two genuinely
//! distinct calls that share a model and token counts, and would undercount
//! them. The line number is used instead: it comes from the file's own contents,
//! so a re-parse reproduces the same keys while distinct records stay distinct.
//! `duration_ms` would not do, since it is optional and two calls can take
//! equally long.
//!
//! Not implemented: `resolve_alias` and `inferred_provider_from_model` (no
//! provider hint is emitted rather than an invented `opencodereview`
//! pseudo-provider), and `workspace_key` — only the final path component
//! survives as a label, since an absolute path is refused in every runtime
//! identity column.

use super::timestamps::{counter, parse_timestamp_value, tokens_total};
use super::{CostStatus, ReaderError, ResponseUsage};
use crate::accounting::{SessionKey, TokenCounts};
use serde_json::Value;

pub const MAX_RESPONSES: usize = 16_384;
const MAX_IDENTITY_BYTES: usize = 4_096;
const MAX_RETAINED_IDENTITY_BYTES: usize = 8 * 1024 * 1024;

pub const CLIENT: &str = "opencodereview";

/// One OpenCodeReview session transcript.
pub struct OpenCodeReviewReader {
    key: SessionKey,
    fallback_timestamp_ms: i64,
    workspace_label: Option<String>,
    rows: Vec<ResponseUsage>,
    seen: std::collections::HashSet<String>,
    ordinal: usize,
    retained_identity_bytes: usize,
}

impl OpenCodeReviewReader {
    pub fn new(session: impl Into<String>) -> Self {
        Self {
            key: SessionKey::new(CLIENT, session),
            fallback_timestamp_ms: 0,
            workspace_label: None,
            rows: Vec::new(),
            seen: std::collections::HashSet::new(),
            ordinal: 0,
            retained_identity_bytes: 0,
        }
    }

    pub fn with_fallback_timestamp(mut self, timestamp_ms: Option<i64>) -> Self {
        self.fallback_timestamp_ms = timestamp_ms.filter(|value| *value >= 0).unwrap_or(0);
        self
    }

    pub fn ingest(&mut self, record: &[u8]) -> Result<(), ReaderError> {
        let offset = self.ordinal;
        self.ordinal = self.ordinal.saturating_add(1);

        let text = std::str::from_utf8(record)
            .map_err(|_| ReaderError::Declined("OpenCodeReview line"))?;
        if !text.contains("llm_response") && !text.contains("session_start") {
            return Ok(());
        }
        let value: Value = serde_json::from_str(text)
            .map_err(|_| ReaderError::Declined("OpenCodeReview record"))?;

        match value.get("type").and_then(Value::as_str) {
            Some("session_start") => {
                // The first `session_start` names the workspace.
                if self.workspace_label.is_none() {
                    self.workspace_label = value
                        .get("cwd")
                        .and_then(Value::as_str)
                        .and_then(workspace_label);
                }
                Ok(())
            }
            Some("llm_response") => self.ingest_response(&value, offset),
            _ => Ok(()),
        }
    }

    fn ingest_response(&mut self, value: &Value, offset: usize) -> Result<(), ReaderError> {
        let Some(usage) = value.get("usage").filter(|usage| usage.is_object()) else {
            return Ok(());
        };
        let tokens = TokenCounts {
            input: counter(usage, "prompt_tokens")?,
            output: counter(usage, "completion_tokens")?,
            cache_read: counter(usage, "cache_read_tokens")?,
            cache_write: counter(usage, "cache_write_tokens")?,
            reasoning: 0,
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
        if model.len() > MAX_IDENTITY_BYTES {
            return Err(ReaderError::Declined("OpenCodeReview model identity"));
        }

        let explicit_timestamp = value.get("timestamp").and_then(parse_timestamp_value);
        let recorded_timestamp = explicit_timestamp.unwrap_or(self.fallback_timestamp_ms);
        let duration_ms = value
            .get("duration_ms")
            .and_then(Value::as_i64)
            .filter(|duration| *duration > 0);
        let timestamp_ms = match (explicit_timestamp, duration_ms) {
            (Some(end), Some(duration)) => end.saturating_sub(duration).max(0),
            _ => recorded_timestamp,
        };

        // Without a recorded timestamp the "timestamp" is the file mtime, which
        // is identical for every record, so the line number is the only thing
        // that can keep two distinct calls apart.
        let discriminator = match explicit_timestamp {
            Some(_) => String::new(),
            None => format!(":line{offset}"),
        };
        let dedup_key = format!(
            "opencodereview:{}:{recorded_timestamp}:{model}:{}:{}:{}:{}{discriminator}",
            self.key.session, tokens.input, tokens.output, tokens.cache_read, tokens.cache_write,
        );
        if dedup_key.len() > MAX_IDENTITY_BYTES {
            return Err(ReaderError::Declined("OpenCodeReview identity"));
        }
        if !self.seen.insert(dedup_key.clone()) {
            return Ok(());
        }
        self.retained_identity_bytes = self
            .retained_identity_bytes
            .checked_add(dedup_key.len())
            .filter(|bytes| *bytes <= MAX_RETAINED_IDENTITY_BYTES)
            .ok_or(ReaderError::Capacity("OpenCodeReview identities"))?;
        if self.rows.len() == MAX_RESPONSES {
            return Err(ReaderError::Capacity("OpenCodeReview responses"));
        }
        self.rows.push(ResponseUsage {
            key: self.key.clone(),
            model: model.to_owned(),
            tokens,
            cache_write_1h: 0,
            timestamp_ms,
            dedup_key: Some(dedup_key),
            // No vendor hint: the pinned reader falls back to an
            // `opencodereview` pseudo-provider that no pricing row can match.
            billing_provider_hint: None,
            service_tier: None,
            agent: None,
            workspace_key: None,
            workspace_label: self.workspace_label.clone(),
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

/// The final component of a working directory, never the whole path.
fn workspace_label(path: &str) -> Option<String> {
    let trimmed = path.trim().trim_end_matches(['/', '\\']);
    let label = trimmed.rsplit(['/', '\\']).next()?;
    if label.is_empty() || label.len() > MAX_IDENTITY_BYTES || label.chars().any(char::is_control) {
        return None;
    }
    Some(label.to_owned())
}
