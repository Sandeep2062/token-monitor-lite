//! Codebuff (formerly Manicode) chat-history reader.
//!
//! Codebuff persists chat history under
//! `~/.config/manicode/projects/<project>/chats/<chatId>/chat-messages.json`.
//! The file is a serialized `ChatMessage[]`; assistant entries carry usage on
//! `metadata.usage`, `metadata.codebuff.usage`, or — for provider-routed calls
//! — the stashed RunState message history under
//! `metadata.runState.sessionState.mainAgentState.messageHistory[*].providerOptions`.
//!
//! Dev and staging channels use the same layout under `manicode-dev` and
//! `manicode-staging`. The `chatId` directory is the chat's ISO-8601 timestamp
//! with the time portion's `:` separators replaced by `-`.

use std::path::Path;

use serde_json::Value;

use crate::accounting::{SessionKey, TokenCounts};

use super::timestamps::{parse_timestamp_str, parse_timestamp_value, tokens_total};
use super::{CostStatus, ReaderError, ResponseUsage};

pub const CLIENT: &str = "codebuff";

const DEFAULT_MODEL: &str = "codebuff-unknown";

const ROLE_KEYS: [&str; 2] = ["variant", "role"];

const INPUT_KEYS: [&str; 4] = [
    "inputTokens",
    "input_tokens",
    "promptTokens",
    "prompt_tokens",
];
const OUTPUT_KEYS: [&str; 4] = [
    "outputTokens",
    "output_tokens",
    "completionTokens",
    "completion_tokens",
];
const CACHE_READ_KEYS: [&str; 4] = [
    "cacheReadInputTokens",
    "cache_read_input_tokens",
    "cachedTokensCreated",
    "cached_tokens_created",
];
const CACHE_WRITE_KEYS: [&str; 4] = [
    "cacheCreationInputTokens",
    "cache_creation_input_tokens",
    "cacheCreationTokens",
    "cache_creation_tokens",
];

/// One assistant message's usage, merged across the three places Codebuff
/// stores it.
#[derive(Default, Debug, Clone)]
struct AssistantUsage {
    model: Option<String>,
    credits: f64,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_input_tokens: i64,
    cache_creation_input_tokens: i64,
}

impl AssistantUsage {
    fn has_signal(&self) -> bool {
        self.input_tokens > 0
            || self.output_tokens > 0
            || self.cache_read_input_tokens > 0
            || self.cache_creation_input_tokens > 0
            || self.credits > 0.0
    }

    /// Fill only the fields that are still unset. The three storage locations
    /// are fallbacks for each other, not additive layers — accumulating them
    /// would double-count the same turn.
    fn merge_fallback(&mut self, other: AssistantUsage) {
        if self.input_tokens <= 0 {
            self.input_tokens = other.input_tokens;
        }
        if self.output_tokens <= 0 {
            self.output_tokens = other.output_tokens;
        }
        if self.cache_read_input_tokens <= 0 {
            self.cache_read_input_tokens = other.cache_read_input_tokens;
        }
        if self.cache_creation_input_tokens <= 0 {
            self.cache_creation_input_tokens = other.cache_creation_input_tokens;
        }
        if self.model.is_none() {
            self.model = other.model;
        }
        if self.credits <= 0.0 {
            self.credits = other.credits;
        }
    }
}

#[derive(Debug)]
pub struct CodebuffReader {
    key: SessionKey,
    fallback_timestamp: Option<i64>,
    rows: Vec<ResponseUsage>,
}

impl CodebuffReader {
    pub fn new(session: impl Into<String>) -> Self {
        Self::new_for(CLIENT, session)
    }

    /// Freebuff persists the byte-identical `chat-messages.json` shape and
    /// parses it through these same helpers, differing only in client id, so
    /// the reader is parameterized rather than duplicated.
    pub fn new_for(client: &str, session: impl Into<String>) -> Self {
        Self {
            key: SessionKey {
                client: client.to_owned(),
                session: session.into(),
            },
            fallback_timestamp: None,
            rows: Vec::new(),
        }
    }

    pub fn with_fallback_timestamp(mut self, timestamp: Option<i64>) -> Self {
        self.fallback_timestamp = timestamp;
        self
    }

    pub fn reset(&mut self) {
        self.rows.clear();
    }

    pub fn has_supported_usage(&self) -> bool {
        !self.rows.is_empty()
    }

    pub fn snapshot(&self) -> Vec<ResponseUsage> {
        self.rows.clone()
    }

    pub fn ingest(&mut self, document: &[u8]) -> Result<(), ReaderError> {
        self.rows.clear();

        let root: Value = serde_json::from_slice(document)
            .map_err(|_| ReaderError::Unsupported("Codebuff chat history"))?;
        let messages = match root.as_array() {
            Some(messages) => messages,
            None => return Ok(()),
        };

        // The chat id doubles as a timestamp when a message carries none: the
        // directory is the chat's start time, filesystem-escaped.
        let chat_id_ts = self
            .key
            .session
            .rsplit('/')
            .next()
            .and_then(parse_chat_id_to_millis)
            .unwrap_or(0);

        for (ordinal, message) in messages.iter().enumerate() {
            if !is_assistant_role(message) {
                continue;
            }
            let usage = extract_assistant_usage(message);
            if !usage.has_signal() {
                continue;
            }

            let timestamp = message_timestamp(message)
                .or(if chat_id_ts > 0 {
                    Some(chat_id_ts)
                } else {
                    None
                })
                .or(self.fallback_timestamp)
                .unwrap_or(0);
            let model = usage
                .model
                .clone()
                .unwrap_or_else(|| DEFAULT_MODEL.to_owned());

            let tokens = TokenCounts {
                input: usage.input_tokens.max(0) as u64,
                output: usage.output_tokens.max(0) as u64,
                cache_read: usage.cache_read_input_tokens.max(0) as u64,
                cache_write: usage.cache_creation_input_tokens.max(0) as u64,
                reasoning: 0,
            };
            if tokens_total(tokens)? == 0 && usage.credits <= 0.0 {
                continue;
            }

            let dedup_key = upstream_message_id(message).unwrap_or_else(|| {
                format!(
                    "codebuff:{}:{}:{}:{}:{}:{}:{}:{}",
                    self.key.session,
                    timestamp,
                    model,
                    ordinal,
                    tokens.input,
                    tokens.output,
                    tokens.cache_read,
                    tokens.cache_write
                )
            });

            self.rows.push(ResponseUsage {
                key: self.key.clone(),
                model,
                tokens,
                cache_write_1h: 0,
                timestamp_ms: timestamp,
                dedup_key: Some(dedup_key),
                billing_provider_hint: provider_hint(&usage, &self.key.session),
                service_tier: None,
                agent: None,
                workspace_key: None,
                workspace_label: None,
                title: None,
                cost: credits_to_cost(usage.credits),
            });
        }

        Ok(())
    }
}

/// `chat-messages.json` holds no provider, so the model family is the only
/// signal. An unknown family stays `None` rather than being labelled.
fn provider_hint(usage: &AssistantUsage, _session: &str) -> Option<String> {
    let model = usage.model.as_deref()?;
    provider_for_model(model)
}

fn provider_for_model(model: &str) -> Option<String> {
    let lower = model.to_ascii_lowercase();
    if lower.contains("claude") || lower.contains("anthropic") {
        Some("anthropic".to_owned())
    } else if lower.contains("gpt")
        || lower.contains("o1")
        || lower.contains("o3")
        || lower.contains("codex")
    {
        Some("openai".to_owned())
    } else if lower.contains("gemini") {
        Some("google".to_owned())
    } else if lower.starts_with("glm") || lower.contains("zhipu") {
        Some("zhipu".to_owned())
    } else if lower.contains("deepseek") {
        Some("deepseek".to_owned())
    } else if lower.contains("qwen") {
        Some("alibaba".to_owned())
    } else if lower.contains("kimi") || lower.contains("moonshot") {
        Some("moonshot".to_owned())
    } else {
        None
    }
}

fn credits_to_cost(credits: f64) -> CostStatus {
    if !credits.is_finite() || credits <= 0.0 {
        return CostStatus::Unavailable;
    }
    let nano = (credits * 1e9).round();
    if nano <= 0.0 || nano >= u64::MAX as f64 {
        return CostStatus::Unavailable;
    }
    CostStatus::Reported(nano as u64)
}

fn is_assistant_role(message: &Value) -> bool {
    for key in ROLE_KEYS {
        if let Some(role) = message.get(key).and_then(Value::as_str) {
            return matches!(role, "ai" | "agent" | "assistant");
        }
    }
    false
}

fn message_timestamp(message: &Value) -> Option<i64> {
    for key in ["timestamp", "createdAt"] {
        if let Some(ts) = message.get(key).and_then(parse_timestamp_value) {
            return Some(ts);
        }
    }
    message
        .get("metadata")
        .and_then(|meta| meta.get("timestamp"))
        .and_then(parse_timestamp_value)
}

fn upstream_message_id(message: &Value) -> Option<String> {
    message
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

fn extract_assistant_usage(message: &Value) -> AssistantUsage {
    let mut usage = AssistantUsage::default();

    if let Some(metadata) = message.get("metadata") {
        if let Some(model) = metadata.get("model").and_then(Value::as_str) {
            usage.model = Some(model.to_owned());
        }
        if let Some(value) = metadata.get("usage") {
            usage.merge_fallback(parse_usage_object(value));
        }
        if let Some(value) = metadata
            .get("codebuff")
            .and_then(|codebuff| codebuff.get("usage"))
        {
            usage.merge_fallback(parse_usage_object(value));
        }
        if let Some(run_state) = extract_usage_from_run_state(metadata) {
            usage.merge_fallback(run_state);
        }
    }

    match message.get("credits").and_then(Value::as_f64) {
        Some(credits) if credits > 0.0 && usage.credits <= 0.0 => usage.credits = credits,
        _ => {}
    }

    usage
}

/// OpenRouter-routed calls land their final counts in the stashed RunState
/// history rather than on `metadata.usage`.
fn extract_usage_from_run_state(metadata: &Value) -> Option<AssistantUsage> {
    let history = metadata
        .get("runState")?
        .get("sessionState")?
        .get("mainAgentState")?
        .get("messageHistory")?
        .as_array()?;

    let mut accumulator = AssistantUsage::default();
    let mut found_any = false;
    for entry in history.iter().rev() {
        if entry.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(options) = entry.get("providerOptions") else {
            continue;
        };
        let mut entry_usage = AssistantUsage::default();
        if let Some(value) = options.get("usage") {
            entry_usage.merge_fallback(parse_usage_object(value));
        }
        if let Some(value) = options.get("codebuff").and_then(|c| c.get("usage")) {
            entry_usage.merge_fallback(parse_usage_object(value));
        }
        if let Some(model) = options
            .get("codebuff")
            .and_then(|c| c.get("model"))
            .and_then(Value::as_str)
        {
            entry_usage.model = Some(model.to_owned());
        }
        if entry_usage.has_signal() || entry_usage.model.is_some() {
            found_any = true;
        }
        accumulator.merge_fallback(entry_usage);
    }
    found_any.then_some(accumulator)
}

/// Different upstreams ship different casings, so both are accepted.
fn parse_usage_object(value: &Value) -> AssistantUsage {
    let mut usage = AssistantUsage {
        input_tokens: pick_number(value, &INPUT_KEYS).unwrap_or(0),
        output_tokens: pick_number(value, &OUTPUT_KEYS).unwrap_or(0),
        cache_read_input_tokens: pick_number(value, &CACHE_READ_KEYS)
            .or_else(|| nested_cached_tokens(value))
            .unwrap_or(0),
        cache_creation_input_tokens: pick_number(value, &CACHE_WRITE_KEYS).unwrap_or(0),
        ..AssistantUsage::default()
    };
    if let Some(credits) = value.get("credits").and_then(Value::as_f64) {
        usage.credits = credits;
    }
    if let Some(model) = value.get("model").and_then(Value::as_str) {
        usage.model = Some(model.to_owned());
    }
    usage
}

fn nested_cached_tokens(value: &Value) -> Option<i64> {
    let details = value
        .get("promptTokensDetails")
        .or_else(|| value.get("prompt_tokens_details"))?;
    details
        .get("cachedTokens")
        .or_else(|| details.get("cached_tokens"))
        .and_then(Value::as_i64)
        .filter(|n| *n > 0)
}

fn pick_number(value: &Value, keys: &[&str]) -> Option<i64> {
    for key in keys {
        // `?` here would abandon the whole list on the first absent key and
        // never reach the snake_case aliases further down.
        let Some(raw) = value.get(*key) else {
            continue;
        };
        let Some(n) = raw
            .as_i64()
            .or_else(|| raw.as_u64().map(|v| v as i64))
            .or_else(|| raw.as_f64().map(|v| v as i64))
        else {
            continue;
        };
        if n > 0 {
            return Some(n);
        }
    }
    None
}

/// Convert a filesystem-safe `chatId` back to epoch milliseconds.
///
/// Only the two separators after the `T` were flipped to `-`; the date
/// portion keeps its own `-` separators, so a blanket replacement would
/// corrupt the date to `2025:12:14T…` and fail RFC 3339 parsing silently.
pub fn parse_chat_id_to_millis(chat_id: &str) -> Option<i64> {
    let t_index = chat_id.find('T')?;
    let (date, time_with_t) = chat_id.split_at(t_index);
    let rebuilt = format!("{date}{}", time_with_t.replacen('-', ":", 2));
    parse_timestamp_str(&rebuilt)
}

/// Derive `(channel, project, chat_id)` from a `chat-messages.json` path, so a
/// malformed layout still yields a deterministic identifier.
pub fn derive_context(path: &Path) -> (String, String, String) {
    let chat_id = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .unwrap_or("unknown")
        .to_owned();

    let chats_dir = path.parent().and_then(Path::parent);
    let project = chats_dir
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .unwrap_or("unknown")
        .to_owned();
    let channel = chats_dir
        .and_then(Path::parent)
        .and_then(Path::parent)
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .unwrap_or("manicode")
        .to_owned();

    (channel, project, chat_id)
}

/// The session identity Codebuff uses: channel, project and chat.
pub fn session_from_path(path: &Path) -> String {
    let (channel, project, chat_id) = derive_context(path);
    format!("{channel}/{project}/{chat_id}")
}
