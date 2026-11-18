//! Quota windows: the unit a provider reports.
//!
//! Field names and derivation rules follow `normalizeLimitWindow` in
//! `src/shared/limits/core.js`; see `docs/migration/limits-spec.md` §1.2–1.3.

use serde::{Deserialize, Serialize};

/// Canonical window cadence, in the reference's `WINDOW_ORDER`.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "camelCase")]
pub enum WindowKind {
    Session,
    Daily,
    Weekly,
    Billing,
}

impl WindowKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Daily => "daily",
            Self::Weekly => "weekly",
            Self::Billing => "billing",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value.trim() {
            "session" => Self::Session,
            "daily" => Self::Daily,
            "weekly" => Self::Weekly,
            "billing" => Self::Billing,
            _ => return None,
        })
    }
}

/// What a window measures.
///
/// Only two exist (`LIMIT_WINDOW_METRICS`). `Credits` is a token/compute
/// allowance; `Spend` is money already consumed. A renderer must not key off the
/// label to tell them apart — that is what this field is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WindowMetric {
    Credits,
    Spend,
}

impl WindowMetric {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Credits => "credits",
            Self::Spend => "spend",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value.trim() {
            "credits" => Self::Credits,
            "spend" => Self::Spend,
            _ => return None,
        })
    }
}

/// One quota window.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Window {
    pub kind: WindowKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metric: Option<WindowMetric>,
    /// Optional display label. Never an identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining: Option<f64>,
    /// Percentage consumed, 0..100. Derived from `used`/`limit` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_percent: Option<f64>,
    /// ISO-8601 instant the window resets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<String>,
    /// Free text describing the reset, as the CLI lane reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_minutes: Option<u64>,
    /// Currency for `Spend` windows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
    /// Extra named buckets a provider exposes beside its canonical quota.
    /// Ordered after canonical windows.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub additional: bool,
    /// Whether a meter is meaningful. False when a spend window is uncapped.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub show_meter: bool,
}

fn default_true() -> bool {
    true
}

fn is_true(value: &bool) -> bool {
    *value
}

impl Window {
    pub fn new(kind: WindowKind) -> Self {
        Self {
            kind,
            metric: None,
            label: None,
            used: None,
            limit: None,
            remaining: None,
            used_percent: None,
            resets_at: None,
            reset_description: None,
            window_minutes: None,
            currency: None,
            additional: false,
            show_meter: true,
        }
    }

    /// Fill in `used_percent` from `used`/`limit` when a provider did not send
    /// one, and derive `remaining` the same way.
    ///
    /// `percentFromWindow` in the reference is explicit that a missing quota is
    /// `null`, never 0: an unknown allowance must not render as a full bar.
    pub fn normalize(mut self) -> Self {
        if self.used_percent.is_none()
            && let (Some(used), Some(limit)) = (self.used, self.limit)
            && limit > 0.0
        {
            let percent = (used / limit * 100.0).clamp(0.0, 100.0);
            self.used_percent = Some(round3(percent));
        } else {
            self.used_percent = self
                .used_percent
                .map(|value| round3(value.clamp(0.0, 100.0)));
        }
        if self.remaining.is_none() {
            self.remaining = match (self.limit, self.used) {
                (Some(limit), Some(used)) => Some(limit - used),
                _ => None,
            };
        }
        self
    }

    /// Percentage consumed, when it is known.
    pub fn used_percent(&self) -> Option<f64> {
        self.used_percent
    }

    /// Percentage still available, when it is known.
    pub fn remaining_percent(&self) -> Option<f64> {
        self.used_percent.map(|value| round3(100.0 - value))
    }

    /// Whether this window would paint a meter.
    pub fn is_meterable(&self) -> bool {
        self.show_meter && self.used_percent.is_some()
    }

    /// The instant this window resets, when it is a usable timestamp.
    pub fn resets_at_ms(&self) -> Option<i64> {
        parse_instant_ms(self.resets_at.as_deref().unwrap_or(""))
    }
}

/// Parse an ISO-8601 instant into epoch milliseconds.
///
/// Accepts the RFC-3339 forms providers send. A bare epoch number is not a date
/// and is rejected, matching `normalizeIsoTimestamp`.
pub fn parse_instant_ms(value: &str) -> Option<i64> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    chrono::DateTime::parse_from_rfc3339(trimmed)
        .ok()
        .map(|instant| instant.timestamp_millis())
}

fn round3(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_is_derived_from_used_and_limit() {
        let window = Window {
            used: Some(235.0),
            limit: Some(2000.0),
            ..Window::new(WindowKind::Billing)
        }
        .normalize();
        assert_eq!(window.used_percent(), Some(11.75));
        assert_eq!(window.remaining_percent(), Some(88.25));
        assert_eq!(window.remaining, Some(1765.0));
        assert!(window.is_meterable());
    }

    #[test]
    fn an_unknown_limit_is_not_a_zero_allowance() {
        // `spend.percent` in the reference reports 0 when unlimited; forwarding
        // that would paint a 0% meter over real spending.
        let window = Window {
            used: Some(12.0),
            limit: None,
            ..Window::new(WindowKind::Billing)
        }
        .normalize();
        assert_eq!(window.used_percent(), None);
        assert!(!window.is_meterable());
    }

    #[test]
    fn explicit_percent_wins_over_derivation_and_is_clamped() {
        let window = Window {
            used: Some(1.0),
            limit: Some(1000.0),
            used_percent: Some(140.0),
            ..Window::new(WindowKind::Session)
        }
        .normalize();
        assert_eq!(window.used_percent(), Some(100.0));
    }

    #[test]
    fn uncapped_spend_hides_the_meter() {
        let window = Window {
            used: Some(3.5),
            show_meter: false,
            ..Window::new(WindowKind::Billing)
        }
        .normalize();
        assert_eq!(window.used_percent(), None);
        assert!(!window.is_meterable());
    }

    #[test]
    fn instants_parse_from_rfc3339_only() {
        assert_eq!(
            parse_instant_ms("2026-11-15T12:00:00Z"),
            Some(1_794_744_000_000)
        );
        assert_eq!(
            parse_instant_ms("2026-11-15T12:00:00.500Z"),
            Some(1_794_744_000_500)
        );
        assert_eq!(
            parse_instant_ms("2026-11-15T12:00:00+02:00"),
            Some(1_794_736_800_000)
        );
        assert_eq!(parse_instant_ms("1700000000"), None);
        assert_eq!(parse_instant_ms(""), None);
        assert_eq!(parse_instant_ms("not a date"), None);
    }

    #[test]
    fn kinds_and_metrics_round_trip() {
        for kind in [
            WindowKind::Session,
            WindowKind::Daily,
            WindowKind::Weekly,
            WindowKind::Billing,
        ] {
            assert_eq!(WindowKind::parse(kind.as_str()), Some(kind));
        }
        for metric in [WindowMetric::Credits, WindowMetric::Spend] {
            assert_eq!(WindowMetric::parse(metric.as_str()), Some(metric));
        }
        assert_eq!(WindowKind::parse("monthly"), None);
    }

    #[test]
    fn window_order_matches_the_reference() {
        assert!(WindowKind::Session < WindowKind::Daily);
        assert!(WindowKind::Daily < WindowKind::Weekly);
        assert!(WindowKind::Weekly < WindowKind::Billing);
    }
}
