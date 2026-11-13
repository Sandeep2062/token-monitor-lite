//! Codex CLI token_count assembly adapted from pinned Tokscale codex.rs.
//! MIT © 2025 Junho Yeo; see docs/migration/parser-provenance.md.
//! Ordinary token_count, pending `token_usage_record` settlement, compaction,
//! fork/subagent turn gating, unknown-model buffering, provider/service-tier
//! attribution, and headless agent attribution are supported. OpenClaw
//! ownership, non-rollout headless documents, and unverifiable fork/record
//! ownership remain explicit unsupported coverage, not approximated totals.

use super::{CostStatus, ReaderError, ResponseUsage, jsonl::MAX_RECORD_BYTES};
use crate::accounting::{SessionKey, TokenCounts};
use chrono::DateTime;
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::{HashSet, VecDeque};

pub const MAX_RESPONSES: usize = 16_384;
const MAX_IDENTITY_BYTES: usize = 4096;
const MAX_RETAINED_IDENTITY_BYTES: usize = 8 * 1024 * 1024;
const SETTLED_USAGE_RECORD_LIMIT: usize = 16;

const CODEX_DEFAULT_AGENT: &str = "Codex";
const CODEX_SUBAGENT_AGENT: &str = "Codex Subagent";
const CODEX_GUARDIAN_AGENT: &str = "Codex Guardian";
const CODEX_HEADLESS_AGENT: &str = "Codex Headless";

pub struct CodexReader {
    session: String,
    fallback_timestamp: Option<i64>,
    metadata_seen: bool,
    model: Option<String>,
    provider: Option<String>,
    thread_tier: Option<String>,
    request_tier: Option<String>,
    previous: Option<Totals>,
    time_cursor: Option<i64>,
    supported_usage: bool,
    responses: Vec<ResponseUsage>,
    retained_identity_bytes: usize,
    usage_event_ordinal: u64,
    current_turn_id: Option<String>,
    turn_id_announced_by_task_started: bool,
    pending_usage_record: Option<PendingUsageRecord>,
    settled_usage_records: VecDeque<Totals>,
    pending_model_rows: Vec<PendingModelRow>,
    session_id_from_meta: Option<String>,
    session_forked_from_id: Option<String>,
    session_is_subagent: bool,
    session_is_guardian: bool,
    session_is_headless: bool,
    session_agent: Option<String>,
    forked_child_session_id: Option<String>,
    forked_child_replay_session_id: Option<String>,
    forked_child_waiting_for_turn_context: bool,
    forked_child_inherited_baseline: Option<Totals>,
    forked_child_inherited_reported_total: Option<u64>,
    forked_child_is_user_fork: bool,
    forked_child_task_started_turn_ids: HashSet<String>,
}

#[derive(Clone)]
struct PendingUsageRecord {
    usage: Totals,
    model: Option<String>,
    provider: Option<String>,
    tier: Option<String>,
    timestamp_ms: Option<i64>,
    turn_id: Option<String>,
    response_id: Option<String>,
    after_compacted: bool,
}

struct PendingModelRow {
    usage: TokenCounts,
    timestamp_ms: Option<i64>,
    fixed_dedup_key: Option<String>,
    total: Option<Totals>,
    parsed_time: Option<i64>,
    event_ordinal: u64,
    dedup_scope: String,
    provider: Option<String>,
    tier: Option<String>,
}

#[derive(Deserialize, Clone)]
struct Usage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cached_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    reasoning_output_tokens: Option<u64>,
    total_tokens: Option<u64>,
    service_tier: Option<String>,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Totals {
    input: u64,
    output: u64,
    cached: u64,
    reasoning: u64,
}

impl Usage {
    fn supported(&self) -> bool {
        self.input_tokens.is_some()
            || self.output_tokens.is_some()
            || self.cached_input_tokens.is_some()
            || self.cache_read_input_tokens.is_some()
            || self.reasoning_output_tokens.is_some()
    }

    fn totals(&self) -> Totals {
        // Validate the optional total even though accounting doesn't sum it.
        let _ = self.total_tokens;
        Totals {
            input: self.input_tokens.unwrap_or(0),
            output: self.output_tokens.unwrap_or(0),
            cached: self
                .cached_input_tokens
                .unwrap_or(0)
                .max(self.cache_read_input_tokens.unwrap_or(0)),
            reasoning: self.reasoning_output_tokens.unwrap_or(0),
        }
    }
}

impl Totals {
    fn delta_from(self, previous: Self) -> Option<Self> {
        Some(Self {
            input: self.input.checked_sub(previous.input)?,
            output: self.output.checked_sub(previous.output)?,
            cached: self.cached.checked_sub(previous.cached)?,
            reasoning: self.reasoning.checked_sub(previous.reasoning)?,
        })
    }

    fn checked_add(self, other: Self) -> Result<Self, ReaderError> {
        Ok(Self {
            input: self
                .input
                .checked_add(other.input)
                .ok_or(ReaderError::Overflow)?,
            output: self
                .output
                .checked_add(other.output)
                .ok_or(ReaderError::Overflow)?,
            cached: self
                .cached
                .checked_add(other.cached)
                .ok_or(ReaderError::Overflow)?,
            reasoning: self
                .reasoning
                .checked_add(other.reasoning)
                .ok_or(ReaderError::Overflow)?,
        })
    }

    fn regression_score(self) -> u128 {
        // Pinned helper includes subsets in this heuristic only. This is not
        // a billable token total; use u128 so its comparisons remain exact.
        u128::from(self.input)
            + u128::from(self.output)
            + u128::from(self.cached)
            + u128::from(self.reasoning)
    }

    fn stale_regression(self, previous: Self, last: Self) -> bool {
        let (current, previous, last) = (
            self.regression_score(),
            previous.regression_score(),
            last.regression_score(),
        );
        current != 0
            && previous != 0
            && last != 0
            && (current * 100 >= previous * 98 || current + last * 2 >= previous)
    }

    fn reports_same_response(self, other: Self) -> bool {
        // The cached and reasoning splits may differ between a usage record
        // and its token_count; only the inclusive input/output are identity.
        self.input == other.input && self.output == other.output
    }

    fn is_within(self, baseline: Self) -> bool {
        self.input <= baseline.input
            && self.output <= baseline.output
            && self.cached <= baseline.cached
            && self.reasoning <= baseline.reasoning
    }

    fn tokens(self) -> TokenCounts {
        let cached = self.cached.min(self.input);
        let reasoning = self.reasoning.min(self.output);
        TokenCounts {
            input: self.input - cached,
            output: self.output - reasoning,
            cache_read: cached,
            cache_write: 0,
            reasoning,
        }
    }
}

impl CodexReader {
    pub fn new(session: impl Into<String>) -> Self {
        Self {
            session: session.into(),
            fallback_timestamp: None,
            metadata_seen: false,
            model: None,
            provider: None,
            thread_tier: None,
            request_tier: None,
            previous: None,
            time_cursor: None,
            supported_usage: false,
            responses: Vec::new(),
            retained_identity_bytes: 0,
            usage_event_ordinal: 0,
            current_turn_id: None,
            turn_id_announced_by_task_started: false,
            pending_usage_record: None,
            settled_usage_records: VecDeque::new(),
            pending_model_rows: Vec::new(),
            session_id_from_meta: None,
            session_forked_from_id: None,
            session_is_subagent: false,
            session_is_guardian: false,
            session_is_headless: false,
            session_agent: None,
            forked_child_session_id: None,
            forked_child_replay_session_id: None,
            forked_child_waiting_for_turn_context: false,
            forked_child_inherited_baseline: None,
            forked_child_inherited_reported_total: None,
            forked_child_is_user_fork: false,
            forked_child_task_started_turn_ids: HashSet::new(),
        }
    }

    pub fn with_fallback_timestamp(mut self, timestamp_ms: Option<i64>) -> Self {
        self.fallback_timestamp = timestamp_ms;
        self
    }

    /// Complete-source validation. Unresolved model-less usage is visible
    /// unsupported, and a still-pending usage record is settled exactly once.
    pub fn finish(&mut self) -> Result<(), ReaderError> {
        if let Some(pending) = self.pending_usage_record.take() {
            self.emit_usage_record(pending)?;
        }
        if !self.pending_model_rows.is_empty() {
            return Err(ReaderError::Unsupported("missing model"));
        }
        Ok(())
    }

    pub fn has_supported_usage(&self) -> bool {
        self.supported_usage
    }

    pub fn snapshot(&self) -> Vec<ResponseUsage> {
        self.responses.clone()
    }

    /// Complete records only; errors leave all captured state unchanged.
    /// Full-source publication/reconciliation belongs to the collector.
    pub fn ingest(&mut self, bytes: &[u8]) -> Result<(), ReaderError> {
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(ReaderError::Capacity("record bytes"));
        }
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(());
        }
        let value: Value = serde_json::from_slice(bytes).map_err(|_| ReaderError::InvalidRecord)?;
        let object = value.as_object().ok_or(ReaderError::InvalidRecord)?;
        let kind = string(object, "type")?.ok_or(ReaderError::InvalidRecord)?;
        if kind == "compacted" {
            self.mark_compacted();
            return Ok(());
        }
        let Some(payload) = object.get("payload").filter(|v| !v.is_null()) else {
            return Err(ReaderError::Unsupported("non-rollout record"));
        };
        let payload = payload.as_object().ok_or(ReaderError::InvalidRecord)?;
        let payload_kind = string(payload, "type")?;
        let is_token_count = kind == "event_msg" && payload_kind.as_deref() == Some("token_count");

        // A forked child replays its parent's history. Skip every record until
        // the child's own turn gate opens, retaining only inherited baselines.
        if self.forked_child_waiting_for_turn_context {
            if kind == "turn_context" && self.forked_turn_starts_own(payload)? {
                self.forked_child_waiting_for_turn_context = false;
                self.forked_child_replay_session_id = None;
                self.forked_child_task_started_turn_ids.clear();
                self.forked_child_is_user_fork = false;
                if let Some(id) = self.forked_child_session_id.clone() {
                    self.session_id_from_meta = Some(id);
                }
                self.model = model_from(payload)?;
            } else {
                if kind == "session_meta" {
                    if let Some(id) = string(payload, "id")?
                        && self
                            .forked_child_session_id
                            .as_deref()
                            .is_some_and(|child| child != id)
                    {
                        self.forked_child_replay_session_id = Some(id);
                    }
                } else if is_token_count {
                    self.remember_inherited_baseline(payload)?;
                } else if kind == "event_msg"
                    && payload_kind.as_deref() == Some("task_started")
                    && self.forked_task_starts_own(payload)?
                    && let Some(turn) = string(payload, "turn_id")?
                {
                    self.forked_child_task_started_turn_ids.insert(turn);
                }
                return Ok(());
            }
        }

        if kind == "token_usage_record" || payload_kind.as_deref() == Some("token_usage_record") {
            return self.usage_record(object, payload);
        }
        if payload_kind.as_deref() == Some("compacted") {
            self.mark_compacted();
            return Ok(());
        }

        // A new turn or terminal event ends any wait for the previous turn's
        // token_count, so the pending record is counted now. `compacted`
        // merely marks it for the next token_count to settle.
        let payload_turn = string(payload, "turn_id")?;
        let starts_other_turn = (kind == "event_msg"
            && payload_kind.as_deref() == Some("task_started"))
            || (kind == "turn_context"
                && self
                    .pending_usage_record
                    .as_ref()
                    .is_some_and(|record| record.turn_id != payload_turn));
        let ends_pending_turn = kind == "event_msg"
            && matches!(
                payload_kind.as_deref(),
                Some("turn_aborted" | "task_complete")
            )
            && self
                .pending_usage_record
                .as_ref()
                .is_some_and(|record| payload_turn.is_none() || record.turn_id == payload_turn);
        if starts_other_turn || ends_pending_turn {
            self.settle_pending()?;
        }

        // The thread-scoped tier is replaced or cleared by the snapshot and
        // never reset at a turn boundary.
        if kind == "event_msg" && payload_kind.as_deref() == Some("thread_settings_applied") {
            if let Some(settings) = payload.get("thread_settings").and_then(Value::as_object) {
                if let Some(provider) = string(settings, "model_provider_id")? {
                    self.provider = Some(provider);
                }
                self.thread_tier = normalize_tier(string(settings, "service_tier")?);
            } else if let Some(tier) = normalize_tier(string(payload, "service_tier")?) {
                self.thread_tier = Some(tier);
            }
            return Ok(());
        }

        // Both `task_started` and `turn_context` start a turn. A
        // `turn_context` without an id keeps the id its own `task_started`
        // announced just before it.
        if kind == "event_msg" && payload_kind.as_deref() == Some("task_started") {
            self.current_turn_id = payload_turn.clone();
            self.turn_id_announced_by_task_started = self.current_turn_id.is_some();
        } else if kind == "turn_context" {
            if payload_turn.is_some() {
                self.current_turn_id = payload_turn.clone();
            } else if !self.turn_id_announced_by_task_started {
                self.current_turn_id = None;
            }
            self.turn_id_announced_by_task_started = false;
        }

        if let Some(tier) = normalize_tier(string(payload, "service_tier")?) {
            self.request_tier = Some(tier);
        }

        if kind == "world_state" && self.model.is_none() {
            if let Some(model) = world_state_model(payload)? {
                self.flush_pending_model_rows(&model)?;
                self.model = Some(model);
            }
            return Ok(());
        }

        if kind == "session_meta" {
            return self.metadata(payload);
        }
        if kind == "turn_context" {
            self.model = model_from(payload)?;
            self.time_cursor = timestamp(object)?;
            self.request_tier = None;
            if let Some(tier) = normalize_tier(string(payload, "service_tier")?) {
                self.request_tier = Some(tier);
            }
            if let Some(model) = self.model.clone() {
                self.flush_pending_model_rows(&model)?;
            }
            return Ok(());
        }
        if kind == "event_msg" {
            match payload_kind.as_deref() {
                Some("token_count") => {
                    let ordinal = self.usage_event_ordinal;
                    let next = ordinal.checked_add(1).ok_or(ReaderError::Overflow)?;
                    self.token_count(object, payload, ordinal)?;
                    self.usage_event_ordinal = next;
                    return Ok(());
                }
                Some("task_started") => {
                    let started = payload
                        .get("started_at")
                        .filter(|v| !v.is_null())
                        .and_then(Value::as_i64)
                        .and_then(|t| t.checked_mul(1000));
                    self.time_cursor = started.or(timestamp(object)?);
                    return Ok(());
                }
                Some("user_message") => {
                    if let Some(message) = string(payload, "message")?
                        && ![
                            "<environment_context>",
                            "<system-reminder>",
                            "<user_instructions>",
                        ]
                        .iter()
                        .any(|prefix| message.trim_start().starts_with(prefix))
                    {
                        self.time_cursor = timestamp(object)?;
                    }
                    return Ok(());
                }
                _ => {}
            }
        }
        if payload.contains_key("usage")
            || object.contains_key("usage")
            || payload_kind.as_deref() == Some("token_count")
        {
            return Err(ReaderError::Unsupported("additional usage record"));
        }
        Ok(())
    }

    fn metadata(&mut self, payload: &Map<String, Value>) -> Result<(), ReaderError> {
        if let Some(origin) = string(payload, "originator")?
            && origin.to_ascii_lowercase().contains("openclaw")
        {
            return Err(ReaderError::Unsupported("non-Codex ownership"));
        }
        let id =
            string(payload, "id")?.ok_or(ReaderError::Unsupported("missing session metadata"))?;
        if id.len() > MAX_IDENTITY_BYTES {
            return Err(ReaderError::Capacity("identity bytes"));
        }
        if !self.forked_child_waiting_for_turn_context
            && self
                .forked_child_session_id
                .as_deref()
                .is_none_or(|child| child != id)
            && id != self.session
        {
            return Err(ReaderError::Unsupported("session identity change"));
        }
        let source = payload.get("source").filter(|value| !value.is_null());
        if source.is_some_and(|value| !value.is_string() && !value.is_object()) {
            return Err(ReaderError::Unsupported("structured source identity"));
        }
        let thread_source = string(payload, "thread_source")?;
        let forked_from = string(payload, "forked_from_id")?
            .filter(|id| !id.is_empty())
            .or_else(|| forked_from_id_from_source(source).map(str::to_owned));
        if let Some(parent) = forked_from {
            let repeated_active_child_meta = !self.forked_child_waiting_for_turn_context
                && self.forked_child_session_id.as_deref() == Some(id.as_str());
            self.session_forked_from_id = Some(parent);
            self.forked_child_session_id = Some(id.clone());
            if !repeated_active_child_meta {
                self.forked_child_waiting_for_turn_context = true;
                self.forked_child_replay_session_id = None;
                self.forked_child_inherited_baseline = None;
                self.forked_child_inherited_reported_total = None;
                self.forked_child_task_started_turn_ids.clear();
                self.forked_child_is_user_fork = thread_source.as_deref() == Some("user");
            }
        }
        if let Some(provider) = string(payload, "model_provider")? {
            self.provider = Some(provider);
        }
        let guardian = thread_source.as_deref() == Some("guardian_review")
            || source
                .and_then(|value| value.get("subagent"))
                .and_then(|value| value.get("other"))
                .and_then(Value::as_str)
                == Some("guardian");
        let subagent = !guardian
            && (thread_source.as_deref() == Some("subagent")
                || source.and_then(|value| value.get("subagent")).is_some());
        if guardian {
            self.session_is_guardian = true;
        } else if subagent {
            self.session_is_subagent = true;
        }
        self.session_agent = Some(
            if self.session_is_guardian {
                CODEX_GUARDIAN_AGENT
            } else if self.session_is_subagent {
                CODEX_SUBAGENT_AGENT
            } else {
                CODEX_DEFAULT_AGENT
            }
            .to_owned(),
        );
        if source.and_then(Value::as_str) == Some("exec") {
            self.session_is_headless = true;
        }
        self.session_id_from_meta = Some(id);
        self.metadata_seen = true;
        Ok(())
    }

    fn usage_record(
        &mut self,
        object: &Map<String, Value>,
        payload: &Map<String, Value>,
    ) -> Result<(), ReaderError> {
        // Another thread's response is not reported by this rollout's
        // token_counts; its own rollout counts it.
        if let (Some(thread), Some(own)) = (
            string(payload, "thread_id")?,
            self.session_id_from_meta.as_deref(),
        ) && thread != own
        {
            return Ok(());
        }
        if let Some(pending) = self.pending_usage_record.take() {
            self.emit_usage_record(pending)?;
        }
        let usage = payload
            .get("usage")
            .filter(|value| !value.is_null())
            .map(|value| {
                serde_json::from_value::<Usage>(value.clone())
                    .map_err(|_| ReaderError::InvalidRecord)
            })
            .transpose()?;
        let Some(usage) = usage.filter(Usage::supported) else {
            return Ok(());
        };
        let tier = normalize_tier(string(payload, "service_tier")?)
            .or(normalize_tier(usage.service_tier.clone()));
        if let Some(tier) = tier.clone() {
            self.request_tier = Some(tier);
        }
        let provider = string(payload, "provider")?.or_else(|| self.provider.clone());
        let turn_id = string(payload, "turn_id")?.or_else(|| self.current_turn_id.clone());
        self.pending_usage_record = Some(PendingUsageRecord {
            usage: usage.totals(),
            model: model_from(payload)?.or_else(|| self.model.clone()),
            provider,
            tier,
            timestamp_ms: timestamp(object)?,
            turn_id,
            response_id: string(payload, "response_id")?,
            after_compacted: false,
        });
        // A record is a usage ledger even before its token_count settles it.
        self.supported_usage = true;
        Ok(())
    }

    fn mark_compacted(&mut self) {
        if let Some(record) = self.pending_usage_record.as_mut() {
            record.after_compacted = true;
        }
    }

    fn settle_pending(&mut self) -> Result<(), ReaderError> {
        if let Some(pending) = self.pending_usage_record.take() {
            self.emit_usage_record(pending)?;
        }
        Ok(())
    }

    fn emit_usage_record(&mut self, pending: PendingUsageRecord) -> Result<(), ReaderError> {
        let tokens = pending.usage.tokens();
        if tokens == TokenCounts::default() {
            return Ok(());
        }
        let dedup_scope = self.dedup_scope_id().to_owned();
        let dedup_key = pending.response_id.as_ref().map_or_else(
            || {
                format!(
                    "codex:usage-record:{dedup_scope}:{}:{}:{}:{}:{}",
                    pending.timestamp_ms.unwrap_or_default(),
                    pending.usage.input,
                    pending.usage.output,
                    pending.usage.cached,
                    pending.usage.reasoning
                )
            },
            |response| format!("codex:usage-record:{dedup_scope}:{response}"),
        );
        let timestamp_ms = pending
            .timestamp_ms
            .or(self.time_cursor)
            .or(self.fallback_timestamp);
        let model = pending.model.clone().or_else(|| self.model.clone());
        let tier = pending
            .tier
            .clone()
            .or_else(|| self.request_tier.clone())
            .or_else(|| self.thread_tier.clone());
        let provider = pending.provider.clone().or_else(|| self.provider.clone());
        let row = PendingModelRow {
            usage: tokens,
            timestamp_ms,
            fixed_dedup_key: Some(dedup_key),
            total: Some(pending.usage),
            parsed_time: pending.timestamp_ms,
            event_ordinal: 0,
            dedup_scope,
            provider,
            tier,
        };
        self.remember_settled_usage_record(pending.usage);
        if let Some(model) = model {
            self.push_model_row(&model, row)?;
        } else {
            self.pending_model_rows.push(row);
        }
        self.supported_usage = true;
        Ok(())
    }

    fn remember_settled_usage_record(&mut self, usage: Totals) {
        if self.settled_usage_records.len() == SETTLED_USAGE_RECORD_LIMIT {
            self.settled_usage_records.pop_front();
        }
        self.settled_usage_records.push_back(usage);
    }

    fn take_settled_usage_record(&mut self, emitted: Totals) -> bool {
        let Some(index) = self
            .settled_usage_records
            .iter()
            .position(|usage| usage.reports_same_response(emitted))
        else {
            return false;
        };
        self.settled_usage_records.remove(index);
        true
    }

    fn dedup_scope_id(&self) -> &str {
        self.session_forked_from_id
            .as_deref()
            .or(self.session_id_from_meta.as_deref())
            .unwrap_or(&self.session)
    }

    fn rollout_id(&self) -> &str {
        self.session_id_from_meta
            .as_deref()
            .filter(|id| !id.is_empty())
            .unwrap_or(&self.session)
    }

    fn push_model_row(&mut self, model: &str, row: PendingModelRow) -> Result<(), ReaderError> {
        let timestamp_ms = row
            .timestamp_ms
            .or(self.time_cursor)
            .or(self.fallback_timestamp)
            .ok_or(ReaderError::InvalidTimestamp)?;
        let dedup_key = row
            .fixed_dedup_key
            .clone()
            .unwrap_or_else(|| self.token_count_dedup_key(model, &row));
        if dedup_key.len() > MAX_IDENTITY_BYTES {
            return Err(ReaderError::Capacity("identity bytes"));
        }
        let identity_bytes = self.session.len()
            + model.len()
            + row.provider.as_ref().map_or(0, String::len)
            + row.tier.as_ref().map_or(0, String::len)
            + dedup_key.len();
        if self.responses.len() + self.pending_model_rows.len() == MAX_RESPONSES
            || self.retained_identity_bytes + identity_bytes > MAX_RETAINED_IDENTITY_BYTES
        {
            return Err(ReaderError::Capacity("captured responses"));
        }
        let agent = if self.session_is_headless {
            Some(CODEX_HEADLESS_AGENT.to_owned())
        } else {
            self.session_agent.clone()
        };
        self.responses.push(ResponseUsage {
            key: SessionKey::new("codex", &self.session),
            model: model.to_owned(),
            tokens: row.usage,
            cache_write_1h: 0,
            timestamp_ms,
            dedup_key: Some(dedup_key),
            billing_provider_hint: row.provider,
            service_tier: row.tier,
            agent,
            cost: CostStatus::Unavailable,
        });
        self.retained_identity_bytes += identity_bytes;
        self.supported_usage = true;
        Ok(())
    }

    fn token_count_dedup_key(&self, model: &str, row: &PendingModelRow) -> String {
        let provider = self.provider.as_deref().unwrap_or("openai");
        if let Some(total) = row.total {
            format!(
                "codex:token_count-total:{}:{provider}:{model}:{}:{}:{}:{}",
                row.dedup_scope, total.input, total.output, total.cached, total.reasoning
            )
        } else if let Some(time) = row.parsed_time {
            format!(
                "codex:token_count:{time}:{provider}:{model}:{}:{}:{}:{}:{}",
                row.usage.input,
                row.usage.output,
                row.usage.cache_read,
                row.usage.cache_write,
                row.usage.reasoning
            )
        } else {
            format!(
                "codex:usage-fallback:{}:{}:{}:{}:{model}:{}:{}:{}:{}:{}:{}",
                self.rollout_id().len(),
                self.rollout_id(),
                row.event_ordinal,
                model.len(),
                row.usage.input,
                row.usage.output,
                row.usage.cache_read,
                row.usage.cache_write,
                0,
                row.usage.reasoning
            )
        }
    }

    fn flush_pending_model_rows(&mut self, model: &str) -> Result<(), ReaderError> {
        let rows = std::mem::take(&mut self.pending_model_rows);
        for row in rows {
            self.push_model_row(model, row)?;
        }
        Ok(())
    }

    fn token_count(
        &mut self,
        object: &Map<String, Value>,
        payload: &Map<String, Value>,
        usage_event_ordinal: u64,
    ) -> Result<(), ReaderError> {
        if !self.metadata_seen {
            return Err(ReaderError::Unsupported("missing session metadata"));
        }
        let Some(info) = payload.get("info").filter(|v| !v.is_null()) else {
            return Ok(());
        };
        let info = info.as_object().ok_or(ReaderError::InvalidRecord)?;
        let last = parse_usage(info, "last_token_usage")?;
        let total = parse_usage(info, "total_token_usage")?;
        let supported = last.as_ref().is_some_and(Usage::supported)
            || total.as_ref().is_some_and(Usage::supported);
        if !supported {
            if last.is_some() || total.is_some() {
                return Err(ReaderError::Unsupported("unclassified token counters"));
            }
            return Ok(());
        }
        let total_count = total.as_ref().map(Usage::totals);
        let last_count = last.as_ref().map(Usage::totals);

        let mut model = model_from(payload)?
            .or(model_from(info)?)
            .or_else(|| self.model.clone())
            .filter(|model| !model.trim().is_empty());
        if let Some(model) = model.as_deref() {
            self.flush_pending_model_rows(model)?;
            self.model = Some(model.to_owned());
        }

        if let Some(tier) = normalize_tier(string(payload, "service_tier")?)
            .or(normalize_tier(string(info, "service_tier")?))
            .or_else(|| last.as_ref().and_then(|usage| usage.service_tier.clone()))
            .or_else(|| total.as_ref().and_then(|usage| usage.service_tier.clone()))
        {
            self.request_tier = Some(tier);
        }

        // A forked child can replay several parent token_count rows after its
        // first child turn, often with child-local timestamps. Suppress them
        // while the cumulative totals stay within the inherited baseline.
        if self.forked_child_inherited_baseline.is_some()
            && self.skip_inherited_snapshot(total.as_ref(), total_count)
        {
            self.supported_usage |= supported;
            self.pending_usage_record = None;
            return Ok(());
        }
        self.forked_child_inherited_baseline = None;
        self.forked_child_inherited_reported_total = None;
        self.supported_usage |= supported;

        let mut rebaseline = None;
        let emitted = match (total_count, last_count, self.previous) {
            (Some(total), Some(last), Some(previous)) => {
                if total == previous
                    || (total.delta_from(previous).is_none()
                        && total.stale_regression(previous, last))
                {
                    None
                } else {
                    Some((last, Some(total)))
                }
            }
            (Some(total), Some(last), None) => Some((last, Some(total))),
            (Some(total), None, Some(previous)) => {
                if total == previous {
                    None
                } else if let Some(delta) = total.delta_from(previous) {
                    Some((delta, Some(total)))
                } else {
                    rebaseline = Some(total);
                    None
                }
            }
            (Some(total), None, None) => Some((total, Some(total))),
            (None, Some(last), Some(previous)) => Some((last, Some(previous.checked_add(last)?))),
            (None, Some(last), None) => Some((last, None)),
            (None, None, _) => None,
        }
        .filter(|(usage, _)| usage.tokens() != TokenCounts::default());
        let Some((emitted_usage, next)) = emitted else {
            if let Some(total) = rebaseline {
                self.previous = Some(total);
            }
            if self
                .pending_usage_record
                .as_ref()
                .is_some_and(|record| record.after_compacted)
            {
                self.settle_pending()?;
            }
            return Ok(());
        };
        self.previous = next;

        // Settle the pending record against the usage this token_count emits.
        let reports_pending = self
            .pending_usage_record
            .as_ref()
            .is_some_and(|record| record.usage.reports_same_response(emitted_usage));
        if reports_pending {
            self.pending_usage_record = None;
        } else if self.take_settled_usage_record(emitted_usage) {
            // A late report of a response already counted on its own. The
            // baseline moved on; any pending record waits for its own report.
            return Ok(());
        } else if self
            .pending_usage_record
            .as_ref()
            .is_some_and(|record| record.after_compacted)
        {
            self.settle_pending()?;
        } else {
            // Outside compaction the next report confirms the record by order.
            self.pending_usage_record = None;
        }

        let parsed_time = timestamp(object)?;
        let time = self.time_cursor.or(parsed_time).or(self.fallback_timestamp);
        let tier = self
            .request_tier
            .clone()
            .or_else(|| self.thread_tier.clone());
        let row = PendingModelRow {
            usage: emitted_usage.tokens(),
            timestamp_ms: time,
            fixed_dedup_key: None,
            total: total_count,
            parsed_time,
            event_ordinal: usage_event_ordinal,
            dedup_scope: self.dedup_scope_id().to_owned(),
            provider: self.provider.clone(),
            tier,
        };
        if let Some(model) = model.take() {
            self.push_model_row(&model, row)?;
        } else {
            if self.responses.len() + self.pending_model_rows.len() >= MAX_RESPONSES {
                return Err(ReaderError::Capacity("captured responses"));
            }
            self.pending_model_rows.push(row);
        }
        if parsed_time.is_some_and(|t| self.time_cursor.is_none_or(|old| t > old)) {
            self.time_cursor = parsed_time;
        }
        self.turn_id_announced_by_task_started = false;
        Ok(())
    }

    fn remember_inherited_baseline(
        &mut self,
        payload: &Map<String, Value>,
    ) -> Result<(), ReaderError> {
        let Some(total) = payload
            .get("info")
            .and_then(Value::as_object)
            .and_then(|info| info.get("total_token_usage"))
            .filter(|value| !value.is_null())
            .map(|value| {
                serde_json::from_value::<Usage>(value.clone())
                    .map_err(|_| ReaderError::InvalidRecord)
            })
            .transpose()?
        else {
            return Ok(());
        };
        let totals = total.totals();
        self.previous = Some(totals);
        self.forked_child_inherited_baseline = Some(totals);
        self.forked_child_inherited_reported_total = total.total_tokens;
        Ok(())
    }

    fn skip_inherited_snapshot(&self, total: Option<&Usage>, totals: Option<Totals>) -> bool {
        if let Some(baseline) = self.forked_child_inherited_reported_total
            && let Some(total) = total.and_then(|usage| usage.total_tokens)
            && total <= baseline
        {
            return true;
        }
        totals
            .zip(self.forked_child_inherited_baseline)
            .is_some_and(|(totals, baseline)| totals.is_within(baseline))
    }

    fn forked_turn_starts_own(&self, payload: &Map<String, Value>) -> Result<bool, ReaderError> {
        let turn_id = string(payload, "turn_id")?;
        if self.forked_child_replay_session_id.is_none() {
            return Ok(true);
        }
        let Some(child) = self.forked_child_session_id.as_deref() else {
            return Ok(true);
        };
        match (turn_id.as_deref(), uuid_v7_order_key(child)) {
            (Some(turn_id), Some(child_key)) => {
                let Some(turn_key) = uuid_v7_order_key(turn_id) else {
                    return Ok(self.forked_child_is_user_fork
                        || self.forked_child_task_started_turn_ids.contains(turn_id));
                };
                let ordering = turn_key[..12].cmp(&child_key[..12]);
                Ok(match ordering {
                    std::cmp::Ordering::Greater => true,
                    std::cmp::Ordering::Less => false,
                    std::cmp::Ordering::Equal => {
                        self.forked_child_is_user_fork
                            || self.forked_child_task_started_turn_ids.contains(turn_id)
                    }
                })
            }
            _ => Ok(true),
        }
    }

    fn forked_task_starts_own(&self, payload: &Map<String, Value>) -> Result<bool, ReaderError> {
        let turn_id = string(payload, "turn_id")?;
        let Some((turn_id, child)) = turn_id
            .as_deref()
            .zip(self.forked_child_session_id.as_deref())
        else {
            return Ok(false);
        };
        let Some(child_key) = uuid_v7_order_key(child) else {
            return Ok(true);
        };
        if let Some(turn_key) = uuid_v7_order_key(turn_id) {
            return Ok(turn_key[..12] >= child_key[..12]);
        }
        let Some(started_at) = payload
            .get("started_at")
            .filter(|value| !value.is_null())
            .and_then(Value::as_i64)
        else {
            return Ok(false);
        };
        let Ok(child_started_at_ms) = i64::from_str_radix(&child_key[..12], 16) else {
            return Ok(false);
        };
        Ok(started_at >= child_started_at_ms / 1000)
    }
}

fn parse_usage(object: &Map<String, Value>, key: &str) -> Result<Option<Usage>, ReaderError> {
    object
        .get(key)
        .filter(|v| !v.is_null())
        .map(|v| serde_json::from_value(v.clone()).map_err(|_| ReaderError::InvalidRecord))
        .transpose()
}

fn string(object: &Map<String, Value>, key: &str) -> Result<Option<String>, ReaderError> {
    object
        .get(key)
        .filter(|v| !v.is_null())
        .map(|v| {
            let text = v.as_str().ok_or(ReaderError::InvalidRecord)?;
            if text.len() > MAX_IDENTITY_BYTES {
                return Err(ReaderError::Capacity("identity bytes"));
            }
            Ok(text.to_string())
        })
        .transpose()
}

fn model_from(object: &Map<String, Value>) -> Result<Option<String>, ReaderError> {
    let info = object.get("model_info").and_then(Value::as_object);
    Ok(string(object, "model")?
        .or(string(object, "model_name")?)
        .or(info.map(|v| string(v, "slug")).transpose()?.flatten()))
}

fn timestamp(object: &Map<String, Value>) -> Result<Option<i64>, ReaderError> {
    string(object, "timestamp")?
        .map(|text| {
            DateTime::parse_from_rfc3339(&text)
                .map(|t| t.timestamp_millis())
                .map_err(|_| ReaderError::InvalidTimestamp)
        })
        .transpose()
}

fn normalize_tier(tier: Option<String>) -> Option<String> {
    tier.map(|tier| tier.trim().to_ascii_lowercase())
        .filter(|tier| !tier.is_empty())
}

fn world_state_model(payload: &Map<String, Value>) -> Result<Option<String>, ReaderError> {
    let state = payload.get("state").and_then(Value::as_object);
    Ok(state
        .map(|state| string(state, "model"))
        .transpose()?
        .flatten()
        .filter(|model| !model.trim().is_empty()))
}

fn forked_from_id_from_source(source: Option<&Value>) -> Option<&str> {
    source?
        .get("subagent")?
        .get("thread_spawn")?
        .get("parent_thread_id")?
        .as_str()
        .filter(|id| !id.is_empty())
}

fn uuid_v7_order_key(id: &str) -> Option<String> {
    let mut parts = id.split('-');
    let first = parts.next()?;
    let second = parts.next()?;
    let third = parts.next()?;
    let fourth = parts.next()?;
    let fifth = parts.next()?;
    if parts.next().is_some()
        || first.len() != 8
        || second.len() != 4
        || third.len() != 4
        || fourth.len() != 4
        || fifth.len() != 12
        || !third.starts_with('7')
    {
        return None;
    }
    let mut key = String::with_capacity(32);
    for part in [first, second, third, fourth, fifth] {
        if !part.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        key.push_str(&part.to_ascii_lowercase());
    }
    Some(key)
}
