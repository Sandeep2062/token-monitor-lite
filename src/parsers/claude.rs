//! Claude CLI assistant-response assembly adapted from the exact pinned
//! Tokscale claudecode.rs/AnthropicUsage helpers. MIT © 2025 Junho Yeo;
//! see docs/migration/parser-provenance.md for pin, hashes and full notices.
//! Advisor iterations, explicit tool-result accounting, headless/result
//! streams, sidechain parent attribution, cc-mirror client ownership,
//! anthropic/ model prefix normalization, and file-mtime timestamp fallback
//! are supported. Arbitrary model-alias tables, Claude Desktop IndexedDB, and
//! cross-file path-derived provider guesses remain unsupported.

use super::{CostStatus, ReaderError, ResponseUsage, jsonl::MAX_RECORD_BYTES};
use crate::accounting::{SessionKey, TokenCounts};
use chrono::DateTime;
use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

pub const MAX_RESPONSES: usize = 16_384;
const MAX_IDENTITY_BYTES: usize = 4096;
const MAX_RETAINED_IDENTITY_BYTES: usize = 8 * 1024 * 1024;

pub struct ClaudeReader {
    client: String,
    session: String,
    responses: Vec<AssembledResponse>,
    dedup: HashMap<(String, Option<String>), usize>,
    stable: HashMap<String, usize>,
    pending_start: Option<i64>,
    fallback_timestamp_ms: Option<i64>,
    current_agent: Option<String>,
    retained_identity_bytes: usize,
    last_model: Option<String>,
    last_provider: Option<String>,
    suppress_unattributed_tool_results: bool,
    headless: Option<HeadlessState>,
}

#[derive(Default)]
struct HeadlessState {
    model: Option<String>,
    provider: Option<String>,
    tokens: TokenCounts,
    cache_write_1h: u64,
    timestamp_ms: Option<i64>,
    skipping_synthetic: bool,
}

struct AssembledResponse {
    usage: ResponseUsage,
    input_is_split: bool,
    // Do not lose a partial one-hour split before its total arrives.
    unclamped_cache_write_1h: u64,
}

#[derive(Deserialize)]
struct Entry {
    timestamp: Option<String>,
    #[serde(rename = "requestId")]
    request_id: Option<String>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    #[serde(rename = "isSidechain", default)]
    is_sidechain: bool,
    #[serde(rename = "agentId")]
    agent_id: Option<String>,
    #[serde(rename = "providerId", alias = "provider_id", alias = "provider")]
    provider: Option<String>,
    message: Option<Message>,
}

#[derive(Deserialize)]
struct Message {
    id: Option<String>,
    model: Option<String>,
    #[serde(rename = "providerId", alias = "provider_id", alias = "provider")]
    provider: Option<String>,
    usage: Option<Usage>,
}

#[derive(Deserialize, Clone)]
struct Usage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_creation: Option<CacheCreation>,
    iterations: Option<Value>,
}

#[derive(Deserialize, Clone)]
struct CacheCreation {
    ephemeral_1h_input_tokens: Option<u64>,
}

impl Usage {
    fn reports_cache_split(&self) -> bool {
        self.cache_read_input_tokens.is_some() || self.cache_creation_input_tokens.is_some()
    }

    fn cache_write_1h(&self) -> u64 {
        self.cache_creation
            .as_ref()
            .and_then(|creation| creation.ephemeral_1h_input_tokens)
            .unwrap_or(0)
    }

    fn tokens(&self) -> TokenCounts {
        TokenCounts {
            input: self.input_tokens.unwrap_or(0),
            output: self.output_tokens.unwrap_or(0),
            cache_read: self.cache_read_input_tokens.unwrap_or(0),
            cache_write: self.cache_creation_input_tokens.unwrap_or(0),
            reasoning: 0,
        }
    }
}

impl ClaudeReader {
    pub fn new(session: impl Into<String>) -> Self {
        Self::with_client("claude", session)
    }

    pub fn with_client(client: impl Into<String>, session: impl Into<String>) -> Self {
        Self {
            client: client.into(),
            session: session.into(),
            responses: Vec::new(),
            dedup: HashMap::new(),
            stable: HashMap::new(),
            pending_start: None,
            fallback_timestamp_ms: None,
            current_agent: None,
            retained_identity_bytes: 0,
            last_model: None,
            last_provider: None,
            suppress_unattributed_tool_results: false,
            headless: None,
        }
    }

    pub fn with_fallback_timestamp(mut self, timestamp_ms: Option<i64>) -> Self {
        self.fallback_timestamp_ms = timestamp_ms;
        self
    }

    /// Admit one complete framed record. Errors do not change captured state.
    /// A collector must withhold the entire failed source generation, not turn
    /// the remaining successful rows into a complete source observation.
    pub fn ingest(&mut self, bytes: &[u8]) -> Result<(), ReaderError> {
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(ReaderError::Capacity("record bytes"));
        }
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(());
        }
        let value: Value = serde_json::from_slice(bytes).map_err(|_| ReaderError::InvalidRecord)?;
        let object = value.as_object().ok_or(ReaderError::InvalidRecord)?;
        let kind = object
            .get("type")
            .and_then(Value::as_str)
            .ok_or(ReaderError::InvalidRecord)?;
        // Sidechain usage is attributed to the declared parent session. The
        // agent id is retained on the row as provenance.
        if object.get("toolUseResult").is_some_and(|v| !v.is_null())
            || kind == "tool_result"
            || !tool_result_values(&value).is_empty()
        {
            let entry: Entry =
                serde_json::from_value(value.clone()).map_err(|_| ReaderError::InvalidRecord)?;
            if entry.is_sidechain || entry.agent_id.is_some() {
                self.current_agent = entry.agent_id.clone().or(self.current_agent.clone());
            }
            if entry
                .session_id
                .as_deref()
                .is_some_and(|id| id != self.session)
            {
                return Err(ReaderError::Unsupported("session identity change"));
            }
            let timestamp = entry
                .timestamp
                .as_deref()
                .map(parse_timestamp)
                .transpose()?;
            return self.ingest_tool_result(&value, &entry, timestamp);
        }
        if matches!(
            kind,
            "message_start" | "message_delta" | "message_stop" | "result" | "stream_event"
        ) {
            return self.ingest_headless(&value, kind);
        }
        if kind != "assistant" && kind != "user" {
            if object.contains_key("usage")
                || matches!(
                    kind,
                    "result" | "stream_event" | "message_start" | "message_delta" | "message_stop"
                )
            {
                return Err(ReaderError::Unsupported("headless usage record"));
            }
            return Ok(());
        }
        if kind == "assistant" {
            let message = object.get("message");
            if message
                .and_then(|m| m.get("model"))
                .and_then(Value::as_str)
                .is_some_and(|m| m.trim().eq_ignore_ascii_case("<synthetic>"))
            {
                self.pending_start = None;
                self.last_model = None;
                self.last_provider = None;
                self.suppress_unattributed_tool_results = true;
                return Ok(());
            }
            if let Some(usage) = message
                .and_then(|m| m.get("usage"))
                .filter(|v| !v.is_null())
            {
                for field in [
                    "reasoning_tokens",
                    "reasoning_output_tokens",
                    "cost",
                    "cost_usd",
                    "total_cost_usd",
                ] {
                    if usage.get(field).is_some_and(|v| !v.is_null()) {
                        return Err(ReaderError::Unsupported("additional billable usage"));
                    }
                }
            }
        }
        let is_user = kind == "user";
        let entry: Entry = serde_json::from_value(value).map_err(|_| ReaderError::InvalidRecord)?;
        if entry.is_sidechain || entry.agent_id.is_some() {
            self.current_agent = entry.agent_id.clone().or(self.current_agent.clone());
        }
        if entry
            .session_id
            .as_deref()
            .is_some_and(|id| id != self.session)
        {
            return Err(ReaderError::Unsupported("session identity change"));
        }
        let timestamp = entry
            .timestamp
            .as_deref()
            .map(parse_timestamp)
            .transpose()?;
        if is_user {
            if timestamp.is_some() {
                self.pending_start = timestamp;
            }
            return Ok(());
        }
        let Some(message) = entry.message else {
            return Ok(());
        };
        let Some(usage) = message.usage else {
            return Ok(());
        };
        let provider = message.provider.or(entry.provider);
        for identity in [
            Some(self.session.as_str()),
            message.id.as_deref(),
            entry.request_id.as_deref(),
            message.model.as_deref(),
            provider.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            if identity.len() > MAX_IDENTITY_BYTES {
                return Err(ReaderError::Capacity("identity bytes"));
            }
        }
        let dedup_key = message.id.map(|id| (id, entry.request_id));
        if let Some(index) = dedup_key
            .as_ref()
            .and_then(|key| self.dedup.get(key))
            .copied()
        {
            let existing = &self.responses[index];
            if message
                .model
                .as_deref()
                .is_some_and(|model| model != existing.usage.model)
            {
                return Err(ReaderError::Unsupported("response model conflict"));
            }
            if provider.as_deref().is_some_and(|p| {
                existing
                    .usage
                    .billing_provider_hint
                    .as_deref()
                    .is_some_and(|old| old != p)
            }) {
                return Err(ReaderError::Unsupported("billing identity conflict"));
            }
            let added_bytes = if existing.usage.billing_provider_hint.is_none() {
                provider.as_ref().map_or(0, String::len)
            } else {
                0
            };
            self.check_capacity(added_bytes)?;
            let existing = &mut self.responses[index];
            merge_duplicate(existing, &usage);
            if let Some(agent) = self.current_agent.as_ref() {
                if existing
                    .usage
                    .agent
                    .as_ref()
                    .is_some_and(|old| old != agent)
                {
                    return Err(ReaderError::Unsupported("agent identity conflict"));
                }
                existing.usage.agent = Some(agent.clone());
            }
            if existing.usage.billing_provider_hint.is_none() {
                existing.usage.billing_provider_hint = provider;
            }
            self.retained_identity_bytes += added_bytes;
            self.last_model = Some(self.responses[index].usage.model.clone());
            self.last_provider = self.responses[index].usage.billing_provider_hint.clone();
            self.suppress_unattributed_tool_results = false;
            self.add_advisor_rows(index, &usage)?;
            return Ok(());
        }
        let model = message
            .model
            .filter(|m| !m.trim().is_empty())
            .ok_or(ReaderError::Unsupported("missing model"))?;
        let model = normalize_model(&model);
        let timestamp_ms = self
            .pending_start
            .or(timestamp)
            .or(self.fallback_timestamp_ms)
            .ok_or(ReaderError::InvalidTimestamp)?;
        let stable_dedup_key = dedup_key.as_ref().map(|(message, request)| {
            request.as_ref().map_or_else(
                || format!("message:{message}"),
                |request| format!("{message}:{request}"),
            )
        });
        if self.responses.len() == MAX_RESPONSES {
            return Err(ReaderError::Capacity("response count"));
        }
        let added_bytes = self.session.len()
            + model.len()
            + provider.as_ref().map_or(0, String::len)
            + dedup_key.as_ref().map_or(0, |(id, request)| {
                id.len() + request.as_ref().map_or(0, String::len)
            })
            + stable_dedup_key.as_ref().map_or(0, String::len);
        self.check_capacity(added_bytes)?;
        let response = AssembledResponse {
            usage: ResponseUsage {
                key: SessionKey::new(self.client.as_str(), &self.session),
                model,
                tokens: usage.tokens(),
                cache_write_1h: 0,
                timestamp_ms,
                dedup_key: stable_dedup_key,
                billing_provider_hint: provider,
                service_tier: None,
                agent: self.current_agent.clone(),
                cost: CostStatus::Unavailable,
            },
            input_is_split: usage.reports_cache_split(),
            unclamped_cache_write_1h: usage.cache_write_1h(),
        };
        if let Some(key) = dedup_key {
            self.dedup.insert(key, self.responses.len());
        }
        self.responses.push(response);
        let index = self.responses.len() - 1;
        if let Some(key) = self.responses[index].usage.dedup_key.clone() {
            self.stable.insert(key, index);
        }
        self.retained_identity_bytes += added_bytes;
        self.pending_start = None;
        self.last_model = Some(self.responses[index].usage.model.clone());
        self.last_provider = self.responses[index].usage.billing_provider_hint.clone();
        self.suppress_unattributed_tool_results = false;
        self.add_advisor_rows(index, &usage)?;
        Ok(())
    }

    fn ingest_headless(&mut self, value: &Value, kind: &str) -> Result<(), ReaderError> {
        let event = if kind == "stream_event" {
            value.get("event").unwrap_or(value)
        } else {
            value
        };
        let event_type = event.get("type").and_then(Value::as_str).unwrap_or(kind);
        match event_type {
            "message_start" => {
                if self
                    .headless
                    .as_ref()
                    .is_some_and(|state| state.skipping_synthetic)
                {
                    self.headless = None;
                }
                self.finalize_headless()?;
                let model = extract_model(event);
                if model
                    .as_deref()
                    .is_some_and(|model| model.trim().eq_ignore_ascii_case("<synthetic>"))
                {
                    self.headless = Some(HeadlessState {
                        skipping_synthetic: true,
                        ..HeadlessState::default()
                    });
                    return Ok(());
                }
                let mut state = HeadlessState {
                    model,
                    provider: extract_provider(event),
                    timestamp_ms: extract_timestamp(event),
                    ..HeadlessState::default()
                };
                if let Some(usage) = event
                    .get("message")
                    .and_then(|message| message.get("usage"))
                    .or_else(|| event.get("usage"))
                {
                    update_headless_usage(&mut state, usage);
                }
                self.headless = Some(state);
            }
            "message_delta" => {
                let Some(state) = self.headless.as_mut() else {
                    return Ok(());
                };
                if state.skipping_synthetic {
                    return Ok(());
                }
                if let Some(usage) = event
                    .get("usage")
                    .or_else(|| event.get("delta").and_then(|delta| delta.get("usage")))
                {
                    update_headless_usage(state, usage);
                }
            }
            "message_stop" => {
                if self
                    .headless
                    .as_ref()
                    .is_some_and(|state| state.skipping_synthetic)
                {
                    self.headless = None;
                } else {
                    self.finalize_headless()?;
                }
            }
            "result" => self.ingest_result(event)?,
            _ => {}
        }
        Ok(())
    }

    fn ingest_result(&mut self, value: &Value) -> Result<(), ReaderError> {
        let Some(usage_value) = value.get("usage").or_else(|| {
            value
                .get("message")
                .and_then(|message| message.get("usage"))
        }) else {
            return Ok(());
        };
        if usage_value.is_null() {
            return Ok(());
        }
        let Some(model) = extract_model(value).or_else(|| self.last_model.clone()) else {
            return Ok(());
        };
        if model.trim().eq_ignore_ascii_case("<synthetic>") {
            return Ok(());
        }
        let usage: Usage =
            serde_json::from_value(usage_value.clone()).map_err(|_| ReaderError::InvalidRecord)?;
        let timestamp_ms = extract_timestamp(value)
            .or(self.pending_start)
            .or(self.fallback_timestamp_ms)
            .ok_or(ReaderError::InvalidTimestamp)?;
        let provider = extract_provider(value).or_else(|| self.last_provider.clone());
        self.push_unkeyed(
            model,
            provider,
            usage.tokens(),
            usage.cache_write_1h(),
            usage.reports_cache_split(),
            timestamp_ms,
        )
    }

    fn finalize_headless(&mut self) -> Result<(), ReaderError> {
        let Some(state) = self.headless.take() else {
            return Ok(());
        };
        if state.skipping_synthetic {
            return Ok(());
        }
        let Some(model) = state.model else {
            return Ok(());
        };
        let timestamp_ms = state
            .timestamp_ms
            .or(self.pending_start)
            .or(self.fallback_timestamp_ms)
            .ok_or(ReaderError::InvalidTimestamp)?;
        self.push_unkeyed(
            model,
            state.provider,
            state.tokens,
            state.cache_write_1h,
            false,
            timestamp_ms,
        )
    }

    fn push_unkeyed(
        &mut self,
        model: String,
        provider: Option<String>,
        tokens: TokenCounts,
        cache_write_1h: u64,
        input_is_split: bool,
        timestamp_ms: i64,
    ) -> Result<(), ReaderError> {
        let model = normalize_model(&model);
        if self.responses.len() == MAX_RESPONSES {
            return Err(ReaderError::Capacity("response count"));
        }
        if model.len() > MAX_IDENTITY_BYTES
            || provider
                .as_ref()
                .is_some_and(|value| value.len() > MAX_IDENTITY_BYTES)
            || self.session.len() > MAX_IDENTITY_BYTES
        {
            return Err(ReaderError::Capacity("identity bytes"));
        }
        let added_bytes =
            self.session.len() + model.len() + provider.as_ref().map_or(0, String::len);
        self.check_capacity(added_bytes)?;
        let response = AssembledResponse {
            usage: ResponseUsage {
                key: SessionKey::new(self.client.as_str(), &self.session),
                model,
                tokens,
                cache_write_1h: 0,
                timestamp_ms,
                dedup_key: None,
                billing_provider_hint: provider,
                service_tier: None,
                agent: self.current_agent.clone(),
                cost: CostStatus::Unavailable,
            },
            input_is_split,
            unclamped_cache_write_1h: cache_write_1h,
        };
        self.responses.push(response);
        let index = self.responses.len() - 1;
        self.last_model = Some(self.responses[index].usage.model.clone());
        self.last_provider = self.responses[index].usage.billing_provider_hint.clone();
        self.suppress_unattributed_tool_results = false;
        self.retained_identity_bytes += added_bytes;
        Ok(())
    }

    fn ingest_tool_result(
        &mut self,
        value: &Value,
        entry: &Entry,
        timestamp: Option<i64>,
    ) -> Result<(), ReaderError> {
        let mut total = 0_u64;
        let mut first_id = None;
        let mut seen = HashSet::new();
        for result in tool_result_values(value) {
            let id = tool_result_id(result);
            if id.as_ref().is_some_and(|id| !seen.insert(id.clone())) {
                continue;
            }
            if first_id.is_none() {
                first_id = id;
            }
            if let Some(tokens) = explicit_tool_result_tokens(result) {
                total = total.checked_add(tokens).ok_or(ReaderError::Overflow)?;
            }
        }
        if total == 0 {
            return Ok(());
        }
        let explicit_model = value.get("model").and_then(Value::as_str).or_else(|| {
            entry
                .message
                .as_ref()
                .and_then(|message| message.model.as_deref())
        });
        let model = match explicit_model {
            Some(model) if model.trim().eq_ignore_ascii_case("<synthetic>") => return Ok(()),
            Some(model) if !model.trim().is_empty() => model.trim().to_owned(),
            Some(_) => return Err(ReaderError::Unsupported("empty tool-result model")),
            None if self.suppress_unattributed_tool_results => {
                return Err(ReaderError::Unsupported("model-less tool result"));
            }
            None => self
                .last_model
                .clone()
                .unwrap_or_else(|| "unknown".to_owned()),
        };
        let provider = value
            .get("providerId")
            .and_then(Value::as_str)
            .or_else(|| value.get("provider_id").and_then(Value::as_str))
            .or_else(|| value.get("provider").and_then(Value::as_str))
            .or_else(|| {
                entry
                    .message
                    .as_ref()
                    .and_then(|message| message.provider.as_deref())
            })
            .or(entry.provider.as_deref())
            .or(self.last_provider.as_deref())
            .map(str::to_owned);
        if model.len() > MAX_IDENTITY_BYTES
            || provider
                .as_ref()
                .is_some_and(|value| value.len() > MAX_IDENTITY_BYTES)
            || first_id
                .as_ref()
                .is_some_and(|value| value.len() > MAX_IDENTITY_BYTES)
        {
            return Err(ReaderError::Capacity("identity bytes"));
        }
        let timestamp_ms = timestamp
            .or(self.pending_start)
            .or(self.fallback_timestamp_ms)
            .ok_or(ReaderError::InvalidTimestamp)?;
        let dedup_key = first_id.map(|id| format!("tool_result:{}:{}", self.session, id));
        if let Some(index) = dedup_key
            .as_ref()
            .and_then(|key| self.stable.get(key))
            .copied()
        {
            let existing = &mut self.responses[index];
            existing.usage.tokens.input = existing.usage.tokens.input.max(total);
            existing.usage.timestamp_ms = existing.usage.timestamp_ms.max(timestamp_ms);
            return Ok(());
        }
        if self.responses.len() == MAX_RESPONSES {
            return Err(ReaderError::Capacity("response count"));
        }
        let added_bytes = self.session.len()
            + model.len()
            + provider.as_ref().map_or(0, String::len)
            + dedup_key.as_ref().map_or(0, String::len);
        self.check_capacity(added_bytes)?;
        let response = AssembledResponse {
            usage: ResponseUsage {
                key: SessionKey::new(self.client.as_str(), &self.session),
                model,
                tokens: TokenCounts {
                    input: total,
                    ..TokenCounts::default()
                },
                cache_write_1h: 0,
                timestamp_ms,
                dedup_key: dedup_key.clone(),
                billing_provider_hint: provider,
                service_tier: None,
                agent: self.current_agent.clone(),
                cost: CostStatus::Unavailable,
            },
            input_is_split: false,
            unclamped_cache_write_1h: 0,
        };
        self.responses.push(response);
        let index = self.responses.len() - 1;
        if let Some(key) = dedup_key {
            self.stable.insert(key, index);
        }
        self.retained_identity_bytes += added_bytes;
        Ok(())
    }

    fn add_advisor_rows(&mut self, parent_index: usize, usage: &Usage) -> Result<(), ReaderError> {
        let Some(iterations) = usage.iterations.as_ref().and_then(Value::as_array) else {
            return Ok(());
        };
        let parent = &self.responses[parent_index].usage;
        let parent_key = parent.dedup_key.clone();
        let parent_provider = parent.billing_provider_hint.clone();
        let parent_session = parent.key.session.clone();
        let parent_timestamp = parent.timestamp_ms;
        let mut merges = Vec::new();
        let mut additions = Vec::new();
        let mut added_bytes = 0_usize;
        for (index, iteration) in iterations.iter().enumerate() {
            if iteration.get("type").and_then(Value::as_str) != Some("advisor_message") {
                continue;
            }
            let Some(model) = iteration.get("model").and_then(Value::as_str) else {
                continue;
            };
            let model = model.trim();
            if model.is_empty() || model.eq_ignore_ascii_case("<synthetic>") {
                continue;
            }
            let Ok(advisor_usage) = serde_json::from_value::<Usage>(iteration.clone()) else {
                continue;
            };
            let dedup_key = parent_key
                .as_ref()
                .map(|key| format!("{key}:advisor:{index}"));
            if let Some(existing_index) = dedup_key
                .as_ref()
                .and_then(|key| self.stable.get(key))
                .copied()
            {
                if self.responses[existing_index].usage.model != model {
                    return Err(ReaderError::Unsupported("advisor model conflict"));
                }
                merges.push((existing_index, advisor_usage));
                continue;
            }
            added_bytes = added_bytes
                .checked_add(parent_session.len())
                .and_then(|value| value.checked_add(model.len()))
                .and_then(|value| {
                    value.checked_add(parent_provider.as_ref().map_or(0, String::len))
                })
                .and_then(|value| value.checked_add(dedup_key.as_ref().map_or(0, String::len)))
                .ok_or(ReaderError::Capacity("retained identity bytes"))?;
            additions.push((
                dedup_key,
                AssembledResponse {
                    usage: ResponseUsage {
                        key: SessionKey::new(self.client.as_str(), &parent_session),
                        model: normalize_model(model),
                        tokens: advisor_usage.tokens(),
                        cache_write_1h: 0,
                        timestamp_ms: parent_timestamp,
                        dedup_key: None,
                        billing_provider_hint: parent_provider.clone(),
                        service_tier: None,
                        agent: self.current_agent.clone(),
                        cost: CostStatus::Unavailable,
                    },
                    input_is_split: advisor_usage.reports_cache_split(),
                    unclamped_cache_write_1h: advisor_usage.cache_write_1h(),
                },
            ));
        }
        self.check_capacity(added_bytes)?;
        if self
            .responses
            .len()
            .checked_add(additions.len())
            .is_none_or(|count| count > MAX_RESPONSES)
        {
            return Err(ReaderError::Capacity("response count"));
        }
        for (existing_index, advisor_usage) in merges {
            merge_duplicate(&mut self.responses[existing_index], &advisor_usage);
        }
        for (dedup_key, mut response) in additions {
            response.usage.dedup_key = dedup_key;
            self.responses.push(response);
            let advisor_index = self.responses.len() - 1;
            if let Some(key) = self.responses[advisor_index].usage.dedup_key.clone() {
                self.stable.insert(key, advisor_index);
            }
        }
        self.retained_identity_bytes += added_bytes;
        Ok(())
    }

    fn check_capacity(&self, added_bytes: usize) -> Result<(), ReaderError> {
        if self.retained_identity_bytes + added_bytes > MAX_RETAINED_IDENTITY_BYTES {
            return Err(ReaderError::Capacity("retained identity bytes"));
        }
        Ok(())
    }

    /// Bounded captured rows in source order; cost remains unavailable. This
    /// materialization is for backend reconciliation, not an unpaged UI model.
    pub fn snapshot(&self) -> Vec<ResponseUsage> {
        self.responses
            .iter()
            .map(|response| {
                let mut row = response.usage.clone();
                row.cache_write_1h = response
                    .unclamped_cache_write_1h
                    .min(row.tokens.cache_write);
                row
            })
            .collect()
    }
}

fn extract_model(value: &Value) -> Option<String> {
    value
        .get("model")
        .and_then(Value::as_str)
        .or_else(|| {
            value
                .get("message")
                .and_then(|message| message.get("model"))
                .and_then(Value::as_str)
        })
        .map(str::to_owned)
}

fn extract_provider(value: &Value) -> Option<String> {
    ["providerId", "provider_id", "provider"]
        .iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .or_else(|| {
            value.get("message").and_then(|message| {
                ["providerId", "provider_id", "provider"]
                    .iter()
                    .find_map(|key| message.get(*key).and_then(Value::as_str))
            })
        })
        .map(str::to_owned)
}

fn extract_timestamp(value: &Value) -> Option<i64> {
    value
        .get("timestamp")
        .or_else(|| value.get("created_at"))
        .or_else(|| {
            value
                .get("message")
                .and_then(|message| message.get("created_at"))
        })
        .and_then(|value| {
            value
                .as_str()
                .and_then(|value| parse_timestamp(value).ok())
                .or_else(|| value.as_i64())
        })
}

fn update_headless_usage(state: &mut HeadlessState, usage: &Value) {
    if let Some(input) = usage.get("input_tokens").and_then(Value::as_u64) {
        state.tokens.input = state.tokens.input.max(input);
    }
    if let Some(output) = usage.get("output_tokens").and_then(Value::as_u64) {
        state.tokens.output = state.tokens.output.max(output);
    }
    if let Some(cache_read) = usage.get("cache_read_input_tokens").and_then(Value::as_u64) {
        state.tokens.cache_read = state.tokens.cache_read.max(cache_read);
    }
    if let Some(cache_write) = usage
        .get("cache_creation_input_tokens")
        .and_then(Value::as_u64)
    {
        state.tokens.cache_write = state.tokens.cache_write.max(cache_write);
    }
    if let Some(cache_write_1h) = usage
        .get("cache_creation")
        .and_then(|creation| creation.get("ephemeral_1h_input_tokens"))
        .and_then(Value::as_u64)
    {
        state.cache_write_1h = state.cache_write_1h.max(cache_write_1h);
    }
}

fn tool_result_values(value: &Value) -> Vec<&Value> {
    let mut results = Vec::new();
    if value.get("type").and_then(Value::as_str) == Some("tool_result") {
        results.push(value);
    }
    for candidate in [
        value.get("tool_result"),
        value.get("toolUseResult"),
        value
            .get("message")
            .and_then(|message| message.get("tool_result")),
    ]
    .into_iter()
    .flatten()
    {
        if candidate.is_object() {
            results.push(candidate);
        }
    }
    if let Some(content) = value
        .get("message")
        .and_then(|message| message.get("content"))
        .or_else(|| value.get("content"))
        && let Some(blocks) = content.as_array()
    {
        for block in blocks {
            if block.get("type").and_then(Value::as_str) == Some("tool_result") {
                results.push(block);
            }
        }
    }
    results
}

fn tool_result_id(value: &Value) -> Option<String> {
    ["tool_use_id", "id", "tool_result_id"]
        .iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .map(str::to_owned)
}

fn explicit_tool_result_tokens(value: &Value) -> Option<u64> {
    for candidate in [
        value.get("input_tokens"),
        value.get("token_count"),
        value.get("tokens"),
        value
            .get("usage")
            .and_then(|usage| usage.get("input_tokens")),
        value
            .get("tool_output")
            .and_then(|output| output.get("input_tokens")),
        value
            .get("tool_output")
            .and_then(|output| output.get("token_count")),
        value
            .get("tool_output")
            .and_then(|output| output.get("tokens")),
        value
            .get("tool_output")
            .and_then(|output| output.get("usage"))
            .and_then(|usage| usage.get("input_tokens")),
    ] {
        if let Some(tokens) = candidate.and_then(Value::as_u64) {
            return Some(tokens);
        }
    }
    None
}

fn normalize_model(model: &str) -> String {
    let model = model.trim();
    model.strip_prefix("anthropic/").unwrap_or(model).to_owned()
}

fn parse_timestamp(value: &str) -> Result<i64, ReaderError> {
    DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.timestamp_millis())
        .map_err(|_| ReaderError::InvalidTimestamp)
}

fn merge_duplicate(existing: &mut AssembledResponse, usage: &Usage) {
    let tokens = &mut existing.usage.tokens;
    match (usage.reports_cache_split(), existing.input_is_split) {
        (true, false) => {
            if let Some(input) = usage.input_tokens {
                tokens.input = input;
            }
        }
        (false, true) => {}
        _ => tokens.input = tokens.input.max(usage.input_tokens.unwrap_or(0)),
    }
    tokens.output = tokens.output.max(usage.output_tokens.unwrap_or(0));
    tokens.cache_read = tokens
        .cache_read
        .max(usage.cache_read_input_tokens.unwrap_or(0));
    tokens.cache_write = tokens
        .cache_write
        .max(usage.cache_creation_input_tokens.unwrap_or(0));
    existing.unclamped_cache_write_1h = existing
        .unclamped_cache_write_1h
        .max(usage.cache_write_1h());
    existing.input_is_split |= usage.reports_cache_split();
}
