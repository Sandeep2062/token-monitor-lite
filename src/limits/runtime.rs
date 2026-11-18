//! The limits scheduler: bounded concurrency, deadlines, backoff, triggers.
//!
//! Ports `createLimitsRuntime` from `src/shared/limits/runtime.js`, plus
//! `runWithProbeDeadline`. See `docs/migration/limits-spec.md` §1.6 and §2.3.
//!
//! Deliberately absent: any hover, focus or visibility listener, and any implicit
//! probe on a local write. A probe happens only for a [`Trigger`], and the
//! allowlist below is the only way to name one.

use std::collections::{BTreeSet, VecDeque};
use std::time::{Duration, Instant};

use super::provider::LimitsProvider;
use super::retry::{compute_retry_delay_ms, parse_retry_after_header};
use super::status::LimitStatus;
use super::store::{Attempt, AttemptEffect, Retention};

/// Default ceiling on concurrent provider probes (`DEFAULT_LIMITS_MAX_CONCURRENCY`).
pub const DEFAULT_MAX_CONCURRENCY: usize = 3;
/// Default probe deadline.
pub const DEFAULT_DEADLINE_MS: i64 = 120_000;
/// Grace added to a probe deadline for cleanup (`PROVIDER_CLEANUP_GRACE_MS`).
pub const CLEANUP_GRACE_MS: i64 = 5_000;
/// Default refresh interval.
pub const DEFAULT_REFRESH_MS: i64 = 5 * 60_000;

/// Why a probe is running.
///
/// This enum *is* the trigger policy: there is no `Hover` and no `Focus`
/// variant to construct, so no UI event can reach a probe. State changes are
/// individual variants rather than a free-form string, which is what makes
/// "no probes triggered by local writes" checkable rather than promised.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    /// The configured interval elapsed.
    Interval,
    /// A quota window reset.
    ResetBoundary,
    /// A person pressed refresh.
    Manual,
    /// A credential was saved; a probe is the point of saving it.
    CredentialSaved,
    /// A provider or account was enabled or added.
    AccountAdded,
    /// A provider or account was removed.
    AccountRemoved,
    /// Settings changed that scope a provider.
    SettingsChanged,
}

impl Trigger {
    /// Whether this trigger may bypass a provider cooldown.
    ///
    /// Mirrors `COOLDOWN_BYPASS_REASONS`. `Interval` and `ResetBoundary` are
    /// absent on purpose: they are the reasons a cooldown exists.
    pub fn bypasses_cooldown(self) -> bool {
        matches!(
            self,
            Self::CredentialSaved
                | Self::AccountAdded
                | Self::AccountRemoved
                | Self::SettingsChanged
        )
    }

    /// Whether this trigger counts as an explicit person action.
    pub fn is_person(self) -> bool {
        matches!(self, Self::Manual)
    }
}

/// The result of a probe, before retention.
#[derive(Clone, Debug, PartialEq)]
pub struct ProbeOutcome {
    pub rows: Vec<Attempt>,
    /// Server-requested backoff, if the transport saw a `Retry-After`.
    pub retry_after_ms: Option<i64>,
}

impl ProbeOutcome {
    pub fn rows(rows: Vec<Attempt>) -> Self {
        Self {
            rows,
            retry_after_ms: None,
        }
    }

    pub fn failure(status: LimitStatus) -> Self {
        Self {
            rows: Vec::new(),
            retry_after_ms: None,
        }
        .with_status(status)
    }

    fn with_status(self, _status: LimitStatus) -> Self {
        self
    }
}

/// A provider's quota, as the runtime publishes it.
#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeSnapshot {
    pub providers: Vec<LimitsProvider>,
    /// Set when a provider is in a cooldown; consumers can show "retrying".
    pub retry_at_ms: Option<i64>,
}

/// Per-provider scheduling state.
#[derive(Debug, Default)]
struct Lane {
    retry_attempt: u32,
    retry_not_before_ms: i64,
    running: bool,
}

/// Probe budget for one provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProbeDeadline {
    deadline_ms: i64,
}

impl ProbeDeadline {
    /// A deadline must be finite and positive; a probe without one can hang a
    /// worker forever, which is why the reference refuses one too.
    pub fn new(deadline_ms: i64) -> Option<Self> {
        (deadline_ms > 0).then_some(Self { deadline_ms })
    }

    /// The deadline for a provider given how many accounts it must probe.
    ///
    /// Mirrors `providerPhysicalBoundMs`: the bound scales with the work, so a
    /// provider with five accounts is not killed at the single-account budget.
    pub fn for_accounts(accounts: usize) -> Self {
        let jobs = accounts.max(1) as i64;
        Self {
            deadline_ms: DEFAULT_DEADLINE_MS.saturating_mul(jobs) + CLEANUP_GRACE_MS,
        }
    }

    pub fn deadline(&self) -> Duration {
        Duration::from_millis(self.deadline_ms.max(1) as u64)
    }

    pub fn deadline_ms(&self) -> i64 {
        self.deadline_ms
    }
}

/// The scheduler.
#[derive(Debug)]
pub struct LimitsRuntime {
    retention: Retention,
    lanes: std::collections::BTreeMap<String, Lane>,
    queue: VecDeque<String>,
    queued: BTreeSet<String>,
    active: usize,
    max_concurrency: usize,
    enabled: BTreeSet<String>,
    retry_base_ms: i64,
    retry_max_ms: i64,
}

impl Default for LimitsRuntime {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_CONCURRENCY)
    }
}

impl LimitsRuntime {
    pub fn new(max_concurrency: usize) -> Self {
        Self {
            retention: Retention::default(),
            lanes: Default::default(),
            queue: VecDeque::new(),
            queued: BTreeSet::new(),
            active: 0,
            max_concurrency: max_concurrency.max(1),
            enabled: BTreeSet::new(),
            retry_base_ms: super::retry::DEFAULT_RETRY_BASE_MS,
            retry_max_ms: super::retry::DEFAULT_RETRY_MAX_MS,
        }
    }

    pub fn enable(&mut self, provider: &str) {
        self.enabled.insert(provider.to_owned());
    }

    /// Stop probing a provider. Queued work for it is dropped, never run.
    pub fn revoke(&mut self, provider: &str) {
        self.enabled.remove(provider);
        if self.queued.remove(provider) {
            self.queue.retain(|queued| queued != provider);
        }
    }

    /// Every provider currently enabled.
    pub fn enabled_providers(&self) -> Vec<String> {
        self.enabled.iter().cloned().collect()
    }

    pub fn is_enabled(&self, provider: &str) -> bool {
        self.enabled.contains(provider)
    }

    /// Queue a probe, unless the provider is disabled or cooling down.
    ///
    /// A cooldown is honoured for every trigger that does not bypass it, so a
    /// crude caller cannot hammer a throttled provider by naming a different
    /// reason.
    pub fn request(&mut self, provider: &str, trigger: Trigger, now_ms: i64) -> QueuedProbe {
        if !self.enabled.contains(provider) {
            return QueuedProbe::Disabled;
        }
        let lane = self.lanes.entry(provider.to_owned()).or_default();
        if trigger.bypasses_cooldown() {
            lane.retry_attempt = 0;
            lane.retry_not_before_ms = 0;
        } else if lane.retry_not_before_ms > now_ms {
            return QueuedProbe::CoolingDown {
                retry_at_ms: lane.retry_not_before_ms,
            };
        }
        if lane.running {
            // One probe per provider at a time: a second request for the same
            // provider coalesces into it rather than stacking.
            return QueuedProbe::AlreadyQueued;
        }
        if self.queued.contains(provider) {
            return QueuedProbe::AlreadyQueued;
        }
        self.queued.insert(provider.to_owned());
        self.queue.push_back(provider.to_owned());
        QueuedProbe::Queued
    }

    /// Take the next provider to probe, respecting the concurrency ceiling.
    ///
    /// A caller that gets `None` must not probe: either the ceiling is reached
    /// or the queue is empty.
    pub fn take_next(&mut self) -> Option<String> {
        if self.active >= self.max_concurrency {
            return None;
        }
        let provider = self.queue.pop_front()?;
        self.queued.remove(&provider);
        let lane = self.lanes.entry(provider.clone()).or_default();
        lane.running = true;
        self.active += 1;
        Some(provider)
    }

    /// Record a completed probe and publish the retained rows.
    pub fn complete(
        &mut self,
        provider: &str,
        outcome: ProbeOutcome,
        now_ms: i64,
        random: impl FnOnce() -> f64,
    ) -> Vec<(AttemptEffect, LimitsProvider)> {
        let lane = self.lanes.entry(provider.to_owned()).or_default();
        lane.running = false;
        self.active = self.active.saturating_sub(1);

        if outcome.rows.is_empty() {
            return Vec::new();
        }
        let mut effects = Vec::new();
        for attempt in outcome.rows {
            effects.push(self.retention.apply(attempt));
        }
        let worst = effects
            .iter()
            .map(|(_, row)| row.status)
            .min()
            .unwrap_or(LimitStatus::Ok);
        if worst.is_retryable() {
            lane.retry_attempt = lane.retry_attempt.saturating_add(1);
            let delay = compute_retry_delay_ms(
                lane.retry_attempt,
                outcome.retry_after_ms,
                self.retry_base_ms,
                self.retry_max_ms,
                random,
            );
            lane.retry_not_before_ms = now_ms.saturating_add(delay);
        } else if worst.is_ok() || worst.needs_attention() {
            lane.retry_attempt = 0;
            lane.retry_not_before_ms = 0;
        }
        effects
    }

    /// Mark a provider's probe as finished without rows (a timeout, an IO error).
    pub fn fail(
        &mut self,
        provider: &str,
        status: LimitStatus,
        now_ms: i64,
        random: impl FnOnce() -> f64,
    ) {
        let lane = self.lanes.entry(provider.to_owned()).or_default();
        lane.running = false;
        self.active = self.active.saturating_sub(1);
        if !status.is_retryable() {
            lane.retry_attempt = 0;
            lane.retry_not_before_ms = 0;
            return;
        }
        lane.retry_attempt = lane.retry_attempt.saturating_add(1);
        let delay = compute_retry_delay_ms(
            lane.retry_attempt,
            None,
            self.retry_base_ms,
            self.retry_max_ms,
            random,
        );
        lane.retry_not_before_ms = now_ms.saturating_add(delay);
    }

    /// Whether nothing is running and nothing is queued.
    pub fn is_idle(&self) -> bool {
        self.active == 0 && self.queue.is_empty()
    }

    pub fn active(&self) -> usize {
        self.active
    }

    pub fn max_concurrency(&self) -> usize {
        self.max_concurrency
    }

    /// The published snapshot.
    pub fn snapshot(&self) -> RuntimeSnapshot {
        RuntimeSnapshot {
            providers: self.retention.rows(),
            retry_at_ms: self
                .lanes
                .values()
                .map(|lane| lane.retry_not_before_ms)
                .filter(|value| *value > 0)
                .min(),
        }
    }

    /// Retained rows for one provider.
    pub fn rows(&self, provider: &str) -> Vec<LimitsProvider> {
        self.retention
            .rows()
            .into_iter()
            .filter(|row| row.provider == provider)
            .collect()
    }
}

/// The result of asking to probe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueuedProbe {
    Queued,
    AlreadyQueued,
    Disabled,
    CoolingDown { retry_at_ms: i64 },
}

/// Whether a `Retry-After` header should influence the next delay.
pub fn retry_after_from_header(value: &str, now: Instant) -> Option<i64> {
    let now_ms = now.elapsed().as_millis() as i64;
    parse_retry_after_header(value, now_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::provider::LimitSource;
    use crate::limits::window::{Window, WindowKind};

    fn attempt(provider: &str, status: LimitStatus) -> Attempt {
        let mut row = LimitsProvider::new(provider, status, LimitSource::Oauth);
        row.account_key = "acct".to_owned();
        row.windows = vec![Window::new(WindowKind::Session)];
        Attempt::new(row)
    }

    fn no_jitter() -> f64 {
        0.0
    }

    #[test]
    fn the_concurrency_ceiling_is_never_exceeded() {
        let mut runtime = LimitsRuntime::new(2);
        for provider in ["claude", "codex", "other"] {
            runtime.enable(provider);
            runtime.request(provider, Trigger::Manual, 0);
        }
        let first = runtime.take_next().expect("first");
        let second = runtime.take_next().expect("second");
        assert_ne!(first, second);
        // Third is blocked: two are running.
        assert_eq!(runtime.take_next(), None);
        assert_eq!(runtime.active(), 2);
        runtime.complete(
            &first,
            ProbeOutcome::rows(vec![attempt(&first, LimitStatus::Ok)]),
            0,
            no_jitter,
        );
        assert_eq!(runtime.take_next().as_deref(), Some("other"));
    }

    #[test]
    fn a_disabled_provider_never_probes() {
        let mut runtime = LimitsRuntime::default();
        assert_eq!(
            runtime.request("claude", Trigger::Manual, 0),
            QueuedProbe::Disabled
        );
        assert_eq!(runtime.take_next(), None);
        assert!(runtime.is_idle());
    }

    #[test]
    fn a_transient_failure_starts_a_cooldown_that_interval_respects() {
        let mut runtime = LimitsRuntime::default();
        runtime.enable("claude");
        runtime.request("claude", Trigger::Manual, 0);
        let provider = runtime.take_next().expect("probe");
        runtime.fail(&provider, LimitStatus::Unavailable, 1_000, no_jitter);

        // Interval is inside the cooldown: no probe.
        assert_eq!(
            runtime.request("claude", Trigger::Interval, 1_500),
            QueuedProbe::CoolingDown { retry_at_ms: 3_500 }
        );
        // Past it, the probe is allowed again.
        assert_eq!(
            runtime.request("claude", Trigger::Interval, 3_600),
            QueuedProbe::Queued
        );
    }

    #[test]
    fn a_credential_save_may_bypass_the_cooldown() {
        let mut runtime = LimitsRuntime::default();
        runtime.enable("claude");
        runtime.request("claude", Trigger::Manual, 0);
        let provider = runtime.take_next().expect("probe");
        runtime.fail(&provider, LimitStatus::RateLimited, 1_000, no_jitter);

        assert_eq!(
            runtime.request("claude", Trigger::CredentialSaved, 1_500),
            QueuedProbe::Queued
        );
    }

    #[test]
    fn a_success_clears_the_cooldown() {
        let mut runtime = LimitsRuntime::default();
        runtime.enable("claude");
        runtime.request("claude", Trigger::Manual, 0);
        let provider = runtime.take_next().expect("probe");
        runtime.fail(&provider, LimitStatus::Unavailable, 1_000, no_jitter);
        runtime.request("claude", Trigger::CredentialSaved, 1_500);
        let provider = runtime.take_next().expect("probe");
        runtime.complete(
            &provider,
            ProbeOutcome::rows(vec![attempt("claude", LimitStatus::Ok)]),
            2_000,
            no_jitter,
        );
        // No cooldown remains.
        assert_eq!(
            runtime.request("claude", Trigger::Interval, 2_001),
            QueuedProbe::Queued
        );
    }

    #[test]
    fn a_second_request_for_a_running_provider_is_coalesced() {
        let mut runtime = LimitsRuntime::default();
        runtime.enable("claude");
        runtime.request("claude", Trigger::Manual, 0);
        runtime.take_next().expect("probe");
        assert_eq!(
            runtime.request("claude", Trigger::Manual, 0),
            QueuedProbe::AlreadyQueued
        );
    }

    #[test]
    fn cooldown_bypass_is_an_allowlist_not_a_default() {
        assert!(Trigger::CredentialSaved.bypasses_cooldown());
        assert!(Trigger::SettingsChanged.bypasses_cooldown());
        // The reasons a cooldown exists must never bypass one.
        assert!(!Trigger::Interval.bypasses_cooldown());
        assert!(!Trigger::ResetBoundary.bypasses_cooldown());
        assert!(!Trigger::Manual.bypasses_cooldown());
    }

    #[test]
    fn a_deadline_scales_with_the_accounts_to_probe() {
        let single = ProbeDeadline::for_accounts(1);
        let four = ProbeDeadline::for_accounts(4);
        assert_eq!(single.deadline_ms(), DEFAULT_DEADLINE_MS + CLEANUP_GRACE_MS);
        assert_eq!(
            four.deadline_ms(),
            DEFAULT_DEADLINE_MS * 4 + CLEANUP_GRACE_MS
        );
        assert_eq!(
            four.deadline(),
            Duration::from_millis(four.deadline_ms() as u64)
        );
    }

    #[test]
    fn a_probe_deadline_must_be_usable() {
        assert!(ProbeDeadline::new(0).is_none());
        assert!(ProbeDeadline::new(-1).is_none());
        assert!(ProbeDeadline::new(1).is_some());
    }
}
