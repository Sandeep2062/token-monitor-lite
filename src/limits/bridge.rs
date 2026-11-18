//! The bridges between the limits lane and the rest of the application.
//!
//! Two directions:
//!
//! * [`collector_source`] turns the real collector into a [`ProbeSource`], so
//!   the scheduler can run it without knowing about HTTP or the keyring.
//! * [`apply`] hands a published snapshot to the UI controller.
//!
//! Nothing here spawns or polls. `main.rs` owns the worker's lifetime.

use std::sync::Arc;

use crate::credentials::CredentialService;

use super::collector::{CredentialTickets, LimitsCollector, ProbeTarget};
use super::provider::LimitSource;
use super::providers::ProviderMeta;
use super::runtime::ProbeDeadline;
use super::scheduler::ProbeSource;
use super::store::Attempt;
use super::transport::HttpsTransport;

/// The providers this build can probe, as their catalog ids.
pub const PROVIDERS: [&str; 2] = ["claude", "codex"];

/// The real probe source.
///
/// Every probe goes through [`LimitsCollector`], which reads the credential from
/// the supplied service and nowhere else. A provider this build does not know is
/// refused here rather than at the network.
pub struct CollectorSource {
    collector: LimitsCollector<HttpsTransport>,
}

impl CollectorSource {
    pub fn new(credentials: Arc<CredentialService>, tickets: CredentialTickets) -> Self {
        Self {
            collector: LimitsCollector::new(credentials, Arc::new(HttpsTransport), tickets),
        }
    }

    pub fn known(provider: &str) -> bool {
        PROVIDERS.contains(&provider)
    }
}

impl ProbeSource for CollectorSource {
    fn probe(&self, provider: &str, deadline: ProbeDeadline) -> Vec<Attempt> {
        let Some(target) = ProbeTarget::for_provider(provider) else {
            return Vec::new();
        };
        let meta = ProviderMeta::default();
        let attempt = self.collector.probe(target, deadline, &meta);
        // A row that could not even identify a source is still a row: its status
        // is what tells a reader whether to sign in or wait.
        vec![attempt]
    }
}

/// Merge a published limits snapshot into a controller.
///
/// The controller decides whether anything changed, so this is safe to call on
/// every delivery.
pub fn apply(
    controller: &mut crate::ui::UiController,
    limits: &super::scheduler::LimitsSnapshot,
    now_ms: i64,
) -> bool {
    let notice = match (&limits.rows, &limits.notice) {
        (rows, Some(notice)) if rows.is_empty() => Some(notice.clone()),
        _ => None,
    };
    controller.accept_limits(limits.rows.clone(), notice, now_ms)
}

/// The status a provider row carries when it was never probed.
pub fn never_probed(provider: &str) -> Attempt {
    let mut row = super::provider::LimitsProvider::new(
        provider,
        super::status::LimitStatus::NotConfigured,
        LimitSource::Oauth,
    );
    row.account_label = String::new();
    Attempt::new(row)
}

/// Whether a provider has a credential configured, without reading it.
///
/// The limits lane starts enabled only for providers the user actually selected
/// and for which a credential exists, so an unconfigured provider is never
/// probed at all.
pub fn should_probe(credentials: &CredentialService, provider: &str, generation: u64) -> bool {
    let Some(target) = ProbeTarget::for_provider(provider) else {
        return false;
    };
    credentials
        .authorize(generation, &target.endpoint.credential_id(), |secret| {
            !secret.is_empty()
        })
        .is_ok_and(|present| present == Some(true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::TestCredentialStore;

    #[test]
    fn only_the_two_wired_providers_are_known() {
        assert!(CollectorSource::known("claude"));
        assert!(CollectorSource::known("codex"));
        // Every other catalog client is a usage source, not a quota source.
        for other in ["gemini", "cursor", "copilot", "roocode", ""] {
            assert!(
                !CollectorSource::known(other),
                "{other} claimed a quota lane"
            );
        }
    }

    #[test]
    fn an_unknown_provider_probes_nothing() {
        let store = TestCredentialStore::default();
        let credentials = Arc::new(CredentialService::start_test(store).expect("service"));
        let tickets = CredentialTickets::default();
        let source = CollectorSource::new(credentials, tickets);
        let rows = source.probe("gemini", ProbeDeadline::for_accounts(1));
        assert!(rows.is_empty());
    }

    #[test]
    fn a_provider_without_a_credential_is_not_enabled() {
        let store = TestCredentialStore::default();
        let credentials = CredentialService::start_test(store).expect("service");
        assert!(!should_probe(&credentials, "claude", 1));
        assert!(!should_probe(&credentials, "codex", 1));
        // An unknown provider is never enabled, whatever the store holds.
        assert!(!should_probe(&credentials, "gemini", 1));
    }

    #[test]
    fn a_provider_with_a_credential_is_enabled() {
        let store = TestCredentialStore::default();
        let credentials = CredentialService::start_test(store).expect("service");
        credentials
            .set(
                1,
                &crate::credentials::stable_credential_id("claude-oauth"),
                crate::credentials::CredentialMaterial::from_secret_bytes(b"token".to_vec())
                    .expect("material"),
            )
            .expect("store");
        assert!(should_probe(&credentials, "claude", 2));
        // The codex credential was never stored.
        assert!(!should_probe(&credentials, "codex", 3));
    }

    #[test]
    fn a_never_probed_row_is_not_configured() {
        let attempt = never_probed("claude");
        assert_eq!(
            attempt.provider.status,
            crate::limits::status::LimitStatus::NotConfigured
        );
        assert!(attempt.provider.windows.is_empty());
    }
}
