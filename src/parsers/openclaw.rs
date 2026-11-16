//! OpenClaw transcript normalization adapted from the pinned Tokscale
//! `openclaw.rs` reader. MIT © 2025 Junho Yeo; see
//! docs/migration/parser-provenance.md.
//!
//! OpenClaw keeps transcripts in two stores that share one event shape
//! (`{"type":"message","id":…,"message":{"role":…,"usage":…}}`):
//!
//! - current installs write per-agent SQLite databases at
//!   `~/.openclaw/agents/<agentId>/agent/openclaw-agent.sqlite`, table
//!   `transcript_events`;
//! - legacy installs wrote one JSONL file per session under
//!   `<agentId>/sessions/`, and current OpenClaw still publishes
//!   `<id>.jsonl.deleted.<ts>` / `<id>.jsonl.reset.<ts>` archives there.
//!
//! This reader supports both stores. The JSONL transcript/archive roles are
//! read through the byte-stream `ingest` path; the per-agent
//! `openclaw-agent.sqlite` `transcript_events` store is read through
//! [`OpenClawReader::ingest_session_event`], which lets the caller supply the
//! row's session id and timestamp. Both feed one interpretation, so the two
//! stores agree event for event.
//!
//! A migrated transcript can exist in both stores at once, so an assistant
//! event with its own id is keyed from the event itself —
//! `openclaw:<event id>:<timestamp>:<input>:<output>` — which makes the SQLite
//! row and the JSONL line of one event agree, and likewise the copies a
//! `/fork` writes under a new session id. An event without a timestamp is
//! keyed by id and usage alone, never by the timestamp a store fills in.
//!
//! OpenClaw also writes assistant rows it authored itself for transcript
//! bookkeeping (`api: "openclaw-transcript"`, provider `openclaw`, model
//! `delivery-mirror` / `gateway-injected`). They are not model output and
//! count nothing.

use super::{CostStatus, ReaderError, ResponseUsage};
use crate::accounting::{SessionKey, TokenCounts};
use serde_json::Value;

pub const MAX_RESPONSES: usize = 16_384;
const MAX_IDENTITY_BYTES: usize = 4_096;
const MAX_RETAINED_IDENTITY_BYTES: usize = 8 * 1024 * 1024;

const TRANSCRIPT_ARTIFACT_API: &str = "openclaw-transcript";
const TRANSCRIPT_ARTIFACT_PROVIDER: &str = "openclaw";
const TRANSCRIPT_ONLY_MODELS: [&str; 2] = ["delivery-mirror", "gateway-injected"];

/// Scope prefix OpenClaw's Codex extension puts on the `idempotencyKey` of
/// every message it mirrors from a Codex app-server thread:
/// `codex-app-server:<thread id>:<turn id>:assistant`.
const CODEX_MIRROR_IDEMPOTENCY_PREFIX: &str = "codex-app-server:";
/// Dedup-key namespace of a row that mirrors a Codex app-server turn.
const CODEX_MIRROR_DEDUP_PREFIX: &str = "openclaw:codex-mirror:";

pub struct OpenClawReader {
    session: String,
    fallback_timestamp_ms: Option<i64>,
    responses: Vec<ResponseUsage>,
    retained_identity_bytes: usize,
    current_model: Option<String>,
    current_provider: Option<String>,
}

impl OpenClawReader {
    pub fn new(session: impl Into<String>) -> Self {
        Self {
            session: session.into(),
            fallback_timestamp_ms: None,
            responses: Vec::new(),
            retained_identity_bytes: 0,
            current_model: None,
            current_provider: None,
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
        let session = self.session.clone();
        self.ingest_event(&session, object, None)
    }

    /// Ingest one `transcript_events` row. `session_id` is the row's session and
    /// `row_created_at_ms` the store's own timestamp, used only when the event
    /// carries none of its own. A session change resets the carried-forward
    /// model/provider, exactly as reading a new JSONL file does.
    pub fn ingest_session_event(
        &mut self,
        session_id: &str,
        row_created_at_ms: Option<i64>,
        event: &serde_json::Map<String, Value>,
    ) -> Result<(), ReaderError> {
        if session_id != self.session {
            if !valid_identity(session_id) {
                return Err(ReaderError::InvalidRecord);
            }
            self.session = session_id.to_owned();
            self.current_model = None;
            self.current_provider = None;
        }
        let session = self.session.clone();
        self.ingest_event(&session, event, row_created_at_ms)
    }

    fn ingest_event(
        &mut self,
        session: &str,
        object: &serde_json::Map<String, Value>,
        row_created_at_ms: Option<i64>,
    ) -> Result<(), ReaderError> {
        let entry_type = object
            .get("type")
            .and_then(Value::as_str)
            .ok_or(ReaderError::InvalidRecord)?;
        match entry_type {
            "model_change" => {
                self.absorb_model(object.get("modelId"), object.get("provider"));
                Ok(())
            }
            "custom" => {
                if object.get("customType").and_then(Value::as_str) != Some("model-snapshot") {
                    return Ok(());
                }
                let data = object.get("data").and_then(Value::as_object);
                self.absorb_model(
                    data.and_then(|data| data.get("modelId")),
                    data.and_then(|data| data.get("provider")),
                );
                Ok(())
            }
            "message" => {
                let message = match object.get("message").and_then(Value::as_object) {
                    Some(message) => message,
                    None => return Ok(()),
                };
                self.ingest_message(session, object.get("id"), message, row_created_at_ms)
            }
            _ => Ok(()),
        }
    }

    pub fn snapshot(&self) -> Vec<ResponseUsage> {
        self.responses.clone()
    }

    pub fn has_supported_usage(&self) -> bool {
        !self.responses.is_empty()
    }

    fn absorb_model(&mut self, model: Option<&Value>, provider: Option<&Value>) {
        if let Some(model) = model.and_then(Value::as_str).and_then(non_empty) {
            self.current_model = Some(model.to_owned());
        }
        if let Some(provider) = provider.and_then(Value::as_str).and_then(non_empty) {
            self.current_provider = Some(provider.to_owned());
        }
    }

    fn ingest_message(
        &mut self,
        session: &str,
        entry_id: Option<&Value>,
        message: &serde_json::Map<String, Value>,
        row_created_at_ms: Option<i64>,
    ) -> Result<(), ReaderError> {
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            return Ok(());
        }
        // Transcript bookkeeping, not a model response.
        if message.get("api").and_then(Value::as_str) == Some(TRANSCRIPT_ARTIFACT_API) {
            return Ok(());
        }
        if message.get("provider").and_then(Value::as_str) == Some(TRANSCRIPT_ARTIFACT_PROVIDER)
            && message
                .get("model")
                .and_then(Value::as_str)
                .is_some_and(|model| TRANSCRIPT_ONLY_MODELS.contains(&model))
        {
            return Ok(());
        }
        let Some(usage) = message.get("usage").and_then(Value::as_object) else {
            return Ok(());
        };

        let message_model = message
            .get("model")
            .and_then(Value::as_str)
            .and_then(non_empty);
        let model = message_model
            .or(self.current_model.as_deref())
            .ok_or(ReaderError::Declined("OpenClaw assistant model"))?
            .to_owned();
        if !valid_identity(&model) {
            return Err(ReaderError::InvalidRecord);
        }
        let message_provider = message
            .get("provider")
            .and_then(Value::as_str)
            .and_then(non_empty);
        let provider = message_provider
            .or(self.current_provider.as_deref())
            .unwrap_or("unknown")
            .to_owned();

        if let Some(message_model) = message_model {
            self.current_model = Some(message_model.to_owned());
        }
        if let Some(message_provider) = message_provider {
            self.current_provider = Some(message_provider.to_owned());
        }

        let message_timestamp = message.get("timestamp").and_then(Value::as_i64);
        let timestamp = message_timestamp
            .or(row_created_at_ms)
            .or(self.fallback_timestamp_ms)
            .ok_or(ReaderError::Declined("OpenClaw assistant timestamp"))?;
        let tokens = self.counts(usage)?;
        if tokens.input == 0
            && tokens.output == 0
            && tokens.cache_read == 0
            && tokens.cache_write == 0
        {
            return Ok(());
        }
        let identity = entry_id
            .and_then(Value::as_str)
            .and_then(non_empty)
            .filter(|id| valid_identity(id))
            .map(|id| {
                dedup_key(
                    id,
                    mirror_turn(message),
                    message_timestamp,
                    tokens.input,
                    tokens.output,
                )
            });
        if let Some(identity) = identity.as_ref() {
            if identity.len() > MAX_IDENTITY_BYTES {
                return Err(ReaderError::Declined("OpenClaw event identity"));
            }
            self.retained_identity_bytes = self
                .retained_identity_bytes
                .checked_add(identity.len())
                .filter(|bytes| *bytes <= MAX_RETAINED_IDENTITY_BYTES)
                .ok_or(ReaderError::Capacity("OpenClaw identities"))?;
        }
        if self.responses.len() == MAX_RESPONSES {
            return Err(ReaderError::Capacity("OpenClaw responses"));
        }
        self.responses.push(ResponseUsage {
            key: SessionKey::new("openclaw", session),
            model,
            tokens,
            cache_write_1h: 0,
            timestamp_ms: timestamp,
            dedup_key: identity,
            billing_provider_hint: Some(provider),
            service_tier: None,
            agent: None,
            workspace_key: None,
            workspace_label: None,
            title: None,
            cost: CostStatus::Unavailable,
        });
        Ok(())
    }

    fn counts(&self, usage: &serde_json::Map<String, Value>) -> Result<TokenCounts, ReaderError> {
        let input = counter(usage, "input")?;
        let output = counter(usage, "output")?;
        let cache_read = counter(usage, "cacheRead")?;
        let cache_write = counter(usage, "cacheWrite")?;
        let reasoning = counter(usage, "reasoningTokens")?;
        if reasoning > output {
            return Err(ReaderError::Unsupported(
                "OpenClaw reasoning exceeds output",
            ));
        }
        Ok(TokenCounts {
            input,
            // `reasoningTokens` is a subset of `output`.
            output: output - reasoning,
            cache_read,
            cache_write,
            reasoning,
        })
    }
}

impl Default for OpenClawReader {
    fn default() -> Self {
        Self::new("openclaw")
    }
}

/// The Codex app-server turn a mirror row names: thread, and turn when present.
fn mirror_turn(message: &serde_json::Map<String, Value>) -> Option<(&str, Option<&str>)> {
    let mut segments = message
        .get("idempotencyKey")
        .and_then(Value::as_str)?
        .strip_prefix(CODEX_MIRROR_IDEMPOTENCY_PREFIX)?
        .split(':');
    let thread = segments.next().filter(|thread| !thread.is_empty())?;
    let turn = segments
        .next()
        .filter(|turn| !turn.is_empty() && *turn != "assistant");
    Some((thread, turn))
}

fn dedup_key(
    event_id: &str,
    mirror: Option<(&str, Option<&str>)>,
    timestamp: Option<i64>,
    input: u64,
    output: u64,
) -> String {
    let identity = match timestamp {
        Some(timestamp) => format!("{event_id}:{timestamp}:{input}:{output}"),
        None => format!("{event_id}:{input}:{output}"),
    };
    match mirror {
        Some((thread, turn)) => {
            format!(
                "{CODEX_MIRROR_DEDUP_PREFIX}{thread}:{}:{identity}",
                turn.unwrap_or("")
            )
        }
        None => format!("openclaw:{identity}"),
    }
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

fn non_empty(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

fn valid_identity(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_IDENTITY_BYTES && !value.chars().any(char::is_control)
}
