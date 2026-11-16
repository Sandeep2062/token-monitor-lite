//! gajae-code (`gjc`) session transcripts, adapted from the pinned reader
//! `crates/tokscale-core/src/sessions/gjc.rs`. See
//! docs/migration/parser-provenance.md.
//!
//! Sessions live at `~/.gjc/agent/sessions/<project-slug>/*.jsonl`, with depth-2
//! per-pass sub-agent children at `<slug>/<session>/N-*.jsonl`. Each line is
//! tagged by `type`:
//!
//! - `session` — the header, carrying `id` and `cwd`. Emits nothing.
//! - `message` — emits **only** assistant messages, whose `message` object
//!   carries `model`, `provider`, a unix-ms `timestamp` and a `usage` block.
//! - anything else, including `service_tier_change` — skipped.
//!
//! # Cost
//!
//! `usage.cost.total` is reused verbatim only when present, finite and
//! non-negative; otherwise the observation reports nothing so the pricing layer
//! can price it from tokens. A negative or non-finite total is treated as
//! unreported rather than clamped, for the same reason as elsewhere: a
//! contradictory figure must not become a billable one.
//!
//! # Dedup
//!
//! The header's session id plus the message id is preferred. When either is
//! missing, a deterministic fallback folds the session, timestamp, model,
//! provider and token breakdown into the key, so a structurally identical replay
//! (the depth-1 parent and its depth-2 child) collapses to one observation while
//! genuinely distinct messages stay apart.
//!
//! Both forms are prefixed `gjc:`. The pinned reader leaves the id form
//! unprefixed, which makes a `session:message` key indistinguishable from
//! another client's — this codebase dedups across files, so the prefix is
//! normalised here rather than copied.
//!
//! Not implemented: the per-message `source` client override (it would fragment
//! one source's client identity), `inferred_provider_from_model` (a missing
//! provider yields no hint rather than an invented `gjc` pseudo-provider), and
//! `workspace_key` — only the final path component survives as a label, since an
//! absolute path is refused in every runtime identity column.

use super::CostStatus;
use super::timestamps::parse_timestamp_str;
use super::{ReaderError, ResponseUsage};
use crate::accounting::{SessionKey, TokenCounts};
use serde_json::Value;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

pub const MAX_RESPONSES: usize = 16_384;
const MAX_IDENTITY_BYTES: usize = 4_096;
const MAX_RETAINED_IDENTITY_BYTES: usize = 8 * 1024 * 1024;

pub const CLIENT: &str = "gjc";

/// One gjc JSONL session transcript.
pub struct GjcReader {
    key: SessionKey,
    fallback_timestamp_ms: i64,
    workspace_label: Option<String>,
    header_session: Option<String>,
    rows: Vec<ResponseUsage>,
    seen: std::collections::HashSet<String>,
    retained_identity_bytes: usize,
}

impl GjcReader {
    pub fn new(session: impl Into<String>) -> Self {
        Self {
            key: SessionKey::new(CLIENT, session),
            fallback_timestamp_ms: 0,
            workspace_label: None,
            header_session: None,
            rows: Vec::new(),
            seen: std::collections::HashSet::new(),
            retained_identity_bytes: 0,
        }
    }

    pub fn with_fallback_timestamp(mut self, timestamp_ms: Option<i64>) -> Self {
        self.fallback_timestamp_ms = timestamp_ms.filter(|value| *value >= 0).unwrap_or(0);
        self
    }

    pub fn ingest(&mut self, record: &[u8]) -> Result<(), ReaderError> {
        let text = std::str::from_utf8(record).map_err(|_| ReaderError::Declined("gjc line"))?;
        if text.trim().is_empty() {
            return Ok(());
        }
        let value: Value =
            serde_json::from_str(text).map_err(|_| ReaderError::Declined("gjc entry"))?;
        match value.get("type").and_then(Value::as_str) {
            Some("session") => {
                if let Some(id) = value
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|id| !id.is_empty() && id.len() <= MAX_IDENTITY_BYTES)
                {
                    self.header_session = Some(id.to_owned());
                }
                if let Some(label) = value
                    .get("cwd")
                    .and_then(Value::as_str)
                    .and_then(workspace_label)
                {
                    self.workspace_label = Some(label);
                }
                Ok(())
            }
            Some("message") => self.ingest_message(&value, text),
            // `service_tier_change` and any other entry type.
            _ => Ok(()),
        }
    }

    fn ingest_message(&mut self, entry: &Value, line: &str) -> Result<(), ReaderError> {
        let Some(message) = entry.get("message") else {
            return Ok(());
        };
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            return Ok(());
        }
        let Some(usage) = message.get("usage").filter(|usage| usage.is_object()) else {
            return Ok(());
        };
        let Some(model) = message
            .get("model")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|model| !model.is_empty())
        else {
            return Ok(());
        };
        if model.len() > MAX_IDENTITY_BYTES {
            return Err(ReaderError::Declined("gjc model identity"));
        }
        let tokens = TokenCounts {
            input: counter(usage, "input")?,
            output: counter(usage, "output")?,
            cache_read: counter(usage, "cacheRead")?,
            cache_write: counter(usage, "cacheWrite")?,
            reasoning: counter(usage, "reasoning")?,
        };
        let session = self
            .header_session
            .clone()
            .unwrap_or_else(|| self.key.session.clone());
        let timestamp_ms = message
            .get("timestamp")
            .and_then(Value::as_i64)
            .filter(|value| *value > 0)
            .or_else(|| {
                entry
                    .get("timestamp")
                    .and_then(Value::as_str)
                    .and_then(parse_timestamp_str)
            })
            .unwrap_or(self.fallback_timestamp_ms);
        let provider = message
            .get("provider")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|provider| !provider.is_empty())
            .map(str::to_owned);

        let dedup_key = match entry
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
        {
            Some(id) => format!("gjc:{session}:{id}"),
            None => derived_key(
                &session,
                timestamp_ms,
                model,
                provider.as_deref(),
                tokens,
                line,
            ),
        };
        if dedup_key.len() > MAX_IDENTITY_BYTES {
            return Err(ReaderError::Declined("gjc identity"));
        }
        if !self.seen.insert(dedup_key.clone()) {
            return Ok(());
        }
        self.retained_identity_bytes = self
            .retained_identity_bytes
            .checked_add(dedup_key.len())
            .filter(|bytes| *bytes <= MAX_RETAINED_IDENTITY_BYTES)
            .ok_or(ReaderError::Capacity("gjc identities"))?;
        if self.rows.len() == MAX_RESPONSES {
            return Err(ReaderError::Capacity("gjc responses"));
        }

        self.rows.push(ResponseUsage {
            key: SessionKey::new(CLIENT, session),
            model: model.to_owned(),
            tokens,
            cache_write_1h: 0,
            timestamp_ms,
            dedup_key: Some(dedup_key),
            billing_provider_hint: provider,
            service_tier: None,
            agent: None,
            workspace_key: None,
            workspace_label: self.workspace_label.clone(),
            title: None,
            cost: usage
                .get("cost")
                .and_then(|cost| cost.get("total"))
                .and_then(Value::as_f64)
                .filter(|total| total.is_finite() && *total >= 0.0)
                .map_or(CostStatus::Unavailable, |total| {
                    CostStatus::Reported((total * 1e9).round() as u64)
                }),
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

/// A deterministic fallback identity for a message with no stable id.
///
/// Folds the session, end timestamp, model, provider, the token breakdown and a
/// hash of the line itself, so a structurally identical replay collapses while
/// distinct messages stay apart. `DefaultHasher` is used because it is stable
/// for a fixed input within a toolchain, which is all a dedup key needs.
fn derived_key(
    session: &str,
    timestamp_ms: i64,
    model: &str,
    provider: Option<&str>,
    tokens: TokenCounts,
    line: &str,
) -> String {
    let mut hasher = DefaultHasher::new();
    line.hash(&mut hasher);
    format!(
        "gjc:{session}:{timestamp_ms}:{model}:{}:{}-{}-{}-{}-{}:{:x}",
        provider.unwrap_or(""),
        tokens.input,
        tokens.output,
        tokens.cache_read,
        tokens.cache_write,
        tokens.reasoning,
        hasher.finish(),
    )
}

fn counter(usage: &Value, key: &str) -> Result<u64, ReaderError> {
    match usage.get(key) {
        None | Some(Value::Null) => Ok(0),
        Some(value) => value.as_u64().ok_or(ReaderError::Declined("gjc counter")),
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
