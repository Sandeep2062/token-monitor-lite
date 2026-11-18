//! Collector tests.
//!
//! Every response is replayed. Nothing here contacts a network or reads a real
//! keyring: the transport is a recording and the credential store is the
//! in-memory test backend. Spec §5 labels these claims.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use super::*;
use crate::credentials::{
    CredentialError, CredentialMaterial, CredentialService, TestCredentialStore,
};
use crate::limits::provider::LimitSource;
use crate::limits::runtime::ProbeDeadline;
use crate::limits::transport::RecordedTransport;

const CLAUDE: &str = "claude-oauth";
const CODEX: &str = "codex-oauth";

fn deadline() -> ProbeDeadline {
    ProbeDeadline::new(5_000).expect("deadline")
}

fn meta() -> ProviderMeta {
    ProviderMeta {
        account_key: "account-1".to_owned(),
        updated_at: Some("2026-11-15T12:00:00Z".to_owned()),
        ..ProviderMeta::default()
    }
}

/// A transport that counts calls, so "no request" is provable.
#[derive(Debug, Default)]
struct CountingTransport {
    calls: AtomicUsize,
    inner: RecordedTransport,
}

impl CountingTransport {
    fn new(responses: Vec<(u16, String)>) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            inner: RecordedTransport::new(responses),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl LimitsTransport for CountingTransport {
    fn fetch(
        &self,
        endpoint: &ProbeEndpoint,
        token: &str,
        deadline: Duration,
    ) -> Result<ProbeResponse, TransportFailure> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(
            token, "test-token",
            "the collector must pass the stored secret"
        );
        self.inner.fetch(endpoint, token, deadline)
    }
}

fn service_with(
    credential_id: Option<(&str, &str)>,
) -> (Arc<CredentialService>, CredentialTickets) {
    let tickets = CredentialTickets::default();
    let store = TestCredentialStore::default();
    let service = CredentialService::start_test(store.clone()).expect("service");
    if let Some((id, token)) = credential_id {
        service
            .set(
                tickets.next(),
                &crate::credentials::stable_credential_id(id),
                CredentialMaterial::from_secret_bytes(token.as_bytes().to_vec()).expect("material"),
            )
            .expect("store");
    }
    (Arc::new(service), tickets)
}

#[test]
fn a_missing_credential_makes_no_request_at_all() {
    let transport = Arc::new(CountingTransport::new(vec![(200, "{}".to_owned())]));
    let (service, tickets) = service_with(None);
    let collector = LimitsCollector::new(service, transport.clone(), tickets);
    let attempt = collector.probe(ProbeTarget::CLAUDE, deadline(), &meta());
    assert_eq!(attempt.provider.status, LimitStatus::NotConfigured);
    assert_eq!(transport.calls(), 0, "a probe ran without a credential");
    assert!(attempt.provider.windows.is_empty());
}

#[test]
fn a_successful_probe_maps_the_recorded_payload() {
    let body = serde_json::json!({
        "five_hour": { "utilization": 30.0, "resets_at": "2026-11-15T17:00:00Z" },
        "seven_day": { "utilization": 10.0 }
    })
    .to_string();
    let transport = Arc::new(CountingTransport::new(vec![(200, body)]));
    let (service, tickets) = service_with(Some((CLAUDE, "test-token")));
    let collector = LimitsCollector::new(service, transport.clone(), tickets);
    let attempt = collector.probe(ProbeTarget::CLAUDE, deadline(), &meta());
    assert_eq!(attempt.provider.status, LimitStatus::Ok);
    assert_eq!(attempt.provider.windows.len(), 2);
    assert_eq!(transport.calls(), 1);
}

#[test]
fn a_rejected_credential_is_unauthorized_and_keeps_the_row_empty() {
    let transport = Arc::new(CountingTransport::new(vec![(401, "{}".to_owned())]));
    let (service, tickets) = service_with(Some((CODEX, "test-token")));
    let collector = LimitsCollector::new(service, transport.clone(), tickets);
    let attempt = collector.probe(ProbeTarget::CODEX, deadline(), &meta());
    assert_eq!(attempt.provider.status, LimitStatus::Unauthorized);
    assert!(attempt.provider.windows.is_empty());
    assert_eq!(attempt.provider.source, LimitSource::Oauth);
}

#[test]
fn a_throttle_is_reported_as_rate_limited() {
    let transport = Arc::new(CountingTransport::new(vec![(429, "{}".to_owned())]));
    let (service, tickets) = service_with(Some((CLAUDE, "test-token")));
    let collector = LimitsCollector::new(service, transport.clone(), tickets);
    let attempt = collector.probe(ProbeTarget::CLAUDE, deadline(), &meta());
    assert_eq!(attempt.provider.status, LimitStatus::RateLimited);
}

#[test]
fn a_success_with_an_unreadable_body_is_unavailable_not_ok() {
    let transport = Arc::new(CountingTransport::new(vec![(200, "not json".to_owned())]));
    let (service, tickets) = service_with(Some((CLAUDE, "test-token")));
    let collector = LimitsCollector::new(service, transport.clone(), tickets);
    let attempt = collector.probe(ProbeTarget::CLAUDE, deadline(), &meta());
    assert_eq!(attempt.provider.status, LimitStatus::Unavailable);
}

#[test]
fn a_success_with_a_json_body_that_has_no_windows_is_unavailable() {
    let transport = Arc::new(CountingTransport::new(vec![(
        200,
        r#"{"ok":true}"#.to_owned(),
    )]));
    let (service, tickets) = service_with(Some((CODEX, "test-token")));
    let collector = LimitsCollector::new(service, transport.clone(), tickets);
    let attempt = collector.probe(ProbeTarget::CODEX, deadline(), &meta());
    assert_eq!(attempt.provider.status, LimitStatus::Unavailable);
    assert!(attempt.provider.windows.is_empty());
}

#[test]
fn a_broken_transport_is_unreachable_not_success() {
    let transport = Arc::new(CountingTransport::new(Vec::new()));
    let (service, tickets) = service_with(Some((CLAUDE, "test-token")));
    let collector = LimitsCollector::new(service, transport.clone(), tickets);
    let attempt = collector.probe(ProbeTarget::CLAUDE, deadline(), &meta());
    assert_eq!(attempt.provider.status, LimitStatus::Unavailable);
}

#[test]
fn a_secret_that_is_not_utf8_is_refused_before_any_request() {
    let tickets = CredentialTickets::default();
    let store = TestCredentialStore::default();
    let service = Arc::new(CredentialService::start_test(store).expect("service"));
    service
        .set(
            tickets.next(),
            &crate::credentials::stable_credential_id(CLAUDE),
            CredentialMaterial::from_secret_bytes(vec![0xff, 0xfe]).expect("material"),
        )
        .expect("store");
    let transport = Arc::new(CountingTransport::new(vec![(200, "{}".to_owned())]));
    let collector = LimitsCollector::new(service, transport.clone(), tickets);
    let attempt = collector.probe(ProbeTarget::CLAUDE, deadline(), &meta());
    assert_eq!(attempt.provider.status, LimitStatus::Unavailable);
    assert_eq!(transport.calls(), 0);
}

#[test]
fn every_probe_row_carries_identity_but_never_a_secret() {
    let body = serde_json::json!({
        "five_hour": { "utilization": 5.0 },
        "seven_day": { "utilization": 6.0 }
    })
    .to_string();
    let transport = Arc::new(CountingTransport::new(vec![(200, body)]));
    let (service, tickets) = service_with(Some((CLAUDE, "test-token")));
    let collector = LimitsCollector::new(service, transport.clone(), tickets);
    let attempt = collector.probe(ProbeTarget::CLAUDE, deadline(), &meta());
    assert_eq!(attempt.provider.account_key, "account-1");
    let rendered = serde_json::to_string(&attempt.provider).expect("json");
    assert!(
        !rendered.contains("test-token"),
        "row leaked the credential"
    );
    let debugged = format!("{:?}", attempt);
    assert!(
        !debugged.contains("test-token"),
        "debug leaked the credential"
    );
}

#[test]
fn a_credential_read_that_cannot_reach_the_keyring_is_unavailable() {
    // The platform keyring being unreachable is not "no credential configured":
    // it must not be reported as one, or a user is told to set up something they
    // already set up.
    let tickets = CredentialTickets::default();
    let store = TestCredentialStore::default();
    store.set_unavailable(true);
    let service = Arc::new(CredentialService::start_test(store).expect("service"));
    let transport = Arc::new(CountingTransport::new(vec![(200, "{}".to_owned())]));
    let collector = LimitsCollector::new(service, transport.clone(), tickets);
    let attempt = collector.probe(ProbeTarget::CLAUDE, deadline(), &meta());
    assert_eq!(attempt.provider.status, LimitStatus::Unavailable);
    assert_eq!(transport.calls(), 0);
}

#[test]
fn an_unknown_provider_is_never_probed() {
    assert!(ProbeTarget::for_provider("something-else").is_none());
    assert_eq!(
        ProbeTarget::for_provider("claude"),
        Some(ProbeTarget::CLAUDE)
    );
    assert_eq!(ProbeTarget::for_provider("codex"), Some(ProbeTarget::CODEX));
}

#[test]
fn exactly_two_providers_are_wired() {
    let providers: Vec<&str> = LimitsCollector::<RecordedTransport>::targets()
        .iter()
        .map(|target| target.endpoint.provider)
        .collect();
    assert_eq!(providers, vec!["claude", "codex"]);
}

#[test]
fn a_secret_is_never_returned_by_a_failed_read() {
    // The sanity check that makes the boundary meaningful: the service reports a
    // missing entry as absence, not as an empty secret, so an empty token can
    // never be sent as a bearer.
    let tickets = CredentialTickets::default();
    let store = TestCredentialStore::default();
    let service = CredentialService::start_test(store).expect("service");
    let presented = service
        .authorize(
            tickets.next(),
            &crate::credentials::stable_credential_id(CLAUDE),
            |secret| secret.len(),
        )
        .expect("read");
    assert_eq!(presented, None);
}

#[test]
fn each_read_consumes_a_monotonic_ticket() {
    // The service raises its recorded generation on every request and refuses a
    // lower one as stale, so a fixed generation would work once and then fail.
    let (service, tickets) = service_with(Some((CLAUDE, "test-token")));
    for _ in 0..3 {
        let presented = service
            .authorize(
                tickets.next(),
                &crate::credentials::stable_credential_id(CLAUDE),
                |secret| secret.len(),
            )
            .expect("read");
        assert_eq!(presented, Some("test-token".len()));
    }
    // A ticket behind the current one is refused rather than silently served.
    let stale = tickets.current().saturating_sub(1);
    let refused = service.authorize(
        stale,
        &crate::credentials::stable_credential_id(CLAUDE),
        |secret| secret.len(),
    );
    assert!(matches!(refused, Err(CredentialError::StaleGeneration)));
}

#[test]
fn tickets_never_regress() {
    let tickets = CredentialTickets::new(5);
    assert_eq!(tickets.next(), 6);
    assert_eq!(tickets.next(), 7);
    assert_eq!(tickets.current(), 7);
}
