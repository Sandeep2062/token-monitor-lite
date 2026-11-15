//! Qoder CN local usage, adapted from the pinned Token Monitor fork reader
//! `crates/tokscale-core/src/token_monitor/qodercn.rs`. See
//! docs/migration/parser-provenance.md.
//!
//! Qoder CN is one opt-in, fork-only client with two lanes:
//!
//! - the legacy `SharedClientCache/cache/db/local.db` SQLite store, whose
//!   `chat_message` rows carry `token_info` JSON;
//! - the Claude-compatible JSONL transcripts that 2026-09+ builds write under
//!   `~/.qoder-cn/projects`.
//!
//! Both report a **cache-inclusive** prompt count, so the two arithmetic rules
//! are the same in each lane: `input` is `prompt - cached` clamped at zero, and
//! `cache_read` is `min(prompt, cached)`. `cache_write` and `reasoning` are
//! never persisted by the app and stay zero rather than being inferred.
//!
//! Not implemented, and recorded as unsupported coverage rather than guessed:
//!
//! - the fork's per-source last-complete-snapshot cache with its length/mtime
//!   and WAL fingerprints. A failed read here is a visible failure that keeps
//!   the prior committed generation, which the runtime already guarantees; the
//!   reader does not maintain a second copy.
//! - the `unpriced:qodercn` provider marker. A routing tier or unmapped model
//!   is reported with no provider hint instead, so catalog pricing cannot
//!   attach a price to a tier name.
//! - cross-format dedup. The fork keeps its own `qodercn:<lane>:<source>:...`
//!   ids so Token Monitor's keys do not change, but it does not invent a key
//!   spanning the two lanes, and neither does this reader.

use super::{CostStatus, ReaderError, ResponseUsage, jsonl::MAX_RECORD_BYTES};
use crate::accounting::{SessionKey, TokenCounts};

pub const MAX_RESPONSES: usize = 16_384;
const MAX_IDENTITY_BYTES: usize = 4_096;
const MAX_RETAINED_IDENTITY_BYTES: usize = 8 * 1024 * 1024;

pub const CLIENT: &str = "qodercn";
pub const DEFAULT_MODEL: &str = "qoder-agent";

/// Qoder CN stores internal model codes; these are the app's own display names.
/// A code that is not listed is a custom model and passes through unchanged.
const MODEL_DISPLAY_NAMES: &[(&str, &str)] = &[
    ("auto", "Auto"),
    ("dashscope_qmodel", "Qwen3.7-Plus"),
    ("dashscope_qwen3_coder", "Qwen3-Coder-Plus"),
    ("dashscope_qwen_max_latest", "Qwen3-Max"),
    ("dfmodel", "DeepSeek-V4-Flash"),
    ("dmodel", "DeepSeek-V4-Pro"),
    ("efficient", "Efficient"),
    ("gm51model", "GLM-5.2"),
    ("gmodel", "GLM-5"),
    ("kmodel", "Kimi-K2.7-Code"),
    ("lite", "Lite"),
    ("mmodel", "MiniMax-M3"),
    ("performance", "Performance"),
    ("q35model", "Qwen3.5-Plus"),
    ("q35model_preview", "Qwen3.8-Max-Preview"),
    ("q36fmodel", "Qwen3.6-Flash"),
    ("qmodel", "Qwen3.7-Plus"),
    ("qmodel_latest", "Qwen3.7-Max"),
    ("qmodel_preview", "Qwen3.8-Max-Preview"),
    ("ultimate", "Ultimate"),
];

/// A routing tier names a tier, not the model behind it, so no catalog price
/// may attach to it.
const ROUTING_TIERS: &[&str] = &["auto", "ultimate", "performance", "efficient", "lite"];

/// One usage row, from either lane.
pub struct QodercnRow<'a> {
    pub lane: Lane,
    /// The source namespace, so two stores with overlapping ids stay distinct.
    pub source: &'a str,
    pub session_id: Option<&'a str>,
    pub message_id: Option<&'a str>,
    /// Cache-inclusive prompt count.
    pub prompt: u64,
    pub cached: u64,
    pub output: u64,
    pub model: Option<&'a str>,
    /// A real path (JSONL `cwd`) or the legacy store's project name.
    pub workspace: Option<&'a str>,
    pub created_at_ms: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lane {
    /// The legacy `local.db` store.
    Database,
    /// A Claude-compatible transcript.
    Jsonl,
}

impl Lane {
    fn label(self) -> &'static str {
        match self {
            Self::Database => "db",
            Self::Jsonl => "jsonl",
        }
    }
}

/// One Qoder CN source.
pub struct QodercnReader {
    /// The source namespace, so two stores with overlapping ids stay distinct.
    source: String,
    responses: Vec<ResponseUsage>,
    retained_identity_bytes: usize,
}

impl QodercnReader {
    /// A transcript source: usage arrives as whole JSONL records.
    pub fn new(source: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            responses: Vec::new(),
            retained_identity_bytes: 0,
        }
    }

    /// Ingest one transcript line. Non-assistant lines carry no usage.
    pub fn ingest_record(&mut self, record: &[u8]) -> Result<(), ReaderError> {
        let Some(line) = parse_transcript_line(record)? else {
            return Ok(());
        };
        let source = self.source.clone();
        self.ingest(QodercnRow {
            lane: Lane::Jsonl,
            source: &source,
            session_id: line.session_id.as_deref(),
            message_id: line.message_id.as_deref(),
            prompt: line.prompt,
            cached: line.cached,
            output: line.output,
            model: line.model.as_deref(),
            workspace: line.workspace.as_deref(),
            created_at_ms: line.created_at_ms,
        })
    }

    pub fn ingest(&mut self, row: QodercnRow<'_>) -> Result<(), ReaderError> {
        // A plan-billed first-party row records credits and a context ratio but
        // no tokens, so an all-zero row is not measured zero usage.
        if row.prompt == 0 && row.output == 0 {
            return Ok(());
        }
        // The prompt count includes the cached part; split it so the two
        // buckets never double-count.
        let input = row.prompt.saturating_sub(row.cached);
        let cache_read = row.prompt.min(row.cached);
        let model = display_model(row.model.unwrap_or(DEFAULT_MODEL));
        if !valid_identity(&model) {
            return Err(ReaderError::InvalidRecord);
        }
        let session_tail = row
            .session_id
            .filter(|value| valid_identity(value))
            .unwrap_or("unknown");
        let session = format!(
            "{CLIENT}:{}:{}:{session_tail}",
            row.lane.label(),
            row.source
        );
        if session.len() > MAX_IDENTITY_BYTES {
            return Err(ReaderError::Unsupported("Qoder CN session identity"));
        }
        let message = row
            .message_id
            .filter(|value| valid_identity(value))
            .unwrap_or(session_tail);
        let identity = format!("{session}:{message}");
        if identity.len() > MAX_IDENTITY_BYTES {
            return Err(ReaderError::Unsupported("Qoder CN message identity"));
        }
        // A real path is kept as the workspace key; a bare project name is kept
        // as the label. Either way an absolute path must not reach the store.
        let (workspace_key, workspace_label) = match row.workspace {
            Some(workspace) if !workspace.is_empty() => classify_workspace(workspace),
            _ => (None, None),
        };
        let usage = ResponseUsage {
            key: SessionKey::new(CLIENT, &session),
            model: model.clone(),
            tokens: TokenCounts {
                input,
                output: row.output,
                cache_read,
                cache_write: 0,
                reasoning: 0,
            },
            cache_write_1h: 0,
            timestamp_ms: row.created_at_ms,
            dedup_key: Some(identity),
            // A routing tier is a tier name, not a model, so it carries no
            // provider hint and cannot be priced from the catalog.
            billing_provider_hint: (!is_routing_tier(&model)).then(|| CLIENT.to_owned()),
            service_tier: None,
            agent: None,
            workspace_key,
            workspace_label,
            title: None,
            cost: CostStatus::Unavailable,
        };
        self.push(usage)
    }

    fn push(&mut self, usage: ResponseUsage) -> Result<(), ReaderError> {
        if self.responses.len() == MAX_RESPONSES {
            return Err(ReaderError::Capacity("Qoder CN responses"));
        }
        let identity_bytes = usage.dedup_key.as_ref().map_or(0, String::len);
        self.retained_identity_bytes = self
            .retained_identity_bytes
            .checked_add(identity_bytes)
            .filter(|bytes| *bytes <= MAX_RETAINED_IDENTITY_BYTES)
            .ok_or(ReaderError::Capacity("Qoder CN identities"))?;
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

/// Extract the usage counters from a legacy `token_info` object.
pub fn database_counters(token_info: &serde_json::Value) -> Result<(u64, u64, u64), ReaderError> {
    let Some(object) = token_info.as_object() else {
        return Err(ReaderError::InvalidRecord);
    };
    let prompt = numeric(object.get("prompt_tokens"))?.ok_or(ReaderError::InvalidRecord)?;
    let cached = numeric(object.get("cached_tokens"))?.unwrap_or(0);
    let output = numeric(object.get("completion_tokens"))?.ok_or(ReaderError::InvalidRecord)?;
    Ok((prompt, cached, output))
}

/// Extract the usage counters from a transcript `message.usage` object.
pub fn jsonl_counters(usage: &serde_json::Value) -> Result<(u64, u64, u64), ReaderError> {
    let Some(object) = usage.as_object() else {
        return Err(ReaderError::InvalidRecord);
    };
    let prompt = numeric(object.get("input_tokens"))?.ok_or(ReaderError::InvalidRecord)?;
    let cached = numeric(object.get("cache_read_input_tokens"))?.unwrap_or(0);
    let output = numeric(object.get("output_tokens"))?.ok_or(ReaderError::InvalidRecord)?;
    Ok((prompt, cached, output))
}

/// The app's display name for an internal model code.
pub fn display_model(code: &str) -> String {
    MODEL_DISPLAY_NAMES
        .iter()
        .find(|(key, _)| *key == code)
        .map(|(_, name)| (*name).to_string())
        .unwrap_or_else(|| code.to_string())
}

/// A transcript model string, with only the `qoder-custom-<profile>/` prefix
/// stripped: the real model may itself be provider-qualified.
pub fn transcript_model(raw: &str) -> String {
    let model = raw.trim();
    if model.is_empty() {
        return String::new();
    }
    let key = match model.find('/') {
        Some(slash) if model.starts_with("qoder-custom-") && slash > 0 => &model[slash + 1..],
        _ => model,
    };
    display_model(key)
}

fn is_routing_tier(model: &str) -> bool {
    ROUTING_TIERS.contains(&model.trim().to_ascii_lowercase().as_str())
}

/// Keep a real path as the workspace key and a bare project name as the label.
///
/// The runtime store rejects absolute-path text in identity columns, so a path
/// is reduced to its final component there, while `project_name` is already a
/// bare name and is safe as a label.
fn classify_workspace(workspace: &str) -> (Option<String>, Option<String>) {
    // The legacy store's "." sentinel means "no project".
    let value = workspace.trim().trim_end_matches(['/', '\\']);
    if value.is_empty() || value == "." {
        return (None, None);
    }
    if value.contains(['/', '\\']) {
        let label = value
            .rsplit(['/', '\\'])
            .find(|part| !part.is_empty())
            .filter(|part| valid_identity(part))
            .map(str::to_owned);
        return (None, label);
    }
    if !valid_identity(value) {
        return (None, None);
    }
    (None, Some(value.to_owned()))
}

/// `Number(value)` truncated, rejecting negatives and non-finite values.
/// JSON `null` coerces to zero, matching the adapter this mirrors.
fn numeric(value: Option<&serde_json::Value>) -> Result<Option<u64>, ReaderError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let number = match value {
        serde_json::Value::Null => 0.0,
        serde_json::Value::Bool(flag) => f64::from(u8::from(*flag)),
        serde_json::Value::Number(number) => number.as_f64().ok_or(ReaderError::InvalidRecord)?,
        serde_json::Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                0.0
            } else {
                trimmed
                    .parse::<f64>()
                    .map_err(|_| ReaderError::InvalidRecord)?
            }
        }
        _ => return Err(ReaderError::InvalidRecord),
    };
    if !number.is_finite() || number < 0.0 || number > MAX_SAFE_INTEGER as f64 {
        return Err(ReaderError::InvalidRecord);
    }
    Ok(Some(number.trunc() as u64))
}

const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

fn valid_identity(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_IDENTITY_BYTES && !value.chars().any(char::is_control)
}

/// A transcript line's parseable subset, so the reader stays byte-slice only.
pub fn parse_transcript_line(line: &[u8]) -> Result<Option<TranscriptUsage>, ReaderError> {
    if line.len() > MAX_RECORD_BYTES {
        return Err(ReaderError::Capacity("Qoder CN line"));
    }
    let value: serde_json::Value =
        serde_json::from_slice(line).map_err(|_| ReaderError::InvalidRecord)?;
    let Some(object) = value.as_object() else {
        return Err(ReaderError::InvalidRecord);
    };
    if object.get("type").and_then(serde_json::Value::as_str) != Some("assistant") {
        return Ok(None);
    }
    let message = object.get("message");
    let Some(usage) = message
        .and_then(|message| message.get("usage"))
        .filter(|usage| usage.is_object())
    else {
        return Ok(None);
    };
    let (prompt, cached, output) = jsonl_counters(usage)?;
    Ok(Some(TranscriptUsage {
        session_id: object
            .get("sessionId")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        message_id: message
            .and_then(|message| message.get("id"))
            .and_then(serde_json::Value::as_str)
            .or_else(|| object.get("uuid").and_then(serde_json::Value::as_str))
            .map(str::to_owned),
        model: message
            .and_then(|message| message.get("model"))
            .and_then(serde_json::Value::as_str)
            .map(transcript_model)
            .filter(|model| !model.is_empty()),
        workspace: object
            .get("cwd")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        created_at_ms: object
            .get("timestamp")
            .and_then(crate::parsers::timestamps::parse_timestamp_value)
            .unwrap_or(0),
        prompt,
        cached,
        output,
    }))
}

/// The usage subset of one transcript line.
pub struct TranscriptUsage {
    pub session_id: Option<String>,
    pub message_id: Option<String>,
    pub model: Option<String>,
    pub workspace: Option<String>,
    pub created_at_ms: i64,
    pub prompt: u64,
    pub cached: u64,
    pub output: u64,
}
