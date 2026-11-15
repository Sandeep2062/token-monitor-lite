//! Shared CodeBuddy/WorkBuddy JSONL normalization adapted from the pinned
//! Tokscale `tencent_buddy.rs`, `codebuddy.rs` and `workbuddy.rs` readers.
//! MIT © 2025 Junho Yeo; see docs/migration/parser-provenance.md.
//!
//! One JSONL object per line. Supported records are completed assistant
//! `message` rows and `function_call` rows carrying usage under
//! `message.usage`, `providerData.usage` or `providerData.rawUsage`. Extension
//! log sinks and WorkBuddy's aggregate SQLite fallback are not implemented.
//! Workspace labels, turn-performance timing and cost stay outside the canonical
//! response.

use super::{CostStatus, ReaderError, ResponseUsage};
use crate::accounting::{SessionKey, TokenCounts};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

pub const MAX_RESPONSES: usize = 16_384;
const MAX_IDENTITY_BYTES: usize = 4_096;
const MAX_RETAINED_IDENTITY_BYTES: usize = 8 * 1024 * 1024;

pub struct BuddyReader {
    client: &'static str,
    default_model: &'static str,
    session: String,
    fallback_timestamp_ms: Option<i64>,
    rows: Vec<ResponseUsage>,
    identities: BTreeMap<String, usize>,
    ordinal: usize,
    retained_identity_bytes: usize,
}

impl BuddyReader {
    pub fn new(
        client: &'static str,
        default_model: &'static str,
        session: impl Into<String>,
    ) -> Self {
        Self {
            client,
            default_model,
            session: session.into(),
            fallback_timestamp_ms: None,
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
        let line_type = object.get("type").and_then(Value::as_str);
        let role = object.get("role").and_then(Value::as_str);
        if line_type == Some("turn-metrics")
            || (line_type == Some("message") && role == Some("user"))
        {
            return Ok(());
        }
        let assistant = line_type == Some("message") && role == Some("assistant");
        let function_call = line_type == Some("function_call");
        if !assistant && !function_call {
            return Ok(());
        }
        if object
            .get("status")
            .and_then(Value::as_str)
            .is_some_and(|status| status != "completed")
        {
            return Ok(());
        }

        let message = object.get("message").and_then(Value::as_object);
        let provider_data = object.get("providerData").and_then(Value::as_object);
        let usage = message
            .and_then(|message| message.get("usage").and_then(Value::as_object))
            .or_else(|| {
                provider_data.and_then(|provider| provider.get("usage").and_then(Value::as_object))
            })
            .or_else(|| {
                provider_data
                    .and_then(|provider| provider.get("rawUsage").and_then(Value::as_object))
            });
        let Some(usage) = usage else {
            return Ok(());
        };
        let Some(tokens) = normalize_usage(usage)? else {
            return Ok(());
        };

        let model = provider_data
            .and_then(|provider| {
                provider
                    .get("model")
                    .and_then(Value::as_str)
                    .or_else(|| provider.get("requestModelId").and_then(Value::as_str))
            })
            .or_else(|| message.and_then(|message| message.get("model").and_then(Value::as_str)))
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .unwrap_or(self.default_model);
        if !valid_identity(model) {
            return Err(ReaderError::InvalidRecord);
        }
        let session = object
            .get("sessionId")
            .and_then(Value::as_str)
            .filter(|session| valid_identity(session))
            .unwrap_or(self.session.as_str());
        let timestamp = object
            .get("timestamp")
            .and_then(Value::as_i64)
            .filter(|timestamp| *timestamp >= 0)
            .or(self.fallback_timestamp_ms)
            .ok_or(ReaderError::Unsupported("Tencent Buddy record timestamp"))?;

        let record_id = provider_data
            .and_then(|provider| {
                provider
                    .get("messageId")
                    .and_then(Value::as_str)
                    .or_else(|| provider.get("traceId").and_then(Value::as_str))
            })
            .or_else(|| object.get("id").and_then(Value::as_str))
            .filter(|id| valid_identity(id));
        let identity = match record_id {
            Some(id) => format!("{}:{session}:{id}", self.client),
            None => format!("{}:{session}:line:{}", self.client, self.ordinal),
        };
        if identity.len() > MAX_IDENTITY_BYTES {
            return Err(ReaderError::Unsupported("Tencent Buddy record identity"));
        }
        self.ordinal += 1;

        let usage_row = ResponseUsage {
            key: SessionKey::new(self.client, session),
            model: model.to_owned(),
            tokens,
            cache_write_1h: 0,
            timestamp_ms: timestamp,
            dedup_key: Some(identity.clone()),
            billing_provider_hint: None,
            service_tier: None,
            agent: None,
            cost: CostStatus::Unavailable,
        };
        let total = tokens_total(tokens)?;
        if let Some(index) = self.identities.get(&identity).copied() {
            if total >= tokens_total(self.rows[index].tokens)? {
                self.rows[index] = usage_row;
            }
            return Ok(());
        }
        if self.rows.len() == MAX_RESPONSES {
            return Err(ReaderError::Capacity("Tencent Buddy responses"));
        }
        self.retained_identity_bytes = self
            .retained_identity_bytes
            .checked_add(identity.len())
            .filter(|bytes| *bytes <= MAX_RETAINED_IDENTITY_BYTES)
            .ok_or(ReaderError::Capacity("Tencent Buddy identities"))?;
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

pub struct BuddyLogReader {
    client: &'static str,
    default_model: &'static str,
    _fallback_session: String,
    fallback_timestamp_ms: Option<i64>,
    rows: Vec<ResponseUsage>,
    ordinal: usize,
}

impl BuddyLogReader {
    pub fn new(
        client: &'static str,
        default_model: &'static str,
        session: impl Into<String>,
    ) -> Self {
        Self {
            client,
            default_model,
            _fallback_session: session.into(),
            fallback_timestamp_ms: None,
            rows: Vec::new(),
            ordinal: 0,
        }
    }

    pub fn with_fallback_timestamp(mut self, timestamp_ms: Option<i64>) -> Self {
        self.fallback_timestamp_ms = timestamp_ms.filter(|value| *value >= 0);
        self
    }

    pub fn ingest(&mut self, record: &[u8]) -> Result<(), ReaderError> {
        let text = std::str::from_utf8(record).map_err(|_| ReaderError::InvalidRecord)?;
        let mut prepared: BTreeMap<String, String> = BTreeMap::new();
        for line in text.lines() {
            if line.contains("[CraftInvokableAgent]") && line.contains("Model prepared:") {
                if let Some((agent, model)) = parse_model_prepared_line(line) {
                    prepared.insert(agent, model);
                }
                continue;
            }
            if !line.contains("[AgentReporter]")
                || !line.contains("Agent execution successful with usage:")
            {
                continue;
            }
            let Some(agent) = bracket_value_after(line, "[AgentReporter]") else {
                continue;
            };
            let Some(usage_text) = line.split("Agent execution successful with usage:").nth(1)
            else {
                continue;
            };
            let usage_text = usage_text.trim();
            let Some(end) = usage_text.rfind('}') else {
                continue;
            };
            let usage: Value = match serde_json::from_str(&usage_text[..=end]) {
                Ok(value) => value,
                Err(_) => continue,
            };
            let Some(usage) = usage.as_object() else {
                continue;
            };
            let Some(tokens) = normalize_usage(usage)? else {
                continue;
            };
            let settled_at = parse_log_timestamp_ms(line)
                .or(self.fallback_timestamp_ms)
                .ok_or(ReaderError::InvalidTimestamp)?;
            let model = prepared
                .get(&agent)
                .map(String::as_str)
                .unwrap_or(self.default_model);
            if !valid_identity(&agent) || !valid_identity(model) {
                return Err(ReaderError::InvalidRecord);
            }
            if self.rows.len() == MAX_RESPONSES {
                return Err(ReaderError::Capacity("Tencent Buddy log responses"));
            }
            let dedup_second = settled_at.div_euclid(1000);
            let identity = format!(
                "{}:extension-log:{agent}:{dedup_second}:{}:{}:{}:{}:{}",
                self.client,
                tokens.input,
                tokens.output,
                tokens.cache_read,
                tokens.cache_write,
                tokens.reasoning
            );
            self.ordinal += 1;
            self.rows.push(ResponseUsage {
                key: SessionKey::new(self.client, agent.as_str()),
                model: model.to_owned(),
                tokens,
                cache_write_1h: 0,
                timestamp_ms: settled_at,
                dedup_key: Some(identity),
                billing_provider_hint: None,
                service_tier: None,
                agent: Some(agent),
                cost: CostStatus::Unavailable,
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

fn parse_model_prepared_line(line: &str) -> Option<(String, String)> {
    let agent = bracket_value_after(line, "[CraftInvokableAgent]")?;
    let after = line.split("Model prepared:").nth(1)?.trim();
    let model = after
        .rsplit_once('(')
        .and_then(|(_, tail)| tail.split_once(')').map(|(model, _)| model.trim()))
        .filter(|model| !model.is_empty())
        .unwrap_or(after)
        .to_owned();
    (!model.is_empty()).then_some((agent, model))
}

fn bracket_value_after(line: &str, marker: &str) -> Option<String> {
    let after = line.split(marker).nth(1)?;
    let start = after.find('[')?;
    let rest = &after[start + 1..];
    let end = rest.find(']')?;
    let value = rest[..end].trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn parse_log_timestamp_ms(line: &str) -> Option<i64> {
    let raw = if let Some(rest) = line.strip_prefix('[') {
        rest.split_once(']')?.0.trim()
    } else {
        line.split_once(" [")
            .map(|(timestamp, _)| timestamp.trim())
            .unwrap_or(line.trim())
    };
    let (date, time) = raw.split_once(' ')?;
    let separator = if date.contains('/') { '/' } else { '-' };
    let parts = date
        .split(separator)
        .filter_map(|part| part.parse::<u32>().ok())
        .collect::<Vec<_>>();
    if parts.len() != 3 {
        return super::timestamps::parse_timestamp_str(raw);
    }
    let normalized = format!("{:04}-{:02}-{:02} {}", parts[0], parts[1], parts[2], time);
    for format in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%d %H:%M:%S"] {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(&normalized, format) {
            return Some(naive.and_utc().timestamp_millis());
        }
    }
    super::timestamps::parse_timestamp_str(&normalized)
}

pub(crate) fn normalize_usage(
    usage: &Map<String, Value>,
) -> Result<Option<TokenCounts>, ReaderError> {
    let cache_read = first_positive(
        usage,
        &[
            "cache_read_input_tokens",
            "cacheReadInputTokens",
            "cacheTokens",
            "prompt_cache_hit_tokens",
            "cached_tokens",
        ],
    )?;
    let cache_write = first_positive(
        usage,
        &[
            "cache_creation_input_tokens",
            "cacheCreationInputTokens",
            "cachedWriteTokens",
            "prompt_cache_write_tokens",
        ],
    )?;
    let output = first_present(
        usage,
        &["output_tokens", "outputTokens", "completion_tokens"],
    )?;
    let reasoning = first_present(
        usage,
        &[
            "completion_thinking_tokens",
            "completionThinkingTokens",
            "reasoning_tokens",
        ],
    )?;
    let miss_input = first_present(usage, &["cachedMissTokens", "cacheMissTokens"])?;
    let reported_input = first_present(usage, &["input_tokens", "inputTokens", "prompt_tokens"])?;
    let total = first_optional(usage, &["total_tokens", "totalTokens"])?;

    let input =
        if option_present(usage, "cachedMissTokens") || option_present(usage, "cacheMissTokens") {
            miss_input
        } else if let Some(total) = total {
            let inclusive = reported_input
                .checked_add(output)
                .ok_or(ReaderError::Overflow)?;
            let exclusive = inclusive
                .checked_add(cache_read)
                .and_then(|value| value.checked_add(cache_write))
                .and_then(|value| value.checked_add(reasoning))
                .ok_or(ReaderError::Overflow)?;
            if cache_read > 0 && total == inclusive && inclusive != exclusive {
                reported_input
                    .checked_sub(cache_read)
                    .ok_or(ReaderError::Unsupported(
                        "Tencent Buddy cache exceeds input",
                    ))?
            } else {
                reported_input
            }
        } else {
            reported_input
        };

    let tokens = TokenCounts {
        input,
        output,
        cache_read,
        cache_write,
        reasoning,
    };
    Ok((tokens_total(tokens)? > 0).then_some(tokens))
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

fn counter(usage: &Map<String, Value>, key: &str) -> Result<Option<u64>, ReaderError> {
    match usage.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value.as_u64().map(Some).ok_or(ReaderError::InvalidRecord),
    }
}

fn first_optional(usage: &Map<String, Value>, keys: &[&str]) -> Result<Option<u64>, ReaderError> {
    for key in keys {
        if let Some(value) = counter(usage, key)? {
            return Ok(Some(value));
        }
    }
    Ok(None)
}

fn first_present(usage: &Map<String, Value>, keys: &[&str]) -> Result<u64, ReaderError> {
    for key in keys {
        if let Some(value) = counter(usage, key)? {
            return Ok(value);
        }
    }
    Ok(0)
}

fn first_positive(usage: &Map<String, Value>, keys: &[&str]) -> Result<u64, ReaderError> {
    let mut first = None;
    for key in keys {
        if let Some(value) = counter(usage, key)? {
            if value > 0 {
                return Ok(value);
            }
            first.get_or_insert(value);
        }
    }
    Ok(first.unwrap_or(0))
}

fn option_present(usage: &Map<String, Value>, key: &str) -> bool {
    usage.get(key).is_some_and(|value| !value.is_null())
}

pub fn valid_identity(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_IDENTITY_BYTES && !value.chars().any(char::is_control)
}
