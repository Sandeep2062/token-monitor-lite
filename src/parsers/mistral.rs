//! Mistral Vibe session usage, adapted from the pinned reader
//! `crates/tokscale-core/src/sessions/vibe.rs`. See
//! docs/migration/parser-provenance.md.
//!
//! Mistral Vibe (Mistral AI's terminal coding agent) stores session logs under
//! `~/.vibe/logs/session/` (or `$VIBE_HOME/logs/session/`):
//!
//! - Vibe 2.x: `session_YYYYMMDD_HHMMSS_<short_id>/meta.json`
//! - Vibe 1.x: the flat `session_*.json`
//!
//! Usage is read from `meta.json`'s `stats` (or the top-level `metadata` key in
//! a legacy file). The tracked client id is `mistral`; the pinned reader calls
//! the format `vibe`.
//!
//! # The cached count is a subset, not an addition
//!
//! `session_cached_tokens` is a **subset** of `session_prompt_tokens`, not a
//! separate bucket: upstream types the field that way and its own cost function
//! subtracts the cached count before charging the input rate. So the split is
//! `input = prompt - cached`, not `input = prompt` with cache added on top.
//!
//! # Session granularity
//!
//! A Vibe transcript carries no per-message model, timestamp or token counts, so
//! one observation per session is the finest row the format supports. That is a
//! property of the file, not a limitation of this reader.
//!
//! # Model resolution
//!
//! `config.active_model` is the pinned alias, but it is **empty by default**:
//! upstream documents `""` as the "unpinned" sentinel meaning "follow the
//! configured default", not missing data. A blank value therefore falls through
//! instead of being accepted, because an empty model id leaves the model blank
//! and — having no pricing row to match — prices the whole session at zero.
//! Resolution mirrors upstream: the pinned alias, then the session's own
//! `config.models` table, then the published default alias.
//!
//! Not implemented: `message_count` (this codebase's observation has no such
//! field), and `normalize_workspace_key` — a raw directory is an absolute path,
//! which the storage layer refuses in every runtime identity column, so only the
//! final component survives as a label.

use super::timestamps::parse_timestamp_value;
use super::{CostStatus, ReaderError, ResponseUsage, jsonl::MAX_RECORD_BYTES};
use crate::accounting::{SessionKey, TokenCounts};
use serde_json::Value;

pub const MAX_RESPONSES: usize = 16_384;
const MAX_IDENTITY_BYTES: usize = 4_096;
const MAX_RETAINED_IDENTITY_BYTES: usize = 8 * 1024 * 1024;

pub const CLIENT: &str = "mistral";

/// The alias Vibe resolves an unpinned session to when the snapshot carries no
/// `config.models` table. Upstream publishes this alias; the `name` on the same
/// record is the CLI-routed wire id, which no pricing dataset carries.
pub const DEFAULT_MODEL_ALIAS: &str = "mistral-medium-latest";

/// One Vibe session metadata document.
pub struct VibeReader {
    key: SessionKey,
    fallback_timestamp_ms: i64,
    responses: Vec<ResponseUsage>,
    retained_identity_bytes: usize,
}

impl VibeReader {
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

    /// Ingest a whole session metadata document.
    pub fn ingest(&mut self, document: &[u8]) -> Result<(), ReaderError> {
        if document.len() > MAX_RECORD_BYTES {
            return Err(ReaderError::Capacity("Vibe session metadata"));
        }
        self.responses.clear();
        self.retained_identity_bytes = 0;

        let root: Value =
            serde_json::from_slice(document).map_err(|_| ReaderError::InvalidRecord)?;
        // Vibe 1.x flat files wrap metadata under a `metadata` key; Vibe 2.x
        // `meta.json` has it at the root.
        let meta = match root.get("metadata").filter(|value| value.is_object()) {
            Some(inner) => inner,
            None => &root,
        };
        let Some(stats) = meta.get("stats").filter(|value| value.is_object()) else {
            return Ok(());
        };
        let tokens = tokens(stats)?;
        if zero(&tokens) {
            return Ok(());
        }

        let session = meta
            .get("session_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty());
        if let Some(session) = session
            && session.len() <= MAX_IDENTITY_BYTES
        {
            self.key = SessionKey::new(CLIENT, session);
        }

        let timestamp_ms = meta
            .get("end_time")
            .and_then(parse_timestamp_value)
            .or_else(|| meta.get("start_time").and_then(parse_timestamp_value))
            .filter(|value| *value > 0)
            .unwrap_or(self.fallback_timestamp_ms);

        let model = resolve_model(meta);
        if model.len() > MAX_IDENTITY_BYTES {
            return Err(ReaderError::Declined("Vibe model identity"));
        }
        let dedup_key = format!("vibe:{}", self.key.session);
        if dedup_key.len() > MAX_IDENTITY_BYTES {
            return Err(ReaderError::Declined("Vibe identity"));
        }
        let workspace_label = origin_directory(meta).and_then(workspace_label);
        let title = meta
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|title| !title.is_empty() && title.len() <= MAX_RECORD_BYTES)
            .map(str::to_owned);

        let usage = ResponseUsage {
            key: self.key.clone(),
            model,
            tokens,
            cache_write_1h: 0,
            timestamp_ms,
            dedup_key: Some(dedup_key.clone()),
            // Vibe sessions report neither a provider record nor a cost.
            billing_provider_hint: None,
            service_tier: None,
            agent: None,
            workspace_key: None,
            workspace_label,
            title,
            cost: CostStatus::Unavailable,
        };
        if self.responses.len() == MAX_RESPONSES {
            return Err(ReaderError::Capacity("Vibe responses"));
        }
        self.retained_identity_bytes = self
            .retained_identity_bytes
            .checked_add(dedup_key.len())
            .filter(|bytes| *bytes <= MAX_RETAINED_IDENTITY_BYTES)
            .ok_or(ReaderError::Capacity("Vibe identities"))?;
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

/// `session_cached_tokens` is a subset of the prompt count, so it is split out
/// rather than added.
fn tokens(stats: &Value) -> Result<TokenCounts, ReaderError> {
    let prompt = counter(stats, "session_prompt_tokens")?;
    let completion = counter(stats, "session_completion_tokens")?;
    let cached = counter(stats, "session_cached_tokens")?;
    Ok(TokenCounts {
        input: prompt.saturating_sub(cached),
        output: completion,
        cache_read: cached,
        cache_write: 0,
        reasoning: 0,
    })
}

/// The pinned alias, then the single configured model, then the published
/// default.
fn resolve_model(meta: &Value) -> String {
    let pinned = meta
        .pointer("/config/active_model")
        .and_then(Value::as_str)
        .or_else(|| meta.get("active_model").and_then(Value::as_str))
        .or_else(|| meta.get("model").and_then(Value::as_str))
        .map(str::trim)
        .filter(|model| !model.is_empty());
    if let Some(model) = pinned {
        return model.to_owned();
    }

    // An unpinned session still names the models it was configured with. The
    // record's own `alias` wins over its map key: both identify the same model,
    // but the alias is what the CLI displays and what upstream pins.
    if let Some(models) = meta.pointer("/config/models").and_then(Value::as_object) {
        let mut aliases: Vec<&str> = models
            .iter()
            .map(|(key, entry)| {
                entry
                    .get("alias")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|alias| !alias.is_empty())
                    .unwrap_or(key.as_str())
            })
            .filter(|alias| !alias.is_empty())
            .collect();
        if aliases.len() == 1 {
            return aliases[0].to_owned();
        }
        // Several configured models and nothing pinned: the transcript cannot
        // say which ran, so pick deterministically rather than letting map
        // iteration order decide.
        if aliases.len() > 1 {
            aliases.sort_unstable();
            if let Some(default) = aliases
                .iter()
                .find(|alias| **alias == DEFAULT_MODEL_ALIAS)
                .or_else(|| aliases.first())
            {
                return (*default).to_owned();
            }
        }
    }

    DEFAULT_MODEL_ALIAS.to_owned()
}

/// The session's origin directory, from either spelling.
fn origin_directory(meta: &Value) -> Option<&str> {
    meta.get("origin_directory")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|directory| !directory.is_empty())
        .or_else(|| {
            meta.pointer("/environment/working_directory")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|directory| !directory.is_empty())
        })
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

/// The final component of an origin directory, never the whole path.
fn workspace_label(path: &str) -> Option<String> {
    let trimmed = path.trim().trim_end_matches(['/', '\\']);
    let label = trimmed.rsplit(['/', '\\']).next()?;
    if label.is_empty() || label.len() > MAX_RECORD_BYTES || label.chars().any(char::is_control) {
        return None;
    }
    Some(label.to_owned())
}
