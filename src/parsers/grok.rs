//! Grok Build session normalization adapted from the pinned Tokscale `grok.rs`
//! reader. MIT © 2025 Junho Yeo; see docs/migration/parser-provenance.md.
//!
//! Supported input is `updates.jsonl` under
//! `~/.grok/sessions/<workspace>/<session-id>/`. Two lanes are interpreted, in
//! this order of authority:
//!
//! - `params.update.sessionUpdate == "turn_completed"` records carry a full
//!   token breakdown. Grok's `inputTokens` includes its `cachedReadTokens`
//!   subset and `outputTokens` includes its `reasoningTokens` subset, so those
//!   overlaps are removed to keep the buckets additive.
//! - Legacy update logs expose only a cumulative `totalTokens` counter. Each
//!   positive per-turn delta is recorded as input tokens, which is exactly what
//!   the counters prove happened; no input/output split is invented.
//!
//! Not implemented, and recorded as unsupported coverage rather than guessed:
//! the sibling `signals.json` rollup top-up for compacted sessions, the
//! `~/.grok/logs/unified.jsonl` per-inference lane with its subagent tree
//! attribution, and provider-reported cost ticks.

use super::{CostStatus, ReaderError, ResponseUsage};
use crate::accounting::{SessionKey, TokenCounts};
use serde_json::Value;

pub const MAX_RESPONSES: usize = 65_536;
const MAX_IDENTITY_BYTES: usize = 4_096;
const MAX_RETAINED_IDENTITY_BYTES: usize = 8 * 1024 * 1024;

pub const CLIENT: &str = "grok";
pub const PROVIDER: &str = "xai";
pub const UNKNOWN_MODEL: &str = "grok-unknown";

pub struct GrokReader {
    session: String,
    fallback_timestamp_ms: Option<i64>,
    workspace_key: Option<String>,
    workspace_label: Option<String>,
    responses: Vec<ResponseUsage>,
    /// Prompt id to the index of the observation it already produced, so a
    /// retransmitted turn replaces its snapshot instead of counting twice.
    prompt_indices: Vec<(String, usize)>,
    retained_identity_bytes: usize,
    current_model: Option<String>,
    last_total: Option<u64>,
    last_total_timestamp_ms: Option<i64>,
    active_turn: Option<ActiveTurn>,
    turn_index: usize,
    usage_index: usize,
}

struct ActiveTurn {
    baseline_total: u64,
    max_total: u64,
    timestamp_ms: i64,
    model: Option<String>,
    turn_index: usize,
}

impl ActiveTurn {
    fn observe(&mut self, total: u64, timestamp_ms: i64) {
        if total > self.max_total {
            self.max_total = total;
            self.timestamp_ms = timestamp_ms;
        }
    }
}

impl GrokReader {
    pub fn new(session: impl Into<String>) -> Self {
        Self {
            session: session.into(),
            fallback_timestamp_ms: None,
            workspace_key: None,
            workspace_label: None,
            responses: Vec::new(),
            prompt_indices: Vec::new(),
            retained_identity_bytes: 0,
            current_model: None,
            last_total: None,
            last_total_timestamp_ms: None,
            active_turn: None,
            turn_index: 0,
            usage_index: 0,
        }
    }

    pub fn with_fallback_timestamp(mut self, timestamp_ms: Option<i64>) -> Self {
        self.fallback_timestamp_ms = timestamp_ms.filter(|value| *value >= 0);
        self
    }

    pub fn with_workspace(mut self, key: Option<String>, label: Option<String>) -> Self {
        self.workspace_key = key;
        self.workspace_label = label;
        self
    }

    pub fn ingest(&mut self, record: &[u8]) -> Result<(), ReaderError> {
        let value: Value =
            serde_json::from_slice(record).map_err(|_| ReaderError::InvalidRecord)?;
        let Some(object) = value.as_object() else {
            return Ok(());
        };
        if object.is_empty() {
            return Ok(());
        }

        if let Some(model) = extract_model(&value).filter(|model| !model.trim().is_empty()) {
            self.current_model = Some(model);
        }

        let timestamp_ms = extract_timestamp_ms(&value).or(self.fallback_timestamp_ms);

        if is_user_message_chunk(&value) {
            self.flush_active_turn();
            let timestamp_ms = timestamp_ms.ok_or(ReaderError::Declined("Grok timestamp"))?;
            self.active_turn = Some(ActiveTurn {
                baseline_total: self.last_total.unwrap_or(0),
                max_total: self.last_total.unwrap_or(0),
                timestamp_ms,
                model: self.current_model.clone(),
                turn_index: self.turn_index,
            });
            self.turn_index = self.turn_index.saturating_add(1);
        }

        if let Some(usage) = completed_turn_usage(&value)?.filter(CompletedUsage::has_tokens) {
            let timestamp_ms = timestamp_ms.ok_or(ReaderError::Declined("Grok usage timestamp"))?;
            let model = self
                .resolved_model(&value)
                .unwrap_or_else(|| UNKNOWN_MODEL.to_owned());
            let prompt_id = extract_prompt_id(&value);
            let dedup_key = match prompt_id.as_ref() {
                Some(prompt_id) => format!("grok:{}:prompt:{prompt_id}", self.session),
                None => format!(
                    "grok:{}:usage:{}:{}",
                    self.session, self.usage_index, usage.event_id
                ),
            };
            let response = self.response(model, timestamp_ms, usage.tokens, dedup_key);
            match prompt_id {
                Some(prompt_id) => {
                    self.merge_prompt_snapshot(prompt_id, response)?;
                }
                None => {
                    self.push(response)?;
                }
            }
            self.usage_index = self.usage_index.saturating_add(1);
        }

        if let Some(total) = extract_total_tokens(&value) {
            let timestamp_ms = timestamp_ms.filter(|value| *value >= 0);
            if let Some(previous) = self.last_total {
                // Grok repeats and rewinds intermediate counters while streaming
                // tool updates, so the cumulative total is treated as monotonic.
                if total < previous {
                    return Ok(());
                }
                if total > previous {
                    let timestamp_ms =
                        timestamp_ms.ok_or(ReaderError::Declined("Grok counter timestamp"))?;
                    if self.active_turn.is_none() {
                        self.active_turn = Some(ActiveTurn {
                            baseline_total: previous,
                            max_total: previous,
                            timestamp_ms,
                            model: self.current_model.clone(),
                            turn_index: self.turn_index,
                        });
                        self.turn_index = self.turn_index.saturating_add(1);
                    }
                    if let Some(turn) = self.active_turn.as_mut() {
                        turn.observe(total, timestamp_ms);
                    }
                    self.last_total_timestamp_ms = Some(timestamp_ms);
                } else if let Some(timestamp_ms) = timestamp_ms {
                    self.last_total_timestamp_ms = Some(timestamp_ms);
                }
            } else {
                if let (Some(turn), Some(timestamp_ms)) = (self.active_turn.as_mut(), timestamp_ms)
                {
                    turn.observe(total, timestamp_ms);
                }
                if let Some(timestamp_ms) = timestamp_ms {
                    self.last_total_timestamp_ms = Some(timestamp_ms);
                }
            }
            self.last_total = Some(total);
        }

        Ok(())
    }

    fn flush_active_turn(&mut self) {
        let Some(turn) = self.active_turn.take() else {
            return;
        };
        let delta = turn.max_total.saturating_sub(turn.baseline_total);
        if delta == 0 {
            return;
        }
        let model = turn.model.unwrap_or_else(|| UNKNOWN_MODEL.to_owned());
        let response = self.response(
            model,
            turn.timestamp_ms,
            TokenCounts {
                input: delta,
                ..TokenCounts::default()
            },
            format!("grok:{}:{}", self.session, turn.turn_index),
        );
        // Capacity is surfaced by `push` on the usage lane; a fallback turn that
        // cannot be retained is dropped rather than failing the whole candidate.
        let _ = self.push(response);
    }

    /// A newer snapshot of the same prompt replaces the earlier one when it
    /// carries at least as many tokens, which is the pinned reader's rule: a
    /// retransmitted turn is never counted twice.
    fn merge_prompt_snapshot(
        &mut self,
        prompt_id: String,
        response: ResponseUsage,
    ) -> Result<(), ReaderError> {
        if let Some((_, index)) = self
            .prompt_indices
            .iter()
            .find(|(existing, _)| existing == &prompt_id)
        {
            let candidate = total(&response.tokens);
            if candidate >= total(&self.responses[*index].tokens) {
                self.responses[*index] = response;
            }
            return Ok(());
        }
        let index = self.push(response)?;
        self.retain_identity(prompt_id.clone())?;
        self.prompt_indices.push((prompt_id, index));
        Ok(())
    }

    fn response(
        &self,
        model: String,
        timestamp_ms: i64,
        tokens: TokenCounts,
        dedup_key: String,
    ) -> ResponseUsage {
        ResponseUsage {
            key: SessionKey::new(CLIENT, self.session.clone()),
            model,
            tokens,
            cache_write_1h: 0,
            timestamp_ms,
            dedup_key: Some(dedup_key),
            billing_provider_hint: Some(PROVIDER.to_owned()),
            service_tier: None,
            agent: None,
            workspace_key: self.workspace_key.clone(),
            workspace_label: self.workspace_label.clone(),
            title: None,
            cost: CostStatus::Unavailable,
        }
    }

    fn push(&mut self, response: ResponseUsage) -> Result<usize, ReaderError> {
        if self.responses.len() == MAX_RESPONSES {
            return Err(ReaderError::Capacity("Grok responses"));
        }
        let index = self.responses.len();
        self.responses.push(response);
        Ok(index)
    }

    fn retain_identity(&mut self, identity: String) -> Result<(), ReaderError> {
        if identity.len() > MAX_IDENTITY_BYTES {
            return Err(ReaderError::InvalidRecord);
        }
        self.retained_identity_bytes = self
            .retained_identity_bytes
            .checked_add(identity.len())
            .filter(|bytes| *bytes <= MAX_RETAINED_IDENTITY_BYTES)
            .ok_or(ReaderError::Capacity("Grok identities"))?;
        Ok(())
    }

    pub fn snapshot(&self) -> Vec<ResponseUsage> {
        let mut responses = self.responses.clone();
        // The still-open turn is projected without mutating state, so `snapshot`
        // stays callable through a shared reference like every other reader.
        if let Some(turn) = self.active_turn.as_ref() {
            let delta = turn.max_total.saturating_sub(turn.baseline_total);
            if delta > 0 {
                let model = turn
                    .model
                    .clone()
                    .unwrap_or_else(|| UNKNOWN_MODEL.to_owned());
                responses.push(self.response(
                    model,
                    turn.timestamp_ms,
                    TokenCounts {
                        input: delta,
                        ..TokenCounts::default()
                    },
                    format!("grok:{}:{}", self.session, turn.turn_index),
                ));
            }
        }
        responses
    }

    /// A declined record can be the user message that opened a turn. The open
    /// turn is flushed rather than dropped: its observed totals are real, and
    /// abandoning it would leave the next turn's baseline pointing at a total
    /// that was never emitted.
    pub fn on_declined(&mut self) {
        self.flush_active_turn();
    }

    pub fn has_supported_usage(&self) -> bool {
        !self.responses.is_empty()
            || self
                .active_turn
                .as_ref()
                .is_some_and(|turn| turn.max_total > turn.baseline_total)
    }

    fn resolved_model(&self, value: &Value) -> Option<String> {
        self.current_model.clone().or_else(|| {
            // Fall back to a single per-model breakdown key, which is
            // unambiguous; several keys are not an answer.
            value
                .pointer("/params/update/usage/modelUsage")
                .and_then(Value::as_object)
                .filter(|models| models.len() == 1)
                .and_then(|models| models.keys().next().cloned())
        })
    }
}

struct CompletedUsage {
    tokens: TokenCounts,
    event_id: String,
}

impl CompletedUsage {
    fn has_tokens(&self) -> bool {
        total(&self.tokens) > 0
    }
}

/// A `turn_completed` record's usage block. Any other `sessionUpdate` kind is
/// not usage; a record with no `sessionUpdate` is accepted, which matches the
/// older shape that had no kind field.
fn completed_turn_usage(value: &Value) -> Result<Option<CompletedUsage>, ReaderError> {
    if value
        .pointer("/params/update/sessionUpdate")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind != "turn_completed")
    {
        return Ok(None);
    }
    let Some(usage) = value.pointer("/params/update/usage") else {
        return Ok(None);
    };
    if !usage.is_object() {
        return Ok(None);
    }

    let raw_input = counter(usage, &["inputTokens", "input_tokens", "promptTokens"])?;
    let raw_output = counter(
        usage,
        &["outputTokens", "output_tokens", "completionTokens"],
    )?;
    let cache_read = counter(
        usage,
        &[
            "cachedReadTokens",
            "cacheReadTokens",
            "cache_read_input_tokens",
        ],
    )?;
    let cache_write = counter(
        usage,
        &[
            "cachedWriteTokens",
            "cacheWriteTokens",
            "cacheCreationTokens",
            "cache_creation_input_tokens",
        ],
    )?;
    let reasoning = counter(
        usage,
        &["reasoningTokens", "thoughtTokens", "thinkingTokens"],
    )?;
    let reported_total = match usage
        .get("totalTokens")
        .or_else(|| usage.get("total_tokens"))
    {
        None | Some(Value::Null) => None,
        Some(value) => Some(value.as_u64().ok_or(ReaderError::InvalidRecord)?),
    };

    // The surrounding contract treats input and output as inclusive buckets even
    // when the optional aggregate total is absent, so the nested cache and
    // reasoning subsets are always removed; the total only has to agree with the
    // inclusive sum when it is present.
    let inclusive_total = raw_input.saturating_add(raw_output);
    if reported_total.is_some_and(|total| total != inclusive_total) {
        return Err(ReaderError::InvalidRecord);
    }

    let event_id = value
        .pointer("/params/_meta/eventId")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_default();

    Ok(Some(CompletedUsage {
        tokens: TokenCounts {
            input: raw_input.saturating_sub(cache_read),
            output: raw_output.saturating_sub(reasoning),
            cache_read,
            cache_write,
            reasoning,
        },
        event_id,
    }))
}

fn total(tokens: &TokenCounts) -> u64 {
    tokens
        .input
        .saturating_add(tokens.output)
        .saturating_add(tokens.cache_read)
        .saturating_add(tokens.cache_write)
        .saturating_add(tokens.reasoning)
}

/// The first present alias decides; a present but non-integer or negative value
/// is a malformed record, not a zero.
fn counter(usage: &Value, names: &[&str]) -> Result<u64, ReaderError> {
    for name in names {
        match usage.get(*name) {
            None | Some(Value::Null) => continue,
            Some(value) => {
                return value.as_u64().ok_or(ReaderError::InvalidRecord);
            }
        }
    }
    Ok(0)
}

fn extract_model(value: &Value) -> Option<String> {
    for path in [
        "/params/update/_meta/modelId",
        "/params/_meta/modelId",
        "/params/modelId",
        "/model_id",
        "/modelId",
        "/model",
    ] {
        if let Some(model) = value
            .pointer(path)
            .and_then(Value::as_str)
            .filter(|model| !model.trim().is_empty())
        {
            return Some(model.to_owned());
        }
    }
    None
}

fn extract_total_tokens(value: &Value) -> Option<u64> {
    for path in [
        "/params/_meta/totalTokens",
        "/params/update/_meta/totalTokens",
        "/params/update/totalTokens",
        "/params/totalTokens",
        "/usage/totalTokens",
        "/totalTokens",
    ] {
        if let Some(total) = value.pointer(path).and_then(Value::as_u64) {
            return Some(total);
        }
    }
    None
}

fn extract_timestamp_ms(value: &Value) -> Option<i64> {
    for path in [
        "/params/_meta/agentTimestampMs",
        "/params/update/_meta/agentTimestampMs",
        "/params/timestamp",
        "/timestamp",
        "/ts",
    ] {
        if let Some(timestamp) = value
            .pointer(path)
            .and_then(Value::as_i64)
            .filter(|timestamp| *timestamp > 0)
        {
            return Some(timestamp);
        }
    }
    None
}

fn extract_prompt_id(value: &Value) -> Option<String> {
    for path in [
        "/params/update/promptId",
        "/params/update/prompt_id",
        "/params/_meta/promptId",
        "/params/_meta/prompt_id",
    ] {
        if let Some(id) = value.pointer(path).and_then(Value::as_str) {
            let id = id.trim();
            if !id.is_empty() && !id.chars().any(char::is_control) {
                return Some(id.to_owned());
            }
        }
    }
    None
}

fn is_user_message_chunk(value: &Value) -> bool {
    value
        .pointer("/params/update/sessionUpdate")
        .and_then(Value::as_str)
        == Some("user_message_chunk")
}

impl Default for GrokReader {
    fn default() -> Self {
        Self::new("grok")
    }
}
