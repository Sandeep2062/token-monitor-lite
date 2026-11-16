//! DeepSeek Harness (DSH) session normalization adapted from the pinned
//! Tokscale `dsh.rs` reader. See docs/migration/parser-provenance.md.
//!
//! DSH writes one append-only JSONL transcript per session under
//! `<DSH_HOME>/sessions/<encoded-cwd>/<session-id>/session.jsonl.zstd`
//! (`DSH_HOME` defaults to `~/.dsh`). The `.zstd` suffix marks the physical
//! encoding only: a backend configured with `compression: none` writes the same
//! rows to a plain `session.jsonl`, so the input layer dispatches on the zstd
//! frame magic rather than on the file name.
//!
//! Three event types carry usage, and each is a real provider call:
//!
//! - `assistant/message`, whose usage is on `data.usage` or, when absent, in the
//!   last `usage` chunk of its embedded `data.stream`;
//! - `assistant/attempt`, which only ever settles from its stream;
//! - `compaction/summary`, the summarize call DSH makes when it compacts a
//!   range. It shares no `(turn, step)` with a loop step, so it is counted in
//!   addition to the messages around it rather than replacing one.
//!
//! `outputTokens` includes `reasoningTokens`, so the reasoning bucket is
//! subtracted from output exactly once.
//!
//! Not implemented, and recorded as unsupported coverage rather than guessed:
//!
//! - per-call generation latency (`duration_ms`) and `is_turn_start`. Both are
//!   derived from `step/start` anchors and `(turn, step)` slots, and both are
//!   aggregation concerns in this codebase rather than properties of one
//!   observation;
//! - the compressed-source cache with its fingerprint.

use super::{CostStatus, ReaderError, ResponseUsage, jsonl::MAX_RECORD_BYTES};
use crate::accounting::{SessionKey, TokenCounts};
use serde_json::Value;
use std::collections::HashSet;

pub const MAX_RESPONSES: usize = 16_384;
const MAX_IDENTITY_BYTES: usize = 4_096;
const MAX_RETAINED_IDENTITY_BYTES: usize = 8 * 1024 * 1024;

pub const CLIENT: &str = "dsh";

/// Where this transcript's own work starts.
///
/// A fork copies its parent's prefix into a new transcript. Events before the
/// cut are inherited, so charging them to the child would bill the parent's
/// calls twice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForkBoundary {
    /// Not a fork, so every event belongs to this session.
    None,
    /// This child owns events at or after `seq`.
    Cut(i64),
    /// The header says the session is seeded but no readable tagged cut exists,
    /// so inherited usage cannot be distinguished from child-owned usage.
    MissingTaggedCut,
}

/// One DSH transcript.
pub struct DshReader {
    session: String,
    responses: Vec<ResponseUsage>,
    /// `(seq, index)` for each emitted row, so the fork cut can be applied once
    /// the whole transcript has been read.
    emitted: Vec<(Option<i64>, usize)>,
    /// The legacy exact cut, present on older headers.
    seed_length: Option<i64>,
    /// The last tagged `session/end-seed { inherited: true }` marker.
    last_tagged_cut: Option<i64>,
    seeded_header: bool,
    session_header_seen: bool,
    workspace_label: Option<String>,
    fallback_provider: Option<String>,
    fallback_model: Option<String>,
    /// The `(turn, step)` slot and response index of the most recent
    /// settlement, so a later sample for the same slot replaces it rather than
    /// being counted as a second call.
    last_settlement: Option<((i64, i64), usize)>,
    seen: HashSet<String>,
    retained_identity_bytes: usize,
}

impl DshReader {
    pub fn new(session: impl Into<String>) -> Self {
        Self {
            session: session.into(),
            responses: Vec::new(),
            emitted: Vec::new(),
            seed_length: None,
            last_tagged_cut: None,
            seeded_header: false,
            session_header_seen: false,
            workspace_label: None,
            fallback_provider: None,
            fallback_model: None,
            last_settlement: None,
            seen: HashSet::new(),
            retained_identity_bytes: 0,
        }
    }

    pub fn ingest(&mut self, record: &[u8]) -> Result<(), ReaderError> {
        if record.len() > MAX_RECORD_BYTES {
            return Err(ReaderError::Capacity("DSH line"));
        }
        if record.iter().all(u8::is_ascii_whitespace) {
            return Ok(());
        }
        let value: Value =
            serde_json::from_slice(record).map_err(|_| ReaderError::InvalidRecord)?;
        let Some(event) = value.get("type").and_then(Value::as_str) else {
            return Ok(());
        };
        match event {
            "session" => self.ingest_session(&value),
            "session/end-seed" => {
                if value.pointer("/data/inherited") == Some(&Value::Bool(true))
                    && let Some(seq) = value.get("seq").and_then(Value::as_i64)
                {
                    self.last_tagged_cut = Some(seq);
                }
            }
            "request/header" => {
                let config = value.pointer("/data/header/config");
                self.fallback_provider = config
                    .and_then(|config| config.get("provider"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                self.fallback_model = config
                    .and_then(|config| config.get("model"))
                    .and_then(non_empty_string)
                    .map(str::to_owned);
            }
            "assistant/message" | "assistant/attempt" | "compaction/summary" => {
                self.ingest_settlement(&value, event)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn ingest_session(&mut self, value: &Value) {
        if let Some(session) = value.get("id").and_then(Value::as_str)
            && !session.is_empty()
        {
            self.session = session.to_owned();
        }
        if let Some(cwd) = value.get("cwd").and_then(Value::as_str) {
            self.workspace_label = workspace_label_from_cwd(cwd);
        }
        if self.session_header_seen {
            return;
        }
        self.session_header_seen = true;
        // A present legacy field owns the decision even when it is zero or
        // malformed; falling through to a marker would silently reinterpret an
        // older persisted contract.
        if let Some(seed_length) = value.get("seedLength").filter(|field| !field.is_null()) {
            self.seed_length = Some(
                seed_length
                    .as_i64()
                    .filter(|length| *length > 0)
                    .unwrap_or(0),
            );
            return;
        }
        self.seeded_header = value.get("isSeeded").and_then(Value::as_bool) == Some(true);
    }

    fn ingest_settlement(&mut self, value: &Value, event: &str) -> Result<(), ReaderError> {
        let summary = event == "compaction/summary";
        let attempt = event == "assistant/attempt";
        let Some(usage) = usage_for_event(value, event) else {
            return Ok(());
        };
        let tokens = tokens_from_usage(usage)?;
        if tokens.input == 0
            && tokens.output == 0
            && tokens.cache_read == 0
            && tokens.cache_write == 0
            && tokens.reasoning == 0
        {
            return Ok(());
        }
        let Some(timestamp) = value.get("time").and_then(Value::as_i64).filter(|t| *t > 0) else {
            return Ok(());
        };
        let source = value.pointer("/data/message/source");
        let model = served_model(source)
            .or(self.fallback_model.as_deref())
            .unwrap_or("unknown")
            .to_owned();
        let provider = source
            .and_then(|source| source.get("provider"))
            .and_then(non_empty_string)
            .or(self.fallback_provider.as_deref())
            .unwrap_or("unknown")
            .to_owned();
        // `data.message.id` is a per-call UUID that a fork copies verbatim, so
        // scoping identity to it (rather than the session) collapses a seeded
        // copy against the parent's original even across files. A summary has
        // no message id; it carries `data.compactionId` instead, and `seq` is
        // the final fallback for older summaries.
        let identity = if attempt {
            value
                .pointer("/data/attemptId")
                .or_else(|| value.pointer("/data/retryId"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(|id| format!("attempt-id:{id}"))
                .or_else(|| {
                    value
                        .get("seq")
                        .and_then(Value::as_i64)
                        .map(|seq| format!("attempt-session:{}:seq:{seq}", self.session))
                })
                .unwrap_or_else(|| format!("attempt-session:{}", self.session))
        } else {
            value
                .pointer("/data/message/id")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(|id| format!("msg:{id}"))
                .or_else(|| {
                    if summary {
                        value
                            .pointer("/data/compactionId")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|id| !id.is_empty())
                            .map(|id| format!("cmp:{id}"))
                    } else {
                        None
                    }
                })
                .or_else(|| {
                    value
                        .get("seq")
                        .and_then(Value::as_i64)
                        .map(|seq| format!("seq:{seq}"))
                })
                .unwrap_or_else(|| format!("sid:{}", self.session))
        };
        // A summary carries no message id, so without a namespace it could
        // collapse against a loop step that happened to share its clock,
        // routing and buckets.
        let kind = if summary {
            "summary:"
        } else if attempt {
            "attempt:"
        } else {
            ""
        };
        let dedup_key = format!(
            "dsh:{kind}{identity}:{timestamp}:{provider}:{model}:{}:{}:{}:{}:{}",
            tokens.input, tokens.output, tokens.cache_read, tokens.cache_write, tokens.reasoning
        );
        if dedup_key.len() > MAX_IDENTITY_BYTES {
            return Err(ReaderError::Declined("DSH identity"));
        }
        if !self.seen.insert(dedup_key.clone()) {
            return Ok(());
        }
        if self.responses.len() == MAX_RESPONSES {
            return Err(ReaderError::Capacity("DSH responses"));
        }
        self.retained_identity_bytes = self
            .retained_identity_bytes
            .checked_add(dedup_key.len())
            .filter(|bytes| *bytes <= MAX_RETAINED_IDENTITY_BYTES)
            .ok_or(ReaderError::Capacity("DSH identities"))?;
        let index = self.responses.len();
        let slot = value
            .pointer("/data/turn")
            .and_then(Value::as_i64)
            .zip(value.pointer("/data/step").and_then(Value::as_i64));
        // A summary shares no `(turn, step)` with a loop step, so it never takes
        // part in the replacement rule.
        let replacement = if summary { None } else { slot };
        let usage_row = ResponseUsage {
            key: SessionKey::new(CLIENT, &self.session),
            model,
            tokens,
            cache_write_1h: 0,
            timestamp_ms: timestamp,
            dedup_key: Some(dedup_key),
            billing_provider_hint: Some(provider),
            service_tier: None,
            agent: None,
            // An absolute `cwd` must never reach the store's identity columns,
            // so only the final component survives, as a label.
            workspace_key: None,
            workspace_label: self.workspace_label.clone(),
            title: None,
            // DSH never embeds a cost, so pricing is the only cost source.
            cost: CostStatus::Unavailable,
        };
        // Another sample for the same attempt slot replaces the previous one:
        // a single call that reports twice is one call, and summing both
        // samples would double-count it.
        if let Some((slot, previous)) = self.last_settlement
            && Some(slot) == replacement
            && previous < index
        {
            self.responses[previous] = usage_row;
            self.emitted.retain(|(_, index)| *index != previous);
            let seq = value.get("seq").and_then(Value::as_i64);
            self.emitted.push((seq, previous));
            return Ok(());
        }
        self.responses.push(usage_row);
        self.emitted
            .push((value.get("seq").and_then(Value::as_i64), index));
        self.last_settlement = replacement.map(|slot| (slot, index));
        Ok(())
    }

    /// Resolve the fork boundary and drop inherited rows.
    ///
    /// The boundary cannot be decided during a streaming pass: a current seeded
    /// header puts the exact cut on the *last* tagged marker, which a nested
    /// fork may inherit from an ancestor. So rows are retained with their `seq`
    /// and filtered here, once the whole transcript has been read.
    pub fn finish(&mut self) -> Result<(), ReaderError> {
        let boundary = if let Some(seed_length) = self.seed_length {
            if seed_length > 0 {
                ForkBoundary::Cut(seed_length)
            } else {
                ForkBoundary::None
            }
        } else if self.seeded_header {
            match self.last_tagged_cut {
                Some(cut) => ForkBoundary::Cut(cut),
                // A seeded session with no readable cut cannot distinguish
                // inherited usage from its own, so it contributes nothing
                // rather than charging the whole copied prefix to the child.
                None => return Err(ReaderError::Declined("DSH seeded fork boundary")),
            }
        } else {
            ForkBoundary::None
        };
        let ForkBoundary::Cut(cut) = boundary else {
            return Ok(());
        };
        let dropped: HashSet<usize> = self
            .emitted
            .iter()
            .filter(|(seq, _)| seq.is_some_and(|seq| seq < cut))
            .map(|(_, index)| *index)
            .collect();
        if dropped.is_empty() {
            return Ok(());
        }
        let mut kept = Vec::with_capacity(self.responses.len() - dropped.len());
        for (index, row) in std::mem::take(&mut self.responses).into_iter().enumerate() {
            if !dropped.contains(&index) {
                kept.push(row);
            }
        }
        self.responses = kept;
        Ok(())
    }

    pub fn snapshot(&self) -> Vec<ResponseUsage> {
        self.responses.clone()
    }

    pub fn has_supported_usage(&self) -> bool {
        !self.responses.is_empty()
    }
}

/// The usage object a settlement carries, if any.
fn usage_for_event<'a>(value: &'a Value, event: &str) -> Option<&'a Value> {
    match event {
        "assistant/message" => value
            .pointer("/data/usage")
            .or_else(|| last_assistant_stream_usage(value)),
        "assistant/attempt" => last_assistant_stream_usage(value),
        "compaction/summary" => value.pointer("/data/usage"),
        _ => None,
    }
}

/// The last `usage` chunk of an embedded Assistant stream.
fn last_assistant_stream_usage(value: &Value) -> Option<&Value> {
    value
        .pointer("/data/stream")?
        .as_array()?
        .iter()
        .rev()
        .find_map(|record| {
            let chunk = record.get("chunk")?;
            (record.get("type")?.as_str()? == "chunk" && chunk.get("type")?.as_str()? == "usage")
                .then(|| chunk.get("usage"))
                .flatten()
        })
}

/// `outputTokens` already includes `reasoningTokens`, so the reasoning bucket is
/// removed from output exactly once rather than being added on top of it.
fn tokens_from_usage(usage: &Value) -> Result<TokenCounts, ReaderError> {
    let output = counter(usage, "outputTokens")?;
    let reasoning = counter(usage, "reasoningTokens")?;
    Ok(TokenCounts {
        input: counter(usage, "inputTokens")?,
        output: output.saturating_sub(reasoning),
        cache_read: counter(usage, "cacheReadTokens")?,
        cache_write: counter(usage, "cacheWriteTokens")?,
        reasoning,
    })
}

fn counter(value: &Value, key: &str) -> Result<u64, ReaderError> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(0),
        Some(Value::Number(number)) => number.as_u64().ok_or(ReaderError::InvalidRecord),
        _ => Err(ReaderError::InvalidRecord),
    }
}

/// The concrete model that served a call.
///
/// `source.model` is the *configured* request model; a floating alias or a
/// provider substitution resolves to another model, which is recorded on
/// `replayState.response.responseModel` and is authoritative. An early gateway
/// heartbeat may leave `keepalive` there, which identifies no model and is the
/// only value discarded on that basis.
fn served_model(source: Option<&Value>) -> Option<&str> {
    source
        .and_then(|value| value.pointer("/replayState/response/responseModel"))
        .and_then(non_empty_string)
        .filter(|model| !model.eq_ignore_ascii_case("keepalive"))
        .or_else(|| {
            source
                .and_then(|value| value.get("model"))
                .and_then(non_empty_string)
        })
}

fn non_empty_string(value: &Value) -> Option<&str> {
    value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// A single trailing path component, so no absolute path reaches the store.
fn workspace_label_from_cwd(cwd: &str) -> Option<String> {
    cwd.trim()
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .filter(|part| part.len() <= 255 && !part.chars().any(char::is_control))
        .map(str::to_owned)
}
