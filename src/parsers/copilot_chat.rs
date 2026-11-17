//! GitHub Copilot chat-session usage, read from VS Code's chat session store.
//!
//! VS Code stores each chat session as an **append-only operation log** at
//! `…/User/workspaceStorage/<workspace>/chatSessions/<sessionId>.jsonl`, and
//! empty-window sessions at
//! `…/User/globalStorage/emptyWindowChatSessions/<sessionId>.jsonl`.
//!
//! Every line is one operation, replayed against a document:
//!
//! | `kind` | meaning |
//! | --- | --- |
//! | `0` | the initial document (`v` is the whole object) |
//! | `1` | set the value at the path `k` (`k` is an array of keys/indices) |
//! | `2` | **append** the elements of `v` to the array at `k` |
//!
//! `kind: 2` is append, not replace — that distinction is the whole format. A
//! request is appended when it starts and its `promptTokens`/`completionTokens`
//! arrive later as `kind: 1` leaf writes, so a reader that treated `kind: 2` as
//! a replacement would see every request patched out of range and read nothing.
//!
//! # Accounting
//!
//! A completed request carries `promptTokens` and `completionTokens`, plus
//! `modelId` (for example `copilot/auto`) and a millisecond `timestamp`. There
//! is **no cache split** in this store: `promptTokenDetails` is a category
//! breakdown (`System`, `User Context`, …), not a cache report. `promptTokens`
//! is therefore taken as the whole input with zero cache, which cannot
//! double-count.
//!
//! `copilotCredits` is a credit count, not money, so no cost is reported.
//!
//! # Not implemented
//!
//! Cache attribution, and the separate `session-store.db` SQLite store, whose
//! `sessions`/`turns` tables hold text only and no counters.

use super::{CostStatus, ReaderError, ResponseUsage};
use crate::accounting::{SessionKey, TokenCounts};
use serde_json::Value;

pub const CLIENT: &str = "copilot";
pub const UNKNOWN_MODEL: &str = "unknown";

pub const MAX_RESPONSES: usize = 16_384;
const MAX_IDENTITY_BYTES: usize = 4_096;
const MAX_DEPTH: usize = 16;

/// One chat session's `.jsonl` operation log.
pub struct CopilotChatReader {
    session: String,
    document: Value,
    responses: Vec<ResponseUsage>,
}

impl CopilotChatReader {
    pub fn new(session: impl Into<String>) -> Self {
        Self {
            session: session.into(),
            document: Value::Null,
            responses: Vec::new(),
        }
    }

    /// Apply one operation line, then re-derive the rows.
    ///
    /// Re-deriving rather than appending is required because a request is
    /// patched after it is appended: its tokens change under a stable identity,
    /// so the snapshot must be rebuilt from the replayed document.
    pub fn ingest(&mut self, record: &[u8]) -> Result<(), ReaderError> {
        let operation: Value =
            serde_json::from_slice(record).map_err(|_| ReaderError::InvalidRecord)?;
        let object = operation.as_object().ok_or(ReaderError::InvalidRecord)?;
        let kind = object
            .get("kind")
            .and_then(Value::as_u64)
            .ok_or(ReaderError::InvalidRecord)?;
        let value = object.get("v").cloned().unwrap_or(Value::Null);
        match kind {
            0 => {
                // The initial document is the authoritative whole object; any
                // earlier state belonged to a previous writer generation.
                if !value.is_object() {
                    return Err(ReaderError::Unsupported("Copilot chat session"));
                }
                self.document = value;
                self.responses.clear();
            }
            1 | 2 => {
                if self.document.is_null() {
                    // An operation before the document cannot be placed. It is
                    // not an error in the file, only a line this reader cannot
                    // use yet.
                    return Ok(());
                }
                let path = object
                    .get("k")
                    .and_then(Value::as_array)
                    .ok_or(ReaderError::InvalidRecord)?;
                if path.is_empty() || path.len() > MAX_DEPTH {
                    return Err(ReaderError::Declined("Copilot chat session path"));
                }
                if kind == 2 {
                    append(&mut self.document, path, value);
                } else {
                    set(&mut self.document, path, value);
                }
            }
            _ => return Ok(()),
        }
        self.rebuild()
    }

    fn rebuild(&mut self) -> Result<(), ReaderError> {
        let mut responses: Vec<ResponseUsage> = Vec::new();
        let mut positions: Vec<(String, usize)> = Vec::new();
        let requests = match self.document.get("requests").and_then(Value::as_array) {
            Some(requests) => requests,
            None => {
                self.responses.clear();
                return Ok(());
            }
        };
        for request in requests {
            let Some(object) = request.as_object() else {
                continue;
            };
            let Some(identifier) = object.get("requestId").and_then(Value::as_str) else {
                continue;
            };
            if identifier.is_empty() {
                continue;
            }
            let input = optional_u64(object, "promptTokens")?;
            let output = optional_u64(object, "completionTokens")?;
            if input == 0 && output == 0 {
                // An in-flight or cancelled request: Copilot appends the
                // request before the stream produces counters.
                continue;
            }
            let Some(timestamp_ms) = object.get("timestamp").and_then(Value::as_i64) else {
                return Err(ReaderError::Declined("Copilot chat session timestamp"));
            };
            if timestamp_ms < 0 {
                return Err(ReaderError::Declined("Copilot chat session timestamp"));
            }
            let dedup_key = format!("{}:{}:{identifier}", CLIENT, self.session);
            if dedup_key.len() > MAX_IDENTITY_BYTES {
                return Err(ReaderError::Declined("Copilot chat session identity"));
            }
            let model = object
                .get("modelId")
                .and_then(Value::as_str)
                .filter(|model| !model.is_empty())
                .unwrap_or(UNKNOWN_MODEL)
                .to_owned();
            let row = ResponseUsage {
                key: SessionKey::new(CLIENT, &self.session),
                model,
                tokens: TokenCounts {
                    input,
                    output,
                    cache_read: 0,
                    cache_write: 0,
                    reasoning: 0,
                },
                cache_write_1h: 0,
                timestamp_ms,
                dedup_key: Some(dedup_key.clone()),
                billing_provider_hint: None,
                service_tier: None,
                agent: None,
                workspace_key: None,
                workspace_label: None,
                title: None,
                cost: CostStatus::Unavailable,
            };
            // A patched request repeats its identity; the later value wins
            // without moving the row.
            match positions
                .iter()
                .find(|(existing, _)| *existing == dedup_key)
                .map(|(_, index)| *index)
            {
                Some(index) => responses[index] = row,
                None => {
                    if responses.len() == MAX_RESPONSES {
                        return Err(ReaderError::Capacity("Copilot chat session responses"));
                    }
                    positions.push((dedup_key, responses.len()));
                    responses.push(row);
                }
            }
        }
        self.responses = responses;
        Ok(())
    }

    pub fn snapshot(&self) -> Vec<ResponseUsage> {
        self.responses.clone()
    }

    pub fn has_supported_usage(&self) -> bool {
        !self.responses.is_empty()
    }
}

/// The last path segment is the leaf; everything before it is a container.
fn split(path: &[Value]) -> Option<(Vec<Segment>, Segment)> {
    let mut segments = Vec::with_capacity(path.len());
    for part in path {
        segments.push(segment(part)?);
    }
    let leaf = segments.pop()?;
    Some((segments, leaf))
}

#[derive(Clone)]
enum Segment {
    Index(usize),
    Key(String),
}

fn segment(part: &Value) -> Option<Segment> {
    match part {
        Value::String(key) => Some(Segment::Key(key.clone())),
        Value::Number(number) => number
            .as_u64()
            .and_then(|index| usize::try_from(index).ok().map(Segment::Index)),
        _ => None,
    }
}

/// Descend to the parent container named by `path`, creating missing
/// containers so a patch that arrives out of order is still applied.
fn parent<'a>(root: &'a mut Value, path: &[Segment]) -> Option<&'a mut Value> {
    let mut current = root;
    for segment in path {
        current = match segment {
            Segment::Key(key) => {
                if !current.is_object() {
                    *current = Value::Object(serde_json::Map::new());
                }
                current
                    .as_object_mut()?
                    .entry(key.clone())
                    .or_insert(Value::Null)
            }
            Segment::Index(index) => {
                if !current.is_array() {
                    *current = Value::Array(Vec::new());
                }
                let array = current.as_array_mut()?;
                while array.len() <= *index {
                    array.push(Value::Null);
                }
                &mut array[*index]
            }
        };
    }
    Some(current)
}

fn set(root: &mut Value, path: &[Value], value: Value) {
    let Some((containers, leaf)) = split(path) else {
        return;
    };
    let Some(container) = parent(root, &containers) else {
        return;
    };
    match leaf {
        Segment::Key(key) => {
            if !container.is_object() {
                *container = Value::Object(serde_json::Map::new());
            }
            if let Some(object) = container.as_object_mut() {
                object.insert(key, value);
            }
        }
        Segment::Index(index) => {
            if !container.is_array() {
                *container = Value::Array(Vec::new());
            }
            if let Some(array) = container.as_array_mut() {
                while array.len() <= index {
                    array.push(Value::Null);
                }
                array[index] = value;
            }
        }
    }
}

fn append(root: &mut Value, path: &[Value], value: Value) {
    let Some((containers, leaf)) = split(path) else {
        return;
    };
    let Some(container) = parent(root, &containers) else {
        return;
    };
    let Segment::Key(key) = leaf else {
        return;
    };
    if !container.is_object() {
        *container = Value::Object(serde_json::Map::new());
    }
    let Some(object) = container.as_object_mut() else {
        return;
    };
    let slot = object
        .entry(key)
        .or_insert_with(|| Value::Array(Vec::new()));
    if !slot.is_array() {
        *slot = Value::Array(Vec::new());
    }
    if let Some(array) = slot.as_array_mut() {
        match value {
            Value::Array(items) => array.extend(items),
            other => array.push(other),
        }
    }
}

fn optional_u64(object: &serde_json::Map<String, Value>, key: &str) -> Result<u64, ReaderError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(0),
        Some(value) => value.as_u64().ok_or(ReaderError::InvalidRecord),
    }
}
