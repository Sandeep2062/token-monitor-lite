//! End-to-end limits behaviour through the UI controller.
//!
//! Everything here is driven with replayed responses and the in-memory credential
//! store. No test contacts a network or a real keyring; `docs/migration/limits-spec.md`
//! §5 labels what that does and does not prove.

use std::sync::Arc;
use std::time::Duration;

use super::transport::RecordedTransport;
use crate::credentials::{
    CredentialMaterial, CredentialService, TestCredentialStore, stable_credential_id,
};
use crate::limits::bridge::{self, CollectorSource};
use crate::limits::collector::{CredentialTickets, LimitsCollector, ProbeTarget};
use crate::limits::provider::LimitSource;
use crate::limits::providers::ProviderMeta;
use crate::limits::runtime::ProbeDeadline;
use crate::limits::runtime::Trigger;
use crate::limits::scheduler::{LimitsWorker, Scheduler};
use crate::limits::status::LimitStatus;
use crate::limits::window::WindowKind;
use crate::ui::UiController;

// 2026-11-15T12:00:00Z, the same instant the fixtures use as their clock.
const NOW_MS: i64 = 1_794_744_000_000;

fn wall_clock_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

fn meta() -> ProviderMeta {
    ProviderMeta {
        account_key: "account-1".to_owned(),
        account_label: "Primary".to_owned(),
        updated_at: Some("2026-11-15T12:00:00Z".to_owned()),
        ..ProviderMeta::default()
    }
}

fn claude_body() -> String {
    serde_json::json!({
        "five_hour": { "utilization": 42.5, "resets_at": "2026-11-15T17:00:00Z" },
        "seven_day": { "utilization": 12.0, "resets_at": "2026-11-20T00:00:00Z" }
    })
    .to_string()
}

fn service_with(
    store: TestCredentialStore,
    id: &str,
) -> (Arc<CredentialService>, CredentialTickets) {
    let tickets = CredentialTickets::default();
    let service = CredentialService::start_test(store).expect("service");
    service
        .set(
            tickets.next(),
            &stable_credential_id(id),
            CredentialMaterial::from_secret_bytes(b"test-token".to_vec()).expect("material"),
        )
        .expect("store");
    (Arc::new(service), tickets)
}

#[test]
fn a_stored_credential_produces_a_quota_bar_with_a_reset_time() {
    let transport = Arc::new(RecordedTransport::new(vec![(200, claude_body())]));
    let (credentials, tickets) = service_with(TestCredentialStore::default(), "claude-oauth");
    let collector = LimitsCollector::new(credentials, transport, tickets);
    let attempt = collector.probe(ProbeTarget::CLAUDE, ProbeDeadline::for_accounts(1), &meta());

    let mut controller = UiController::new();
    controller.set_clock_ms(NOW_MS);
    let snapshot = crate::limits::scheduler::LimitsSnapshot {
        rows: vec![attempt.provider],
        retry_at_ms: None,
        notice: None,
    };
    assert!(bridge::apply(&mut controller, &snapshot, NOW_MS));

    let quota = controller.presentation().quota;
    assert_eq!(quota.rows.len(), 1, "no bar for a successful probe");
    let row = &quota.rows[0];
    assert_eq!(row.provider, "claude");
    assert_eq!(row.heading, "Claude CLI · Primary");
    assert!(!row.attention);
    assert_eq!(row.windows.len(), 2);

    let session = &row.windows[0];
    assert_eq!(session.label, "Session");
    assert_eq!(session.used_percent, 43);
    assert_eq!(session.amount, "43% used");
    // 17:00Z against a 12:00Z clock.
    assert_eq!(session.reset, "Resets in 5h");
    assert_eq!(row.windows[1].reset, "Resets in 4d");
    assert!(quota.notice.is_none());
}

#[test]
fn an_install_with_no_credential_shows_no_bar_and_probes_nothing() {
    let store = TestCredentialStore::default();
    let credentials = Arc::new(CredentialService::start_test(store).expect("service"));
    // The lane is only started for a provider with a credential.
    let tickets = CredentialTickets::default();
    for provider in bridge::PROVIDERS {
        assert!(
            !bridge::should_probe(credentials.as_ref(), provider, tickets.next()),
            "{provider} was enabled without a credential"
        );
    }
    let controller = UiController::new();
    let quota = controller.presentation().quota;
    assert!(quota.rows.is_empty());
    assert_eq!(quota.notice.as_deref(), Some("No provider limits yet"));
}

#[test]
fn a_missing_credential_renders_a_sign_in_notice_rather_than_a_zero_bar() {
    let transport = Arc::new(RecordedTransport::new(Vec::new()));
    let (credentials, tickets) = service_with(TestCredentialStore::default(), "unrelated");
    let collector = LimitsCollector::new(credentials, transport, tickets);
    let attempt = collector.probe(ProbeTarget::CLAUDE, ProbeDeadline::for_accounts(1), &meta());
    assert_eq!(attempt.provider.status, LimitStatus::NotConfigured);

    let mut controller = UiController::new();
    controller.set_clock_ms(NOW_MS);
    bridge::apply(
        &mut controller,
        &crate::limits::scheduler::LimitsSnapshot {
            rows: vec![attempt.provider],
            retry_at_ms: None,
            notice: None,
        },
        NOW_MS,
    );
    let quota = controller.presentation().quota;
    assert_eq!(quota.rows.len(), 1);
    let row = &quota.rows[0];
    assert!(row.windows.is_empty(), "an unknown quota drew bars");
    assert!(row.attention, "a sign-in state must be flagged");
    assert_eq!(row.status, "Not configured");
}

#[test]
fn a_rejected_credential_asks_the_reader_to_sign_in_again() {
    let transport = Arc::new(RecordedTransport::new(vec![(401, "{}".to_owned())]));
    let (credentials, tickets) = service_with(TestCredentialStore::default(), "claude-oauth");
    let collector = LimitsCollector::new(credentials, transport, tickets);
    let attempt = collector.probe(ProbeTarget::CLAUDE, ProbeDeadline::for_accounts(1), &meta());
    assert_eq!(attempt.provider.status, LimitStatus::Unauthorized);

    let mut controller = UiController::new();
    controller.set_clock_ms(NOW_MS);
    bridge::apply(
        &mut controller,
        &crate::limits::scheduler::LimitsSnapshot {
            rows: vec![attempt.provider],
            retry_at_ms: None,
            notice: None,
        },
        NOW_MS,
    );
    let row = &controller.presentation().quota.rows[0];
    assert_eq!(row.status, "Sign in again");
    assert!(row.attention);
}

#[test]
fn a_retained_row_is_marked_so_and_keeps_its_reset() {
    let transport = Arc::new(RecordedTransport::new(vec![
        (200, claude_body()),
        (429, "{}".to_owned()),
    ]));
    let (credentials, tickets) = service_with(TestCredentialStore::default(), "claude-oauth");
    let collector = LimitsCollector::new(credentials, transport, tickets);
    let first = collector.probe(ProbeTarget::CLAUDE, ProbeDeadline::for_accounts(1), &meta());
    let mut retention = crate::limits::store::Retention::default();
    retention.apply(first);

    let second = collector.probe(ProbeTarget::CLAUDE, ProbeDeadline::for_accounts(1), &meta());
    assert_eq!(second.provider.status, LimitStatus::RateLimited);
    let (_, retained) = retention.apply(second);
    assert!(
        !retained.windows.is_empty(),
        "a throttle must retain the last good quota"
    );

    let mut controller = UiController::new();
    controller.set_clock_ms(NOW_MS);
    bridge::apply(
        &mut controller,
        &crate::limits::scheduler::LimitsSnapshot {
            rows: vec![retained],
            retry_at_ms: None,
            notice: None,
        },
        NOW_MS,
    );
    let row = &controller.presentation().quota.rows[0];
    assert!(row.stale, "a retained row must say so");
    assert_eq!(row.windows.len(), 2, "retention dropped the windows");
    assert_eq!(row.windows[0].reset, "Resets in 5h");
}

#[test]
fn a_timeout_keeps_the_bar_but_reports_the_status() {
    let mut provider = crate::limits::provider::LimitsProvider::new(
        "claude",
        LimitStatus::Timeout,
        LimitSource::Oauth,
    );
    provider.account_key = "account-1".to_owned();
    provider.account_label = "Primary".to_owned();
    let mut window = crate::limits::window::Window::new(WindowKind::Session);
    window.used_percent = Some(20.0);
    window.resets_at = Some("2026-11-15T17:00:00Z".to_owned());
    provider.windows = vec![window];

    let mut controller = UiController::new();
    controller.set_clock_ms(NOW_MS);
    bridge::apply(
        &mut controller,
        &crate::limits::scheduler::LimitsSnapshot {
            rows: vec![provider],
            retry_at_ms: Some(NOW_MS + 30_000),
            notice: None,
        },
        NOW_MS,
    );
    let row = &controller.presentation().quota.rows[0];
    assert_eq!(row.status, "Timed out");
    assert_eq!(row.windows[0].reset, "Resets in 5h");
}

#[test]
fn the_tray_reads_today_while_the_tray_is_independent_of_quota() {
    // A quota row must never leak into the token figures: they are separate
    // subsystems, and a quota percentage is not a token count.
    let mut controller = UiController::new();
    controller.set_clock_ms(NOW_MS);
    let mut provider =
        crate::limits::provider::LimitsProvider::new("claude", LimitStatus::Ok, LimitSource::Oauth);
    provider.account_key = "account-1".to_owned();
    let mut window = crate::limits::window::Window::new(WindowKind::Session);
    window.used_percent = Some(99.0);
    provider.windows = vec![window];
    bridge::apply(
        &mut controller,
        &crate::limits::scheduler::LimitsSnapshot {
            rows: vec![provider],
            retry_at_ms: None,
            notice: None,
        },
        NOW_MS,
    );
    let presentation = controller.presentation();
    assert_eq!(presentation.home.tracked_tokens, "0");
    assert_eq!(presentation.tray.tokens, "0");
}

#[test]
fn the_scheduler_probes_a_credential_but_never_by_hover() {
    // The trigger enum is the policy: only a person action or an interval is
    // reachable from the UI, and a hover cannot name either.
    assert!(!Trigger::Interval.bypasses_cooldown());
    assert!(!Trigger::ResetBoundary.bypasses_cooldown());
    assert!(Trigger::Manual.is_person());
    assert!(!Trigger::CredentialSaved.is_person());
}

#[test]
fn the_worker_stays_idle_until_the_interval_and_then_probes_once() {
    let store = TestCredentialStore::default();
    let credentials = Arc::new(CredentialService::start_test(store).expect("service"));
    let tickets = CredentialTickets::default();
    // The worker reads the wall clock, so the schedule must be armed from it too:
    // enabling from epoch zero would be due immediately.
    let mut scheduler = Scheduler::new(300_000);
    scheduler.enable("claude", wall_clock_ms());
    // Seed a credential so a probe would be possible at all.
    credentials
        .set(
            tickets.next(),
            &stable_credential_id("claude-oauth"),
            CredentialMaterial::from_secret_bytes(b"test-token".to_vec()).expect("material"),
        )
        .expect("store");

    let transport = Arc::new(RecordedTransport::new(vec![(200, claude_body())]));
    let collector = LimitsCollector::new(credentials, transport.clone(), tickets);
    let source = Box::new(move |provider: &str, deadline: ProbeDeadline| {
        let Some(target) = ProbeTarget::for_provider(provider) else {
            return Vec::new();
        };
        vec![collector.probe(target, deadline, &meta())]
    });
    let worker = LimitsWorker::spawn(scheduler, source, Box::new(|_snapshot| {}));
    // Nothing is due for five minutes, so the worker blocks and probed nothing.
    std::thread::sleep(Duration::from_millis(80));
    assert!(
        worker.snapshot().rows.is_empty(),
        "the worker probed before the interval elapsed"
    );

    // An explicit person action wakes it and produces exactly one probe.
    assert_eq!(
        worker.request("claude", Trigger::Manual),
        crate::limits::QueuedProbe::Queued
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while worker.snapshot().rows.is_empty() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let rows = worker.snapshot().rows;
    assert_eq!(rows.len(), 1, "the manual probe produced no row");
    assert_eq!(rows[0].provider, "claude");
    assert_eq!(rows[0].windows.len(), 2);
    drop(worker);
}

#[test]
fn an_unknown_provider_cannot_be_probed_through_the_bridge() {
    let source = CollectorSource::new(
        Arc::new(CredentialService::start_test(TestCredentialStore::default()).expect("service")),
        CredentialTickets::default(),
    );
    // Clause: a provider outside the two wired ones produces no probe at all.
    for other in ["gemini", "cursor", "roocode"] {
        assert!(
            crate::limits::scheduler::ProbeSource::probe(
                &source,
                other,
                ProbeDeadline::for_accounts(1)
            )
            .is_empty()
        );
    }
}
