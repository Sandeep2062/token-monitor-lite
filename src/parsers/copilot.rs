//! GitHub Copilot OTel JSONL normalization adapted from the pinned Tokscale
//! `copilot.rs` reader. MIT © 2025 Junho Yeo; see
//! docs/migration/parser-provenance.md.
//!
//! Supported records are flat OTel JSONL objects with direct `attributes`:
//! chat spans, inference logs, agent-turn logs, or agent summary spans.
//! Cross-line trace enrichment, VS Code session-state stores, agent-relationship
//! merging, and cost are not implemented. A file that mixes sources uses only
//! the highest-priority source present (chat span > inference > agent turn >
//! agent summary) so the same turn is not counted twice.

use super::{CostStatus, ReaderError, ResponseUsage};
use crate::accounting::{SessionKey, TokenCounts};
use chrono::DateTime;
use serde_json::{Map, Value};

pub const MAX_RESPONSES: usize = 16_384;
const MAX_IDENTITY_BYTES: usize = 4_096;
const MAX_RETAINED_IDENTITY_BYTES: usize = 8 * 1024 * 1024;
const MODEL_ATTRS: &[&str] = &["gen_ai.response.model", "gen_ai.request.model"];
const SESSION_ATTRS: &[&str] = &[
    "gen_ai.conversation.id",
    "copilot_chat.session_id",
    "copilot_chat.chat_session_id",
    "session.id",
    "github.copilot.interaction_id",
];

struct PendingRow {
    rank: u8,
    identity: String,
    tokens_total: u64,
    usage: ResponseUsage,
}

pub struct CopilotReader {
    session: String,
    fallback_timestamp_ms: Option<i64>,
    rows: Vec<PendingRow>,
    retained_identity_bytes: usize,
}

impl CopilotReader {
    pub fn new(session: impl Into<String>) -> Self {
        Self {
            session: session.into(),
            fallback_timestamp_ms: None,
            rows: Vec::new(),
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
        let Some(attributes) = object.get("attributes").and_then(Value::as_object) else {
            return Ok(());
        };
        let Some((rank, source)) = usage_source(object, attributes) else {
            return Ok(());
        };
        let input = attr_u64(attributes, &["gen_ai.usage.input_tokens"])?;
        let output = attr_u64(attributes, &["gen_ai.usage.output_tokens"])?;
        let cache_read = attr_u64(
            attributes,
            &[
                "gen_ai.usage.cache_read.input_tokens",
                "gen_ai.usage.cache_read_input_tokens",
            ],
        )?;
        let cache_write = attr_u64(
            attributes,
            &[
                "gen_ai.usage.cache_write.input_tokens",
                "gen_ai.usage.cache_creation.input_tokens",
                "gen_ai.usage.cache_write_input_tokens",
                "gen_ai.usage.cache_creation_input_tokens",
            ],
        )?;
        let reasoning = attr_u64(
            attributes,
            &[
                "gen_ai.usage.reasoning.output_tokens",
                "gen_ai.usage.reasoning_tokens",
            ],
        )?;
        if input == 0 && output == 0 && cache_read == 0 && cache_write == 0 && reasoning == 0 {
            return Ok(());
        }
        let model = first_attr(attributes, MODEL_ATTRS)
            .ok_or(ReaderError::Unsupported("Copilot record model"))?;
        if !valid_identity(model) {
            return Err(ReaderError::InvalidRecord);
        }
        let session = first_attr(attributes, SESSION_ATTRS)
            .or_else(|| object.get("traceId").and_then(Value::as_str))
            .unwrap_or(self.session.as_str());
        if !valid_identity(session) {
            return Err(ReaderError::InvalidRecord);
        }
        let timestamp = record_timestamp(object)
            .or(self.fallback_timestamp_ms)
            .ok_or(ReaderError::Unsupported("Copilot record timestamp"))?;
        let response_id = attributes.get("gen_ai.response.id").and_then(Value::as_str);
        let trace_id = object.get("traceId").and_then(Value::as_str);
        let span_id = object.get("spanId").and_then(Value::as_str);
        let identity = response_id
            .filter(|value| valid_identity(value))
            .map(|value| format!("copilot:{session}:response:{value}"))
            .or_else(|| {
                trace_id
                    .zip(span_id)
                    .map(|(trace, span)| format!("copilot:{session}:span:{trace}:{span}"))
            })
            .unwrap_or_else(|| format!("copilot:{session}:{source}:{}", self.rows.len()));
        if identity.len() > MAX_IDENTITY_BYTES {
            return Err(ReaderError::Unsupported("Copilot record identity"));
        }
        let cache_for_input = cache_read.min(input);
        let input = input
            .checked_sub(cache_for_input)
            .ok_or(ReaderError::Overflow)?;
        let tokens = TokenCounts {
            input,
            output,
            cache_read,
            cache_write,
            reasoning,
        };
        let tokens_total = tokens
            .input
            .checked_add(tokens.output)
            .and_then(|value| value.checked_add(tokens.cache_read))
            .and_then(|value| value.checked_add(tokens.cache_write))
            .and_then(|value| value.checked_add(tokens.reasoning))
            .ok_or(ReaderError::Overflow)?;
        self.retained_identity_bytes = self
            .retained_identity_bytes
            .checked_add(identity.len())
            .filter(|bytes| *bytes <= MAX_RETAINED_IDENTITY_BYTES)
            .ok_or(ReaderError::Capacity("Copilot identities"))?;
        if self.rows.len() == MAX_RESPONSES {
            return Err(ReaderError::Capacity("Copilot responses"));
        }
        self.rows.push(PendingRow {
            rank,
            identity: identity.clone(),
            tokens_total,
            usage: ResponseUsage {
                key: SessionKey::new("copilot", session),
                model: model.to_owned(),
                tokens,
                cache_write_1h: 0,
                timestamp_ms: timestamp,
                dedup_key: Some(identity),
                billing_provider_hint: Some("github-copilot".to_owned()),
                service_tier: None,
                agent: attributes
                    .get("gen_ai.agent.id")
                    .and_then(Value::as_str)
                    .filter(|value| valid_identity(value))
                    .map(str::to_owned),
                cost: CostStatus::Unavailable,
            },
        });
        Ok(())
    }

    pub fn snapshot(&self) -> Vec<ResponseUsage> {
        let Some(rank) = self.rows.iter().map(|row| row.rank).max() else {
            return Vec::new();
        };
        let mut survivors: std::collections::BTreeMap<&str, usize> =
            std::collections::BTreeMap::new();
        let mut keep = vec![true; self.rows.len()];
        for (index, row) in self.rows.iter().enumerate() {
            if row.rank != rank {
                keep[index] = false;
                continue;
            }
            match survivors.get(row.identity.as_str()).copied() {
                Some(current) => {
                    if row.tokens_total > self.rows[current].tokens_total {
                        keep[current] = false;
                        survivors.insert(&row.identity, index);
                    } else {
                        keep[index] = false;
                    }
                }
                None => {
                    survivors.insert(&row.identity, index);
                }
            }
        }
        self.rows
            .iter()
            .zip(keep)
            .filter_map(|(row, keep)| keep.then_some(row.usage.clone()))
            .collect()
    }

    pub fn has_supported_usage(&self) -> bool {
        !self.rows.is_empty()
    }
}

fn usage_source(
    object: &Map<String, Value>,
    attributes: &Map<String, Value>,
) -> Option<(u8, &'static str)> {
    let span = is_span(object);
    let operation = attributes
        .get("gen_ai.operation.name")
        .and_then(Value::as_str);
    let name = object.get("name").and_then(Value::as_str);
    let event = attributes.get("event.name").and_then(Value::as_str);
    let body = record_body(object);
    if span && (operation == Some("chat") || name.is_some_and(|name| name.starts_with("chat "))) {
        return Some((3, "chat"));
    }
    if !span
        && (event == Some("gen_ai.client.inference.operation.details")
            || body.is_some_and(|body| body.starts_with("GenAI inference:")))
    {
        return Some((2, "inference"));
    }
    if !span
        && (event == Some("copilot_chat.agent.turn")
            || body.is_some_and(|body| body.starts_with("copilot_chat.agent.turn")))
    {
        return Some((1, "agent-turn"));
    }
    if span
        && (operation == Some("invoke_agent")
            || name.is_some_and(|name| name.starts_with("invoke_agent ")))
    {
        return Some((0, "agent-summary"));
    }
    None
}

fn is_span(object: &Map<String, Value>) -> bool {
    match object.get("type").and_then(Value::as_str) {
        Some("span") => true,
        Some(_) => false,
        None => {
            let has_name = object.get("name").and_then(Value::as_str).is_some();
            let has_identity = object.get("spanId").and_then(Value::as_str).is_some()
                || object.get("traceId").and_then(Value::as_str).is_some();
            let has_timing = object.get("startTime").is_some()
                || object.get("endTime").is_some()
                || object.get("duration").is_some();
            has_name && (has_identity || has_timing || object.get("kind").is_some())
        }
    }
}

fn record_body(object: &Map<String, Value>) -> Option<&str> {
    match object.get("body") {
        Some(Value::String(value)) => Some(value.as_str()),
        Some(Value::Object(body)) => body.get("stringValue").and_then(Value::as_str),
        _ => None,
    }
}

fn first_attr<'a>(attributes: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|key| {
        attributes
            .get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    })
}

fn attr_u64(attributes: &Map<String, Value>, keys: &[&str]) -> Result<u64, ReaderError> {
    for key in keys {
        match attributes.get(*key) {
            None | Some(Value::Null) => {}
            Some(Value::Number(number)) => {
                return number.as_u64().ok_or(ReaderError::InvalidRecord);
            }
            Some(Value::String(text)) => {
                return text
                    .trim()
                    .parse::<u64>()
                    .map_err(|_| ReaderError::InvalidRecord);
            }
            Some(_) => return Err(ReaderError::InvalidRecord),
        }
    }
    Ok(0)
}

fn record_timestamp(object: &Map<String, Value>) -> Option<i64> {
    if let Some(value) = object.get("timeUnixNano").and_then(Value::as_u64) {
        return i64::try_from(value / 1_000_000)
            .ok()
            .filter(|value| *value > 0);
    }
    for key in ["timestamp", "startTime", "endTime"] {
        if let Some(value) = object.get(key).and_then(timestamp_value) {
            return Some(value);
        }
    }
    None
}

fn timestamp_value(value: &Value) -> Option<i64> {
    if let Some(text) = value.as_str() {
        if let Ok(parsed) = text.trim().parse::<i64>() {
            return (parsed > 0).then_some(parsed);
        }
        return DateTime::parse_from_rfc3339(text.trim())
            .ok()
            .map(|value| value.timestamp_millis())
            .filter(|value| *value > 0);
    }
    value.as_i64().filter(|value| *value > 0).or_else(|| {
        value
            .as_f64()
            .filter(|value| {
                value.is_finite()
                    && *value > 0.0
                    && *value <= i64::MAX as f64
                    && value.fract() == 0.0
            })
            .map(|value| value as i64)
    })
}

fn valid_identity(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_IDENTITY_BYTES && !value.chars().any(char::is_control)
}
