//! Droid (Factory.ai) session normalization adapted from the pinned Tokscale
//! `droid.rs` reader. MIT © 2025 Junho Yeo; see
//! docs/migration/parser-provenance.md.
//!
//! Droid writes one `<uuid>.settings.json` per session under
//! `~/.factory/sessions/`, carrying the session's cumulative `tokenUsage`
//! totals. Its `.jsonl` transcript beside that file records *when* each
//! assistant reply happened but never *how much* it cost, so the pinned
//! reader apportions the cumulative totals across replies by context and
//! output byte weight.
//!
//! That apportioning is a deliberate refinement, not a correctness
//! requirement: the pinned reader itself falls back to a single record
//! carrying the identical total when the transcript is missing, unreadable,
//! or has no replies yet. The native reader always takes that single-record
//! path: it reports the session's recorded cumulative totals once, anchored
//! at the last write. Token magnitudes are therefore identical; only the
//! per-reply date distribution differs, which is recorded as unsupported
//! coverage rather than guessed from byte weights.

use super::{CostStatus, ReaderError, ResponseUsage};
use crate::accounting::{SessionKey, TokenCounts};
use chrono::DateTime;
use serde_json::Value;

pub const MAX_RESPONSES: usize = 16_384;
const MAX_IDENTITY_BYTES: usize = 4_096;

/// Model families Droid can record, used only to name the provider when the
/// session carries no `providerLock`.
const PROVIDER_FAMILIES: [(&str, &str); 8] = [
    ("claude", "anthropic"),
    ("opus", "anthropic"),
    ("sonnet", "anthropic"),
    ("haiku", "anthropic"),
    ("gpt", "openai"),
    ("o1", "openai"),
    ("o3", "openai"),
    ("gemini", "google"),
];

pub struct DroidReader {
    session: String,
    fallback_timestamp_ms: Option<i64>,
    responses: Vec<ResponseUsage>,
}

impl DroidReader {
    pub fn new(session: impl Into<String>) -> Self {
        Self {
            session: session.into(),
            fallback_timestamp_ms: None,
            responses: Vec::new(),
        }
    }

    pub fn with_fallback_timestamp(mut self, timestamp_ms: Option<i64>) -> Self {
        self.fallback_timestamp_ms = timestamp_ms.filter(|value| *value >= 0);
        self
    }

    /// Read one `<uuid>.settings.json` document. A session contributes at most
    /// one observation: its cumulative usage.
    pub fn ingest(&mut self, document: &[u8]) -> Result<(), ReaderError> {
        let response = self.parse(document)?;
        if let Some(response) = response {
            if self.responses.len() == MAX_RESPONSES {
                return Err(ReaderError::Capacity("Droid responses"));
            }
            self.responses.push(response);
        }
        Ok(())
    }

    fn parse(&self, document: &[u8]) -> Result<Option<ResponseUsage>, ReaderError> {
        let value: Value =
            serde_json::from_slice(document).map_err(|_| ReaderError::InvalidRecord)?;
        let settings = value.as_object().ok_or(ReaderError::InvalidRecord)?;
        let Some(usage) = settings.get("tokenUsage").and_then(Value::as_object) else {
            return Ok(None);
        };

        let input = counter(usage, "inputTokens")?;
        let output = counter(usage, "outputTokens")?;
        let cache_write = counter(usage, "cacheCreationTokens")?;
        let cache_read = counter(usage, "cacheReadTokens")?;
        let reasoning = counter(usage, "thinkingTokens")?;
        if input == 0 && output == 0 && cache_write == 0 && cache_read == 0 && reasoning == 0 {
            return Ok(None);
        }

        let model = settings
            .get("model")
            .and_then(Value::as_str)
            .map(normalize_model)
            .filter(|model| !model.is_empty())
            .ok_or(ReaderError::Unsupported("Droid session model"))?;
        if model.len() > MAX_IDENTITY_BYTES || model.chars().any(char::is_control) {
            return Err(ReaderError::InvalidRecord);
        }
        let provider_lock = settings
            .get("providerLock")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let provider = provider_lock
            .map(str::to_owned)
            .unwrap_or_else(|| provider_for_model(&model).to_owned());

        // `providerLockTimestamp` records when the provider was *selected*, not
        // when the tokens were spent, so the file's mtime is the better anchor.
        // It still floors the result: usage cannot predate provider selection,
        // so a rewound mtime cannot drag the record earlier than the session
        // could have run.
        let lock = settings
            .get("providerLockTimestamp")
            .and_then(Value::as_str)
            .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
            .map(|value| value.timestamp_millis())
            .filter(|value| *value > 0);
        let timestamp = match (self.fallback_timestamp_ms, lock) {
            (Some(modified), Some(lock)) => modified.max(lock),
            (Some(modified), None) => modified,
            (None, Some(lock)) => lock,
            (None, None) => return Err(ReaderError::Unsupported("Droid session timestamp")),
        };

        Ok(Some(ResponseUsage {
            key: SessionKey::new("droid", self.session.clone()),
            model,
            tokens: TokenCounts {
                input,
                output,
                cache_read,
                cache_write,
                reasoning,
            },
            cache_write_1h: 0,
            timestamp_ms: timestamp,
            dedup_key: Some(format!("droid:{}", self.session)),
            billing_provider_hint: Some(provider),
            service_tier: None,
            agent: None,
            workspace_key: None,
            workspace_label: None,
            title: None,
            cost: CostStatus::Unavailable,
        }))
    }

    pub fn snapshot(&self) -> Vec<ResponseUsage> {
        self.responses.clone()
    }

    pub fn has_supported_usage(&self) -> bool {
        !self.responses.is_empty()
    }
}

impl Default for DroidReader {
    fn default() -> Self {
        Self::new("droid")
    }
}

/// Droid's custom model spelling: `custom:Claude-Opus-4.5-Thinking-[Anthropic]-0`.
/// The `custom:` prefix and any `[...]` vendor annotation are dropped, trailing
/// hyphens are trimmed, the result is lowercased, dots become hyphens, and runs
/// of hyphens collapse — exactly the pinned reader's normalization, since the
/// normalized id is what pricing and provider inference key on.
fn normalize_model(model: &str) -> String {
    let stripped = model.strip_prefix("custom:").unwrap_or(model);
    let mut unbracketed = String::with_capacity(stripped.len());
    let mut in_bracket = false;
    for character in stripped.chars() {
        match character {
            '[' => in_bracket = true,
            ']' => in_bracket = false,
            _ if !in_bracket => unbracketed.push(character),
            _ => {}
        }
    }
    let normalized = unbracketed
        .trim()
        .trim_end_matches('-')
        .to_ascii_lowercase()
        .replace('.', "-");
    let mut collapsed = String::with_capacity(normalized.len());
    let mut last_was_hyphen = false;
    for character in normalized.chars() {
        if character == '-' {
            if !last_was_hyphen {
                collapsed.push(character);
            }
            last_was_hyphen = true;
        } else {
            collapsed.push(character);
            last_was_hyphen = false;
        }
    }
    collapsed
}

fn provider_for_model(model: &str) -> &'static str {
    PROVIDER_FAMILIES
        .iter()
        .find(|(prefix, _)| model.starts_with(prefix))
        .map_or("unknown", |(_, provider)| *provider)
}

fn counter(usage: &serde_json::Map<String, Value>, field: &str) -> Result<u64, ReaderError> {
    match usage.get(field) {
        None | Some(Value::Null) => Ok(0),
        Some(value) => value
            .as_i64()
            .and_then(|value| u64::try_from(value).ok())
            .ok_or(ReaderError::InvalidRecord),
    }
}
