//! Gemini CLI transcript normalization adapted from the pinned Tokscale
//! `gemini.rs` reader. MIT © 2025 Junho Yeo; see
//! docs/migration/parser-provenance.md.
//!
//! Supported shapes are session documents (`session-*.json` and
//! `tmp/<id>/chats/*.json`) with per-message `tokens`/`model`, and headless
//! JSONL/JSON events with direct token payloads or `stats`. Counters are
//! checked non-negative, cache-inclusive prompt totals are split once, and
//! tool tokens stay in input (matching the pinned reader). Negative clamping,
//! malformed-line skipping, unknown models, and cross-file fork merging are
//! deliberately not approximated.

use super::{CostStatus, ReaderError, ResponseUsage};
use crate::accounting::{SessionKey, TokenCounts};
use chrono::DateTime;
use serde_json::{Map, Value};

pub const MAX_RESPONSES: usize = 16_384;
const MAX_IDENTITY_BYTES: usize = 4_096;
const MAX_RETAINED_IDENTITY_BYTES: usize = 8 * 1024 * 1024;
const PROVIDER: &str = "google";

pub struct GeminiReader {
    session: String,
    fallback_timestamp_ms: Option<i64>,
    current_model: Option<String>,
    responses: Vec<ResponseUsage>,
    retained_identity_bytes: usize,
}

impl GeminiReader {
    pub fn new(session: impl Into<String>) -> Self {
        Self {
            session: session.into(),
            fallback_timestamp_ms: None,
            current_model: None,
            responses: Vec::new(),
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
        if object.contains_key("messages") && !object["messages"].is_array() {
            return Err(ReaderError::Unsupported("Gemini messages"));
        }
        if let Some(messages) = object.get("messages").and_then(Value::as_array) {
            let session = required_string(object, "sessionId")
                .or_else(|| required_string(object, "session_id"))
                .ok_or(ReaderError::Unsupported("Gemini session id"))?;
            if !valid_identity(session) {
                return Err(ReaderError::InvalidRecord);
            }
            for message in messages {
                self.ingest_session_message(session, message)?;
            }
            return Ok(());
        }
        self.ingest_event(object)
    }

    pub fn snapshot(&self) -> Vec<ResponseUsage> {
        self.responses.clone()
    }

    pub fn has_supported_usage(&self) -> bool {
        !self.responses.is_empty()
    }

    fn ingest_session_message(
        &mut self,
        session: &str,
        message: &Value,
    ) -> Result<(), ReaderError> {
        let object = message.as_object().ok_or(ReaderError::InvalidRecord)?;
        let Some(tokens) = object.get("tokens") else {
            return Ok(());
        };
        let tokens = tokens.as_object().ok_or(ReaderError::InvalidRecord)?;
        let Some(model) = object.get("model").and_then(Value::as_str) else {
            return Ok(());
        };
        if !valid_identity(model) {
            return Err(ReaderError::InvalidRecord);
        }
        let timestamp = event_timestamp(object)
            .or(self.fallback_timestamp_ms)
            .ok_or(ReaderError::Unsupported("Gemini message timestamp"))?;
        let usage = normalize_session_tokens(tokens)?;
        let id = object.get("id").and_then(Value::as_str);
        self.push(session, model, timestamp, usage, id)
    }

    fn ingest_event(&mut self, object: &Map<String, Value>) -> Result<(), ReaderError> {
        let event_type = object.get("type").and_then(Value::as_str).unwrap_or("");
        if let Some(session) = object
            .get("session_id")
            .or_else(|| object.get("sessionId"))
            .and_then(Value::as_str)
        {
            if !valid_identity(session) {
                return Err(ReaderError::InvalidRecord);
            }
            self.session = session.to_owned();
        }
        if let Some(model) = object.get("model").and_then(Value::as_str) {
            if !valid_identity(model) {
                return Err(ReaderError::InvalidRecord);
            }
            self.current_model = Some(model.to_owned());
        }
        if event_type == "init" {
            return Ok(());
        }
        if event_type == "gemini" || object.get("tokens").is_some() {
            let Some(tokens_value) = object.get("tokens") else {
                return Ok(());
            };
            if tokens_value.is_null() {
                return Ok(());
            }
            let model = self
                .current_model
                .as_deref()
                .ok_or(ReaderError::Unsupported("Gemini event model"))?;
            let tokens = tokens_value.as_object().ok_or(ReaderError::InvalidRecord)?;
            let timestamp = event_timestamp(object)
                .or(self.fallback_timestamp_ms)
                .ok_or(ReaderError::Unsupported("Gemini event timestamp"))?;
            let usage = normalize_direct_tokens(tokens)?;
            let id = object.get("id").and_then(Value::as_str);
            let session = self.session.clone();
            return self.push(&session, model, timestamp, usage, id);
        }
        let Some(stats) = object
            .get("stats")
            .or_else(|| object.get("result").and_then(|result| result.get("stats")))
        else {
            return Ok(());
        };
        let timestamp = event_timestamp(object)
            .or(self.fallback_timestamp_ms)
            .ok_or(ReaderError::Unsupported("Gemini stats timestamp"))?;
        let hint = object
            .get("model")
            .and_then(Value::as_str)
            .filter(|value| valid_identity(value))
            .or(self.current_model.as_deref());
        self.ingest_stats(stats, hint, timestamp)
    }

    fn ingest_stats(
        &mut self,
        stats: &Value,
        model_hint: Option<&str>,
        timestamp: i64,
    ) -> Result<(), ReaderError> {
        if let Some(models) = stats.get("models").and_then(Value::as_object) {
            if !models.is_empty() {
                for (model, value) in models {
                    if !valid_identity(model) {
                        return Err(ReaderError::InvalidRecord);
                    }
                    let tokens = value.get("tokens").unwrap_or(value);
                    let tokens = tokens.as_object().ok_or(ReaderError::InvalidRecord)?;
                    let usage = normalize_stats_tokens(value, tokens)?;
                    let session = self.session.clone();
                    self.push(&session, model, timestamp, usage, None)?;
                }
                return Ok(());
            }
        }
        let model = model_hint
            .filter(|value| valid_identity(value))
            .ok_or(ReaderError::Unsupported("Gemini stats model"))?;
        let tokens = stats.get("tokens").unwrap_or(stats);
        let tokens = tokens.as_object().ok_or(ReaderError::InvalidRecord)?;
        let usage = normalize_stats_tokens(stats, tokens)?;
        let session = self.session.clone();
        self.push(&session, model, timestamp, usage, None)
    }

    fn push(
        &mut self,
        session: &str,
        model: &str,
        timestamp_ms: i64,
        tokens: TokenCounts,
        message_id: Option<&str>,
    ) -> Result<(), ReaderError> {
        if self.responses.len() == MAX_RESPONSES {
            return Err(ReaderError::Capacity("Gemini responses"));
        }
        let dedup_key = match message_id {
            None => None,
            Some(id) if valid_identity(id) => Some(format!("gemini:{session}:{id}")),
            Some(_) => return Err(ReaderError::InvalidRecord),
        };
        if dedup_key
            .as_ref()
            .is_some_and(|key| key.len() > MAX_IDENTITY_BYTES)
        {
            return Err(ReaderError::Unsupported("Gemini message identity"));
        }
        let identity_bytes = dedup_key.as_ref().map_or(0, String::len);
        self.retained_identity_bytes = self
            .retained_identity_bytes
            .checked_add(identity_bytes)
            .filter(|bytes| *bytes <= MAX_RETAINED_IDENTITY_BYTES)
            .ok_or(ReaderError::Capacity("Gemini identities"))?;
        self.responses.push(ResponseUsage {
            key: SessionKey::new("gemini", session),
            model: model.to_owned(),
            tokens,
            cache_write_1h: 0,
            timestamp_ms,
            dedup_key,
            billing_provider_hint: Some(PROVIDER.to_owned()),
            service_tier: None,
            agent: None,
            cost: CostStatus::Unavailable,
        });
        Ok(())
    }
}

#[derive(Default)]
struct GeminiTokens {
    input: Option<u64>,
    output: Option<u64>,
    cached: Option<u64>,
    reasoning: Option<u64>,
    tool: Option<u64>,
    total: Option<u64>,
}

fn parse_tokens(object: &Map<String, Value>) -> Result<GeminiTokens, ReaderError> {
    Ok(GeminiTokens {
        input: first_u64(
            object,
            &[
                "input",
                "prompt",
                "input_tokens",
                "prompt_tokens",
                "promptTokenCount",
            ],
        )?,
        output: first_u64(
            object,
            &[
                "output",
                "candidates",
                "output_tokens",
                "completion_tokens",
                "candidatesTokenCount",
            ],
        )?,
        cached: first_u64(
            object,
            &["cached", "cached_tokens", "cachedContentTokenCount"],
        )?,
        reasoning: first_u64(object, &["thoughts", "reasoning", "thoughts_tokens"])?,
        tool: first_u64(object, &["tool", "tool_tokens"])?,
        total: first_u64(object, &["total", "totalTokenCount", "total_tokens"])?,
    })
}

fn normalize_session_tokens(object: &Map<String, Value>) -> Result<TokenCounts, ReaderError> {
    let tokens = parse_tokens(object)?;
    build_tokens(tokens, false, true)
}

fn normalize_direct_tokens(object: &Map<String, Value>) -> Result<TokenCounts, ReaderError> {
    let tokens = parse_tokens(object)?;
    build_tokens(tokens, false, true)
}

fn normalize_stats_tokens(
    wrapper: &Value,
    object: &Map<String, Value>,
) -> Result<TokenCounts, ReaderError> {
    let tokens = parse_tokens(object)?;
    let has_prompt = [
        "prompt",
        "input_tokens",
        "prompt_tokens",
        "promptTokenCount",
    ]
    .iter()
    .any(|key| object.contains_key(*key));
    let net_input = object.get("input").is_some();
    let input_includes_cache =
        has_prompt || (wrapper.get("tokens").is_some() && net_input) || !net_input;
    build_tokens(tokens, input_includes_cache, false)
}

fn build_tokens(
    tokens: GeminiTokens,
    input_includes_cache: bool,
    total_adjusts_input: bool,
) -> Result<TokenCounts, ReaderError> {
    let mut input = tokens.input.unwrap_or(0);
    let cached = tokens.cached.unwrap_or(0);
    let output = tokens.output.unwrap_or(0);
    let reasoning = tokens.reasoning.unwrap_or(0);
    let tool = tokens.tool.unwrap_or(0);
    match (input_includes_cache, total_adjusts_input, tokens.total) {
        (true, _, _) => {
            input = input.saturating_sub(cached.min(input));
        }
        (false, true, Some(total)) => {
            let inclusive = input
                .checked_add(output)
                .and_then(|value| value.checked_add(reasoning))
                .and_then(|value| value.checked_add(tool))
                .ok_or(ReaderError::Overflow)?;
            let exclusive = inclusive.checked_add(cached).ok_or(ReaderError::Overflow)?;
            if cached > 0 && total == inclusive && total != exclusive {
                input = input.saturating_sub(cached.min(input));
            }
        }
        _ => {}
    }
    Ok(TokenCounts {
        input: input.checked_add(tool).ok_or(ReaderError::Overflow)?,
        output,
        cache_read: cached,
        cache_write: 0,
        reasoning,
    })
}

fn first_u64(object: &Map<String, Value>, keys: &[&str]) -> Result<Option<u64>, ReaderError> {
    for key in keys {
        match object.get(*key) {
            None | Some(Value::Null) => {}
            Some(value) => {
                return value.as_u64().map(Some).ok_or(ReaderError::InvalidRecord);
            }
        }
    }
    Ok(None)
}

fn event_timestamp(object: &Map<String, Value>) -> Option<i64> {
    object
        .get("timestamp")
        .or_else(|| object.get("created_at"))
        .and_then(flexible_timestamp)
}

fn flexible_timestamp(value: &Value) -> Option<i64> {
    if let Some(text) = value.as_str() {
        return DateTime::parse_from_rfc3339(text)
            .ok()
            .map(|value| value.timestamp_millis());
    }
    value.as_i64().filter(|value| *value >= 0).or_else(|| {
        value
            .as_f64()
            .filter(|value| {
                value.is_finite()
                    && *value >= 0.0
                    && *value <= i64::MAX as f64
                    && value.fract() == 0.0
            })
            .map(|value| value as i64)
    })
}

fn required_string<'a>(object: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    object.get(key).and_then(Value::as_str)
}

fn valid_identity(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_IDENTITY_BYTES && !value.chars().any(char::is_control)
}
