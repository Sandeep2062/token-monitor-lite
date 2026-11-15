//! Cursor usage-cache normalization adapted from the pinned Tokscale
//! `cursor.rs` reader. MIT © 2025 Junho Yeo; see
//! docs/migration/parser-provenance.md.
//!
//! The supported source is the read-only JSON cache produced by the reference
//! Cursor usage sync (`usage.json` / `usage.<account>.json` with a
//! `usageEventsDisplay` array). Native Cursor cloud sync, CSV exports, and the
//! desktop title/turn-state database are not implemented. Counters are checked
//! non-negative; repeated snapshots of one `usageEventId` collapse to the
//! largest token total. Cost stays unavailable in Gate 2A.

use super::{CostStatus, ReaderError, ResponseUsage};
use crate::accounting::{SessionKey, TokenCounts};
use chrono::DateTime;
use serde_json::{Map, Value};

pub const MAX_RESPONSES: usize = 16_384;
const MAX_IDENTITY_BYTES: usize = 4_096;
const MAX_RETAINED_IDENTITY_BYTES: usize = 8 * 1024 * 1024;

struct PendingRow {
    usage: ResponseUsage,
    event_id: Option<String>,
    token_total: u64,
}

pub struct CursorReader {
    account: String,
    rows: Vec<PendingRow>,
    retained_identity_bytes: usize,
}

impl CursorReader {
    pub fn new(account: impl Into<String>) -> Self {
        Self {
            account: account.into(),
            rows: Vec::new(),
            retained_identity_bytes: 0,
        }
    }

    pub fn ingest(&mut self, record: &[u8]) -> Result<(), ReaderError> {
        let value: Value =
            serde_json::from_slice(record).map_err(|_| ReaderError::InvalidRecord)?;
        let object = value.as_object().ok_or(ReaderError::InvalidRecord)?;
        let events = object
            .get("usageEventsDisplay")
            .and_then(Value::as_array)
            .ok_or(ReaderError::Unsupported("Cursor usage cache"))?;
        for event in events {
            self.ingest_event(event)?;
        }
        Ok(())
    }

    pub fn snapshot(&self) -> Vec<ResponseUsage> {
        let mut fallback: std::collections::BTreeMap<(String, String, i64), String> =
            std::collections::BTreeMap::new();
        for row in &self.rows {
            let Some(id) = row.event_id.as_ref() else {
                continue;
            };
            fallback
                .entry((
                    row.usage.key.session.clone(),
                    row.usage.model.clone(),
                    row.usage.timestamp_ms,
                ))
                .and_modify(|current| {
                    if id < current {
                        current.clone_from(id);
                    }
                })
                .or_insert_with(|| id.clone());
        }
        let mut keep = vec![true; self.rows.len()];
        let mut survivors: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        for (index, row) in self.rows.iter().enumerate() {
            let key = row.event_id.clone().or_else(|| {
                fallback
                    .get(&(
                        row.usage.key.session.clone(),
                        row.usage.model.clone(),
                        row.usage.timestamp_ms,
                    ))
                    .cloned()
            });
            let Some(key) = key else {
                continue;
            };
            match survivors.get(&key).copied() {
                Some(current) => {
                    if row.token_total > self.rows[current].token_total {
                        keep[current] = false;
                        survivors.insert(key, index);
                    } else {
                        keep[index] = false;
                    }
                }
                None => {
                    survivors.insert(key, index);
                }
            }
        }
        self.rows
            .iter()
            .zip(keep)
            .filter_map(|(row, keep)| keep.then_some(row.usage.clone()))
            .collect()
    }

    pub fn has_supported_usage(&self) -> bool {
        !self.rows.is_empty()
    }

    fn ingest_event(&mut self, event: &Value) -> Result<(), ReaderError> {
        let object = event.as_object().ok_or(ReaderError::InvalidRecord)?;
        let Some(model) = object
            .get("model")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|model| !model.is_empty())
        else {
            return Ok(());
        };
        if !valid_identity(model) {
            return Err(ReaderError::InvalidRecord);
        }
        let Some(timestamp_ms) = event_timestamp(object)? else {
            return Ok(());
        };
        let conversation = object
            .get("conversationId")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty());
        let session = conversation
            .map(str::to_owned)
            .unwrap_or_else(|| synthetic_session(&self.account, timestamp_ms));
        if !valid_identity(&session) {
            return Err(ReaderError::InvalidRecord);
        }
        let tokens = match object.get("tokenUsage") {
            None | Some(Value::Null) => TokenCounts::default(),
            Some(Value::Object(tokens)) => TokenCounts {
                input: optional_u64(tokens, "inputTokens")?,
                output: optional_u64(tokens, "outputTokens")?,
                cache_read: optional_u64(tokens, "cacheReadTokens")?,
                cache_write: optional_u64(tokens, "cacheWriteTokens")?,
                reasoning: 0,
            },
            Some(_) => return Err(ReaderError::InvalidRecord),
        };
        let event_id = object
            .get("usageEventId")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_owned);
        if event_id.as_ref().is_some_and(|id| !valid_identity(id)) {
            return Err(ReaderError::InvalidRecord);
        }
        let dedup_key = event_id.as_ref().map(|id| format!("cursor-event:{id}"));
        if dedup_key
            .as_ref()
            .is_some_and(|key| key.len() > MAX_IDENTITY_BYTES)
        {
            return Err(ReaderError::Unsupported("Cursor event identity"));
        }
        let identity_bytes = dedup_key.as_ref().map_or(0, String::len);
        self.retained_identity_bytes = self
            .retained_identity_bytes
            .checked_add(identity_bytes)
            .filter(|bytes| *bytes <= MAX_RETAINED_IDENTITY_BYTES)
            .ok_or(ReaderError::Capacity("Cursor identities"))?;
        if self.rows.len() == MAX_RESPONSES {
            return Err(ReaderError::Capacity("Cursor responses"));
        }
        let token_total = tokens
            .input
            .checked_add(tokens.output)
            .and_then(|value| value.checked_add(tokens.cache_read))
            .and_then(|value| value.checked_add(tokens.cache_write))
            .ok_or(ReaderError::Overflow)?;
        self.rows.push(PendingRow {
            usage: ResponseUsage {
                key: SessionKey::new("cursor", session),
                model: model.to_owned(),
                tokens,
                cache_write_1h: 0,
                timestamp_ms,
                dedup_key,
                billing_provider_hint: Some("cursor".to_owned()),
                service_tier: None,
                agent: None,
                cost: CostStatus::Unavailable,
            },
            event_id,
            token_total,
        });
        Ok(())
    }
}

fn event_timestamp(object: &Map<String, Value>) -> Result<Option<i64>, ReaderError> {
    match object.get("timestamp") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => {
            if let Some(value) = number.as_i64() {
                return Ok((value > 0).then_some(value));
            }
            if let Some(value) = number.as_u64() {
                return i64::try_from(value)
                    .map(|value| (value > 0).then_some(value))
                    .map_err(|_| ReaderError::InvalidTimestamp);
            }
            let value = number.as_f64().ok_or(ReaderError::InvalidTimestamp)?;
            if !value.is_finite() || value <= 0.0 || value > i64::MAX as f64 || value.fract() != 0.0
            {
                return Err(ReaderError::InvalidTimestamp);
            }
            Ok(Some(value as i64))
        }
        Some(Value::String(text)) => match text.trim().parse::<i64>() {
            Ok(value) if value > 0 => Ok(Some(value)),
            Ok(_) => Ok(None),
            Err(_) => Err(ReaderError::InvalidTimestamp),
        },
        Some(_) => Err(ReaderError::InvalidTimestamp),
    }
}

fn optional_u64(object: &Map<String, Value>, key: &str) -> Result<u64, ReaderError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(0),
        Some(Value::Number(number)) => number.as_u64().ok_or(ReaderError::InvalidRecord),
        Some(Value::String(text)) => text
            .trim()
            .parse::<u64>()
            .map_err(|_| ReaderError::InvalidRecord),
        Some(_) => Err(ReaderError::InvalidRecord),
    }
}

fn synthetic_session(account: &str, timestamp_ms: i64) -> String {
    let day = DateTime::from_timestamp_millis(timestamp_ms)
        .map(|value| value.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "unknown".to_owned());
    let account = account
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    format!("cursor-{account}-{day}")
}

fn valid_identity(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_IDENTITY_BYTES && !value.chars().any(char::is_control)
}
