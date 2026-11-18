//! Scheduling limits probes without polling.
//!
//! The worker waits on a condition variable until the next deadline. While
//! nothing is due it is blocked, not looping: there is no interval timer, no
//! animation frame, and no wakeup per UI delivery. `docs/migration/limits-spec.md`
//! §2.3 owns the trigger policy; this module only obeys it.
//!
//! The deterministic core ([`Scheduler`]) takes a clock and a probe callback, so
//! every rule below is testable without a thread, a network, or a keyring.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use super::provider::LimitsProvider;
use super::runtime::{LimitsRuntime, ProbeDeadline, QueuedProbe, Trigger};
use super::store::Attempt;

/// Produces one provider's rows. Injected so tests can count calls.
pub trait ProbeSource: Send + Sync {
    /// Probe one provider. Never blocks longer than `deadline`.
    fn probe(&self, provider: &str, deadline: ProbeDeadline) -> Vec<Attempt>;
}

impl<F> ProbeSource for F
where
    F: Fn(&str, ProbeDeadline) -> Vec<Attempt> + Send + Sync,
{
    fn probe(&self, provider: &str, deadline: ProbeDeadline) -> Vec<Attempt> {
        self(provider, deadline)
    }
}

/// The default interval between quota probes, five minutes.
///
/// Matches the reference's refresh cadence. A quota window changes slowly, so a
/// shorter interval would spend credentials for no new information.
pub const INTERVAL_MS: i64 = 5 * 60_000;

/// What the UI reads.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LimitsSnapshot {
    pub rows: Vec<LimitsProvider>,
    /// Set when a provider is cooling down after a failure.
    pub retry_at_ms: Option<i64>,
    /// Why the last refresh produced nothing, when it did.
    pub notice: Option<String>,
}

/// The deterministic scheduler.
pub struct Scheduler {
    runtime: LimitsRuntime,
    /// When the next interval probe is due.
    next_interval_ms: i64,
    interval_ms: i64,
    snapshot: LimitsSnapshot,
}

impl Scheduler {
    /// A scheduler with no provider enabled. Nothing is due until one is.
    pub fn new(interval_ms: i64) -> Self {
        Self {
            runtime: LimitsRuntime::default(),
            next_interval_ms: i64::MAX,
            interval_ms: interval_ms.max(1),
            snapshot: LimitsSnapshot::default(),
        }
    }

    /// Enable a provider. Enabling is not a probe: the first one is scheduled at
    /// the next interval, so starting the app never makes a network call.
    pub fn enable(&mut self, provider: &str, now_ms: i64) {
        self.runtime.enable(provider);
        if self.next_interval_ms == i64::MAX {
            self.next_interval_ms = now_ms.saturating_add(self.interval_ms);
        }
    }

    pub fn disable(&mut self, provider: &str) {
        self.runtime.revoke(provider);
    }

    /// Ask for a refresh. `Manual` is a person pressing the button;
    /// `CredentialSaved` is the point of saving a credential.
    pub fn request(&mut self, provider: &str, trigger: Trigger, now_ms: i64) -> QueuedProbe {
        self.runtime.request(provider, trigger, now_ms)
    }

    /// The instant the next probe is due, or `None` when nothing is scheduled.
    ///
    /// A caller waiting on this is idle until it returns, which is what makes
    /// "no idle polling" true rather than aspirational.
    pub fn next_deadline_ms(&self) -> Option<i64> {
        if self.runtime.is_idle() && self.next_interval_ms == i64::MAX {
            return None;
        }
        let cooling = self.snapshot.retry_at_ms.unwrap_or(i64::MAX);
        Some(self.next_interval_ms.min(cooling).max(0))
    }

    /// Run whatever is due at `now_ms`. Returns true when the snapshot changed.
    ///
    /// This is the only place a probe happens.
    pub fn run_due(&mut self, now_ms: i64, source: &dyn ProbeSource) -> bool {
        let before = self.snapshot.clone();

        // An interval tick re-arms every enabled provider.
        if self.next_interval_ms != i64::MAX && now_ms >= self.next_interval_ms {
            for provider in self.runtime.enabled_providers() {
                self.runtime.request(&provider, Trigger::Interval, now_ms);
            }
            self.next_interval_ms = now_ms.saturating_add(self.interval_ms);
        }

        // Drain the queue, honouring the concurrency ceiling.
        let mut ran = false;
        while let Some(provider) = self.runtime.take_next() {
            ran = true;
            let deadline = ProbeDeadline::for_accounts(1);
            let attempts = source.probe(&provider, deadline);
            if attempts.is_empty() {
                self.runtime.fail(
                    &provider,
                    super::status::LimitStatus::Unavailable,
                    now_ms,
                    || 0.0,
                );
                continue;
            }
            let outcome = super::runtime::ProbeOutcome::rows(attempts);
            for (_, row) in self.runtime.complete(&provider, outcome, now_ms, || 0.0) {
                self.publish_row(row);
            }
            // A provider that keeps failing must not spin the loop.
            if self
                .runtime
                .rows(&provider)
                .iter()
                .all(|row| row.status.is_retryable())
            {
                self.runtime.fail(
                    &provider,
                    super::status::LimitStatus::Unavailable,
                    now_ms,
                    || 0.0,
                );
            }
        }

        if ran {
            let runtime = self.runtime.snapshot();
            self.snapshot.retry_at_ms = runtime.retry_at_ms;
        }
        self.snapshot != before
    }

    /// Replace a row wholesale. One row per provider and account.
    fn publish_row(&mut self, row: LimitsProvider) {
        let identity = (row.provider.clone(), row.account_key.clone());
        self.snapshot
            .rows
            .retain(|current| (current.provider.clone(), current.account_key.clone()) != identity);
        self.snapshot.rows.push(row);
        self.snapshot.rows.sort_by(|left, right| {
            (left.provider.as_str(), left.account_key.as_str())
                .cmp(&(right.provider.as_str(), right.account_key.as_str()))
        });
    }

    /// Explain why there is nothing to show, when there is nothing.
    pub fn set_notice(&mut self, notice: Option<String>) {
        self.snapshot.notice = notice;
    }

    pub fn snapshot(&self) -> LimitsSnapshot {
        self.snapshot.clone()
    }

    pub fn is_idle(&self) -> bool {
        self.runtime.is_idle()
    }
}

/// The shared scheduler plus the signal that wakes its worker.
struct Shared {
    scheduler: Mutex<Scheduler>,
    wake: Condvar,
}

/// A worker thread that blocks until work is due.
///
/// The thread exists only after a caller asks for it, and it exits when the
/// handle is dropped. It never spins: each iteration sleeps on the condition
/// variable until either the next deadline or an explicit signal.
pub struct LimitsWorker {
    shared: Arc<Shared>,
    stopped: Arc<Mutex<bool>>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl LimitsWorker {
    /// Start a worker. `on_change` runs on the worker thread and is expected to
    /// hand the value to the UI, not to touch it directly.
    pub fn spawn(
        scheduler: Scheduler,
        source: Box<dyn ProbeSource>,
        on_change: Box<dyn Fn(LimitsSnapshot) + Send>,
    ) -> Self {
        let shared = Arc::new(Shared {
            scheduler: Mutex::new(scheduler),
            wake: Condvar::new(),
        });
        let stopped = Arc::new(Mutex::new(false));
        let worker_shared = shared.clone();
        let worker_stopped = stopped.clone();
        let join = std::thread::Builder::new()
            .name("token-monitor-limits".to_owned())
            .spawn(move || {
                loop {
                    let wait = {
                        let mut scheduler = match worker_shared.scheduler.lock() {
                            Ok(scheduler) => scheduler,
                            Err(_) => return,
                        };
                        let now_ms = now_ms();
                        let due = scheduler
                            .next_deadline_ms()
                            .map(|deadline| deadline.saturating_sub(now_ms))
                            .unwrap_or(i64::MAX);
                        match scheduler.run_due(now_ms, source.as_ref()) {
                            true => {
                                let snapshot = scheduler.snapshot();
                                drop(scheduler);
                                on_change(snapshot);
                                0
                            }
                            false => due,
                        }
                    };
                    // Blocked here while nothing is due. This is the whole reason
                    // there is no idle CPU cost.
                    let guard = match worker_shared.scheduler.lock() {
                        Ok(guard) => guard,
                        Err(_) => return,
                    };
                    let should_stop = worker_stopped
                        .lock()
                        .map(|stopped| *stopped)
                        .unwrap_or(true);
                    if should_stop {
                        return;
                    }
                    let _ = worker_shared.wake.wait_timeout(guard, wait_duration(wait));
                }
            })
            .ok();
        Self {
            shared,
            stopped,
            join,
        }
    }

    /// Ask the worker to look again now.
    pub fn wake(&self) {
        self.shared.wake.notify_all();
    }

    /// Read the current rows. Cheap: a lock and a clone of a handful of rows.
    pub fn snapshot(&self) -> LimitsSnapshot {
        self.shared
            .scheduler
            .lock()
            .map(|scheduler| scheduler.snapshot())
            .unwrap_or_default()
    }

    /// Submit a trigger and wake the worker.
    pub fn request(&self, provider: &str, trigger: Trigger) -> QueuedProbe {
        let now = now_ms();
        let queued = self
            .shared
            .scheduler
            .lock()
            .map(|mut scheduler| scheduler.request(provider, trigger, now))
            .unwrap_or(QueuedProbe::Disabled);
        self.wake();
        queued
    }
}

impl Drop for LimitsWorker {
    fn drop(&mut self) {
        if let Ok(mut stopped) = self.stopped.lock() {
            *stopped = true;
        }
        self.shared.wake.notify_all();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn wait_duration(wait_ms: i64) -> Duration {
    // A bounded wait keeps the worker responsive to a clock that moved without a
    // signal; `i64::MAX` means "no deadline", so wait indefinitely.
    if wait_ms == i64::MAX {
        Duration::from_secs(3600)
    } else {
        Duration::from_millis(wait_ms.clamp(1, 3_600_000) as u64)
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests;
