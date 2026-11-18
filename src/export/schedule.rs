//! Auto-export cadence.
//!
//! `docs/export.md` offers a cadence "30 seconds to 60 minutes; default every
//! minute". See `docs/migration/export-spec.md` §5.
//!
//! The cadence is a *ceiling*, not a tick: an export is attempted at most once per
//! interval, and the attempt is then skipped entirely when the payload did not
//! change. So a short interval costs nothing on an idle machine, and a long one
//! only delays a real update.

/// Lowest selectable cadence.
pub const MIN_INTERVAL_SECONDS: u64 = 30;
/// Highest selectable cadence.
pub const MAX_INTERVAL_SECONDS: u64 = 60 * 60;
/// The documented default.
pub const DEFAULT_INTERVAL_SECONDS: u64 = 60;

/// Whether an automatic export is due.
///
/// `last_attempt_ms` is the last time an attempt *ran*, whether or not it wrote
/// anything: a folder that keeps failing must not be retried on every tick.
pub fn is_due(interval_seconds: u64, last_attempt_ms: Option<i64>, now_ms: i64) -> bool {
    let interval_ms = interval_seconds
        .clamp(MIN_INTERVAL_SECONDS, MAX_INTERVAL_SECONDS)
        .saturating_mul(1_000) as i64;
    match last_attempt_ms {
        Some(last) => now_ms.saturating_sub(last) >= interval_ms,
        // Nothing has been attempted yet, so the first tick may run.
        None => true,
    }
}

/// Clamp a stored cadence into the selectable range.
pub fn clamp_interval(seconds: u64) -> u64 {
    seconds.clamp(MIN_INTERVAL_SECONDS, MAX_INTERVAL_SECONDS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_tick_is_due() {
        assert!(is_due(DEFAULT_INTERVAL_SECONDS, None, 0));
    }

    #[test]
    fn nothing_is_due_before_the_interval_elapses() {
        let now = 1_000_000;
        assert!(!is_due(60, Some(now), now));
        assert!(!is_due(60, Some(now), now + 59_999));
        assert!(is_due(60, Some(now), now + 60_000));
    }

    #[test]
    fn an_interval_outside_the_range_is_clamped() {
        // An edited settings file asking for 1 s must not make the app write
        // every second.
        assert!(!is_due(1, Some(0), 999));
        assert!(is_due(1, Some(0), 30_000));
        // And asking for a day must not exceed the documented hour.
        assert!(is_due(86_400, Some(0), 3_600_000));
        assert!(!is_due(86_400, Some(0), 3_599_999));
    }

    #[test]
    fn the_range_matches_the_documentation() {
        assert_eq!(MIN_INTERVAL_SECONDS, 30);
        assert_eq!(MAX_INTERVAL_SECONDS, 3600);
        assert_eq!(DEFAULT_INTERVAL_SECONDS, 60);
        assert_eq!(clamp_interval(0), 30);
        assert_eq!(clamp_interval(120), 120);
        assert_eq!(clamp_interval(99_999), 3600);
    }

    #[test]
    fn a_backwards_clock_does_not_trigger_a_storm() {
        // If the clock jumps back, the difference is negative: not due.
        assert!(!is_due(60, Some(500_000), 100_000));
    }
}
