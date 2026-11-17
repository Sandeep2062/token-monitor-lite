//! GitHub Copilot CLI session usage, read from the CLI's own session-state log.
//!
//! The Copilot CLI appends one event log per session at
//! `~/.copilot/session-state/<sessionId>/events.jsonl`. Most events are
//! conversation plumbing; the billed one is `session.usage_record`, whose
//! `data.usage` carries the authoritative counters:
//!
//! ```json
//! {"type":"session.usage_record","timestamp":"2026-10-24T15:36:45.326Z",
//!  "data":{"usage":{"model":"gpt-5.6-luna","inputTokens":20347,
//!  "outputTokens":61,"cacheReadTokens":0,"cacheWriteTokens":20344,
//!  "reasoningTokens":40,"cost":1.0,
//!  "accounting":{"sourceSessionId":"…","sequence":1,"usageId":"…"}}}}
//! ```
//!
//! # Accounting
//!
//! `inputTokens` is **gross**: it equals `input + cache_read + cache_write`
//! from the record's own `copilotUsage.tokenDetails` billing breakdown. That was
//! checked against every record on this host — 947 of them, zero mismatches —
//! so the overlap is subtracted once and a record claiming more cache than
//! input is refused rather than clamped.
//!
//! `outputTokens` is **already disjoint** from `reasoningTokens`:
//! `tokenDetails` carries no reasoning row and its `output` entry equals
//! `outputTokens` exactly, so reasoning is not subtracted from output.
//!
//! `cost` is **not money**. It is a constant `1.0` on every record and equals
//! the session's `totalPremiumRequests`, i.e. a premium-request count. The
//! billing prices in `tokenDetails` are denominated in GitHub AI Units
//! (`costPerBatch` nano-AIU per `batchSize` tokens), and converting those to
//! currency needs a plan-specific rate, so no cost is reported.
//!
//! # Identity
//!
//! `accounting.sourceSessionId` plus `accounting.sequence` is the CLI's own
//! accounting identity and is unique per recorded call, so a forked session
//! does not collide with its parent.
//!
//! # Known limitation
//!
//! If the CLI's OpenTelemetry exporter is enabled **and** its session-state log
//! is present, the same calls are described twice, by two lanes with no shared
//! identifier: this lane keys on `accounting.usageId`, while the OTel lane keys
//! on `gen_ai.response.id`. Cross-lane merging cannot join them, so enabling
//! both double-counts. See `docs/migration/reader-contracts.md`.

use super::{CostStatus, ReaderError, ResponseUsage};
use crate::accounting::{SessionKey, TokenCounts};
use serde_json::Value;

pub const CLIENT: &str = "copilot";
pub const UNKNOWN_MODEL: &str = "unknown";
/// Copilot bills through GitHub.
pub const PROVIDER: &str = "copilot";

pub const MAX_RESPONSES: usize = 16_384;
const MAX_IDENTITY_BYTES: usize = 4_096;
const MAX_RETAINED_IDENTITY_BYTES: usize = 8 * 1024 * 1024;

/// One Copilot CLI session's `events.jsonl`.
pub struct CopilotCliReader {
    session: String,
    responses: Vec<ResponseUsage>,
    retained_identity_bytes: usize,
}

impl CopilotCliReader {
    pub fn new(session: impl Into<String>) -> Self {
        Self {
            session: session.into(),
            responses: Vec::new(),
            retained_identity_bytes: 0,
        }
    }

    /// Read one event line. Every other event type contributes nothing.
    pub fn ingest(&mut self, record: &[u8]) -> Result<(), ReaderError> {
        let event: Value =
            serde_json::from_slice(record).map_err(|_| ReaderError::InvalidRecord)?;
        let object = event.as_object().ok_or(ReaderError::InvalidRecord)?;
        if object.get("type").and_then(Value::as_str) != Some("session.usage_record") {
            return Ok(());
        }
        let usage = object
            .get("data")
            .and_then(|data| data.get("usage"))
            .and_then(Value::as_object)
            .ok_or(ReaderError::Unsupported("Copilot usage record"))?;
        let timestamp_ms = object
            .get("timestamp")
            .and_then(crate::parsers::timestamps::parse_timestamp_value)
            .ok_or(ReaderError::Declined("Copilot session timestamp"))?;

        let reported_input = optional_u64(usage, "inputTokens")?;
        let cache_read = optional_u64(usage, "cacheReadTokens")?;
        let cache_write = optional_u64(usage, "cacheWriteTokens")?;
        let output = optional_u64(usage, "outputTokens")?;
        let reasoning = optional_u64(usage, "reasoningTokens")?;
        let cached = cache_read
            .checked_add(cache_write)
            .ok_or(ReaderError::Overflow)?;
        // `inputTokens` is gross of cache, verified against the record's own
        // `tokenDetails` breakdown; the overlap is removed once.
        let input = reported_input
            .checked_sub(cached)
            .ok_or(ReaderError::Declined("Copilot cache/input"))?;
        if input == 0 && output == 0 && reasoning == 0 && cached == 0 {
            // A record that bills nothing carries no size; counting it would
            // add a zero row indistinguishable from a measured one.
            return Ok(());
        }

        let accounting = usage
            .get("accounting")
            .and_then(Value::as_object)
            .ok_or(ReaderError::Declined("Copilot accounting"))?;
        let source_session = accounting
            .get("sourceSessionId")
            .and_then(Value::as_str)
            .filter(|value| valid_identity(value))
            .unwrap_or(&self.session);
        let sequence = accounting
            .get("sequence")
            .and_then(Value::as_u64)
            .ok_or(ReaderError::Declined("Copilot accounting"))?;
        let identity = format!("copilot:{source_session}:usage:{sequence}");
        if identity.len() > MAX_IDENTITY_BYTES {
            return Err(ReaderError::Declined("Copilot identity"));
        }

        let model = usage
            .get("model")
            .and_then(Value::as_str)
            .filter(|model| valid_identity(model))
            .unwrap_or(UNKNOWN_MODEL)
            .to_owned();
        let provider = usage
            .get("provider")
            .and_then(|provider| provider.get("id"))
            .and_then(Value::as_str)
            .filter(|provider| valid_identity(provider))
            .unwrap_or(PROVIDER)
            .to_owned();

        self.retained_identity_bytes = self
            .retained_identity_bytes
            .checked_add(identity.len())
            .filter(|bytes| *bytes <= MAX_RETAINED_IDENTITY_BYTES)
            .ok_or(ReaderError::Capacity("Copilot identities"))?;
        if self.responses.len() == MAX_RESPONSES {
            return Err(ReaderError::Capacity("Copilot responses"));
        }

        self.responses.push(ResponseUsage {
            key: SessionKey::new(CLIENT, &self.session),
            model,
            tokens: TokenCounts {
                input,
                output,
                cache_read,
                cache_write,
                reasoning,
            },
            cache_write_1h: 0,
            timestamp_ms,
            dedup_key: Some(identity),
            billing_provider_hint: Some(provider),
            service_tier: None,
            agent: None,
            workspace_key: None,
            workspace_label: None,
            title: None,
            // `cost` is a premium-request count and the billing prices are in
            // AI Units, so there is no currency amount to report.
            cost: CostStatus::Unavailable,
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

fn optional_u64(object: &serde_json::Map<String, Value>, key: &str) -> Result<u64, ReaderError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(0),
        Some(value) => value.as_u64().ok_or(ReaderError::InvalidRecord),
    }
}

fn valid_identity(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_IDENTITY_BYTES && !value.chars().any(char::is_control)
}
