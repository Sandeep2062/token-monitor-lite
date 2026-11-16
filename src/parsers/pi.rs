//! Pi-format JSONL normalization shared by Pi, Oh My Pi and OmO Native
//! (Senpi), adapted from the pinned Tokscale `pi.rs`, `omp.rs` and `senpi.rs`
//! readers. MIT © 2025 Junho Yeo; see docs/migration/parser-provenance.md.
//!
//! Supported records are the `session` header, assistant `message` usage,
//! `toolResult` usage, root-level `usage` entries, `compaction` /
//! `branch_summary` usage, and `model_change` attribution. `reasoning` is an
//! informational subset of `output` and is never added to the canonical total.
//! Titles, agent/subagent naming, workspace attribution, reported cost, RLM
//! accounting, turn-duration performance and cross-file fork reconciliation
//! beyond the stable dedup key are not implemented.

use super::{CostStatus, ReaderError, ResponseUsage};
use crate::accounting::{SessionKey, TokenCounts};
use chrono::DateTime;
use serde_json::{Map, Value};
use std::collections::BTreeMap;

pub const MAX_RESPONSES: usize = 16_384;
const MAX_IDENTITY_BYTES: usize = 4_096;
const MAX_RETAINED_IDENTITY_BYTES: usize = 8 * 1024 * 1024;
const OPERATION_TYPES: &[&str] = &["compaction", "branch_summary", "usage"];
const TOOLS_SUMMARIES_MODEL: &str = "tools/summaries";

pub struct PiReader {
    client: &'static str,
    fallback_session: String,
    fallback_timestamp_ms: Option<i64>,
    header_seen: bool,
    header_session: Option<String>,
    current_model: Option<String>,
    rows: Vec<ResponseUsage>,
    identities: BTreeMap<String, usize>,
    ordinal: usize,
    retained_identity_bytes: usize,
}

impl PiReader {
    pub fn new(client: &'static str, session: impl Into<String>) -> Self {
        Self {
            client,
            fallback_session: session.into(),
            fallback_timestamp_ms: None,
            header_seen: false,
            header_session: None,
            current_model: None,
            rows: Vec::new(),
            identities: BTreeMap::new(),
            ordinal: 0,
            retained_identity_bytes: 0,
        }
    }

    pub fn with_fallback_timestamp(mut self, timestamp_ms: Option<i64>) -> Self {
        self.fallback_timestamp_ms = timestamp_ms.filter(|value| *value >= 0);
        self
    }

    pub fn ingest(&mut self, record: &[u8]) -> Result<(), ReaderError> {
        let value: Value =
            serde_json::from_slice(record).map_err(|_| ReaderError::InvalidRecord)?;
        let object = value.as_object().ok_or(ReaderError::InvalidRecord)?;
        let entry_type = object
            .get("type")
            .and_then(Value::as_str)
            .ok_or(ReaderError::InvalidRecord)?;

        if !self.header_seen {
            if entry_type == "title" {
                return Ok(());
            }
            if entry_type != "session" {
                return Err(ReaderError::Unsupported("Pi session header"));
            }
            let id = object
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| valid_identity(id))
                .ok_or(ReaderError::InvalidRecord)?;
            self.header_seen = true;
            self.header_session = Some(id.to_owned());
            return Ok(());
        }

        match entry_type {
            "model_change" => {
                let model = object
                    .get("modelId")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|model| !model.is_empty());
                let provider = object
                    .get("provider")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|provider| !provider.is_empty());
                if let (Some(model), Some(_provider)) = (model, provider) {
                    self.current_model = Some(model.to_owned());
                }
                return Ok(());
            }
            "session_info" | "title_change" => return Ok(()),
            _ => {}
        }

        let message = object.get("message").and_then(Value::as_object);
        let role = message.and_then(|message| message.get("role").and_then(Value::as_str));
        let is_operation = OPERATION_TYPES.contains(&entry_type);
        let (usage_object, recorded_model, recorded_provider, updates_current_model) =
            if entry_type == "message" && role == Some("assistant") {
                let message = message.ok_or(ReaderError::Declined("Pi assistant message"))?;
                let usage = message
                    .get("usage")
                    .and_then(Value::as_object)
                    .ok_or(ReaderError::Declined("Pi assistant usage"))?;
                let model = message
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|model| !model.is_empty())
                    .ok_or(ReaderError::Declined("Pi assistant model"))?;
                let provider = message
                    .get("provider")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|provider| !provider.is_empty());
                (
                    usage,
                    Some(model.to_owned()),
                    provider.map(str::to_owned),
                    true,
                )
            } else if entry_type == "message" && role == Some("toolResult") {
                let message = message.ok_or(ReaderError::Declined("Pi tool result"))?;
                let usage = message
                    .get("usage")
                    .and_then(Value::as_object)
                    .ok_or(ReaderError::Declined("Pi tool-result usage"))?;
                let model = message
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|model| !model.is_empty())
                    .map(str::to_owned);
                let provider = message
                    .get("provider")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|provider| !provider.is_empty())
                    .map(str::to_owned);
                (usage, model, provider, false)
            } else if is_operation {
                let usage = object
                    .get("usage")
                    .and_then(Value::as_object)
                    .ok_or(ReaderError::Declined("Pi operation usage"))?;
                let model = object
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|model| !model.is_empty())
                    .map(str::to_owned);
                let provider = object
                    .get("provider")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|provider| !provider.is_empty())
                    .map(str::to_owned);
                (usage, model, provider, false)
            } else {
                return Ok(());
            };

        let (tokens, cache_write_1h) = normalize_usage(usage_object)?;
        if tokens_total(tokens)? == 0 && cache_write_1h == 0 {
            return Ok(());
        }

        let (model, provider) = if entry_type != "message" && recorded_model.is_none() {
            if entry_type == "usage" {
                (
                    self.current_model
                        .clone()
                        .unwrap_or_else(|| "unknown".to_owned()),
                    None,
                )
            } else {
                (TOOLS_SUMMARIES_MODEL.to_owned(), None)
            }
        } else if let Some(model) = recorded_model {
            (model, recorded_provider)
        } else {
            (TOOLS_SUMMARIES_MODEL.to_owned(), None)
        };
        if !valid_identity(&model) {
            return Err(ReaderError::InvalidRecord);
        }
        if updates_current_model {
            self.current_model = Some(model.clone());
        }

        let session = self
            .header_session
            .as_deref()
            .unwrap_or(self.fallback_session.as_str());
        let timestamp = response_timestamp(object, message, self.fallback_timestamp_ms)?;
        let response_id = message
            .and_then(|message| message.get("responseId").and_then(Value::as_str))
            .filter(|id| valid_identity(id));
        let entry_id = object
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| valid_identity(id));
        let identity = if let Some(response_id) = response_id {
            format!("{}:response:{response_id}", self.client)
        } else if let Some(entry_id) = entry_id {
            let provider = provider.as_deref().unwrap_or("");
            format!(
                "{}:{entry_type}:{entry_id}:{timestamp}:{provider}:{model}:{}:{}:{}:{}",
                self.client, tokens.input, tokens.output, tokens.cache_read, tokens.cache_write
            )
        } else {
            format!("{}:{session}:line:{}", self.client, self.ordinal)
        };
        if identity.len() > MAX_IDENTITY_BYTES {
            return Err(ReaderError::Declined("Pi record identity"));
        }
        self.ordinal += 1;

        let usage_row = ResponseUsage {
            key: SessionKey::new(self.client, session),
            model,
            tokens,
            cache_write_1h,
            timestamp_ms: timestamp,
            dedup_key: Some(identity.clone()),
            billing_provider_hint: provider,
            service_tier: None,
            agent: None,
            workspace_key: None,
            workspace_label: None,
            title: None,
            cost: CostStatus::Unavailable,
        };
        if let Some(index) = self.identities.get(&identity).copied() {
            if usage_row.tokens != self.rows[index].tokens
                || usage_row.timestamp_ms != self.rows[index].timestamp_ms
                || usage_row.model != self.rows[index].model
            {
                return Err(ReaderError::Declined("Pi duplicate response identity"));
            }
            return Ok(());
        }
        if self.rows.len() == MAX_RESPONSES {
            return Err(ReaderError::Capacity("Pi responses"));
        }
        self.retained_identity_bytes = self
            .retained_identity_bytes
            .checked_add(identity.len())
            .filter(|bytes| *bytes <= MAX_RETAINED_IDENTITY_BYTES)
            .ok_or(ReaderError::Capacity("Pi identities"))?;
        self.identities.insert(identity, self.rows.len());
        self.rows.push(usage_row);
        Ok(())
    }

    pub fn snapshot(&self) -> Vec<ResponseUsage> {
        self.rows.clone()
    }

    pub fn has_supported_usage(&self) -> bool {
        !self.rows.is_empty()
    }
}

fn normalize_usage(usage: &Map<String, Value>) -> Result<(TokenCounts, u64), ReaderError> {
    let input = counter(usage, "input")?;
    let output = counter(usage, "output")?;
    let cache_read = counter(usage, "cacheRead")?;
    let cache_write = counter(usage, "cacheWrite")?;
    let cache_write_1h = counter(usage, "cacheWrite1h")?;
    if cache_write_1h > cache_write {
        return Err(ReaderError::Declined("Pi cache write 1h"));
    }
    // `reasoning` is intentionally read but not mapped: Pi documents it as a
    // subset of `output`, while the canonical reasoning counter is additive.
    // Validate the informational counters so a damaged negative value is still
    // visible instead of silently ignored.
    let _reasoning = counter(usage, "reasoning")?;
    let _reasoning_tokens = counter(usage, "reasoningTokens")?;
    let tokens = TokenCounts {
        input,
        output,
        cache_read,
        cache_write,
        reasoning: 0,
    };
    Ok((tokens, cache_write_1h))
}

fn counter(usage: &Map<String, Value>, key: &str) -> Result<u64, ReaderError> {
    match usage.get(key) {
        None | Some(Value::Null) => Ok(0),
        Some(value) => value.as_u64().ok_or(ReaderError::InvalidRecord),
    }
}

fn response_timestamp(
    entry: &Map<String, Value>,
    message: Option<&Map<String, Value>>,
    fallback: Option<i64>,
) -> Result<i64, ReaderError> {
    if let Some(timestamp) = message
        .and_then(|message| message.get("timestamp"))
        .and_then(Value::as_i64)
        .filter(|timestamp| *timestamp > 0)
    {
        return Ok(timestamp);
    }
    if let Some(timestamp) = entry.get("timestamp").and_then(Value::as_str) {
        return DateTime::parse_from_rfc3339(timestamp)
            .map(|timestamp| timestamp.timestamp_millis())
            .map_err(|_| ReaderError::InvalidTimestamp);
    }
    fallback.ok_or(ReaderError::Declined("Pi record timestamp"))
}

fn tokens_total(tokens: TokenCounts) -> Result<u64, ReaderError> {
    tokens
        .input
        .checked_add(tokens.output)
        .and_then(|value| value.checked_add(tokens.cache_read))
        .and_then(|value| value.checked_add(tokens.cache_write))
        .and_then(|value| value.checked_add(tokens.reasoning))
        .ok_or(ReaderError::Overflow)
}

fn valid_identity(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_IDENTITY_BYTES && !value.chars().any(char::is_control)
}
