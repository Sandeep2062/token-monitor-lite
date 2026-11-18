//! Provider quota limits: status, windows, retention, scheduling.
//!
//! Ports the limits lane of the pinned reference — `src/shared/limits/*` and the
//! `claude`/`codex` provider modules — without its filesystem credential lanes.
//! `docs/migration/limits-spec.md` states what is implemented, what is
//! deliberately narrowed, and which claims are VERIFIED versus ASSUMED.

#[cfg(test)]
mod e2e_tests;

pub mod bridge;
pub mod collector;
pub mod provider;
pub mod providers;
pub mod reset_boundary;
pub mod retry;
pub mod runtime;
pub mod scheduler;
pub mod status;
pub mod store;
pub mod transport;
pub mod window;

pub use provider::{LimitSource, LimitsProvider};
pub use runtime::{LimitsRuntime, ProbeDeadline, QueuedProbe, RuntimeSnapshot, Trigger};
pub use status::LimitStatus;
pub use store::{Attempt, AttemptEffect, Retention};
pub use window::{Window, WindowKind, WindowMetric};

use serde::{Deserialize, Serialize};

/// A complete limits snapshot.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LimitsSummary {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    pub refresh_ms: i64,
    #[serde(default)]
    pub providers: Vec<LimitsProvider>,
}

impl LimitsSummary {
    /// Rows worth showing: those with quota, plus any that need a person.
    ///
    /// A transient failure with no retained quota and no windows is noise — the
    /// row would render as an empty bar. A `notConfigured`/`unauthorized` row is
    /// kept even when empty so the UI can say why.
    pub fn visible(&self) -> Vec<&LimitsProvider> {
        self.providers
            .iter()
            .filter(|row| row.has_windows() || row.status.needs_attention())
            .collect()
    }

    /// Providers that reported usable quota.
    pub fn ok(&self) -> Vec<&LimitsProvider> {
        self.providers
            .iter()
            .filter(|row| row.status.is_ok())
            .collect()
    }

    /// Whether any row is stale.
    pub fn any_stale(&self) -> bool {
        self.providers.iter().any(|row| row.stale)
    }

    /// Apply staleness to every row.
    pub fn mark_stale(mut self, now_ms: i64) -> Self {
        let stale_after_ms = self.refresh_ms.saturating_mul(2);
        for row in &mut self.providers {
            let current = std::mem::replace(
                row,
                LimitsProvider::new("", LimitStatus::Ok, LimitSource::Oauth),
            );
            *row = current.with_staleness(now_ms, stale_after_ms);
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(provider: &str, status: LimitStatus, windows: usize) -> LimitsProvider {
        let mut row = LimitsProvider::new(provider, status, LimitSource::Oauth);
        row.windows = (0..windows)
            .map(|_| Window::new(WindowKind::Session))
            .collect();
        row
    }

    #[test]
    fn only_rows_with_something_to_show_are_visible() {
        let summary = LimitsSummary {
            refresh_ms: 300_000,
            providers: vec![
                row("claude", LimitStatus::Ok, 2),
                row("codex", LimitStatus::Unavailable, 0),
                row("claude", LimitStatus::NotConfigured, 0),
                row("codex", LimitStatus::Unauthorized, 0),
            ],
            updated_at: None,
        };
        let visible = summary
            .visible()
            .iter()
            .map(|row| row.status)
            .collect::<Vec<_>>();
        assert_eq!(
            visible,
            vec![
                LimitStatus::Ok,
                LimitStatus::NotConfigured,
                LimitStatus::Unauthorized
            ]
        );
        assert_eq!(summary.ok().len(), 1);
    }

    #[test]
    fn staleness_applies_to_every_row() {
        let mut fresh = row("claude", LimitStatus::Ok, 1);
        fresh.updated_at = Some("2026-11-15T12:00:00Z".to_owned());
        let summary = LimitsSummary {
            refresh_ms: 300_000,
            providers: vec![fresh],
            updated_at: None,
        };
        let now = 1_794_744_000_000;
        assert!(!summary.clone().mark_stale(now + 1_000).any_stale());
        assert!(summary.mark_stale(now + 10 * 300_000).any_stale());
    }
}
