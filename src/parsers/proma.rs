//! Proma Agent session normalization adapted from the pinned Tokscale
//! `proma.rs` reader. MIT © 2025 Junho Yeo; see
//! docs/migration/parser-provenance.md.
//!
//! Proma persists usage in three unrelated shapes, and its own reader picks
//! between them per file:
//!
//! - `agent-sessions/*.jsonl` display journals, which mirror a native session;
//! - `sdk-config/sessions` and `sdk-config/projects` legacy Claude SDK
//!   transcripts, whose `assistant` records carry `message.usage`;
//! - native Pi session files, which are Pi's own `type: "session"` format.
//!
//! A Pi-format file is recognised by its first line being a `session` record,
//! exactly as the pinned reader does, and is handed to the Pi reader under the
//! `proma` client. Everything else is treated as an SDK transcript. This keeps
//! one parser per shape without duplicating Pi's rules.
//!
//! Not implemented, and recorded as unsupported coverage rather than guessed:
//!
//! - cross-file reconciliation. The pinned reader loads every source, reads a
//!   sibling `agent-sessions.json` index, then drops display-journal copies in
//!   favour of their native sessions and de-duplicates copied fork history by
//!   `(uuid, response id)`. That is a whole-root operation over several files
//!   and an index document, which a per-source byte reader cannot perform. Each
//!   file is therefore reported independently, and no content or token-count
//!   fingerprint is invented as an identity.
//! - provider inference from the model name. The pinned reader guesses via
//!   `inferred_provider_from_model`; the native reader keeps only an explicit
//!   `provider`/`provider_id`.
//! - fork-era archive rows, which the Token Monitor fork marks on the session
//!   archive side rather than in the reader.

use super::{CostStatus, ReaderError, ResponseUsage, jsonl::MAX_RECORD_BYTES, pi::PiReader};
use crate::accounting::{SessionKey, TokenCounts};
use serde_json::{Map, Value};

pub const MAX_RESPONSES: usize = 16_384;
const MAX_IDENTITY_BYTES: usize = 4_096;
const MAX_RETAINED_IDENTITY_BYTES: usize = 8 * 1024 * 1024;

pub const CLIENT: &str = "proma";
/// Proma routes to whatever the user configured, so there is no single
/// provider. Usage is attributed to Proma only when nothing more specific is
/// recorded.
pub const DEFAULT_PROVIDER: &str = "proma";
const UNKNOWN_MODEL: &str = "unknown";

/// One Proma source file.
pub struct PromaReader {
    /// Set once the first record proves this is a native Pi session.
    pi: Option<Box<PiReader>>,
    sdk: Option<SdkState>,
    fallback_session: String,
    fallback_timestamp_ms: Option<i64>,
    retained_identity_bytes: usize,
}

#[derive(Default)]
struct SdkState {
    responses: Vec<ResponseUsage>,
}

impl PromaReader {
    pub fn new(session: impl Into<String>) -> Self {
        Self {
            pi: None,
            sdk: None,
            fallback_session: session.into(),
            fallback_timestamp_ms: None,
            retained_identity_bytes: 0,
        }
    }

    pub fn with_fallback_timestamp(mut self, timestamp_ms: Option<i64>) -> Self {
        self.fallback_timestamp_ms = timestamp_ms.filter(|value| *value >= 0);
        self
    }

    /// Ingest one line.
    ///
    /// The first record decides the shape. A `type: "session"` record means the
    /// whole file is Pi-format and is delegated; anything else is an SDK
    /// transcript.
    pub fn ingest(&mut self, record: &[u8]) -> Result<(), ReaderError> {
        if record.len() > MAX_RECORD_BYTES {
            return Err(ReaderError::Capacity("Proma line"));
        }
        if self.pi.is_none() && self.sdk.is_none() {
            let value: Value =
                serde_json::from_slice(record).map_err(|_| ReaderError::InvalidRecord)?;
            let object = value.as_object().ok_or(ReaderError::InvalidRecord)?;
            if object.get("type").and_then(Value::as_str) == Some("session") {
                let mut pi = PiReader::new(CLIENT, self.fallback_session.clone())
                    .with_fallback_timestamp(self.fallback_timestamp_ms);
                pi.ingest(record)?;
                self.pi = Some(Box::new(pi));
                return Ok(());
            }
            self.sdk = Some(SdkState {
                responses: Vec::new(),
            });
        }
        if let Some(pi) = self.pi.as_mut() {
            return pi.ingest(record);
        }
        let value: Value =
            serde_json::from_slice(record).map_err(|_| ReaderError::InvalidRecord)?;
        let Some(object) = value.as_object() else {
            return Err(ReaderError::InvalidRecord);
        };
        let Some(usage) = normalize_sdk_record(
            object,
            &self.fallback_session,
            self.fallback_timestamp_ms,
            self.pending_ordinal(),
        )?
        else {
            return Ok(());
        };
        self.push_sdk(usage)
    }

    fn pending_ordinal(&self) -> usize {
        self.sdk.as_ref().map_or(0, |sdk| sdk.responses.len())
    }

    fn push_sdk(&mut self, usage: ResponseUsage) -> Result<(), ReaderError> {
        let sdk = self
            .sdk
            .as_mut()
            .expect("SDK state is initialized before the first SDK record");
        if sdk.responses.len() >= MAX_RESPONSES {
            return Err(ReaderError::Capacity("Proma responses"));
        }
        let identity_bytes = usage.dedup_key.as_ref().map_or(0, String::len);
        self.retained_identity_bytes = self
            .retained_identity_bytes
            .checked_add(identity_bytes)
            .filter(|bytes| *bytes <= MAX_RETAINED_IDENTITY_BYTES)
            .ok_or(ReaderError::Capacity("Proma identities"))?;
        sdk.responses.push(usage);
        Ok(())
    }

    pub fn snapshot(&self) -> Vec<ResponseUsage> {
        if let Some(pi) = self.pi.as_ref() {
            return pi.snapshot();
        }
        self.sdk
            .as_ref()
            .map(|sdk| sdk.responses.clone())
            .unwrap_or_default()
    }

    pub fn has_supported_usage(&self) -> bool {
        if let Some(pi) = self.pi.as_ref() {
            return pi.has_supported_usage();
        }
        self.sdk
            .as_ref()
            .is_some_and(|sdk| !sdk.responses.is_empty())
    }
}

/// Normalize one legacy SDK `assistant` record.
///
/// `result` records are session aggregates, not another API request, so only
/// `assistant` lines are usage. A `<synthetic>` model marks a locally
/// constructed message that was never billed.
fn normalize_sdk_record(
    record: &Map<String, Value>,
    fallback_session: &str,
    fallback_timestamp_ms: Option<i64>,
    ordinal: usize,
) -> Result<Option<ResponseUsage>, ReaderError> {
    if record.get("type").and_then(Value::as_str) != Some("assistant") {
        return Ok(None);
    }
    let Some(body) = record.get("message").and_then(Value::as_object) else {
        return Ok(None);
    };
    let Some(usage) = body.get("usage").filter(|value| value.is_object()) else {
        return Ok(None);
    };
    let usage = usage.as_object().ok_or(ReaderError::InvalidRecord)?;
    // A `<synthetic>` model marks a locally constructed message that was never
    // billed, so it is not usage even though it carries a usage block.
    let model = match body.get("model").and_then(Value::as_str).map(str::trim) {
        Some("<synthetic>") => return Ok(None),
        Some(model) if !model.is_empty() => model.to_owned(),
        _ => UNKNOWN_MODEL.to_owned(),
    };
    if !valid_identity(&model) {
        return Err(ReaderError::InvalidRecord);
    }
    let cache_write = counter(usage, "cache_creation_input_tokens")?;
    let tokens = TokenCounts {
        input: counter(usage, "input_tokens")?,
        output: counter(usage, "output_tokens")?,
        cache_read: counter(usage, "cache_read_input_tokens")?,
        cache_write,
        // Proma records Claude's one-hour cache-write breakdown separately; the
        // native model folds it into `cache_write` and reports no separate
        // bucket, so it is not claimed twice.
        reasoning: 0,
    };
    if totals(&tokens) == 0 {
        return Ok(None);
    }
    let session = text(record, "session_id")
        .or_else(|| text(record, "sessionId"))
        .unwrap_or_else(|| fallback_session.to_owned());
    if !valid_identity(&session) {
        return Err(ReaderError::InvalidRecord);
    }
    let timestamp_ms = record
        .get("_createdAt")
        .and_then(crate::parsers::timestamps::parse_timestamp_value)
        .or_else(|| {
            record
                .get("timestamp")
                .and_then(crate::parsers::timestamps::parse_timestamp_value)
        })
        .or(fallback_timestamp_ms)
        .ok_or(ReaderError::Declined("Proma record timestamp"))?;
    let identity = format!(
        "proma:sdk:{}:{}",
        text(body, "id").unwrap_or_default(),
        ordinal
    );
    if identity.len() > MAX_IDENTITY_BYTES {
        return Err(ReaderError::Declined("Proma message identity"));
    }
    Ok(Some(ResponseUsage {
        key: SessionKey::new(CLIENT, &session),
        model,
        tokens,
        cache_write_1h: 0,
        timestamp_ms,
        dedup_key: Some(identity),
        billing_provider_hint: text(body, "provider").or_else(|| Some(DEFAULT_PROVIDER.to_owned())),
        service_tier: text(body, "service_tier"),
        agent: None,
        workspace_key: None,
        workspace_label: None,
        title: None,
        cost: CostStatus::Unavailable,
    }))
}

fn counter(usage: &Map<String, Value>, key: &str) -> Result<u64, ReaderError> {
    match usage.get(key) {
        None | Some(Value::Null) => Ok(0),
        Some(value) => match value.as_i64() {
            Some(number) if number >= 0 => Ok(number as u64),
            _ => Err(ReaderError::Declined("Proma counter")),
        },
    }
}

fn totals(tokens: &TokenCounts) -> u64 {
    tokens
        .input
        .saturating_add(tokens.output)
        .saturating_add(tokens.cache_read)
        .saturating_add(tokens.cache_write)
        .saturating_add(tokens.reasoning)
}

fn text(value: &Map<String, Value>, field: &str) -> Option<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn valid_identity(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_IDENTITY_BYTES && !value.chars().any(char::is_control)
}
