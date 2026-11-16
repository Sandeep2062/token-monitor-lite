//! Unsloth Studio inference usage, adapted from the pinned reader
//! `crates/tokscale-core/src/sessions/unsloth.rs`. See
//! docs/migration/parser-provenance.md.
//!
//! Unsloth Studio stores durable chat and API usage in `studio.db` beneath
//! `$UNSLOTH_STUDIO_HOME` (normally `~/.unsloth/studio`). Two lanes are read:
//!
//! - `chat_messages`, where internal Studio responses keep usage in
//!   assistant-message `metadata_json.contextUsage`. The thread's `model_id` is
//!   the last-resort model.
//! - `api_usage_events`, the content-free ledger for authenticated external API
//!   requests. It records only prompt/completion/total counts, so cache and
//!   reasoning stay zero because the table does not carry them.
//!
//! Neither lane selects message content, prompts, attachments or response
//! previews: only scalar usage and routing fields cross the database boundary.
//!
//! # Pricing
//!
//! `responseDetails.providerType` distinguishes local zero-cost inference from
//! recognized metered providers. Local inference is an authoritative zero. A
//! recognized metered route keeps its canonical provider so catalog pricing
//! applies. Anything else — custom, subscription-backed or a future route —
//! carries no provider hint at all, so it stays unpriced rather than being
//! billed against a guess. The external lane is always local.

use super::{CostStatus, ReaderError, ResponseUsage, jsonl::MAX_RECORD_BYTES};
use crate::accounting::{SessionKey, TokenCounts};

pub const MAX_RESPONSES: usize = 16_384;
const MAX_IDENTITY_BYTES: usize = 4_096;
const MAX_RETAINED_IDENTITY_BYTES: usize = 8 * 1024 * 1024;

pub const CLIENT: &str = "unsloth";
pub const STUDIO_AGENT: &str = "Unsloth";
pub const API_AGENT: &str = "Unsloth API";

/// Routes Studio reports that name a billable upstream. The value is the
/// provider namespace the pricing catalog uses.
const METERED_PROVIDERS: [(&str, &str); 9] = [
    ("anthropic", "anthropic"),
    ("deepseek", "deepseek"),
    ("gemini", "google"),
    ("huggingface", "huggingface"),
    // Unsloth names this route by its product brand; the catalog keys it by
    // the company.
    ("kimi", "moonshotai"),
    ("mistral", "mistralai"),
    ("openai", "openai"),
    ("openrouter", "openrouter"),
    ("qwen", "qwen"),
];

/// Which Studio lane a row came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lane {
    /// Internal Studio chat, which carries a provider type.
    Chat,
    /// Authenticated external API request, which runs locally.
    Api,
}

/// One usage row from either lane.
pub struct UnslothRow<'a> {
    pub lane: Lane,
    /// Message id for `Chat`, event id for `Api`. Always the dedup identity.
    pub id: &'a str,
    /// Chat thread id; `None` for the external lane.
    pub thread_id: Option<&'a str>,
    /// Endpoint name, carried as presentation metadata by the external lane.
    pub endpoint: Option<&'a str>,
    pub created_at_secs: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    /// Zero for the external lane, which does not persist these.
    pub cached_tokens: i64,
    pub cache_write_tokens: i64,
    pub reasoning_tokens: i64,
    /// `contextUsage.modelId` — the model that was asked for.
    pub requested_model: Option<&'a str>,
    /// `responseDetails.responseModelId` — the model that answered.
    pub response_model: Option<&'a str>,
    /// `responseDetails.providerType`.
    pub provider_type: Option<&'a str>,
    /// The thread's own `model_id`, the last resort.
    pub thread_model: Option<&'a str>,
}

/// One Unsloth `studio.db`.
pub struct UnslothReader {
    responses: Vec<ResponseUsage>,
    retained_identity_bytes: usize,
}

impl UnslothReader {
    pub fn new() -> Self {
        Self {
            responses: Vec::new(),
            retained_identity_bytes: 0,
        }
    }

    pub fn ingest(&mut self, row: UnslothRow<'_>) -> Result<(), ReaderError> {
        let id = row.id.trim();
        if id.is_empty() || id.len() > MAX_IDENTITY_BYTES {
            return Err(ReaderError::InvalidRecord);
        }
        let Some(tokens) = tokens(&row)? else {
            return Ok(());
        };
        let timestamp_ms = secs_to_ms(row.created_at_secs);
        if timestamp_ms <= 0 {
            return Ok(());
        }

        let (agent, session, dedup_key, cost, billing_provider_hint, title) = match row.lane {
            Lane::Chat => {
                let (provider, authoritative_zero) = provider_for_pricing(row.provider_type);
                let session = match row.thread_id.map(str::trim) {
                    Some(thread) if !thread.is_empty() => thread.to_owned(),
                    _ => format!("unsloth:chat:{id}"),
                };
                (
                    STUDIO_AGENT,
                    session,
                    format!("unsloth:chat:{id}"),
                    if authoritative_zero {
                        CostStatus::Reported(0)
                    } else {
                        CostStatus::Unavailable
                    },
                    provider,
                    None,
                )
            }
            Lane::Api => (
                API_AGENT,
                "unsloth:api".to_owned(),
                format!("unsloth:api:{id}"),
                // The external endpoint ran on this machine.
                CostStatus::Reported(0),
                Some("local".to_owned()),
                clean(row.endpoint),
            ),
        };
        if dedup_key.len() > MAX_IDENTITY_BYTES {
            return Err(ReaderError::Declined("Unsloth identity"));
        }

        let usage = ResponseUsage {
            key: SessionKey::new(CLIENT, &session),
            model: model(&row),
            tokens,
            cache_write_1h: 0,
            timestamp_ms,
            dedup_key: Some(dedup_key.clone()),
            billing_provider_hint,
            service_tier: None,
            agent: Some(agent.to_owned()),
            workspace_key: None,
            workspace_label: None,
            title,
            cost,
        };
        if self.responses.len() == MAX_RESPONSES {
            return Err(ReaderError::Capacity("Unsloth responses"));
        }
        self.retained_identity_bytes = self
            .retained_identity_bytes
            .checked_add(dedup_key.len())
            .filter(|bytes| *bytes <= MAX_RETAINED_IDENTITY_BYTES)
            .ok_or(ReaderError::Capacity("Unsloth identities"))?;
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

impl Default for UnslothReader {
    fn default() -> Self {
        Self::new()
    }
}

/// The responding model wins over the requested one, then the thread's.
fn model(row: &UnslothRow<'_>) -> String {
    clean(row.response_model)
        .or_else(|| clean(row.requested_model))
        .or_else(|| clean(row.thread_model))
        .unwrap_or_else(|| "unknown".to_owned())
}

/// Map a Studio route to the provider namespace pricing uses.
///
/// The flag reports whether the zero cost is authoritative. Local inference is
/// genuinely free; a metered route is not, and an unrecognized route gets no
/// hint at all so nothing is billed against a guess.
fn provider_for_pricing(provider_type: Option<&str>) -> (Option<String>, bool) {
    let Some(provider_type) = clean(provider_type) else {
        // No route recorded: the pinned reader treats that as unpriced too.
        return (None, false);
    };
    let lowered = provider_type.to_ascii_lowercase();
    if lowered == "local" {
        return (Some("local".to_owned()), true);
    }
    for (route, canonical) in METERED_PROVIDERS {
        if lowered == route {
            return (Some(canonical.to_owned()), false);
        }
    }
    // Custom, subscription-backed and future routes carry no reliable billing
    // identity, so they stay unpriced.
    (None, false)
}

/// Move the inclusive cache and reasoning shares out of input/output.
fn tokens(row: &UnslothRow<'_>) -> Result<Option<TokenCounts>, ReaderError> {
    let prompt = counter(row.prompt_tokens)?;
    let completion = counter(row.completion_tokens)?;
    let cache_read = counter(row.cached_tokens)?.min(prompt);
    let cache_write = counter(row.cache_write_tokens)?.min(prompt.saturating_sub(cache_read));
    let reasoning = counter(row.reasoning_tokens)?.min(completion);
    // A reported total larger than the bucket sum is trusted, so a provider
    // that counts differently is not shortened.
    let total = counter(row.total_tokens)?.max(prompt.saturating_add(completion));
    if total == 0 {
        return Ok(None);
    }
    Ok(Some(TokenCounts {
        input: total
            .saturating_sub(completion)
            .saturating_sub(cache_read)
            .saturating_sub(cache_write),
        output: completion.saturating_sub(reasoning),
        cache_read,
        cache_write,
        reasoning,
    }))
}

fn counter(value: i64) -> Result<u64, ReaderError> {
    if value < 0 {
        return Err(ReaderError::InvalidRecord);
    }
    Ok(value as u64)
}

/// Studio stores epoch seconds.
fn secs_to_ms(seconds: i64) -> i64 {
    if seconds <= 0 {
        return 0;
    }
    seconds.saturating_mul(1000)
}

fn clean(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty() && value.len() <= MAX_RECORD_BYTES)
        .map(str::to_owned)
}
