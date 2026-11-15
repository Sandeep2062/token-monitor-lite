//! Cherry Studio built-in chat usage, adapted from the pinned reader
//! `crates/tokscale-core/src/sessions/cherrystudio.rs`. See
//! docs/migration/parser-provenance.md.
//!
//! Cherry Studio keeps two unrelated ledgers. Its Agent / Claude Code mode
//! writes ordinary Claude Code transcripts under the `.claude/projects` roots,
//! which this codebase's `claude` client already reads. Its built-in chat
//! surfaces instead record one row per model invocation in
//! `ai_usage_record`, which is what this reader covers. Reading both would
//! count Agent mode twice, so the transcript lane is deliberately out of scope
//! here and that is a scope decision, not a missing feature.
//!
//! # Bucket accounting
//!
//! The ledger's `input_tokens` and `output_tokens` are **inclusive**: the
//! prompt count already contains both cache buckets and the completion count
//! already contains reasoning. `TokenCounts` models five non-overlapping
//! buckets, so the cache and reasoning shares are moved out of
//! input/output rather than added on top of them. Adding them would count
//! every cache write and every reasoning token twice in the total.
//!
//! When the build has its own `no_cache_tokens` column that value is preferred,
//! because it is exactly the bucket `TokenCounts::input` models and it is the
//! app's own arithmetic rather than a re-derivation of it.
//!
//! # Scope and exclusions
//!
//! - Rows tagged anything other than `chat` are skipped: `agent-session` rows
//!   are the same billed calls the `claude` transcripts already report.
//! - `record_kind = 'legacy-aggregate'` rows are pre-summed history for the
//!   same calls, so reading them beside per-invocation rows double counts.
//! - The schema is probed rather than assumed. Cherry Studio has shipped
//!   several spellings of this table, and naming a column a build does not
//!   have would fail the whole statement to prepare and take the lane to zero.
//! - Cost is taken from the ledger's own `cost` column only when it is zero
//!   (authoritative in any currency) or denominated in USD. A nonzero amount in
//!   another or unknown currency is left to pricing rather than copied as USD.
//!
//! Not implemented, recorded as unsupported coverage rather than guessed: the
//! `total_tokens` column (a total cannot supply individual buckets), and the
//! dedicated `model`/`topic` history joins beyond the optional session title.

use super::{CostStatus, ReaderError, ResponseUsage, jsonl::MAX_RECORD_BYTES};
use crate::accounting::{SessionKey, TokenCounts};

pub const MAX_RESPONSES: usize = 16_384;
const MAX_IDENTITY_BYTES: usize = 4_096;
const MAX_RETAINED_IDENTITY_BYTES: usize = 8 * 1024 * 1024;

pub const CLIENT: &str = "cherrystudio";

/// The `message_kind` stamped on a built-in chat turn, as opposed to
/// `agent-session` (Agent / Claude Code mode, covered by the transcripts).
pub const CHAT_MESSAGE_KIND: &str = "chat";

/// Nano-USD per US dollar.
const NANO_USD: f64 = 1_000_000_000.0;

/// One `ai_usage_record` row, in the column order [`crate::source_inputs`]
/// selects. Every numeric and text field is nullable because the table's
/// columns vary between builds.
pub struct CherryRow<'a> {
    /// The ledger's own primary key, which is stable even when `created_at`
    /// collides.
    pub id: &'a str,
    pub message_id: Option<&'a str>,
    pub session_title: Option<&'a str>,
    pub provider_id: Option<&'a str>,
    pub model_id: Option<&'a str>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cache_write_tokens: Option<i64>,
    pub reasoning_tokens: Option<i64>,
    /// The ledger's own uncached share of the prompt, when this build has it.
    pub no_cache_tokens: Option<i64>,
    pub cost: Option<f64>,
    pub cost_currency: Option<&'a str>,
    pub created_at: Option<i64>,
    pub message_kind: Option<&'a str>,
}

/// One Cherry Studio chat-ledger source.
pub struct CherryReader {
    responses: Vec<ResponseUsage>,
    retained_identity_bytes: usize,
}

impl CherryReader {
    pub fn new() -> Self {
        Self {
            responses: Vec::new(),
            retained_identity_bytes: 0,
        }
    }

    pub fn ingest(&mut self, row: CherryRow<'_>) -> Result<(), ReaderError> {
        // Belt and braces: the filter runs in SQL when the column exists, but a
        // future spelling must not silently slip through and re-count Agent
        // mode against the transcripts.
        if row
            .message_kind
            .is_some_and(|kind| kind != CHAT_MESSAGE_KIND)
        {
            return Ok(());
        }
        if row.id.is_empty() || row.id.len() > MAX_IDENTITY_BYTES {
            return Err(ReaderError::InvalidRecord);
        }

        let tokens = tokens(&row)?;
        // The ledger's nullable `cost_currency` has no USD default, so a
        // missing currency cannot identify a nonzero USD charge. An explicit
        // zero is authoritative in every currency.
        let reported = row.cost.filter(|cost| {
            *cost == 0.0
                || row
                    .cost_currency
                    .is_some_and(|c| c.eq_ignore_ascii_case("USD"))
        });
        if zero(&tokens) && !reported.is_some_and(|cost| cost > 0.0) {
            return Ok(());
        }

        let model = clean(row.model_id).unwrap_or_else(|| "unknown".to_owned());
        if model == "<synthetic>" {
            return Ok(());
        }
        let provider = clean(row.provider_id);
        // The provider is the grouping key that keeps turns together in the
        // Sessions view, because most rows name neither a topic nor a
        // conversation.
        let session = provider.clone().unwrap_or_else(|| "unknown".to_owned());
        let timestamp_ms = row
            .created_at
            .filter(|created_at| *created_at > 0)
            .unwrap_or(0);
        let session_title = clean(row.session_title);

        let cost = match reported {
            Some(cost) => CostStatus::Reported(nano_usd(cost)?),
            None => CostStatus::Unavailable,
        };

        let usage = ResponseUsage {
            key: SessionKey::new(CLIENT, &session),
            model,
            tokens,
            cache_write_1h: 0,
            timestamp_ms,
            // The ledger's own primary key is a stable identity even when
            // `created_at` collides.
            dedup_key: Some(format!("cherrystudio-sqlite:{}", row.id)),
            // The row's provider id is an explicit record of who was billed.
            billing_provider_hint: provider,
            service_tier: None,
            agent: None,
            workspace_key: None,
            workspace_label: None,
            title: session_title,
            cost,
        };
        self.push(usage)
    }

    fn push(&mut self, usage: ResponseUsage) -> Result<(), ReaderError> {
        if self.responses.len() == MAX_RESPONSES {
            return Err(ReaderError::Capacity("Cherry Studio responses"));
        }
        let identity_bytes = usage.dedup_key.as_ref().map_or(0, String::len);
        self.retained_identity_bytes = self
            .retained_identity_bytes
            .checked_add(identity_bytes)
            .filter(|bytes| *bytes <= MAX_RETAINED_IDENTITY_BYTES)
            .ok_or(ReaderError::Capacity("Cherry Studio identities"))?;
        self.responses.push(usage);
        Ok(())
    }

    pub fn snapshot(&self) -> Vec<ResponseUsage> {
        self.responses.clone()
    }

    pub fn has_supported_usage(&self) -> bool {
        !self.responses.is_empty()
    }
}

impl Default for CherryReader {
    fn default() -> Self {
        Self::new()
    }
}

/// Move the inclusive cache and reasoning shares out of input/output.
///
/// `input_tokens` is the whole prompt and `output_tokens` the whole
/// completion, with the cache and reasoning shares counted again in their own
/// columns — the same shape mismatch documented for ZCode. Feeding the raw
/// columns through would double count every cache write and reasoning token.
fn tokens(row: &CherryRow<'_>) -> Result<TokenCounts, ReaderError> {
    let cache_read = counter(row.cache_read_tokens)?;
    let cache_write = counter(row.cache_write_tokens)?;
    let reasoning = counter(row.reasoning_tokens)?;
    // Prefer the ledger's own uncached count; otherwise subtract both cache
    // buckets, which is the arithmetic Cherry Studio itself uses.
    let input = match row.no_cache_tokens {
        Some(no_cache) => counter(Some(no_cache))?,
        None => counter(row.input_tokens)?
            .saturating_sub(cache_read)
            .saturating_sub(cache_write),
    };
    // `output_tokens` already contains the reasoning share.
    let output = counter(row.output_tokens)?.saturating_sub(reasoning);
    Ok(TokenCounts {
        input,
        output,
        cache_read,
        cache_write,
        reasoning,
    })
}

/// A negative column is a visible rejection, not a clamp: the ledger writes
/// unsigned counters, so a negative value means the row is not trustworthy.
fn counter(value: Option<i64>) -> Result<u64, ReaderError> {
    match value {
        None => Ok(0),
        Some(value) => u64::try_from(value).map_err(|_| ReaderError::InvalidRecord),
    }
}

/// A row with no counted tokens carries no usage unless it also reports cost.
fn zero(tokens: &TokenCounts) -> bool {
    tokens.input == 0
        && tokens.output == 0
        && tokens.cache_read == 0
        && tokens.cache_write == 0
        && tokens.reasoning == 0
}

fn nano_usd(dollars: f64) -> Result<u64, ReaderError> {
    if !dollars.is_finite() || dollars < 0.0 {
        return Err(ReaderError::InvalidRecord);
    }
    let nanos = dollars * NANO_USD;
    if nanos > u64::MAX as f64 {
        return Err(ReaderError::Overflow);
    }
    Ok(nanos.round() as u64)
}

/// Strip null/blank spellings so an empty column degrades to `None` instead of
/// becoming an empty model or provider id.
fn clean(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty() && value.len() <= MAX_RECORD_BYTES)
        .map(str::to_owned)
}
