//! The collector: credential → transport → mapped rows.
//!
//! Ports `collectLimitProviders` from `src/shared/limits/collector.js` and the
//! `probe` half of `providers/claude/limits.js` / `providers/codex/limits.js`.
//!
//! Two boundaries are absolute:
//!
//! * The credential comes from [`CredentialService`] and nowhere else. There is
//!   no plaintext fallback, no environment lookup and no file lane. When the
//!   keyring has no entry the outcome is `notConfigured`, not an attempt at a
//!   different source.
//! * A probe cannot be started by a UI event. [`LimitsCollector::probe`] is
//!   reached only from a [`super::runtime::Trigger`] path, and the transport is
//!   injected, so a test can prove no request is made.
//!
//! Nothing in this module formats, compares or stores a credential, and no error
//! carries one.

use std::sync::Arc;

use serde_json::Value;

use crate::credentials::{CredentialError, CredentialService};

use super::provider::LimitSource;
use super::providers::{self, ProviderMeta};
use super::runtime::ProbeDeadline;
use super::status::LimitStatus;
use super::store::Attempt;
use super::transport::{
    CLAUDE_USAGE, CODEX_USAGE, LimitsTransport, ProbeEndpoint, ProbeResponse, TransportFailure,
};

/// The credential-service generation ticket.
///
/// The service treats a generation as a monotonic ticket: every request *raises*
/// the recorded generation and a lower one is refused as stale
/// (`check_generation`). A read is therefore not free — it consumes a ticket —
/// so the collector cannot invent a constant. It shares the application's
/// counter, which is what [`CredentialTickets::next`] does. Spec §3.2.
#[derive(Debug, Clone, Default)]
pub struct CredentialTickets {
    generation: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl CredentialTickets {
    pub fn new(start: u64) -> Self {
        Self {
            generation: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(start)),
        }
    }

    /// Allocate the next ticket. Monotonic, so it never regresses.
    pub fn next(&self) -> u64 {
        self.generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            .saturating_add(1)
    }

    /// The most recently allocated ticket.
    pub fn current(&self) -> u64 {
        self.generation.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// One provider the collector knows how to probe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeTarget {
    pub endpoint: ProbeEndpoint,
}

impl ProbeTarget {
    pub const CLAUDE: Self = Self {
        endpoint: CLAUDE_USAGE,
    };
    pub const CODEX: Self = Self {
        endpoint: CODEX_USAGE,
    };

    pub fn all() -> [Self; 2] {
        [Self::CLAUDE, Self::CODEX]
    }

    pub fn for_provider(provider: &str) -> Option<Self> {
        Self::all()
            .into_iter()
            .find(|target| target.endpoint.provider == provider)
    }
}

/// The collector. Owns no thread and runs no loop; a caller drives it.
#[derive(Debug)]
pub struct LimitsCollector<T: LimitsTransport> {
    credentials: Arc<CredentialService>,
    transport: Arc<T>,
    tickets: CredentialTickets,
}

/// What one authenticated attempt produced. Never carries the credential.
#[derive(Clone, Debug, PartialEq, Eq)]
enum AttemptOutcome {
    Response(ProbeResponse),
    Transport(TransportFailure),
    MissingCredential,
    UnavailableSecret,
}

impl<T: LimitsTransport> LimitsCollector<T> {
    pub fn new(
        credentials: Arc<CredentialService>,
        transport: Arc<T>,
        tickets: CredentialTickets,
    ) -> Self {
        Self {
            credentials,
            transport,
            tickets,
        }
    }

    /// Probe one provider.
    ///
    /// Never panics, never retries and never sleeps: the caller schedules it. A
    /// probe that cannot read a credential makes no request at all, so a missing
    /// keyring entry cannot look like a network failure.
    pub fn probe(
        &self,
        target: ProbeTarget,
        deadline: ProbeDeadline,
        meta: &ProviderMeta,
    ) -> Attempt {
        let source = LimitSource::Oauth;
        let provider = target.endpoint.provider;
        let status = match self.attempt(target, deadline) {
            Ok(AttemptOutcome::Response(response)) if response.is_success() => {
                return self.map_success(provider, &response, meta);
            }
            Ok(AttemptOutcome::Response(response)) => response.failure_status(),
            Ok(AttemptOutcome::Transport(TransportFailure::Timeout)) => LimitStatus::Timeout,
            Ok(AttemptOutcome::Transport(TransportFailure::Unreachable)) => {
                LimitStatus::Unavailable
            }
            Ok(AttemptOutcome::MissingCredential) => LimitStatus::NotConfigured,
            Ok(AttemptOutcome::UnavailableSecret) => LimitStatus::Unavailable,
            Err(CredentialError::Unavailable) => LimitStatus::Unavailable,
            Err(CredentialError::TimedOut | CredentialError::Busy) => LimitStatus::Timeout,
            Err(CredentialError::Closed | CredentialError::WorkerPanicked) => LimitStatus::Error,
            Err(_) => LimitStatus::NotConfigured,
        };
        providers::failure(provider, status, source, meta)
    }

    /// One authenticated request, with the secret borrowed only for its duration.
    fn attempt(
        &self,
        target: ProbeTarget,
        deadline: ProbeDeadline,
    ) -> Result<AttemptOutcome, CredentialError> {
        let outcome = self.credentials.authorize(
            self.tickets.next(),
            &target.endpoint.credential_id(),
            |secret| {
                if secret.is_empty() {
                    return AttemptOutcome::MissingCredential;
                }
                // A token that is not UTF-8 is not a bearer token.
                let Ok(token) = std::str::from_utf8(secret) else {
                    return AttemptOutcome::UnavailableSecret;
                };
                match self
                    .transport
                    .fetch(&target.endpoint, token, deadline.deadline())
                {
                    Ok(response) => AttemptOutcome::Response(response),
                    Err(failure) => AttemptOutcome::Transport(failure),
                }
            },
        )?;
        Ok(outcome.unwrap_or(AttemptOutcome::MissingCredential))
    }

    /// Map a successful body to a row, or say why it was not usable.
    fn map_success(
        &self,
        provider: &str,
        response: &ProbeResponse,
        meta: &ProviderMeta,
    ) -> Attempt {
        let payload: Value = serde_json::from_str(&response.body).unwrap_or(Value::Null);
        let mapped = match provider {
            "claude" => providers::claude_from_usage(&payload, meta),
            "codex" => providers::codex_from_rate_limits(&payload, meta),
            _ => None,
        };
        mapped.unwrap_or_else(|| {
            // A 2xx with no recognizable quota is not "ok": it is a body this
            // build cannot read, which is what `unavailable` means.
            providers::failure(provider, LimitStatus::Unavailable, LimitSource::Oauth, meta)
        })
    }

    /// The targets this build can probe.
    pub fn targets() -> [ProbeTarget; 2] {
        ProbeTarget::all()
    }
}

#[cfg(test)]
mod tests;
