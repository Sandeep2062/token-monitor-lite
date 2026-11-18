//! Trends history: the ~370-day window, unknown-versus-empty days, and monthly
//! rollups.
//!
//! The distinction these pin is the one the inventory states as a rule — "missing
//! is unknown, not zero". A day before tracking began must never render as a real
//! zero, because every average built on it would then be understated.

use std::sync::Arc;

use token_monitor_lite::accounting::{SessionKey, TokenCounts};
use token_monitor_lite::aggregation::{CanonicalSnapshot, SessionDayRollup};
use token_monitor_lite::analytics::{DayState, MAX_TREND_DAYS, daily_series, monthly_rollups};

/// 2026-11-13T12:00:00Z.
const NOW_MS: i64 = 1_794_571_200_000;

fn day(text: &str) -> chrono::NaiveDate {
    chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d").expect("date")
}

fn snapshot(rows: Vec<(chrono::NaiveDate, u64)>) -> CanonicalSnapshot {
    let session_days = rows
        .into_iter()
        .map(|(at, input)| SessionDayRollup {
            day: at,
            key: SessionKey::new("codex", "session-1"),
            tokens: TokenCounts {
                input,
                ..TokenCounts::default()
            },
            cache_write_1h: 0,
        })
        .collect::<Vec<_>>();
    CanonicalSnapshot {
        generation: 1,
        timezone_name: "UTC".to_owned(),
        totals: TokenCounts::default(),
        cache_write_1h: 0,
        day_models: Arc::from([]),
        session_models: Arc::from([]),
        session_days: session_days.into(),
        retained_periods: Arc::from([]),
        sources: Arc::from([]),
    }
}

#[test]
fn the_window_is_roughly_a_year_not_thirty_days() {
    assert_eq!(MAX_TREND_DAYS, 370);
    let series = daily_series(&snapshot(Vec::new()), MAX_TREND_DAYS, NOW_MS).expect("series");
    assert_eq!(series.len(), 370);
    assert_eq!(series.last().expect("last").day, day("2026-11-13"));
    assert_eq!(series.first().expect("first").day, day("2025-11-09"));
}

#[test]
fn a_day_before_tracking_began_is_unknown_not_zero() {
    // One captured day: everything before it has no evidence behind it.
    let series =
        daily_series(&snapshot(vec![(day("2026-11-13"), 10)]), 10, NOW_MS).expect("series");
    assert_eq!(series.len(), 10);
    // The last day is today and is observed.
    assert_eq!(series[9].state, DayState::Observed);
    assert_eq!(series[9].total, 10);
    // The first nine precede tracking, so they are unknown.
    for entry in &series[..9] {
        assert_eq!(
            entry.state,
            DayState::Unknown,
            "{} should be unknown",
            entry.day
        );
        assert_eq!(entry.total, 0, "an unknown day must not invent tokens");
    }
}

#[test]
fn a_gap_inside_the_tracked_span_is_empty_not_unknown() {
    // Two captured days with a gap: the middle day was available and reported
    // nothing, which is a real zero.
    let series = daily_series(
        &snapshot(vec![(day("2026-11-10"), 5), (day("2026-11-13"), 5)]),
        10,
        NOW_MS,
    )
    .expect("series");
    let middle = series
        .iter()
        .find(|entry| entry.day == day("2026-11-11"))
        .expect("the gap day");
    assert_eq!(middle.state, DayState::Empty);
    assert_eq!(middle.total, 0);
    assert!(
        middle.state.is_known(),
        "an empty day may count as a real zero"
    );
}

#[test]
fn an_observed_day_reports_its_tokens() {
    let series = daily_series(&snapshot(vec![(day("2026-11-12"), 42)]), 5, NOW_MS).expect("series");
    let observed = series
        .iter()
        .find(|entry| entry.day == day("2026-11-12"))
        .expect("observed day");
    assert_eq!(observed.state, DayState::Observed);
    assert_eq!(observed.total, 42);
}

#[test]
fn an_empty_snapshot_is_entirely_unknown() {
    // Nothing captured at all: no day can be claimed as zero.
    let series = daily_series(&snapshot(Vec::new()), 7, NOW_MS).expect("series");
    // Today is always reported as observed because the monitor is running, but
    // every preceding day is unknown.
    for entry in &series[..series.len() - 1] {
        assert_eq!(entry.state, DayState::Unknown);
    }
}

#[test]
fn days_outside_the_range_are_not_included() {
    let series = daily_series(
        &snapshot(vec![(day("2026-01-01"), 100), (day("2026-11-13"), 10)]),
        7,
        NOW_MS,
    )
    .expect("series");
    assert_eq!(series.len(), 7);
    let total = series.iter().fold(0_u128, |sum, entry| sum + entry.total);
    // Only the in-window day counts.
    assert_eq!(total, 10);
}

#[test]
fn a_range_beyond_the_cap_is_refused_rather_than_truncated() {
    assert!(daily_series(&snapshot(Vec::new()), MAX_TREND_DAYS + 1, NOW_MS).is_err());
    assert!(daily_series(&snapshot(Vec::new()), 0, NOW_MS).is_err());
    assert!(daily_series(&snapshot(Vec::new()), MAX_TREND_DAYS, NOW_MS).is_ok());
}

#[test]
fn monthly_rollups_sum_observed_days_and_flag_partial_months() {
    // November has two observed days; the rest of the month is unknown, so the
    // rollup must be marked partial.
    let series = daily_series(
        &snapshot(vec![(day("2026-11-10"), 5), (day("2026-11-13"), 7)]),
        40,
        NOW_MS,
    )
    .expect("series");
    let months = monthly_rollups(&series);
    let november = months
        .iter()
        .find(|month| month.month == "2026-11")
        .expect("November");
    assert_eq!(november.total, 12, "the observed days must sum");
    assert!(
        !november.is_complete(),
        "a month with unknown days must not look complete"
    );
    assert!(november.unknown_days > 0);
    assert!(november.observed_days + november.unknown_days > 0);
    assert!(november.state.is_some());
}

#[test]
fn a_month_whose_days_are_all_unknown_sums_to_nothing() {
    // A one-day window with no history at all: every day unknown, so the month
    // carries no state and no total.
    let series = daily_series(&snapshot(Vec::new()), 2, NOW_MS).expect("series");
    let months = monthly_rollups(&series);
    // Today is observed, so the current month exists; any earlier month does not
    // appear at all rather than appearing as a zero month.
    assert!(
        months
            .iter()
            .all(|month| month.total > 0 || month.observed_days > 0),
        "an all-unknown month was reported as a zero month"
    );
}

#[test]
fn monthly_rollups_are_ordered_oldest_first() {
    let series = daily_series(
        &snapshot(vec![(day("2026-09-15"), 1), (day("2026-11-13"), 2)]),
        90,
        NOW_MS,
    )
    .expect("series");
    let months = monthly_rollups(&series);
    let ordered = months.windows(2).all(|pair| pair[0].month < pair[1].month);
    assert!(ordered, "months are not sorted: {:?}", months.len());
}

#[test]
fn the_series_is_contiguous_and_ascending() {
    let series = daily_series(&snapshot(vec![(day("2026-11-13"), 1)]), 30, NOW_MS).expect("series");
    for pair in series.windows(2) {
        let expected = pair[0]
            .day
            .succ_opt()
            .expect("the series must not end at the maximum date");
        assert_eq!(pair[1].day, expected, "the series has a gap");
    }
}
