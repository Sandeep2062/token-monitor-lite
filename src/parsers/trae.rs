//! Trae usage-API cache, adapted from the pinned reader
//! `crates/tokscale-core/src/sessions/trae.rs`. See
//! docs/migration/parser-provenance.md.
//!
//! Reads `trae-cache/sessions/*.json` — the raw JSON dumped from Trae's usage
//! API, an array with one object per session. The API already returns exact
//! token counts, so nothing here is estimated.
//!
//! # Records that cannot be placed are dropped, not defaulted
//!
//! A record with no `session_id` cannot be deduplicated: every such record would
//! collide on one key. A record with no positive `usage_time` would land at
//! epoch 0. Both are dropped rather than given fabricated placeholders.
//!
//! `usage_time` is epoch **seconds** and is multiplied by 1000. The multiplier
//! is checked, because this cache is untrusted input: a crafted value near
//! `i64::MAX` would panic in debug builds and silently wrap negative in release.
//!
//! # Auto mode
//!
//! Auto-mode sessions come back with an empty `model_name`, because the system
//! picks a model per turn. They are bucketed under `trae-<mode>` so the cost is
//! still attributed instead of disappearing into an empty model cell.
//!
//! Not implemented: `pricing::aliases::resolve_alias` beyond the explicit display
//! name table below (an unknown name passes through unchanged), and
//! `workspace_key` — this cache carries no workspace at all.

use super::{CostStatus, ReaderError, ResponseUsage, jsonl::MAX_RECORD_BYTES};
use crate::accounting::{SessionKey, TokenCounts};
use serde_json::Value;

pub const MAX_RESPONSES: usize = 16_384;
const MAX_IDENTITY_BYTES: usize = 4_096;
const MAX_RETAINED_IDENTITY_BYTES: usize = 8 * 1024 * 1024;

pub const CLIENT: &str = "trae";

/// One Trae usage-API cache file.
pub struct TraeReader {
    fallback_timestamp_ms: i64,
    rows: Vec<ResponseUsage>,
    retained_identity_bytes: usize,
}

impl TraeReader {
    pub fn new(_session: impl Into<String>) -> Self {
        Self {
            fallback_timestamp_ms: 0,
            rows: Vec::new(),
            retained_identity_bytes: 0,
        }
    }

    pub fn with_fallback_timestamp(mut self, timestamp_ms: Option<i64>) -> Self {
        self.fallback_timestamp_ms = timestamp_ms.filter(|value| *value >= 0).unwrap_or(0);
        self
    }

    pub fn ingest(&mut self, document: &[u8]) -> Result<(), ReaderError> {
        if document.len() > MAX_RECORD_BYTES {
            return Err(ReaderError::Capacity("Trae usage cache"));
        }
        self.rows.clear();
        self.retained_identity_bytes = 0;

        let root: Value =
            serde_json::from_slice(document).map_err(|_| ReaderError::InvalidRecord)?;
        // A non-array cache carries nothing this reader understands.
        let Some(sessions) = root.as_array() else {
            return Ok(());
        };
        for session in sessions {
            let Some((session_id, usage_time, tokens, model, provider, cost)) = parse(session)?
            else {
                continue;
            };
            let dedup_key = format!("trae:{session_id}:{usage_time}");
            if dedup_key.len() > MAX_IDENTITY_BYTES {
                return Err(ReaderError::Declined("Trae identity"));
            }
            self.retained_identity_bytes = self
                .retained_identity_bytes
                .checked_add(dedup_key.len())
                .filter(|bytes| *bytes <= MAX_RETAINED_IDENTITY_BYTES)
                .ok_or(ReaderError::Capacity("Trae identities"))?;
            if self.rows.len() == MAX_RESPONSES {
                return Err(ReaderError::Capacity("Trae responses"));
            }
            // `usage_time` is epoch seconds; the checked multiply in `parse`
            // guarantees this cannot overflow.
            let timestamp_ms = usage_time
                .checked_mul(1000)
                .unwrap_or(self.fallback_timestamp_ms);
            self.rows.push(ResponseUsage {
                key: SessionKey::new(CLIENT, session_id),
                model,
                tokens,
                cache_write_1h: 0,
                timestamp_ms,
                dedup_key: Some(dedup_key),
                billing_provider_hint: Some(provider),
                service_tier: None,
                agent: None,
                workspace_key: None,
                workspace_label: None,
                title: None,
                cost: cost
                    .filter(|cost| cost.is_finite() && *cost > 0.0)
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

type Parsed = (String, i64, TokenCounts, String, String, Option<f64>);

fn parse(session: &Value) -> Result<Option<Parsed>, ReaderError> {
    // A record with no real id cannot be deduplicated, and one with no positive
    // `usage_time` would land at epoch 0; both are dropped rather than defaulted.
    let Some(session_id) = session
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty() && id.len() <= MAX_IDENTITY_BYTES)
    else {
        return Ok(None);
    };
    let Some(usage_time) = session
        .get("usage_time")
        .and_then(Value::as_i64)
        .filter(|time| *time > 0)
    else {
        return Ok(None);
    };
    // The cache is untrusted: a crafted value near `i64::MAX` would panic in
    // debug and wrap negative in release.
    if usage_time.checked_mul(1000).is_none() {
        return Ok(None);
    }

    let model_raw = session
        .get("model_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    let mode = session.get("mode").and_then(Value::as_str).unwrap_or("");
    let model = if !model_raw.is_empty() {
        normalize_model(model_raw)
    } else if !mode.is_empty() {
        format!("trae-{}", mode.to_ascii_lowercase())
    } else {
        "trae-unknown".to_owned()
    };
    let provider = provider_for_model(&model).to_owned();

    let extra = session.get("extra_info");
    let counter = |key: &str| -> Result<u64, ReaderError> {
        match extra.and_then(|extra| extra.get(key)) {
            None | Some(Value::Null) => Ok(0),
            Some(value) => value.as_u64().ok_or(ReaderError::Declined("Trae counter")),
        }
    };
    let tokens = TokenCounts {
        input: counter("input_token")?,
        output: counter("output_token")?,
        cache_read: counter("cache_read_token")?,
        cache_write: counter("cache_write_token")?,
        reasoning: 0,
    };
    if tokens.input == 0 && tokens.output == 0 && tokens.cache_read == 0 && tokens.cache_write == 0
    {
        return Ok(None);
    }
    let cost = session.get("dollar_float").and_then(Value::as_f64);
    Ok(Some((
        session_id.to_owned(),
        usage_time,
        tokens,
        model,
        provider,
        cost,
    )))
}

/// Trae's display names mapped to tiktoken-style ids. An unknown name passes
/// through unchanged rather than being guessed at.
fn normalize_model(name: &str) -> String {
    match name {
        "GPT-5.4" => "gpt-5.4",
        "GPT-5.3-Codex" | "GPT-5.3 Codex" => "gpt-5.3-codex",
        "GPT-5.3" => "gpt-5.3",
        "GPT-5.2-Codex" | "GPT-5.2 Codex" => "gpt-5.2-codex",
        "GPT-5.2" => "gpt-5.2",
        "GPT-5.1-Codex" | "GPT-5.1 Codex" => "gpt-5.1-codex",
        "GPT-5.1" => "gpt-5.1",
        "Gemini 3.1 Pro" => "gemini-3.1-pro",
        "Gemini 3.1" => "gemini-3.1",
        "GLM 5.1" | "GLM-5.1" => "glm-5.1",
        "Claude Sonnet 4.6" | "Claude-Sonnet-4.6" => "claude-sonnet-4.6",
        "Claude Sonnet 4.5" | "Claude-Sonnet-4.5" => "claude-sonnet-4.5",
        other => other,
    }
    .to_owned()
}

/// The vendor a display name names, or `trae` for Trae's own routing.
fn provider_for_model(name: &str) -> &'static str {
    if name.contains("GPT") || name.contains("gpt") {
        "openai"
    } else if name.contains("Claude") || name.contains("claude") {
        "anthropic"
    } else if name.contains("Gemini") || name.contains("gemini") {
        "google"
    } else if name.contains("GLM") || name.contains("glm") {
        "zhipu"
    } else {
        "trae"
    }
}
