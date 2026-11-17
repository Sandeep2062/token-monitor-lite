//! BB (`get-bb/bb`) session reader.
//!
//! BB is an agentic IDE and multi-agent orchestrator that keeps sessions as
//! **threads** in a local SQLite database (`~/.bb/bb.db`, `$BB_DATA_DIR/bb.db`,
//! or development instances at `~/.bb-dev/<instance>/bb.db`).
//!
//! Agent turns record a `thread/tokenUsage/updated` event carrying
//! `tokenUsage.last` (this generation) and `tokenUsage.total` (cumulative).
//! `provider/modelFallback` events name a model that later turns actually used.
//!
//! Both Anthropic (disjoint uncached input and cache buckets) and
//! OpenAI/Codex (cached tokens reported *inside* `inputTokens`, reasoning
//! reported *inside* `outputTokens`) conventions appear in the same database,
//! and are normalized to additive buckets without double-counting.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::accounting::{SessionKey, TokenCounts};

use super::{CostStatus, ReaderError, ResponseUsage};

pub const CLIENT: &str = "bb";

/// A thread's resolved metadata, joined in by the caller's query.
#[derive(Debug, Clone, Default)]
pub struct BbThreadMeta {
    pub id: String,
    pub project_id: Option<String>,
    pub project_name: Option<String>,
    pub provider_id: String,
    pub model_override: Option<String>,
    pub default_model: Option<String>,
    pub service_tier: Option<String>,
    pub title: Option<String>,
    pub parent_thread_id: Option<String>,
}

/// One `events` row plus its thread metadata.
#[derive(Debug, Clone)]
pub struct BbRow {
    pub thread_id: String,
    pub turn_id: Option<String>,
    pub sequence: i64,
    pub event_type: String,
    pub data: String,
    pub created_at_ms: Option<i64>,
    pub meta: Option<BbThreadMeta>,
}

/// The `tokenUsage` breakdown as BB stores it.
#[derive(Debug, Default, Clone)]
struct Breakdown {
    total_tokens: Option<i64>,
    input_tokens: Option<i64>,
    cached_input_tokens: Option<i64>,
    cache_read_input_tokens: Option<i64>,
    cache_write_input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    reasoning_output_tokens: Option<i64>,
}

impl Breakdown {
    fn is_zero(&self) -> bool {
        self.total_tokens.unwrap_or(0) == 0
            && self.input_tokens.unwrap_or(0) == 0
            && self.output_tokens.unwrap_or(0) == 0
            && self.cached_input_tokens.unwrap_or(0) == 0
            && self.cache_read_input_tokens.unwrap_or(0) == 0
            && self.cache_write_input_tokens.unwrap_or(0) == 0
            && self.reasoning_output_tokens.unwrap_or(0) == 0
    }

    /// Normalize to additive buckets, plus the separate 1-hour cache tier.
    fn to_counts(&self, prompt_cache_ttl_ms: Option<i64>) -> (TokenCounts, u64) {
        let counter = |v: Option<i64>| v.unwrap_or(0).max(0) as u64;

        let cache_read = counter(self.cache_read_input_tokens.or(self.cached_input_tokens));
        let cache_write = counter(self.cache_write_input_tokens);
        let raw_input = counter(self.input_tokens);
        let raw_output = counter(self.output_tokens);
        let reasoning = counter(self.reasoning_output_tokens);
        let total = self.total_tokens.map(|t| t.max(0) as u64);

        // Codex/OpenAI: `inputTokens` is cumulative and already contains the
        // cache buckets. Only treat it that way when the arithmetic actually
        // proves it — otherwise a genuine Anthropic row would lose its input.
        let uncached_input = if raw_input >= cache_read + cache_write
            && total.is_some_and(|total| total == raw_input + raw_output)
        {
            raw_input - cache_read - cache_write
        } else {
            raw_input
        };

        // Codex/OpenAI: reasoning sits inside `outputTokens`.
        let output = if raw_output >= reasoning
            && total.is_some_and(|total| {
                total == uncached_input + cache_read + cache_write + raw_output
            }) {
            raw_output - reasoning
        } else {
            raw_output
        };

        let cache_write_1h = if prompt_cache_ttl_ms.is_some_and(|ttl| ttl >= 3_600_000) {
            cache_write
        } else {
            0
        };

        (
            TokenCounts {
                input: uncached_input,
                output,
                cache_read,
                cache_write,
                reasoning,
            },
            cache_write_1h,
        )
    }
}

fn json_i64(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_u64().map(|v| v as i64))
        .or_else(|| value.as_f64().map(|v| v as i64))
}

fn read_breakdown(value: &Value) -> Breakdown {
    Breakdown {
        total_tokens: value.get("totalTokens").and_then(json_i64),
        input_tokens: value.get("inputTokens").and_then(json_i64),
        cached_input_tokens: value.get("cachedInputTokens").and_then(json_i64),
        cache_read_input_tokens: value.get("cacheReadInputTokens").and_then(json_i64),
        cache_write_input_tokens: value.get("cacheWriteInputTokens").and_then(json_i64),
        output_tokens: value.get("outputTokens").and_then(json_i64),
        reasoning_output_tokens: value.get("reasoningOutputTokens").and_then(json_i64),
    }
}

#[derive(Debug)]
pub struct BbReader {
    rows: Vec<ResponseUsage>,
    fallback_timestamp: i64,
    current_thread: Option<String>,
    active_fallback_model: Option<String>,
    turn_first_seen: HashMap<String, bool>,
    thread_totals: HashMap<String, TokenCounts>,
    seen: HashSet<String>,
}

impl BbReader {
    pub fn new(fallback_timestamp: i64) -> Self {
        Self {
            rows: Vec::new(),
            fallback_timestamp,
            current_thread: None,
            active_fallback_model: None,
            turn_first_seen: HashMap::new(),
            thread_totals: HashMap::new(),
            seen: HashSet::new(),
        }
    }

    pub fn ingest(&mut self, row: &BbRow) -> Result<(), ReaderError> {
        // The caller orders by (thread_id, sequence), so a change of thread is
        // the boundary after which a fallback model no longer applies.
        if self.current_thread.as_deref() != Some(row.thread_id.as_str()) {
            self.current_thread = Some(row.thread_id.clone());
            self.active_fallback_model = None;
        }

        let data: Value =
            serde_json::from_str(&row.data).map_err(|_| ReaderError::Declined("BB event data"))?;

        if row.event_type == "provider/modelFallback" {
            if let Some(model) = data
                .get("fallbackModel")
                .and_then(Value::as_str)
                .filter(|model| !model.is_empty())
            {
                self.active_fallback_model = Some(model.to_owned());
            }
            return Ok(());
        }

        let Some(usage) = data.get("tokenUsage") else {
            return Ok(());
        };
        let ttl = usage.get("promptCacheTtlMs").and_then(json_i64);

        let (counts, cache_write_1h) = match usage.get("last").map(read_breakdown) {
            Some(last) if !last.is_zero() => {
                if let Some(total) = usage.get("total") {
                    let (total_counts, total_1h) = read_breakdown(total).to_counts(ttl);
                    self.thread_totals
                        .insert(row.thread_id.clone(), total_counts);
                    let _ = total_1h;
                }
                last.to_counts(ttl)
            }
            _ => {
                let Some(total) = usage.get("total").filter(|v| !read_breakdown(v).is_zero())
                else {
                    return Ok(());
                };
                let (current, current_1h) = read_breakdown(total).to_counts(ttl);
                let previous = self
                    .thread_totals
                    .get(&row.thread_id)
                    .copied()
                    .unwrap_or_default();
                let delta = TokenCounts {
                    input: current.input.saturating_sub(previous.input),
                    output: current.output.saturating_sub(previous.output),
                    cache_read: current.cache_read.saturating_sub(previous.cache_read),
                    cache_write: current.cache_write.saturating_sub(previous.cache_write),
                    reasoning: current.reasoning.saturating_sub(previous.reasoning),
                };
                self.thread_totals.insert(row.thread_id.clone(), current);
                (delta, current_1h.saturating_sub(0))
            }
        };

        let total = counts
            .input
            .saturating_add(counts.output)
            .saturating_add(counts.cache_read)
            .saturating_add(counts.cache_write)
            .saturating_add(counts.reasoning);
        if total == 0 {
            return Ok(());
        }

        let timestamp = row
            .created_at_ms
            .filter(|ts| *ts > 0)
            .unwrap_or(self.fallback_timestamp);

        let (provider, model, meta) =
            resolve_provider_and_model(row.meta.as_ref(), self.active_fallback_model.as_deref());

        let is_turn_start = match &row.turn_id {
            Some(turn) => {
                let seen = self.turn_first_seen.entry(turn.clone()).or_insert(false);
                if *seen {
                    false
                } else {
                    *seen = true;
                    true
                }
            }
            None => false,
        };

        // A forked thread replays past turns with fresh event ids but the same
        // `turn_id` and sequence, so identity is built from those.
        let dedup_key = match &row.turn_id {
            Some(turn) => format!(
                "bb:{turn}:{}:{}:{}",
                row.sequence, counts.input, counts.output
            ),
            None => format!(
                "bb:{timestamp}:{}:{}:{}",
                row.sequence, counts.input, counts.output
            ),
        };

        if !self.seen.insert(dedup_key.clone()) {
            return Ok(());
        }

        self.rows.push(ResponseUsage {
            key: SessionKey {
                client: CLIENT.to_owned(),
                session: row.thread_id.clone(),
            },
            model,
            tokens: counts,
            cache_write_1h,
            timestamp_ms: timestamp,
            dedup_key: Some(dedup_key),
            billing_provider_hint: Some(provider),
            service_tier: meta.as_ref().and_then(|m| m.service_tier.clone()),
            agent: Some("BB".to_owned()),
            workspace_key: meta.as_ref().and_then(|m| m.project_id.clone()),
            workspace_label: meta.as_ref().and_then(|m| m.project_name.clone()),
            title: meta.as_ref().and_then(|m| m.title.clone()),
            cost: CostStatus::Unavailable,
        });

        let _ = is_turn_start;
        Ok(())
    }

    pub fn has_supported_usage(&self) -> bool {
        !self.rows.is_empty()
    }

    pub fn snapshot(&self) -> Vec<ResponseUsage> {
        self.rows.clone()
    }

    pub fn reset(&mut self) {
        self.rows.clear();
        self.current_thread = None;
        self.active_fallback_model = None;
        self.turn_first_seen.clear();
        self.thread_totals.clear();
        self.seen.clear();
    }
}

/// `claude-code` and `codex` name BB's *runners*, not vendors, so they are
/// remapped. Anything else falls through to a delimited model-family match and
/// finally to a normalized form of the raw id, which is what the reference
/// does for custom proxies (`custom-proxy` -> `custom_proxy`).
fn resolve_provider_and_model(
    meta: Option<&BbThreadMeta>,
    active_fallback: Option<&str>,
) -> (String, String, Option<BbThreadMeta>) {
    let Some(meta) = meta else {
        let model = active_fallback.unwrap_or("default").to_owned();
        let provider = infer_provider_delimited(&model).unwrap_or_else(|| "unknown".to_owned());
        return (provider, model, None);
    };

    let model = active_fallback
        .map(str::to_owned)
        .or_else(|| meta.model_override.clone())
        .or_else(|| meta.default_model.clone())
        .unwrap_or_else(|| "unknown".to_owned());

    let raw = meta.provider_id.trim();
    let provider = match raw {
        "claude-code" => "anthropic".to_owned(),
        "codex" => "openai".to_owned(),
        other => {
            if is_known_provider(other) {
                other.to_owned()
            } else {
                infer_provider_delimited(&model).unwrap_or_else(|| normalize_provider_id(other))
            }
        }
    };

    (provider, model, Some(meta.clone()))
}

fn is_known_provider(value: &str) -> bool {
    matches!(
        value,
        "anthropic"
            | "openai"
            | "google"
            | "zhipu"
            | "deepseek"
            | "alibaba"
            | "moonshot"
            | "mistral"
            | "xai"
            | "meta"
    )
}

const PROVIDER_TOKENS: [(&str, &str); 8] = [
    ("claude", "anthropic"),
    ("anthropic", "anthropic"),
    ("gpt", "openai"),
    ("codex", "openai"),
    ("gemini", "google"),
    ("glm", "zhipu"),
    ("deepseek", "deepseek"),
    ("qwen", "alibaba"),
];

/// Match on **delimited** tokens so a custom id like `myclaudex-1` is not
/// silently attributed to Anthropic.
fn infer_provider_delimited(model: &str) -> Option<String> {
    for token in model.split(|c: char| !c.is_ascii_alphanumeric()) {
        if token.is_empty() {
            continue;
        }
        let lower = token.to_ascii_lowercase();
        for (needle, provider) in PROVIDER_TOKENS {
            if lower == needle {
                return Some(provider.to_owned());
            }
        }
    }
    None
}

fn normalize_provider_id(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut last_underscore = false;
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_underscore = false;
        } else if !last_underscore {
            out.push('_');
            last_underscore = true;
        }
    }
    let trimmed = out.trim_matches('_').to_owned();
    if trimmed.is_empty() {
        "unknown".to_owned()
    } else {
        trimmed
    }
}
