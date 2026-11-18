//! A limits provider row: one account's quota, as the UI consumes it.
//!
//! Shape follows `normalizeLimitProvider` in `src/shared/limits/core.js`;
//! see `docs/migration/limits-spec.md` §1.3.

use super::status::LimitStatus;
use super::window::{Window, WindowKind};
use serde::{Deserialize, Serialize};

/// Where a row came from.
///
/// The reference allows `oauth | cli | web | rpc | local | api`. Only `Oauth` is
/// produced today: the CLI and app-server lanes are deliberately unimplemented
/// (spec §2.1.5), so no row can claim them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LimitSource {
    Oauth,
    Cli,
    Web,
    Rpc,
    Local,
    Api,
}

impl LimitSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Oauth => "oauth",
            Self::Cli => "cli",
            Self::Web => "web",
            Self::Rpc => "rpc",
            Self::Local => "local",
            Self::Api => "api",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value.trim() {
            "oauth" => Self::Oauth,
            "cli" => Self::Cli,
            "web" => Self::Web,
            "rpc" => Self::Rpc,
            "local" => Self::Local,
            "api" => Self::Api,
            _ => return None,
        })
    }
}

/// A provider's quota for one account.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LimitsProvider {
    /// Catalog id: `claude` or `codex`.
    pub provider: String,
    /// Stable account identity. Never a credential, never a credential path.
    #[serde(default)]
    pub account_key: String,
    #[serde(default)]
    pub account_label: String,
    #[serde(default)]
    pub account_name: String,
    #[serde(default)]
    pub account_email: String,
    #[serde(default)]
    pub plan_label: String,
    pub status: LimitStatus,
    pub source: LimitSource,
    /// ISO-8601 instant of the attempt that produced this row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub windows: Vec<Window>,
    /// Derived, not sent: older than the configured refresh interval.
    #[serde(default)]
    pub stale: bool,
}

impl LimitsProvider {
    pub fn new(provider: impl Into<String>, status: LimitStatus, source: LimitSource) -> Self {
        Self {
            provider: provider.into(),
            account_key: String::new(),
            account_label: String::new(),
            account_name: String::new(),
            account_email: String::new(),
            plan_label: String::new(),
            status,
            source,
            updated_at: None,
            windows: Vec::new(),
            stale: false,
        }
    }

    /// Sort windows into display order: canonical kinds first, then additional
    /// buckets, each by canonical kind.
    ///
    /// The reference sorts per provider; a single stable rule covers both
    /// `WINDOW_ORDER` and codex's "canonical before additional" intent.
    pub fn normalize(mut self) -> Self {
        self.windows = self
            .windows
            .into_iter()
            .map(Window::normalize)
            .collect::<Vec<_>>();
        self.windows.sort_by(|left, right| {
            left.additional
                .cmp(&right.additional)
                .then(left.kind.cmp(&right.kind))
                .then(left.label.cmp(&right.label))
        });
        self
    }

    /// Whether this row carries anything worth showing.
    pub fn has_windows(&self) -> bool {
        !self.windows.is_empty()
    }

    /// The window to headline: session first, then daily, weekly, billing —
    /// whichever exists earliest in that order.
    pub fn primary_window(&self) -> Option<&Window> {
        [WindowKind::Session, WindowKind::Daily, WindowKind::Weekly]
            .into_iter()
            .find_map(|kind| self.windows.iter().find(|window| window.kind == kind))
            .or_else(|| {
                self.windows
                    .iter()
                    .find(|window| window.kind == WindowKind::Billing)
            })
    }

    /// The soonest reset among all windows, in epoch milliseconds.
    pub fn next_reset_ms(&self) -> Option<i64> {
        self.windows.iter().filter_map(Window::resets_at_ms).min()
    }

    /// Mark the row stale when its attempt is older than `stale_after_ms`.
    pub fn with_staleness(mut self, now_ms: i64, stale_after_ms: i64) -> Self {
        self.stale = match super::window::parse_instant_ms(self.updated_at.as_deref().unwrap_or(""))
        {
            Some(updated) => now_ms.saturating_sub(updated) > stale_after_ms.max(0),
            // A row with no timestamp cannot be shown as fresh.
            None => true,
        };
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(kind: WindowKind, percent: f64) -> Window {
        Window {
            used_percent: Some(percent),
            ..Window::new(kind)
        }
    }

    #[test]
    fn windows_sort_canonical_first_then_by_cadence() {
        let row = LimitsProvider {
            windows: vec![
                Window {
                    additional: true,
                    ..window(WindowKind::Session, 5.0)
                },
                window(WindowKind::Billing, 20.0),
                window(WindowKind::Session, 10.0),
                window(WindowKind::Weekly, 30.0),
            ],
            ..LimitsProvider::new("codex", LimitStatus::Ok, LimitSource::Oauth)
        }
        .normalize();
        let order = row
            .windows
            .iter()
            .map(|window| (window.kind, window.additional))
            .collect::<Vec<_>>();
        assert_eq!(
            order,
            vec![
                (WindowKind::Session, false),
                (WindowKind::Weekly, false),
                (WindowKind::Billing, false),
                (WindowKind::Session, true),
            ]
        );
    }

    #[test]
    fn primary_window_prefers_session_then_falls_back_to_billing() {
        let row = LimitsProvider {
            windows: vec![window(WindowKind::Billing, 20.0)],
            ..LimitsProvider::new("claude", LimitStatus::Ok, LimitSource::Oauth)
        }
        .normalize();
        assert_eq!(
            row.primary_window().map(|w| w.kind),
            Some(WindowKind::Billing)
        );

        let row = LimitsProvider {
            windows: vec![
                window(WindowKind::Weekly, 30.0),
                window(WindowKind::Session, 10.0),
            ],
            ..LimitsProvider::new("claude", LimitStatus::Ok, LimitSource::Oauth)
        }
        .normalize();
        assert_eq!(
            row.primary_window().map(|w| w.kind),
            Some(WindowKind::Session)
        );
    }

    #[test]
    fn next_reset_is_the_soonest_window_reset() {
        let row = LimitsProvider {
            windows: vec![
                Window {
                    resets_at: Some("2026-11-20T00:00:00Z".to_owned()),
                    ..Window::new(WindowKind::Weekly)
                },
                Window {
                    resets_at: Some("2026-11-15T12:00:00Z".to_owned()),
                    ..Window::new(WindowKind::Session)
                },
                Window::new(WindowKind::Daily),
            ],
            ..LimitsProvider::new("claude", LimitStatus::Ok, LimitSource::Oauth)
        };
        assert_eq!(row.next_reset_ms(), Some(1_794_744_000_000));
    }

    #[test]
    fn staleness_is_derived_from_the_attempt_instant() {
        let row = LimitsProvider {
            updated_at: Some("2026-11-15T12:00:00Z".to_owned()),
            ..LimitsProvider::new("claude", LimitStatus::Ok, LimitSource::Oauth)
        };
        let now = 1_794_744_000_000;
        assert!(!row.clone().with_staleness(now + 1_000, 300_000).stale);
        assert!(row.clone().with_staleness(now + 300_001, 300_000).stale);
        // No timestamp at all can never be shown as fresh.
        let undated = LimitsProvider::new("claude", LimitStatus::Ok, LimitSource::Oauth);
        assert!(undated.with_staleness(now, 300_000).stale);
    }

    #[test]
    fn sources_round_trip() {
        for source in [
            LimitSource::Oauth,
            LimitSource::Cli,
            LimitSource::Web,
            LimitSource::Rpc,
            LimitSource::Local,
            LimitSource::Api,
        ] {
            assert_eq!(LimitSource::parse(source.as_str()), Some(source));
        }
        assert_eq!(LimitSource::parse("guess"), None);
    }
}
