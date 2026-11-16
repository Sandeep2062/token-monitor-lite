//! Junie session events, adapted from the pinned reader
//! `crates/tokscale-core/src/sessions/junie.rs`. See
//! docs/migration/parser-provenance.md.
//!
//! Junie stores local sessions under `~/.junie/sessions/<session-id>/events.jsonl`.
//! Each `LlmResponseMetadataEvent` carries an array of per-model usage rows under
//! `event.agentEvent.modelUsage`, and each row is one observation.
//!
//! # Skip decisions use the parsed event kind, never a substring
//!
//! Junie's state-snapshot events are large and carry no usage. A raw substring
//! pre-filter is used only as a cheap gate; the authoritative skip is made on
//! the parsed `kind`, because a legitimate usage line may merely *mention* a
//! skipped kind in its text.
//!
//! # The recorded timestamp is the call's end
//!
//! `timestampMs` is written when the response event is logged, and `usage.time`
//! is that call's latency, so the start is back-calculated. That subtraction
//! applies **only** when `timestampMs` was recorded: with no explicit value the
//! timestamp is a session-id-derived or file-mtime fallback, and subtracting a
//! call's latency from it would move the message into the wrong day.
//!
//! # The dedup suffix is the row's position within its event
//!
//! Not a file-global counter, so a replayed event reproduces the same key and
//! collapses, while several genuinely distinct rows inside one event still stay
//! distinct. Distinct calls differ in `timestampMs`, which is already in the key.
//!
//! Not implemented: `resolve_alias`, `canonical_provider`,
//! `inferred_provider_from_model`, `with_pricing_model`, `duration_ms`,
//! `is_turn_start` and the local-timezone session-id timestamp — none of those
//! exist in this codebase, so the provider is the record's own value (else
//! `junie`), the model is the recorded id, and no turn structure is claimed.

use super::timestamps::optional_counter;
use super::{CostStatus, ReaderError, ResponseUsage};
use crate::accounting::{SessionKey, TokenCounts};
use serde_json::Value;

pub const MAX_RESPONSES: usize = 16_384;
const MAX_IDENTITY_BYTES: usize = 4_096;
const MAX_RETAINED_IDENTITY_BYTES: usize = 8 * 1024 * 1024;

pub const CLIENT: &str = "junie";

const USAGE_EVENT_KIND: &str = "LlmResponseMetadataEvent";
const USER_PROMPT_KIND: &str = "UserPromptEvent";

/// One Junie session event stream.
pub struct JunieReader {
    key: SessionKey,
    fallback_timestamp_ms: i64,
    rows: Vec<ResponseUsage>,
    seen: std::collections::HashSet<String>,
    retained_identity_bytes: usize,
}

impl JunieReader {
    pub fn new(session: impl Into<String>) -> Self {
        Self {
            key: SessionKey::new(CLIENT, session),
            fallback_timestamp_ms: 0,
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
        // Cheap gate only. Junie state snapshots can be very large and never
        // carry usage; the authoritative skip is on the parsed kind below.
        let text = std::str::from_utf8(record).map_err(|_| ReaderError::Declined("Junie line"))?;
        if !text.contains(USAGE_EVENT_KIND) && !text.contains(USER_PROMPT_KIND) {
            return Ok(());
        }
        let value: Value =
            serde_json::from_str(text).map_err(|_| ReaderError::Declined("Junie event"))?;

        // A prompt marks the turn; its usage arrives in the following event.
        // No turn structure is emitted because this codebase's observation has
        // no turn field.
        if kind(&value) == Some(USER_PROMPT_KIND) {
            return Ok(());
        }

        let Some(event) = value
            .pointer("/event/agentEvent")
            .filter(|event| string(event, "kind") == Some(USAGE_EVENT_KIND))
        else {
            return Ok(());
        };
        let explicit_timestamp = counter(&value, "timestampMs").filter(|value| *value > 0);
        let timestamp_ms = explicit_timestamp.unwrap_or(self.fallback_timestamp_ms);
        let Some(usages) = event.get("modelUsage").and_then(Value::as_array) else {
            return Ok(());
        };

        for (usage_index, usage) in usages.iter().enumerate() {
            let Some(model_raw) = string(usage, "model") else {
                continue;
            };
            if model_raw.len() > MAX_IDENTITY_BYTES {
                return Err(ReaderError::Declined("Junie model identity"));
            }
            let tokens = tokens(usage)?;
            let reported_cost = usage
                .get("cost")
                .and_then(Value::as_f64)
                .filter(|cost| cost.is_finite() && *cost >= 0.0);
            if zero(&tokens) && reported_cost.unwrap_or(0.0) == 0.0 {
                continue;
            }
            // `usage.time` is the call's latency in ms.
            let duration_ms = counter(usage, "time").filter(|value| *value > 0);
            let start_ms = match (explicit_timestamp, duration_ms) {
                (Some(end), Some(duration)) => end.saturating_sub(duration).max(0),
                _ => timestamp_ms,
            };
            let dedup_key = format!(
                "junie:{}:{start_ms}:{model_raw}:{}:{}:{}:{}:{}:{usage_index}",
                self.key.session,
                tokens.input,
                tokens.output,
                tokens.cache_read,
                tokens.cache_write,
                tokens.reasoning,
            );
            if dedup_key.len() > MAX_IDENTITY_BYTES {
                return Err(ReaderError::Declined("Junie identity"));
            }
            if !self.seen.insert(dedup_key.clone()) {
                continue;
            }
            self.retained_identity_bytes = self
                .retained_identity_bytes
                .checked_add(dedup_key.len())
                .filter(|bytes| *bytes <= MAX_RETAINED_IDENTITY_BYTES)
                .ok_or(ReaderError::Capacity("Junie identities"))?;
            if self.rows.len() == MAX_RESPONSES {
                return Err(ReaderError::Capacity("Junie responses"));
            }
            self.rows.push(ResponseUsage {
                key: self.key.clone(),
                model: model_raw.to_owned(),
                tokens,
                cache_write_1h: 0,
                timestamp_ms: start_ms,
                dedup_key: Some(dedup_key),
                billing_provider_hint: Some(
                    string(usage, "provider")
                        .map(str::to_owned)
                        .unwrap_or_else(|| CLIENT.to_owned()),
                ),
                service_tier: None,
                agent: agent_name(event),
                workspace_key: None,
                workspace_label: None,
                title: None,
                // A cost of zero here means "none recorded", not "billed
                // nothing", so only a positive figure is claimed.
                cost: reported_cost
                    .filter(|cost| *cost > 0.0)
                    .map_or(CostStatus::Unavailable, |cost| {
                        CostStatus::Reported((cost * 1e9).round() as u64)
                    }),
            });
        }
        Ok(())
    }

    pub fn snapshot(&self) -> Vec<ResponseUsage> {
        self.rows.clone()
    }

    pub fn has_supported_usage(&self) -> bool {
        !self.rows.is_empty()
    }
}

fn kind(value: &Value) -> Option<&str> {
    string(value, "kind")
}

fn string<'a>(value: &'a Value, field: &str) -> Option<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn counter(value: &Value, field: &str) -> Option<i64> {
    let raw = value.get(field)?;
    if let Some(number) = raw.as_i64() {
        return Some(number);
    }
    if let Some(number) = raw.as_u64() {
        return i64::try_from(number).ok();
    }
    raw.as_f64()
        .filter(|number| number.is_finite())
        .map(|number| number as i64)
}

fn agent_name(event: &Value) -> Option<String> {
    let agent = event.get("agent")?;
    string(agent, "name")
        .or_else(|| string(agent, "id"))
        .map(str::to_owned)
}

/// The first present counter among several spellings Junie has used.
fn first(usage: &Value, fields: &[&str]) -> Result<u64, ReaderError> {
    for field in fields {
        if usage.get(*field).is_some() {
            return optional_counter(usage, field).map(|value| value.unwrap_or(0));
        }
    }
    Ok(0)
}

fn tokens(usage: &Value) -> Result<TokenCounts, ReaderError> {
    Ok(TokenCounts {
        input: first(usage, &["inputTokens", "input"])?,
        output: first(usage, &["outputTokens", "output"])?,
        cache_read: first(
            usage,
            &["cacheInputTokens", "cacheReadInputTokens", "cacheRead"],
        )?,
        cache_write: first(
            usage,
            &[
                "cacheCreateTokens",
                "cacheCreationInputTokens",
                "cacheWrite",
            ],
        )?,
        reasoning: first(
            usage,
            &["reasoningTokens", "reasoningOutputTokens", "thinkingTokens"],
        )?,
    })
}

fn zero(tokens: &TokenCounts) -> bool {
    tokens.input == 0
        && tokens.output == 0
        && tokens.cache_read == 0
        && tokens.cache_write == 0
        && tokens.reasoning == 0
}
