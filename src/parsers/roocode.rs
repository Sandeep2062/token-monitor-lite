//! Task-log usage for the Cline family, read from the VS Code `globalStorage`
//! task tree.
//!
//! Three clients write the same on-disk shape, because Roo Code and Kilo Code
//! are forks of Cline:
//!
//! | client | root under `…/User/globalStorage` |
//! | --- | --- |
//! | `roocode` | `rooveterinaryinc.roo-cline/tasks` |
//! | `cline` | `saoudrizwan.claude-dev/tasks` |
//! | `kilo` | `kilocode.kilo-code/tasks` |
//!
//! Each keeps one directory per task at `<root>/<taskId>/`. Two files are
//! involved, and only one of them carries usage:
//!
//! - `api_conversation_history.json` is an Anthropic `MessageParam[]` — the
//!   provider-facing conversation. It has **no token counters at all**, so it
//!   contributes nothing and is deliberately not read.
//! - `ui_messages.json` is a `ClineMessage[]` and is the whole source. Each
//!   completed API request appends a `say: "api_req_started"` message whose
//!   `text` is a JSON string holding `tokensIn`, `tokensOut`, `cacheWrites`,
//!   `cacheReads` and `cost`.
//!
//! So this is a **single-file** reader. The earlier "two files per task" note
//! was wrong about which file carries usage; see
//! `docs/migration/source-groups.md`.
//!
//! The shape is verified against Roo Code (`tests/roocode.rs`). `cline` and
//! `kilo` reuse it on the strength of the fork lineage, and because the failure
//! mode of a wrong assumption here is *no rows* rather than wrong rows: a
//! payload without the expected counters is skipped.
//!
//! # Accounting
//!
//! `tokensIn` is **gross**: Roo computes it as `input + cacheWrites +
//! cacheReads` for the Anthropic protocol and as the raw provider
//! `input_tokens` (which already includes cached tokens) for OpenAI. Cache is
//! therefore subtracted once, exactly as the Cline CLI reader does. A row whose
//! cache exceeds `tokensIn` is refused rather than clamped, because clamping
//! silently under-counts.
//!
//! # Model
//!
//! A task's model lives in Roo's provider profile settings, not in the task
//! directory, so this reader cannot resolve it. Every row is reported as
//! [`UNKNOWN_MODEL`] rather than guessing from a neighbouring task: the
//! codebase's rule is that an unresolvable model is left visible, never
//! invented. `cost` **is** recorded verbatim, so spend stays correct even
//! though the per-model price cannot be attributed.
//!
//! # Not implemented
//!
//! `condense_context` cost rows, subtask task-stack linkage, and Roo's own
//! `provider` profile resolution.

use super::{CostStatus, ReaderError, ResponseUsage};
use crate::accounting::{SessionKey, TokenCounts};
use serde_json::Value;

pub const CLIENT: &str = "roocode";
pub const CLINE_CLIENT: &str = "cline";
pub const KILO_CLIENT: &str = "kilo";
pub const UNKNOWN_MODEL: &str = "unknown";

pub const MAX_RESPONSES: usize = 16_384;
const MAX_IDENTITY_BYTES: usize = 4_096;
const MAX_RETAINED_IDENTITY_BYTES: usize = 8 * 1024 * 1024;

/// One task's `ui_messages.json`.
pub struct RooCodeReader {
    client: &'static str,
    session: String,
    fallback_timestamp_ms: Option<i64>,
    responses: Vec<ResponseUsage>,
    retained_identity_bytes: usize,
}

impl RooCodeReader {
    pub fn new(session: impl Into<String>) -> Self {
        Self::with_client(CLIENT, session)
    }

    pub fn with_client(client: &'static str, session: impl Into<String>) -> Self {
        Self {
            client,
            session: session.into(),
            fallback_timestamp_ms: None,
            responses: Vec::new(),
            retained_identity_bytes: 0,
        }
    }

    pub fn with_fallback_timestamp(mut self, timestamp_ms: Option<i64>) -> Self {
        self.fallback_timestamp_ms = timestamp_ms.filter(|value| *value >= 0);
        self
    }

    pub fn ingest(&mut self, document: &[u8]) -> Result<(), ReaderError> {
        let root: Value =
            serde_json::from_slice(document).map_err(|_| ReaderError::InvalidRecord)?;
        let messages = root
            .as_array()
            .ok_or(ReaderError::Unsupported("task-log ui_messages"))?;
        for (ordinal, message) in messages.iter().enumerate() {
            self.ingest_message(message, ordinal)?;
        }
        Ok(())
    }

    fn ingest_message(&mut self, message: &Value, ordinal: usize) -> Result<(), ReaderError> {
        let object = message.as_object().ok_or(ReaderError::InvalidRecord)?;
        // `api_req_started` is always a `say`; an `ask` of the same name is not
        // a usage record.
        if object.get("type").and_then(Value::as_str) != Some("say")
            || object.get("say").and_then(Value::as_str) != Some("api_req_started")
        {
            return Ok(());
        }
        // The payload is a JSON *string*, not an object.
        let Some(text) = object.get("text").and_then(Value::as_str) else {
            return Ok(());
        };
        let Ok(payload) = serde_json::from_str::<Value>(text) else {
            return Err(ReaderError::Declined("task-log request payload"));
        };
        let Some(payload) = payload.as_object() else {
            return Ok(());
        };
        let reported_input = optional_u64(payload, "tokensIn")?;
        let output = optional_u64(payload, "tokensOut")?;
        let cache_write = optional_u64(payload, "cacheWrites")?;
        let cache_read = optional_u64(payload, "cacheReads")?;
        let cached = cache_write
            .checked_add(cache_read)
            .ok_or(ReaderError::Overflow)?;
        // `tokensIn` is gross of cache in both protocols, so the overlap is
        // removed once. A row that claims more cache than input is a shape this
        // reader refuses rather than clamps.
        let input = reported_input
            .checked_sub(cached)
            .ok_or(ReaderError::Declined("task-log cache/input"))?;
        if reported_input == 0 && output == 0 && cached == 0 {
            return Ok(());
        }

        let timestamp_ms = object
            .get("ts")
            .and_then(Value::as_i64)
            .or(self.fallback_timestamp_ms)
            .ok_or(ReaderError::Declined("task-log message timestamp"))?;
        let dedup_key = format!("{}:{}:{timestamp_ms}:{ordinal}", self.client, self.session);
        if dedup_key.len() > MAX_IDENTITY_BYTES {
            return Err(ReaderError::Declined("task-log identity"));
        }
        self.retained_identity_bytes = self
            .retained_identity_bytes
            .checked_add(dedup_key.len())
            .filter(|bytes| *bytes <= MAX_RETAINED_IDENTITY_BYTES)
            .ok_or(ReaderError::Capacity("task-log identities"))?;
        if self.responses.len() == MAX_RESPONSES {
            return Err(ReaderError::Capacity("task-log responses"));
        }

        self.responses.push(ResponseUsage {
            key: SessionKey::new(self.client, &self.session),
            model: UNKNOWN_MODEL.to_owned(),
            tokens: TokenCounts {
                input,
                output,
                cache_read,
                cache_write,
                reasoning: 0,
            },
            cache_write_1h: 0,
            timestamp_ms,
            dedup_key: Some(dedup_key),
            billing_provider_hint: None,
            service_tier: None,
            agent: None,
            workspace_key: None,
            workspace_label: None,
            title: None,
            // The task log records a cost, so spend is exact even though the
            // model — and therefore the price — is not resolvable here.
            cost: cost(payload.get("cost")),
        });
        Ok(())
    }

    pub fn snapshot(&self) -> Vec<ResponseUsage> {
        self.responses.clone()
    }

    pub fn has_supported_usage(&self) -> bool {
        !self.responses.is_empty()
    }
}

/// Roo writes a decimal USD figure. Nano-USD keeps it integral.
fn cost(value: Option<&Value>) -> CostStatus {
    let Some(value) = value else {
        return CostStatus::Unavailable;
    };
    let Some(usd) = value.as_f64() else {
        return CostStatus::Unavailable;
    };
    if !usd.is_finite() || usd < 0.0 {
        return CostStatus::Unavailable;
    }
    match (usd * 1_000_000_000.0).round() {
        nanos if nanos <= u64::MAX as f64 => CostStatus::Reported(nanos as u64),
        _ => CostStatus::Unavailable,
    }
}

fn optional_u64(object: &serde_json::Map<String, Value>, key: &str) -> Result<u64, ReaderError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(0),
        Some(value) => value.as_u64().ok_or(ReaderError::InvalidRecord),
    }
}
