//! Bounded source reading. Client-specific parsers are separate modules/tasks.

pub mod claude;
pub mod codex;
pub mod jsonl;

use crate::accounting::{SessionKey, TokenCounts};

/// Captured usage, not price/provider-inference or live-activity evidence.
/// Timestamp is the response's request-start time when the source supplies it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResponseUsage {
    pub key: SessionKey,
    pub model: String,
    pub tokens: TokenCounts,
    pub cache_write_1h: u64,
    pub timestamp_ms: i64,
    /// Parser-defined stable response identity. `None` is deliberately never
    /// deduplicated across files: matching timestamps or token values are not
    /// sufficient identity evidence.
    pub dedup_key: Option<String>,
    pub billing_provider_hint: Option<String>,
    pub service_tier: Option<String>,
    /// Optional subagent/sidechain display identity. Not part of request identity.
    pub agent: Option<String>,
    pub cost: CostStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CostStatus {
    Unavailable,
}

/// Diagnostics contain only fixed field names, never source values or text.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ReaderError {
    #[error("invalid source record or token counter")]
    InvalidRecord,
    #[error("invalid or missing response timestamp")]
    InvalidTimestamp,
    #[error("unsupported source coverage: {0}")]
    Unsupported(&'static str),
    #[error("source reader capacity exceeded: {0}")]
    Capacity(&'static str),
    #[error("token count overflow")]
    Overflow,
}
