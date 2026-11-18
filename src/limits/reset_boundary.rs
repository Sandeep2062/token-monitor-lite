//! The next refresh after a quota window resets.
//!
//! From `src/shared/limits/resetBoundary.js`; see
//! `docs/migration/limits-spec.md` §1.6. The point is not to poll while the
//! window is still open, and not to wait an interval after it closes: the quota
//! is known to have changed at a known instant, so refresh just after it.

use super::provider::LimitsProvider;

/// How long after a reset to probe.
///
/// A provider's own reset usually lands a little before its counters catch up,
/// so probing exactly on the boundary reads the pre-reset quota.
pub const RESET_BOUNDARY_DELAY_MS: i64 = 30_000;
/// Floor for a scheduled timer.
pub const RESET_BOUNDARY_MIN_TIMER_MS: i64 = 5_000;
/// The ceiling of a 32-bit millisecond timer.
pub const RESET_BOUNDARY_MAX_TIMER_MS: i64 = 2_147_483_647;

/// A refresh to schedule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Boundary {
    /// Absolute epoch milliseconds.
    pub refresh_at_ms: i64,
    /// Delay to hand a timer, clamped for the platform.
    pub delay_ms: i64,
    /// The windows whose reset caused this.
    pub keys: Vec<String>,
}

/// One reset instant belonging to a provider row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoundaryEntry {
    pub reset_at_ms: i64,
    pub key: String,
    pub provider: String,
    pub account_key: String,
}

/// Every reset instant in the snapshot, skipping ones already attempted.
///
/// Already-attempted keys are excluded so a refresh triggered by one boundary
/// does not immediately re-trigger itself when the provider has not yet
/// published the new quota.
pub fn entries(rows: &[LimitsProvider], attempted: &[String]) -> Vec<BoundaryEntry> {
    let mut entries = Vec::new();
    for row in rows {
        for window in &row.windows {
            let Some(reset_at_ms) = window.resets_at_ms() else {
                continue;
            };
            let key = format!(
                "{}:{}:{}:{}",
                row.provider,
                row.account_key,
                window.kind.as_str(),
                reset_at_ms
            );
            if attempted.iter().any(|done| done == &key) {
                continue;
            }
            entries.push(BoundaryEntry {
                reset_at_ms,
                key,
                provider: row.provider.clone(),
                account_key: row.account_key.clone(),
            });
        }
    }
    entries
}

/// The soonest boundary, if any.
pub fn next_boundary(
    rows: &[LimitsProvider],
    now_ms: i64,
    attempted: &[String],
) -> Option<Boundary> {
    let entries = entries(rows, attempted);
    let soonest = entries.iter().map(|entry| entry.reset_at_ms).min()?;
    let refresh_at_ms = soonest.saturating_add(RESET_BOUNDARY_DELAY_MS);
    Some(Boundary {
        refresh_at_ms,
        delay_ms: (refresh_at_ms - now_ms)
            .clamp(RESET_BOUNDARY_MIN_TIMER_MS, RESET_BOUNDARY_MAX_TIMER_MS),
        keys: entries
            .iter()
            .filter(|entry| entry.reset_at_ms == soonest)
            .map(|entry| entry.key.clone())
            .collect(),
    })
}

/// Provider ids whose quota resets at the soonest boundary.
pub fn providers_due(rows: &[LimitsProvider], boundary: &Boundary) -> Vec<String> {
    let mut providers = entries(rows, &[])
        .into_iter()
        .filter(|entry| boundary.keys.iter().any(|key| key == &entry.key))
        .map(|entry| entry.provider)
        .collect::<Vec<_>>();
    providers.sort();
    providers.dedup();
    providers
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::provider::LimitSource;
    use crate::limits::status::LimitStatus;
    use crate::limits::window::{Window, WindowKind};

    fn row(provider: &str, resets: &[&str]) -> LimitsProvider {
        LimitsProvider {
            windows: resets
                .iter()
                .map(|instant| Window {
                    resets_at: Some((*instant).to_owned()),
                    ..Window::new(WindowKind::Session)
                })
                .collect(),
            ..LimitsProvider::new(provider, LimitStatus::Ok, LimitSource::Oauth)
        }
    }

    const NOW: i64 = 1_794_744_000_000;

    #[test]
    fn the_soonest_reset_schedules_a_refresh_thirty_seconds_later() {
        let rows = vec![
            row("claude", &["2026-11-15T13:00:00Z", "2026-11-20T00:00:00Z"]),
            row("codex", &["2026-11-16T00:00:00Z"]),
        ];
        let boundary = next_boundary(&rows, NOW, &[]).expect("boundary");
        assert_eq!(
            boundary.refresh_at_ms,
            1_794_747_600_000 + RESET_BOUNDARY_DELAY_MS
        );
        assert_eq!(
            boundary.delay_ms,
            3_600_000 + RESET_BOUNDARY_DELAY_MS - (NOW - NOW)
        );
        assert_eq!(boundary.keys.len(), 1);
        assert_eq!(providers_due(&rows, &boundary), vec!["claude".to_owned()]);
    }

    #[test]
    fn simultaneous_resets_share_one_refresh() {
        let rows = vec![
            row("claude", &["2026-11-15T13:00:00Z"]),
            row("codex", &["2026-11-15T13:00:00Z"]),
        ];
        let boundary = next_boundary(&rows, NOW, &[]).expect("boundary");
        assert_eq!(boundary.keys.len(), 2);
        assert_eq!(
            providers_due(&rows, &boundary),
            vec!["claude".to_owned(), "codex".to_owned()]
        );
    }

    #[test]
    fn an_attempted_boundary_is_skipped() {
        let rows = vec![row(
            "claude",
            &["2026-11-15T13:00:00Z", "2026-11-16T00:00:00Z"],
        )];
        let first = next_boundary(&rows, NOW, &[]).expect("first");
        let second = next_boundary(&rows, NOW, &first.keys).expect("second");
        assert!(second.refresh_at_ms > first.refresh_at_ms);
        assert_ne!(second.keys, first.keys);
    }

    #[test]
    fn the_timer_floor_keeps_a_reset_timer_responsive() {
        // A boundary whose reset is already past still must not spin: the delay
        // floors at five seconds rather than going negative.
        let rows = vec![row("claude", &["2026-11-15T11:00:00Z"])];
        let boundary = next_boundary(&rows, NOW, &[]).expect("boundary");
        assert_eq!(boundary.delay_ms, RESET_BOUNDARY_MIN_TIMER_MS);
    }

    #[test]
    fn the_timer_ceiling_respects_the_platform_limit() {
        let rows = vec![row("claude", &["2036-11-15T12:00:00Z"])];
        let boundary = next_boundary(&rows, NOW, &[]).expect("boundary");
        assert_eq!(boundary.delay_ms, RESET_BOUNDARY_MAX_TIMER_MS);
    }

    #[test]
    fn no_reset_instants_means_no_timer() {
        assert!(next_boundary(&[row("claude", &[])], NOW, &[]).is_none());
        assert!(next_boundary(&[], NOW, &[]).is_none());
    }
}
