//! Kiro CLI session normalization adapted from the pinned Tokscale `kiro.rs`
//! reader. MIT © 2025 Junho Yeo; see docs/migration/parser-provenance.md.
//!
//! Supported input is Kiro CLI's `~/.kiro/sessions/cli/<session>.json`
//! session document. Its `session_state.conversation_metadata`
//! `user_turn_metadatas` array holds one entry per turn, and each entry may
//! carry authoritative `input_token_count` / `output_token_count` /
//! `cache_read_input_token_count` / `cache_write_input_token_count` counters
//! plus provider-reported `metering_usage` credits.
//!
//! Not implemented, and recorded as unsupported coverage rather than guessed:
//!
//! - the hybrid estimation model. Kiro's Auto agent commonly persists **zero**
//!   counters, and the pinned reader then estimates from the sibling
//!   `messages.jsonl` content plus `context_usage_percentage`. That sibling is
//!   a separate document this single-file reader cannot reach, and fabricating
//!   counts would be indistinguishable from measured ones downstream. A turn
//!   whose counters are all zero therefore contributes nothing.
//! - the SQLite `conversations_v2` store, the IDE `sessions/<workspace>/`
//!   layout, and the IDE `globalStorage` snapshots, which are three further
//!   independently-shaped stores.
//! - `duration_ms`, request counts, and `is_turn_start`, which are aggregation
//!   concerns in this codebase rather than properties of one observation.

use super::{CostStatus, ReaderError, ResponseUsage, jsonl::MAX_RECORD_BYTES};
use crate::accounting::{SessionKey, TokenCounts};
use serde_json::{Map, Value};

pub const MAX_RESPONSES: usize = 16_384;
const MAX_IDENTITY_BYTES: usize = 4_096;
const MAX_RETAINED_IDENTITY_BYTES: usize = 8 * 1024 * 1024;

pub const CLIENT: &str = "kiro";
/// Kiro bills through Amazon Bedrock.
pub const PROVIDER: &str = "amazon-bedrock";
/// The pinned reader's documented default when the header names no model.
pub const DEFAULT_MODEL: &str = "auto";

/// USD value of one Kiro credit, matching the pinned reader's published
/// pay-as-you-go overage rate. A turn whose `metering_usage` carries credits
/// gets a provider-reported cost instead of a token-priced estimate.
const CREDIT_TO_USD: f64 = 0.04;

/// One Kiro CLI session document.
pub struct KiroReader {
    fallback_session: String,
    fallback_timestamp_ms: Option<i64>,
    responses: Vec<ResponseUsage>,
    retained_identity_bytes: usize,
}

impl KiroReader {
    pub fn new(session: impl Into<String>) -> Self {
        Self {
            fallback_session: session.into(),
            fallback_timestamp_ms: None,
            responses: Vec::new(),
            retained_identity_bytes: 0,
        }
    }

    pub fn with_fallback_timestamp(mut self, timestamp_ms: Option<i64>) -> Self {
        self.fallback_timestamp_ms = timestamp_ms.filter(|value| *value >= 0);
        self
    }

    /// Read one whole session document.
    ///
    /// A document that parses but carries no turn array contributes nothing;
    /// a document that does not parse is a visible candidate failure.
    pub fn ingest(&mut self, document: &[u8]) -> Result<(), ReaderError> {
        if document.len() > MAX_RECORD_BYTES {
            return Err(ReaderError::Capacity("Kiro session"));
        }
        let value: Value =
            serde_json::from_slice(document).map_err(|_| ReaderError::InvalidRecord)?;
        let root = value.as_object().ok_or(ReaderError::InvalidRecord)?;
        let session = root
            .get("session_id")
            .and_then(Value::as_str)
            .filter(|value| valid_identity(value))
            .unwrap_or(&self.fallback_session)
            .to_owned();
        if !valid_identity(&session) {
            return Err(ReaderError::InvalidRecord);
        }
        let state = root
            .get("session_state")
            .and_then(Value::as_object)
            .ok_or(ReaderError::Unsupported("Kiro session state"))?;
        let model = state
            .get("rts_model_state")
            .and_then(Value::as_object)
            .and_then(|model| model.get("model_info"))
            .and_then(Value::as_object)
            .and_then(|info| info.get("model_id"))
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(DEFAULT_MODEL)
            .to_owned();
        if !valid_identity(&model) {
            return Err(ReaderError::InvalidRecord);
        }
        // Kiro records the session's absolute `cwd`, but persisted runtime
        // identity must never contain an absolute path. Only the final path
        // component is kept, as a label; the key stays unset.
        let workspace_label = root
            .get("cwd")
            .and_then(Value::as_str)
            .and_then(workspace_label_from_cwd);
        let turns = state
            .get("conversation_metadata")
            .and_then(Value::as_object)
            .and_then(|metadata| metadata.get("user_turn_metadatas"))
            .and_then(Value::as_array);
        let Some(turns) = turns else {
            return Ok(());
        };
        for (index, turn) in turns.iter().enumerate() {
            let Some(turn) = turn.as_object() else {
                return Err(ReaderError::InvalidRecord);
            };
            let Some(usage) = normalize_turn(
                turn,
                &session,
                &model,
                index,
                workspace_label.as_deref(),
                self.fallback_timestamp_ms,
            )?
            else {
                continue;
            };
            self.push(usage)?;
        }
        Ok(())
    }

    fn push(&mut self, usage: ResponseUsage) -> Result<(), ReaderError> {
        if self.responses.len() == MAX_RESPONSES {
            return Err(ReaderError::Capacity("Kiro responses"));
        }
        let identity_bytes = usage.dedup_key.as_ref().map_or(0, String::len);
        self.retained_identity_bytes = self
            .retained_identity_bytes
            .checked_add(identity_bytes)
            .filter(|bytes| *bytes <= MAX_RETAINED_IDENTITY_BYTES)
            .ok_or(ReaderError::Capacity("Kiro identities"))?;
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

/// Normalize one `user_turn_metadatas` entry.
///
/// Real counters win when present and positive. A turn whose counters are all
/// zero yields nothing: the pinned reader estimates there, and the native
/// reader deliberately does not invent counts.
fn normalize_turn(
    turn: &Map<String, Value>,
    session: &str,
    model: &str,
    index: usize,
    workspace: Option<&str>,
    fallback_timestamp_ms: Option<i64>,
) -> Result<Option<ResponseUsage>, ReaderError> {
    let input = positive_counter(turn, "input_token_count")?;
    let output = positive_counter(turn, "output_token_count")?;
    // A negative counter is a rejection even when another counter would make
    // the turn non-empty, because the row is then not interpretable.
    if input == 0 && output == 0 {
        return Ok(None);
    }
    let cache_read = positive_counter(turn, "cache_read_input_token_count")?;
    let cache_write = positive_counter(turn, "cache_write_input_token_count")?;
    let timestamp_ms = match turn.get("end_timestamp") {
        Some(value) => crate::parsers::timestamps::parse_timestamp_value(value)
            .ok_or(ReaderError::Declined("Kiro turn timestamp"))?,
        None => fallback_timestamp_ms.ok_or(ReaderError::Declined("Kiro turn timestamp"))?,
    };
    let identity = format!("{session}:{index}");
    if identity.len() > MAX_IDENTITY_BYTES {
        return Err(ReaderError::Declined("Kiro turn identity"));
    }
    Ok(Some(ResponseUsage {
        key: SessionKey::new(CLIENT, session),
        model: model.to_owned(),
        tokens: TokenCounts {
            input,
            output,
            cache_read,
            cache_write,
            reasoning: 0,
        },
        cache_write_1h: 0,
        timestamp_ms,
        dedup_key: Some(identity),
        billing_provider_hint: Some(PROVIDER.to_owned()),
        service_tier: None,
        agent: None,
        workspace_key: None,
        workspace_label: workspace.map(str::to_owned),
        title: None,
        cost: credit_cost(turn)?,
    }))
}

/// A provider-reported cost from the turn's `metering_usage` credit entries.
///
/// Only entries whose `unit` is `credit` count, matching the pinned reader. A
/// turn with no credit entries is unpriced rather than assumed free.
fn credit_cost(turn: &Map<String, Value>) -> Result<CostStatus, ReaderError> {
    let Some(entries) = turn.get("metering_usage") else {
        return Ok(CostStatus::Unavailable);
    };
    let Some(entries) = entries.as_array() else {
        return Err(ReaderError::InvalidRecord);
    };
    let mut credits = 0.0_f64;
    for entry in entries {
        let Some(entry) = entry.as_object() else {
            return Err(ReaderError::InvalidRecord);
        };
        if entry.get("unit").and_then(Value::as_str) != Some("credit") {
            continue;
        }
        let Some(value) = entry.get("value") else {
            continue;
        };
        if value.is_null() {
            continue;
        }
        let value = value.as_f64().ok_or(ReaderError::InvalidRecord)?;
        if !value.is_finite() || value < 0.0 {
            return Err(ReaderError::InvalidRecord);
        }
        credits += value;
    }
    if credits <= 0.0 {
        return Ok(CostStatus::Unavailable);
    }
    let usd = credits * CREDIT_TO_USD;
    let nano = (usd * 1_000_000_000.0).round();
    if !nano.is_finite() || nano < 0.0 || nano > u64::MAX as f64 {
        return Err(ReaderError::InvalidRecord);
    }
    Ok(CostStatus::Reported(nano as u64))
}

/// Read a counter that is only meaningful when positive.
///
/// Kiro persists `0` for the Auto agent, which means "not measured" rather than
/// "no tokens", so zero is treated as absent. A negative value is not clamped.
fn positive_counter(turn: &Map<String, Value>, key: &str) -> Result<u64, ReaderError> {
    let Some(value) = turn.get(key) else {
        return Ok(0);
    };
    if value.is_null() {
        return Ok(0);
    }
    let Some(number) = value.as_i64() else {
        return Err(ReaderError::InvalidRecord);
    };
    if number < 0 {
        return Err(ReaderError::InvalidRecord);
    }
    Ok(number as u64)
}

/// The final component of the session's `cwd`, used as a workspace label.
///
/// The full path is deliberately not retained: the store rejects absolute-path
/// text in every runtime identity column, so persisting `cwd` verbatim would
/// fail the commit and leak the user's home directory.
fn workspace_label_from_cwd(cwd: &str) -> Option<String> {
    let trimmed = cwd.trim_end_matches(['/', '\\']);
    let name = trimmed.rsplit(['/', '\\']).find(|part| !part.is_empty())?;
    if name.len() > MAX_IDENTITY_BYTES || name.chars().any(char::is_control) {
        return None;
    }
    Some(name.to_owned())
}

fn valid_identity(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_IDENTITY_BYTES && !value.chars().any(char::is_control)
}
