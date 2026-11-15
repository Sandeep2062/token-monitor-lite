//! Zed hosted-thread normalization, adapted from the pinned Tokscale `zed.rs`
//! reader. MIT © 2025 Junho Yeo; see docs/migration/parser-provenance.md.
//!
//! Only threads whose model provider is `zed.dev` are counted; external ACP
//! agents are owned by their own clients. Request usage supersedes the
//! cumulative thread usage. Workspace folders, titles, imported threads and
//! cost are not projected.

use super::timestamps::{parse_timestamp_str, tokens_total};
use super::{CostStatus, ReaderError, ResponseUsage};
use crate::accounting::{SessionKey, TokenCounts};
use serde_json::Value;

const MAX_RESPONSES: usize = 16_384;
const HOSTED_PROVIDER: &str = "zed.dev";

pub struct ZedReader {
    rows: Vec<ResponseUsage>,
}

impl ZedReader {
    pub fn new() -> Self {
        Self { rows: Vec::new() }
    }

    pub fn ingest_thread(
        &mut self,
        thread_id: &str,
        updated_at: &str,
        created_at: Option<&str>,
        thread: &Value,
    ) -> Result<(), ReaderError> {
        if thread.get("imported").and_then(Value::as_bool) == Some(true) {
            return Ok(());
        }
        let model = thread
            .get("model")
            .and_then(Value::as_object)
            .ok_or(ReaderError::Unsupported("Zed thread model"))?;
        let provider = model
            .get("provider")
            .and_then(Value::as_str)
            .map(str::trim)
            .ok_or(ReaderError::Unsupported("Zed thread provider"))?;
        if !provider.eq_ignore_ascii_case(HOSTED_PROVIDER) {
            return Ok(());
        }
        let model_id = model
            .get("model")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .ok_or(ReaderError::Unsupported("Zed thread model id"))?;
        if !super::tencent_buddy::valid_identity(thread_id) {
            return Err(ReaderError::InvalidRecord);
        }
        let Some(tokens) = thread_usage(thread)? else {
            return Ok(());
        };
        let timestamp = created_at
            .and_then(parse_timestamp_str)
            .or_else(|| parse_timestamp_str(updated_at))
            .or_else(|| {
                thread
                    .get("updated_at")
                    .and_then(Value::as_str)
                    .and_then(parse_timestamp_str)
            })
            .ok_or(ReaderError::InvalidTimestamp)?;
        if self.rows.len() == MAX_RESPONSES {
            return Err(ReaderError::Capacity("Zed responses"));
        }
        self.rows.push(ResponseUsage {
            key: SessionKey::new("zed", thread_id),
            model: model_id.to_owned(),
            tokens,
            cache_write_1h: 0,
            timestamp_ms: timestamp,
            dedup_key: Some(format!("zed:{thread_id}")),
            billing_provider_hint: Some(HOSTED_PROVIDER.to_owned()),
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

impl Default for ZedReader {
    fn default() -> Self {
        Self::new()
    }
}

fn thread_usage(thread: &Value) -> Result<Option<TokenCounts>, ReaderError> {
    if let Some(request) = thread.get("request_token_usage") {
        let mut total = TokenCounts::default();
        let usages: Box<dyn Iterator<Item = &Value> + '_> = match request {
            Value::Object(map) => Box::new(map.values()),
            Value::Array(values) => Box::new(values.iter()),
            _ => return Err(ReaderError::InvalidRecord),
        };
        for usage in usages {
            let tokens = token_usage(usage)?;
            if tokens_total(tokens)? > 0 {
                total = total
                    .checked_add(tokens)
                    .map_err(|_| ReaderError::Overflow)?;
            }
        }
        if tokens_total(total)? > 0 {
            return Ok(Some(total));
        }
    }
    let Some(cumulative) = thread.get("cumulative_token_usage") else {
        return Ok(None);
    };
    let tokens = token_usage(cumulative)?;
    Ok((tokens_total(tokens)? > 0).then_some(tokens))
}

fn token_usage(value: &Value) -> Result<TokenCounts, ReaderError> {
    Ok(TokenCounts {
        input: counter(value, "input_tokens")?,
        output: counter(value, "output_tokens")?,
        cache_read: counter(value, "cache_read_input_tokens")?,
        cache_write: counter(value, "cache_creation_input_tokens")?,
        reasoning: 0,
    })
}

fn counter(value: &Value, key: &str) -> Result<u64, ReaderError> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(0),
        Some(value) => value
            .as_u64()
            .or_else(|| {
                value
                    .as_i64()
                    .filter(|number| *number >= 0)
                    .and_then(|number| u64::try_from(number).ok())
            })
            .or_else(|| value.as_str().and_then(|text| text.parse::<u64>().ok()))
            .ok_or(ReaderError::InvalidRecord),
    }
}
