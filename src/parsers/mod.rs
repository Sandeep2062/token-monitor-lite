//! Bounded source reading. Client-specific parsers are separate modules/tasks.

pub mod amp;
pub mod antigravity;
pub mod augment;
pub mod catpaw;
pub mod cherrystudio;
pub mod claude;
pub mod cline;
pub mod codebuddy;
pub mod codebuff;
pub mod codex;
pub mod commandcode;
pub mod copilot;
pub mod cursor;
pub mod devin;
pub mod droid;
pub mod dsh;
pub mod fx;
pub mod gemini;
pub mod gjc;
pub mod goose;
pub mod grok;
pub mod hermes;
pub mod jcode;
pub mod jsonl;
pub mod junie;
pub mod kilo;
pub mod kimchi;
pub mod kimi;
pub mod kiro;
pub mod lmstudio;
pub mod mcode;
pub mod mimo;
pub mod mistral;
pub mod muse;
pub mod mux;
pub mod omp;
pub mod openclaw;
pub mod opencode;
pub mod opencodereview;
pub mod pi;
pub mod prime_agent;
pub mod proma;
pub mod qodercn;
pub mod qwen;
pub mod reasonix;
pub mod senpi;
pub mod tencent_buddy;
pub mod timestamps;
pub mod trae;
pub mod unsloth;
pub mod workbuddy;
pub mod zcode;
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
    /// A single record cannot be interpreted, but the source as a whole is
    /// still sound. The driver skips this record, counts it, and publishes
    /// every other record in the same generation.
    ///
    /// This is the record-level counterpart of [`Self::Unsupported`]. Use it
    /// whenever the failure is confined to one record: a missing or conflicting
    /// identity, an unparseable timestamp, a counter that does not reconcile.
    /// Rejecting the whole source for one of those freezes the source's totals
    /// permanently, because a log only grows and the same record is re-read
    /// every pass.
    #[error("declined record: {0}")]
    Declined(&'static str),
    /// The source as a whole cannot be interpreted: framing, schema version,
    /// ownership, or a structural contract. The complete candidate is rejected
    /// and the prior committed generation is retained.
    #[error("unsupported source coverage: {0}")]
    Unsupported(&'static str),
    #[error("source reader capacity exceeded: {0}")]
    Capacity(&'static str),
    #[error("token count overflow")]
    Overflow,
}
