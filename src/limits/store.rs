//! Last-good retention.
//!
//! Mirrors `applyAttempt` in `src/shared/limits/runtime.js`; see
//! `docs/migration/limits-spec.md` §1.6. The rule set exists so one bad attempt
//! cannot erase quota the account still has, while a real state change (signed
//! out, credential rejected, provider disabled) does clear it.

use std::collections::BTreeMap;

use super::provider::LimitsProvider;
use super::status::LimitStatus;
use super::window::Window;

/// What an attempt did to a retained row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttemptEffect {
    /// Usable quota; the row replaces the retained one.
    Replaced,
    /// A transient failure; the retained row is untouched.
    Retained,
    /// A real state change; the retained row is dropped.
    Cleared,
    /// Usable quota, but some windows were not re-reported. The row keeps the
    /// older windows, is reported as `unavailable`, and restores the older
    /// `updated_at`, so nobody mistakes cached quota for a fresh reading.
    PartiallyRetained,
}

/// One attempt against one account.
#[derive(Clone, Debug, PartialEq)]
pub struct Attempt {
    pub provider: LimitsProvider,
    /// Window labels that this attempt invalidated: a provider that answers
    /// without them has dropped those quotas and they must not be carried.
    pub invalidated_labels: Vec<String>,
}

impl Attempt {
    pub fn new(provider: LimitsProvider) -> Self {
        Self {
            provider,
            invalidated_labels: Vec::new(),
        }
    }

    pub fn invalidating(mut self, labels: impl IntoIterator<Item = String>) -> Self {
        let mut labels = labels.into_iter().collect::<Vec<_>>();
        labels.sort();
        labels.dedup();
        self.invalidated_labels = labels;
        self
    }
}

/// The retained state for one account.
#[derive(Clone, Debug, Default, PartialEq)]
struct Entry {
    last_good: Option<LimitsProvider>,
    last_attempt: Option<LimitsProvider>,
}

/// All accounts for one provider.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Retention {
    entries: BTreeMap<String, Entry>,
}

impl Retention {
    /// Apply one attempt and return the row the UI should now show.
    pub fn apply(&mut self, attempt: Attempt) -> (AttemptEffect, LimitsProvider) {
        let key = identity_key(&attempt.provider);
        let entry = self.entries.entry(key.clone()).or_default();
        let mut row = attempt.provider.clone();
        let status = row.status;

        if status.is_ok() {
            // A window identity is its label when it has one, else its kind, so
            // two unlabelled session windows are the same window.
            let identity_of = |window: &Window| -> String {
                window
                    .label
                    .clone()
                    .unwrap_or_else(|| window.kind.as_str().to_owned())
            };
            let invalidated = &attempt.invalidated_labels;
            let mut retained: Vec<Window> = Vec::new();
            if let Some(previous) = entry.last_good.as_ref()
                && !invalidated.is_empty()
            {
                // Retention is narrow on purpose: a window survives only when the
                // provider did NOT invalidate it and did NOT re-report it.
                // Anything invalidated is gone; anything re-reported is the new
                // reading.
                retained = previous
                    .windows
                    .iter()
                    .filter(|window| {
                        let identity = identity_of(window);
                        !invalidated.iter().any(|dropped| dropped == &identity)
                            && !row
                                .windows
                                .iter()
                                .any(|current| identity_of(current) == identity)
                    })
                    .cloned()
                    .collect();
            }
            if retained.is_empty() {
                entry.last_good = Some(row.clone());
                entry.last_attempt = Some(row.clone());
                return (AttemptEffect::Replaced, row);
            }
            // Independently reported windows are newer, cached ones are not.
            row.status = LimitStatus::Unavailable;
            row.updated_at = entry
                .last_good
                .as_ref()
                .and_then(|previous| previous.updated_at.clone());
            let mut windows = retained;
            windows.extend(row.windows.iter().cloned());
            row.windows = windows;
            entry.last_good = Some(row.clone());
            entry.last_attempt = Some(row.clone());
            return (AttemptEffect::PartiallyRetained, row);
        }

        let effect = if status.is_transient() {
            // A transient failure keeps the last good quota and *publishes it*,
            // carrying the failure's status so a reader can see both. Returning
            // the bare failure instead would blank a bar that is still valid,
            // which is the one thing retention exists to prevent.
            let mut published = match entry.last_good.as_ref() {
                Some(previous) => {
                    let mut trimmed = previous.clone();
                    if !attempt.invalidated_labels.is_empty() {
                        trimmed.windows = previous
                            .windows
                            .iter()
                            .filter(|window| {
                                let label = window.label.as_deref().unwrap_or("");
                                !attempt
                                    .invalidated_labels
                                    .iter()
                                    .any(|dropped| dropped == label)
                            })
                            .cloned()
                            .collect();
                    }
                    // The published row reports the failure; the retained entry
                    // keeps the reading it came from, so a later success still
                    // knows what it replaced.
                    let retained_entry = trimmed.clone();
                    trimmed.status = status;
                    trimmed.stale = true;
                    entry.last_good = Some(retained_entry);
                    trimmed
                }
                None => row.clone(),
            };
            published.updated_at = entry
                .last_good
                .as_ref()
                .and_then(|previous| previous.updated_at.clone())
                .or(published.updated_at);
            entry.last_attempt = Some(row.clone());
            return (AttemptEffect::Retained, published);
        } else {
            entry.last_good = None;
            AttemptEffect::Cleared
        };
        entry.last_attempt = Some(row.clone());
        (effect, row)
    }

    /// The retained row for an account, if any.
    pub fn last_good(&self, provider: &str, account_key: &str) -> Option<&LimitsProvider> {
        self.entries
            .get(&format!("{provider}\u{0}{account_key}"))
            .and_then(|entry| entry.last_good.as_ref())
    }

    /// Every retained row, for a snapshot rebuild.
    pub fn rows(&self) -> Vec<LimitsProvider> {
        self.entries
            .values()
            .filter_map(|entry| entry.last_good.clone())
            .collect()
    }

    /// Drop accounts that no longer exist.
    pub fn retain_keys(&mut self, keep: &[String]) {
        let keep = keep
            .iter()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        self.entries
            .retain(|key, _| keep.iter().any(|candidate| candidate == key));
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

fn identity_key(row: &LimitsProvider) -> String {
    format!("{}\u{0}{}", row.provider, row.account_key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::provider::LimitSource;
    use crate::limits::window::{WindowKind, WindowMetric};

    fn row(status: LimitStatus, windows: Vec<Window>) -> LimitsProvider {
        LimitsProvider {
            status,
            updated_at: Some("2026-11-15T12:00:00Z".to_owned()),
            account_key: "acct".to_owned(),
            windows,
            ..LimitsProvider::new("claude", status, LimitSource::Oauth)
        }
    }

    fn session(percent: f64) -> Window {
        Window {
            used_percent: Some(percent),
            ..Window::new(WindowKind::Session)
        }
    }

    #[test]
    fn a_success_replaces_the_retained_row() {
        let mut retention = Retention::default();
        let (effect, shown) =
            retention.apply(Attempt::new(row(LimitStatus::Ok, vec![session(10.0)])));
        assert_eq!(effect, AttemptEffect::Replaced);
        assert_eq!(shown.status, LimitStatus::Ok);

        let (effect, shown) =
            retention.apply(Attempt::new(row(LimitStatus::Ok, vec![session(42.0)])));
        assert_eq!(effect, AttemptEffect::Replaced);
        assert_eq!(shown.windows[0].used_percent, Some(42.0));
        assert_eq!(retention.rows().len(), 1);
    }

    #[test]
    fn a_transient_failure_keeps_the_last_good_quota() {
        let mut retention = Retention::default();
        retention.apply(Attempt::new(row(LimitStatus::Ok, vec![session(10.0)])));

        for status in [
            LimitStatus::Timeout,
            LimitStatus::RateLimited,
            LimitStatus::SourceRateLimited,
            LimitStatus::Unavailable,
            LimitStatus::Error,
        ] {
            let (effect, shown) = retention.apply(Attempt::new(row(status, Vec::new())));
            assert_eq!(effect, AttemptEffect::Retained, "{status}");
            // The failure is what the row reports, so nobody thinks it is fresh.
            assert_eq!(shown.status, status);
            let retained = retention.last_good("claude", "acct").expect("retained");
            assert_eq!(retained.status, LimitStatus::Ok);
            assert_eq!(retained.windows[0].used_percent, Some(10.0));
        }
    }

    #[test]
    fn a_real_state_change_clears_the_retained_row() {
        for status in [
            LimitStatus::NotConfigured,
            LimitStatus::Unauthorized,
            LimitStatus::Disabled,
        ] {
            let mut retention = Retention::default();
            retention.apply(Attempt::new(row(LimitStatus::Ok, vec![session(10.0)])));
            let (effect, _) = retention.apply(Attempt::new(row(status, Vec::new())));
            assert_eq!(effect, AttemptEffect::Cleared, "{status}");
            assert!(retention.last_good("claude", "acct").is_none(), "{status}");
            assert!(retention.rows().is_empty(), "{status}");
        }
    }

    #[test]
    fn a_partial_success_keeps_older_windows_and_says_so() {
        let mut retention = Retention::default();
        let weekly = Window {
            label: Some("Weekly".to_owned()),
            used_percent: Some(30.0),
            ..Window::new(WindowKind::Weekly)
        };
        retention.apply(Attempt::new(row(
            LimitStatus::Ok,
            vec![session(10.0), weekly],
        )));

        // The next attempt re-reports the session window and declares that a
        // credits window it no longer reports is invalidated. The weekly window
        // is neither, so it survives, visibly stale.
        let attempt = Attempt::new(row(LimitStatus::Ok, vec![session(55.0)]))
            .invalidating(["Credits".to_owned()]);
        let (effect, shown) = retention.apply(attempt);
        assert_eq!(effect, AttemptEffect::PartiallyRetained);
        // Cached quota must not look fresh.
        assert_eq!(shown.status, LimitStatus::Unavailable);
        assert_eq!(shown.updated_at.as_deref(), Some("2026-11-15T12:00:00Z"));
        let labels = shown
            .windows
            .iter()
            .map(|window| {
                window
                    .label
                    .clone()
                    .unwrap_or_else(|| window.kind.as_str().to_owned())
            })
            .collect::<Vec<_>>();
        assert!(labels.contains(&"Weekly".to_owned()), "{labels:?}");
    }

    #[test]
    fn an_invalidated_window_is_not_carried() {
        let mut retention = Retention::default();
        let credits = Window {
            metric: Some(WindowMetric::Credits),
            label: Some("Credits".to_owned()),
            used_percent: Some(30.0),
            ..Window::new(WindowKind::Billing)
        };
        retention.apply(Attempt::new(row(
            LimitStatus::Ok,
            vec![session(10.0), credits],
        )));

        let attempt = Attempt::new(row(LimitStatus::Unavailable, Vec::new()))
            .invalidating(["Credits".to_owned()]);
        retention.apply(attempt);
        let retained = retention.last_good("claude", "acct").expect("retained");
        assert!(
            !retained
                .windows
                .iter()
                .any(|window| window.label.as_deref() == Some("Credits")),
            "an invalidated window must not survive: {:?}",
            retained.windows
        );
        assert_eq!(retained.windows.len(), 1);
    }

    #[test]
    fn accounts_are_retained_separately() {
        let mut retention = Retention::default();
        for account in ["one", "two"] {
            let mut attempt = Attempt::new(row(LimitStatus::Ok, vec![session(10.0)]));
            attempt.provider.account_key = account.to_owned();
            retention.apply(attempt);
        }
        assert_eq!(retention.rows().len(), 2);
        assert!(retention.last_good("claude", "one").is_some());
        assert!(retention.last_good("claude", "three").is_none());
    }

    #[test]
    fn removing_an_account_drops_only_that_account() {
        let mut retention = Retention::default();
        for account in ["keep", "drop"] {
            let mut attempt = Attempt::new(row(LimitStatus::Ok, vec![session(10.0)]));
            attempt.provider.account_key = account.to_owned();
            retention.apply(attempt);
        }
        retention.retain_keys(&["claude\u{0}keep".to_owned()]);
        assert!(retention.last_good("claude", "keep").is_some());
        assert!(retention.last_good("claude", "drop").is_none());
    }
}
