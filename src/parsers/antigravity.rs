//! Antigravity normalized usage-cache JSONL, adapted from the pinned Tokscale
//! `antigravity.rs` reader. MIT © 2025 Junho Yeo; see
//! docs/migration/parser-provenance.md.
//!
//! Supported rows are `session_meta` (model fallback) and `usage` records with
//! direct counters. Native Antigravity conversation SQLite stores are not read
//! by this cache reader.

use super::timestamps::counter;
use super::{CostStatus, ReaderError, ResponseUsage};
use crate::accounting::{SessionKey, TokenCounts};
use serde_json::Value;

const MAX_RESPONSES: usize = 16_384;

pub struct AntigravityReader {
    session: String,
    fallback_timestamp_ms: Option<i64>,
    session_model: Option<String>,
    rows: Vec<ResponseUsage>,
    ordinal: usize,
}

impl AntigravityReader {
    pub fn new(session: impl Into<String>) -> Self {
        Self {
            session: session.into(),
            fallback_timestamp_ms: None,
            session_model: None,
            rows: Vec::new(),
            ordinal: 0,
        }
    }

    pub fn with_fallback_timestamp(mut self, timestamp_ms: Option<i64>) -> Self {
        self.fallback_timestamp_ms = timestamp_ms.filter(|value| *value >= 0);
        self
    }

    pub fn ingest(&mut self, record: &[u8]) -> Result<(), ReaderError> {
        let value: Value =
            serde_json::from_slice(record).map_err(|_| ReaderError::InvalidRecord)?;
        match value.get("type").and_then(Value::as_str) {
            Some("session_meta") => {
                if let Some(model) = value
                    .get("modelId")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|model| !model.is_empty())
                {
                    self.session_model = Some(model.to_owned());
                }
                return Ok(());
            }
            Some("usage") => {}
            _ => return Ok(()),
        }
        let session = value
            .get("sessionId")
            .and_then(Value::as_str)
            .filter(|session| super::tencent_buddy::valid_identity(session))
            .unwrap_or(self.session.as_str());
        let timestamp = value
            .get("timestamp")
            .and_then(|timestamp| {
                timestamp
                    .as_i64()
                    .or_else(|| {
                        timestamp
                            .as_u64()
                            .and_then(|value| i64::try_from(value).ok())
                    })
                    .or_else(|| {
                        timestamp
                            .as_str()
                            .and_then(super::timestamps::parse_timestamp_str)
                    })
            })
            .filter(|timestamp| *timestamp > 0)
            .or(self.fallback_timestamp_ms)
            .ok_or(ReaderError::InvalidTimestamp)?;
        let model = value
            .get("modelId")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .map(str::to_owned)
            .or_else(|| self.session_model.clone())
            .unwrap_or_else(|| "unknown".to_owned());
        let tokens = TokenCounts {
            input: counter(&value, "input")?,
            output: counter(&value, "output")?,
            cache_read: counter(&value, "cacheRead")?,
            cache_write: counter(&value, "cacheWrite")?,
            reasoning: counter(&value, "reasoning")?,
        };
        if tokens_total(tokens)? == 0 {
            return Ok(());
        }
        let identity = value
            .get("responseId")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|identity| !identity.is_empty())
            .map(|identity| format!("antigravity:{session}:{identity}"))
            .unwrap_or_else(|| format!("antigravity:{session}:line:{}", self.ordinal));
        self.ordinal += 1;
        if self.rows.len() == MAX_RESPONSES {
            return Err(ReaderError::Capacity("Antigravity responses"));
        }
        self.rows.push(ResponseUsage {
            key: SessionKey::new("antigravity", session),
            model,
            tokens,
            cache_write_1h: 0,
            timestamp_ms: timestamp,
            dedup_key: Some(identity),
            billing_provider_hint: value
                .get("providerId")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|provider| !provider.is_empty())
                .map(str::to_owned),
            service_tier: None,
            agent: None,
            cost: CostStatus::Unavailable,
        });
        Ok(())
    }

    pub fn snapshot(&self) -> Vec<ResponseUsage> {
        self.rows.clone()
    }

    pub fn has_supported_usage(&self) -> bool {
        !self.rows.is_empty()
    }
}

/// Minimal Antigravity generation-metadata protobuf reader. Supported fields
/// are the ones the pinned `antigravity_cli.rs` reader extracts: chat model
/// `#1`, usage `#4` (`#1` fixed input, `#2` input, `#5` cache read, `#9`
/// output, `#10` reasoning, `#11` response id), and model ids `#19`/`#21`.
/// Step timestamps and trajectory creation time are not decoded; the database
/// file mtime is the documented fallback.
pub struct AntigravityDbReader {
    session: String,
    fallback_timestamp_ms: i64,
    rows: Vec<ResponseUsage>,
    seen: std::collections::BTreeSet<String>,
    ordinal: usize,
}

impl AntigravityDbReader {
    pub fn new(session: impl Into<String>, fallback_timestamp_ms: i64) -> Self {
        Self {
            session: session.into(),
            fallback_timestamp_ms: fallback_timestamp_ms.max(0),
            rows: Vec::new(),
            seen: std::collections::BTreeSet::new(),
            ordinal: 0,
        }
    }

    pub fn ingest_blob(&mut self, blob: &[u8]) -> Result<(), ReaderError> {
        let chat =
            message_field(blob, 1).ok_or(ReaderError::Unsupported("Antigravity chat model"))?;
        let usage = message_field(chat, 4).ok_or(ReaderError::Unsupported("Antigravity usage"))?;
        let input = varint_field(usage, 1)
            .unwrap_or(0)
            .saturating_add(varint_field(usage, 2).unwrap_or(0));
        let cache_read = varint_field(usage, 5).unwrap_or(0);
        let output = varint_field(usage, 9).unwrap_or(0);
        let reasoning = varint_field(usage, 10).unwrap_or(0);
        let tokens = TokenCounts {
            input,
            output,
            cache_read,
            cache_write: 0,
            reasoning,
        };
        if tokens_total(tokens)? == 0 {
            return Ok(());
        }
        let response_id = string_field(usage, 11)
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_owned);
        if let Some(id) = &response_id
            && !self.seen.insert(id.clone())
        {
            return Ok(());
        }
        let model = string_field(chat, 19)
            .map(str::trim)
            .filter(|model| !model.is_empty() && !model.eq_ignore_ascii_case("gemini-default"))
            .or_else(|| {
                string_field(chat, 21).and_then(|label| match label.trim() {
                    "Gemini 3.5 Flash (Low)" => Some("gemini-3.5-flash-extra-low"),
                    "Gemini 3.5 Flash (Medium)" => Some("gemini-3.5-flash-medium"),
                    "Gemini 3.5 Flash (High)" => Some("gemini-3.5-flash-high"),
                    _ => None,
                })
            })
            .or_else(|| string_field(chat, 19))
            .unwrap_or("unknown");
        if !super::tencent_buddy::valid_identity(model) {
            return Err(ReaderError::InvalidRecord);
        }
        let identity = response_id
            .map(|id| format!("antigravity:{}:{id}", self.session))
            .unwrap_or_else(|| format!("antigravity:{}:line:{}", self.session, self.ordinal));
        self.ordinal += 1;
        if self.rows.len() == MAX_RESPONSES {
            return Err(ReaderError::Capacity("Antigravity responses"));
        }
        self.rows.push(ResponseUsage {
            key: SessionKey::new("antigravity", self.session.as_str()),
            model: model.to_owned(),
            tokens,
            cache_write_1h: 0,
            timestamp_ms: self.fallback_timestamp_ms,
            dedup_key: Some(identity),
            billing_provider_hint: None,
            service_tier: None,
            agent: None,
            cost: CostStatus::Unavailable,
        });
        Ok(())
    }

    pub fn snapshot(&self) -> Vec<ResponseUsage> {
        self.rows.clone()
    }

    pub fn has_supported_usage(&self) -> bool {
        !self.rows.is_empty()
    }
}

fn field_iterator(data: &[u8]) -> Vec<(u64, &[u8], u64)> {
    let mut fields = Vec::new();
    let mut index = 0_usize;
    while index < data.len() {
        let Some((tag, next)) = read_varint(data, index) else {
            break;
        };
        index = next;
        let field = tag >> 3;
        let wire = tag & 7;
        match wire {
            0 => {
                let Some((value, next)) = read_varint(data, index) else {
                    break;
                };
                fields.push((field, &[] as &[u8], value));
                index = next;
            }
            1 => {
                if index + 8 > data.len() {
                    break;
                }
                fields.push((field, &data[index..index + 8], 0));
                index += 8;
            }
            2 => {
                let Some((length, next)) = read_varint(data, index) else {
                    break;
                };
                let Ok(length) = usize::try_from(length) else {
                    break;
                };
                if next + length > data.len() {
                    break;
                }
                fields.push((field, &data[next..next + length], 0));
                index = next + length;
            }
            5 => {
                if index + 4 > data.len() {
                    break;
                }
                fields.push((field, &data[index..index + 4], 0));
                index += 4;
            }
            _ => break,
        }
    }
    fields
}

fn read_varint(data: &[u8], mut index: usize) -> Option<(u64, usize)> {
    let mut value = 0_u64;
    for shift in 0..10 {
        let byte = *data.get(index)?;
        index += 1;
        value |= u64::from(byte & 0x7f) << (shift * 7);
        if byte & 0x80 == 0 {
            return Some((value, index));
        }
    }
    None
}

fn message_field(data: &[u8], wanted: u64) -> Option<&[u8]> {
    field_iterator(data)
        .into_iter()
        .find(|(field, _, _)| *field == wanted)
        .map(|(_, bytes, _)| bytes)
}

fn varint_field(data: &[u8], wanted: u64) -> Option<u64> {
    field_iterator(data)
        .into_iter()
        .find(|(field, bytes, _)| {
            *field == wanted && !bytes.is_empty() || *field == wanted && bytes.is_empty()
        })
        .map(|(_, _, value)| value)
}

fn string_field(data: &[u8], wanted: u64) -> Option<&str> {
    message_field(data, wanted).and_then(|bytes| std::str::from_utf8(bytes).ok())
}

fn tokens_total(tokens: TokenCounts) -> Result<u64, ReaderError> {
    super::timestamps::tokens_total(tokens)
}
