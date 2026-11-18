//! Scheduler tests.
//!
//! The clock is supplied, so every timing rule is checked without sleeping and
//! the "nothing happens while idle" claim is a count, not a promise.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::limits::provider::LimitSource;
use crate::limits::status::LimitStatus;
use crate::limits::window::{Window, WindowKind};

/// A source that records how many probes happened.
#[derive(Debug)]
struct CountingSource {
    calls: AtomicUsize,
    status: LimitStatus,
}

impl CountingSource {
    fn ok() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            status: LimitStatus::Ok,
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl ProbeSource for CountingSource {
    fn probe(&self, provider: &str, _deadline: ProbeDeadline) -> Vec<Attempt> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut row = LimitsProvider::new(provider, self.status, LimitSource::Oauth);
        row.account_key = "account-1".to_owned();
        row.windows = vec![Window::new(WindowKind::Session)];
        vec![Attempt::new(row)]
    }
}

fn attempt(provider: &str) -> Attempt {
    let mut row = LimitsProvider::new(provider, LimitStatus::Ok, LimitSource::Oauth);
    row.account_key = "account-1".to_owned();
    row.windows = vec![Window::new(WindowKind::Session)];
    Attempt::new(row)
}

#[test]
fn enabling_a_provider_probes_nothing() {
    let source = CountingSource::ok();
    let mut scheduler = Scheduler::new(300_000);
    scheduler.enable("claude", 0);
    scheduler.enable("codex", 0);
    assert_eq!(source.calls(), 0);
    assert!(scheduler.snapshot().rows.is_empty());
    // The first probe is due one interval later, not now.
    assert_eq!(scheduler.next_deadline_ms(), Some(300_000));
}

#[test]
fn nothing_runs_before_the_interval_elapses() {
    let source = CountingSource::ok();
    let mut scheduler = Scheduler::new(300_000);
    scheduler.enable("claude", 0);
    for now in [0, 1_000, 299_999] {
        assert!(!scheduler.run_due(now, &source));
    }
    assert_eq!(source.calls(), 0, "a probe ran before the interval");
    assert!(scheduler.run_due(300_000, &source));
    assert_eq!(source.calls(), 1);
}

#[test]
fn an_idle_scheduler_has_no_deadline_and_never_probes() {
    let source = CountingSource::ok();
    let mut scheduler = Scheduler::new(300_000);
    assert_eq!(scheduler.next_deadline_ms(), None);
    for now in [0, 60_000, 120_000] {
        assert!(!scheduler.run_due(now, &source));
    }
    assert_eq!(source.calls(), 0);
    assert!(scheduler.is_idle());
}

#[test]
fn a_manual_refresh_probes_without_naming_an_interval() {
    let source = CountingSource::ok();
    let mut scheduler = Scheduler::new(300_000);
    scheduler.enable("claude", 0);
    assert_eq!(
        scheduler.request("claude", Trigger::Manual, 5_000),
        QueuedProbe::Queued
    );
    assert!(scheduler.run_due(5_000, &source));
    assert_eq!(source.calls(), 1);
    assert_eq!(scheduler.snapshot().rows.len(), 1);
}

#[test]
fn a_disabled_provider_is_never_probed() {
    let source = CountingSource::ok();
    let mut scheduler = Scheduler::new(300_000);
    assert_eq!(
        scheduler.request("claude", Trigger::Manual, 0),
        QueuedProbe::Disabled
    );
    assert!(!scheduler.run_due(0, &source));
    assert_eq!(source.calls(), 0);
}

#[test]
fn revoking_a_provider_drops_its_queued_probe() {
    let source = CountingSource::ok();
    let mut scheduler = Scheduler::new(300_000);
    scheduler.enable("claude", 0);
    scheduler.request("claude", Trigger::Manual, 0);
    scheduler.disable("claude");
    scheduler.run_due(0, &source);
    assert_eq!(source.calls(), 0);
}

/// A source that answers with nothing at all.
#[derive(Debug, Default)]
struct SilentSource {
    calls: AtomicUsize,
}

impl SilentSource {
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl ProbeSource for SilentSource {
    fn probe(&self, _provider: &str, _deadline: ProbeDeadline) -> Vec<Attempt> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Vec::new()
    }
}

#[test]
fn a_failing_provider_never_spins_the_loop() {
    let source = SilentSource::default();
    let mut scheduler = Scheduler::new(300_000);
    scheduler.enable("claude", 0);
    scheduler.request("claude", Trigger::Manual, 1_000);
    scheduler.run_due(1_000, &source);
    assert_eq!(source.calls(), 1, "a failure must not retry immediately");
    assert!(scheduler.is_idle());
    // Running again at the same instant does not hammer the provider.
    scheduler.run_due(1_000, &source);
    assert_eq!(source.calls(), 1);
    // A cooldown is published, and it is a deadline rather than a busy loop.
    assert!(
        scheduler.snapshot().retry_at_ms.is_some(),
        "an empty probe must start a cooldown"
    );
    assert!(scheduler.next_deadline_ms().is_some());
}

#[test]
fn a_probe_that_returns_nothing_publishes_no_row() {
    let source = SilentSource::default();
    let mut scheduler = Scheduler::new(300_000);
    scheduler.enable("claude", 0);
    scheduler.request("claude", Trigger::Manual, 1_000);
    scheduler.run_due(1_000, &source);
    assert_eq!(source.calls(), 1);
    assert!(scheduler.snapshot().rows.is_empty());
}

#[test]
fn the_snapshot_replaces_a_row_rather_than_appending() {
    let mut scheduler = Scheduler::new(300_000);
    let source = CountingSource::ok();
    scheduler.enable("claude", 0);
    for now in [1_000, 2_000, 3_000] {
        scheduler.request("claude", Trigger::CredentialSaved, now);
        scheduler.run_due(now, &source);
    }
    assert_eq!(scheduler.snapshot().rows.len(), 1, "rows accumulated");
    assert_eq!(source.calls(), 3);
}

#[test]
fn rows_of_different_accounts_are_kept_apart() {
    let mut scheduler = Scheduler::new(300_000);
    let source = Box::new(|provider: &str, _deadline: ProbeDeadline| {
        let mut first = LimitsProvider::new(provider, LimitStatus::Ok, LimitSource::Oauth);
        first.account_key = "a".to_owned();
        let mut second = LimitsProvider::new(provider, LimitStatus::Ok, LimitSource::Oauth);
        second.account_key = "b".to_owned();
        vec![Attempt::new(first), Attempt::new(second)]
    });
    scheduler.enable("claude", 0);
    scheduler.request("claude", Trigger::Manual, 0);
    scheduler.run_due(0, source.as_ref());
    assert_eq!(scheduler.snapshot().rows.len(), 2);
}

#[test]
fn the_deadline_only_moves_forward_one_interval_at_a_time() {
    let source = CountingSource::ok();
    let mut scheduler = Scheduler::new(300_000);
    scheduler.enable("claude", 0);
    assert_eq!(scheduler.next_deadline_ms(), Some(300_000));
    scheduler.run_due(300_000, &source);
    assert_eq!(source.calls(), 1);
    assert_eq!(scheduler.next_deadline_ms(), Some(600_000));
    // A late tick re-arms from now, not from the missed deadline, and still
    // probes once.
    scheduler.run_due(950_000, &source);
    assert_eq!(source.calls(), 2);
    assert_eq!(scheduler.next_deadline_ms(), Some(1_250_000));
}

#[test]
fn an_unchanged_reprobe_reports_no_change() {
    // The delivery pass only repaints on a change, so a probe that reproduces
    // the same rows must not claim one.
    let source = CountingSource::ok();
    let mut scheduler = Scheduler::new(300_000);
    scheduler.enable("claude", 0);
    assert!(scheduler.run_due(300_000, &source));
    assert!(!scheduler.run_due(600_000, &source));
    assert_eq!(source.calls(), 2);
    assert_eq!(scheduler.snapshot().rows.len(), 1);
}

#[test]
fn an_attempt_helper_is_never_empty() {
    assert_eq!(attempt("claude").provider.provider, "claude");
}

#[test]
fn the_worker_starts_idle_and_releases_its_thread() {
    let source = Arc::new(CountingSource::ok());
    let scheduler = Scheduler::new(300_000);
    let deliveries = Arc::new(AtomicUsize::new(0));
    let counter = deliveries.clone();
    let probe_source = source.clone();
    let worker = LimitsWorker::spawn(
        scheduler,
        Box::new(move |provider: &str, deadline: ProbeDeadline| {
            probe_source.probe(provider, deadline)
        }),
        Box::new(move |_snapshot| {
            counter.fetch_add(1, Ordering::SeqCst);
        }),
    );
    // Nothing enabled: the worker has nothing due and must not probe.
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert_eq!(source.calls(), 0);
    assert_eq!(deliveries.load(Ordering::SeqCst), 0);
    assert!(worker.snapshot().rows.is_empty());
    drop(worker);
}
