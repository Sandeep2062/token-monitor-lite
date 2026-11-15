//! Devin CLI usage, adapted from the pinned reader
//! `crates/tokscale-core/src/sessions/devin.rs`. See
//! docs/migration/parser-provenance.md.
//!
//! Devin CLI keeps sessions in `~/.local/share/devin/cli/sessions.db`. Usage
//! lives in `message_nodes.chat_message`, a JSON blob whose
//! `metadata.metrics` carries the counters. The separate `metadata` SQL column
//! is always NULL in real Devin databases and is not read.
//!
//! # Why the Desktop lane is out of scope
//!
//! The pinned reader also covers Devin Desktop's NDJSON event streams
//! (`acp-events/*.ndjson`). Those file names are independent UUIDs, so it can
//! only attribute them by building a title-to-session map from **this** CLI
//! database and then reconciling across sibling files. This codebase's reader
//! contract is a byte slice per source with no sibling or cross-source access,
//! so that lane is documented as unsupported rather than attributed from a
//! guess. `acp-messages` is not a supported source either.
//!
//! # Model identity
//!
//! The per-message `metadata.generation_model` wins over `sessions.model`,
//! because the session column can be `"adaptive"` — a Devin routing mode, not
//! a model id — or empty. A row that resolves to no real model is skipped
//! rather than reported under a fictitious one.
//!
//! # Duplicate rows
//!
//! Devin can persist the same assistant response in more than one
//! `message_nodes` row (for example with and without UI-only tool-call
//! metadata). `row_id` differs between those copies while
//! `metadata.request_id` identifies the underlying API request, so the request
//! id is the dedup identity and `row_id` is only the fallback for older rows
//! that lack it.
//!
//! # Timestamp
//!
//! `message_nodes.created_at` is stamped when the row is written — once the
//! assistant message and its metrics are final, i.e. the turn's **end**.
//! `metrics.total_time_ms` is that turn's elapsed generation time, so the start
//! anchor is back-calculated from it. The subtraction only applies when
//! `created_at` is this row's own recorded time; when it is absent the value
//! falls back to the database mtime, and subtracting from that would move the
//! message into the wrong day rather than anchoring it correctly.
//!
//! Not implemented: `duration_ms` (this codebase's observation has no such
//! field), and `inferred_provider_from_model` — an unrecognized model keeps no
//! provider hint rather than being attributed to `devin`.

use super::{CostStatus, ReaderError, ResponseUsage, jsonl::MAX_RECORD_BYTES};
use crate::accounting::{SessionKey, TokenCounts};
use serde_json::Value;

pub const MAX_RESPONSES: usize = 16_384;
const MAX_IDENTITY_BYTES: usize = 4_096;
const MAX_RETAINED_IDENTITY_BYTES: usize = 8 * 1024 * 1024;

pub const CLIENT: &str = "devin";

/// `sessions.model` can hold this instead of a model id. It names a routing
/// mode, so it must never be reported as a model.
const ROUTING_MODES: [&str; 1] = ["adaptive"];

/// One `message_nodes` row joined to its session.
pub struct DevinRow<'a> {
    pub row_id: i64,
    pub session_id: &'a str,
    /// The `chat_message` JSON blob.
    pub chat_message: &'a [u8],
    /// `created_at * 1000`, already converted from Unix seconds. `None` when the
    /// column is NULL, in which case the caller's mtime fallback applies.
    pub created_at_ms: Option<i64>,
    pub session_model: Option<&'a str>,
    pub working_directory: Option<&'a str>,
}

/// One Devin CLI database.
pub struct DevinReader {
    responses: Vec<ResponseUsage>,
    retained_identity_bytes: usize,
}

impl DevinReader {
    pub fn new() -> Self {
        Self {
            responses: Vec::new(),
            retained_identity_bytes: 0,
        }
    }

    /// Ingest one row, using `fallback_timestamp_ms` where the row has none.
    pub fn ingest(
        &mut self,
        row: DevinRow<'_>,
        fallback_timestamp_ms: i64,
    ) -> Result<(), ReaderError> {
        if row.chat_message.len() > MAX_RECORD_BYTES {
            return Err(ReaderError::Capacity("Devin chat message"));
        }
        if row.session_id.is_empty() || row.session_id.len() > MAX_IDENTITY_BYTES {
            return Err(ReaderError::InvalidRecord);
        }
        let message: Value =
            serde_json::from_slice(row.chat_message).map_err(|_| ReaderError::InvalidRecord)?;
        // Only assistant rows carry usage; user and tool rows are not calls.
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            return Ok(());
        }
        let metadata = message.get("metadata");
        let metrics = metadata.and_then(|metadata| metadata.get("metrics"));

        let Some(model) = model(&row, metadata) else {
            // No real model: reporting the row under a fictitious one would be
            // worse than omitting it.
            return Ok(());
        };
        let tokens = tokens(metrics, metadata)?;
        if zero(&tokens) {
            // An assistant row with no attributable usage must not become a
            // precedence marker for anything else.
            return Ok(());
        }
        self.push(row, metadata, model, tokens, fallback_timestamp_ms)
    }

    fn push(
        &mut self,
        row: DevinRow<'_>,
        metadata: Option<&Value>,
        model: String,
        tokens: TokenCounts,
        fallback_timestamp_ms: i64,
    ) -> Result<(), ReaderError> {
        let duration_ms = metrics_duration(metadata);
        // Back-anchor only when `created_at` is this row's own completion time.
        // Otherwise the value is the database mtime, and subtracting a turn's
        // duration from it would land on the wrong day.
        let timestamp_ms = match (row.created_at_ms, duration_ms.filter(|ms| *ms > 0)) {
            (Some(end), Some(duration)) => back_anchor(end, duration),
            _ => row.created_at_ms.unwrap_or(fallback_timestamp_ms),
        };
        let request_id = metadata
            .and_then(|metadata| metadata.get("request_id"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty());
        // `request_id` identifies the underlying API call, which Devin may
        // persist in several rows; `row_id` is only for older rows lacking it.
        let dedup_key = match request_id {
            Some(request_id) => format!("devin-cli:{}:request:{request_id}", row.session_id),
            None => format!("devin-cli:{}:row:{}", row.session_id, row.row_id),
        };
        if dedup_key.len() > MAX_IDENTITY_BYTES {
            return Err(ReaderError::Unsupported("Devin identity"));
        }
        // A raw working directory is an absolute path, which the storage layer
        // refuses in every runtime identity column; only its final component
        // survives, as a label.
        let workspace_label = row.working_directory.and_then(workspace_label);

        let usage = ResponseUsage {
            key: SessionKey::new(CLIENT, row.session_id),
            model,
            tokens,
            cache_write_1h: 0,
            timestamp_ms,
            dedup_key: Some(dedup_key.clone()),
            // No inference table here, so an unrecognized model gets no hint
            // rather than being attributed to Devin.
            billing_provider_hint: None,
            service_tier: None,
            agent: None,
            workspace_key: None,
            workspace_label,
            title: None,
            cost: CostStatus::Unavailable,
        };
        if self.responses.len() == MAX_RESPONSES {
            return Err(ReaderError::Capacity("Devin responses"));
        }
        self.retained_identity_bytes = self
            .retained_identity_bytes
            .checked_add(dedup_key.len())
            .filter(|bytes| *bytes <= MAX_RETAINED_IDENTITY_BYTES)
            .ok_or(ReaderError::Capacity("Devin identities"))?;
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

impl Default for DevinReader {
    fn default() -> Self {
        Self::new()
    }
}

/// The per-message generation model, then the session's — never a routing mode.
fn model(row: &DevinRow<'_>, metadata: Option<&Value>) -> Option<String> {
    let generated = metadata
        .and_then(|metadata| metadata.get("generation_model"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|model| !model.is_empty());
    let session = row
        .session_model
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .filter(|model| {
            !ROUTING_MODES
                .iter()
                .any(|mode| mode.eq_ignore_ascii_case(model))
        });
    generated.or(session).map(str::to_owned)
}

/// The metric counters, falling back to `num_tokens` as output.
///
/// A row with `metrics` but no counted tokens falls back to `num_tokens`
/// attributed to output, so the message is still counted; that fallback is the
/// only way an older row reports anything.
fn tokens(metrics: Option<&Value>, metadata: Option<&Value>) -> Result<TokenCounts, ReaderError> {
    let from_metrics = match metrics {
        Some(metrics) => TokenCounts {
            input: counter(metrics, "input_tokens")?,
            output: counter(metrics, "output_tokens")?,
            cache_read: counter(metrics, "cache_read_tokens")?,
            cache_write: counter(metrics, "cache_creation_tokens")?,
            reasoning: 0,
        },
        None => TokenCounts::default(),
    };
    if !zero(&from_metrics) {
        return Ok(from_metrics);
    }
    let Some(num_tokens) = metadata.and_then(|metadata| metadata.get("num_tokens")) else {
        return Ok(from_metrics);
    };
    let num_tokens = num_tokens.as_u64().ok_or(ReaderError::InvalidRecord)?;
    Ok(TokenCounts {
        output: num_tokens,
        ..TokenCounts::default()
    })
}

fn metrics_duration(metadata: Option<&Value>) -> Option<i64> {
    metadata
        .and_then(|metadata| metadata.get("metrics"))
        .and_then(|metrics| metrics.get("total_time_ms"))
        .and_then(Value::as_i64)
        .map(|value| value.max(0))
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

/// The start of a turn that ended at `end_ms` and took `duration_ms`.
fn back_anchor(end_ms: i64, duration_ms: i64) -> i64 {
    end_ms.saturating_sub(duration_ms).max(0)
}

/// The final component of a working directory, never the whole path.
fn workspace_label(path: &str) -> Option<String> {
    let trimmed = path.trim().trim_end_matches(['/', '\\']);
    let label = trimmed.rsplit(['/', '\\']).next()?;
    if label.is_empty() || label.len() > MAX_RECORD_BYTES || label.chars().any(char::is_control) {
        return None;
    }
    Some(label.to_owned())
}
