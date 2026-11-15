//! Bounded source reading. Client-specific parsers are separate modules/tasks.

pub mod amp;
pub mod antigravity;
pub mod claude;
pub mod cline;
pub mod codebuddy;
pub mod codex;
pub mod copilot;
pub mod cursor;
pub mod droid;
pub mod gemini;
pub mod goose;
pub mod hermes;
pub mod jsonl;
pub mod kilo;
pub mod mcode;
pub mod muse;
pub mod omp;
pub mod openclaw;
pub mod opencode;
pub mod pi;
pub mod qwen;
pub mod reasonix;
pub mod senpi;
pub mod tencent_buddy;
pub mod timestamps;
pub mod workbuddy;
pub mod zed;

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
    /// Source-recorded workspace/project identity, when the source carries one.
    pub workspace_key: Option<String>,
    pub workspace_label: Option<String>,
    /// Source-recorded session/turn title. Presentation metadata only.
    pub title: Option<String>,
    pub cost: CostStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CostStatus {
    Unavailable,
    /// Integer nano-USD reported by the source record.
    Reported(u64),
    /// Integer nano-USD estimated from a source-reported rate.
    Estimated(u64),
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
