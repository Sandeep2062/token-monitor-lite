//! Builder tests: snapshot → export, with unknown days and costs.
//!
//! The snapshot is built in-memory rather than read from disk, so these prove the
//! projection rules — especially that an unknown day emits no rows.

use std::sync::Arc;

use crate::accounting::{SessionKey, TokenCounts};
use crate::aggregation::{CanonicalSnapshot, DayModelRollup, SessionDayRollup, SourceRef};
use crate::pricing::PricingCatalog;

use super::*;

/// 2026-07-03T12:00:00Z.
const NOW_MS: i64 = 1_783_080_000_000;

fn tokens(input: u64, output: u64) -> TokenCounts {
    TokenCounts {
        input,
        output,
        cache_read: 0,
        cache_write: 0,
        reasoning: 0,
    }
}

fn snapshot(rows: Vec<(chrono::NaiveDate, &str, &str, TokenCounts)>) -> CanonicalSnapshot {
    let mut session_days = Vec::new();
    let mut day_models = Vec::new();
    for (day, client, model, value) in rows {
        session_days.push(SessionDayRollup {
            day,
            key: SessionKey::new(client, "session-1"),
            tokens: value,
            cache_write_1h: 0,
        });
        day_models.push(DayModelRollup {
            day,
            client: client.to_owned(),
            model: model.to_owned(),
            tokens: value,
            cache_write_1h: 0,
            first_seen_ms: NOW_MS,
            last_seen_ms: NOW_MS,
            sources: Arc::<[SourceRef]>::from([]),
        });
    }
    CanonicalSnapshot {
        generation: 1,
        timezone_name: "UTC".to_owned(),
        totals: TokenCounts::default(),
        cache_write_1h: 0,
        day_models: day_models.into(),
        session_models: Arc::from([]),
        session_days: session_days.into(),
        retained_periods: Arc::from([]),
        sources: Arc::from([]),
    }
}

fn day(text: &str) -> chrono::NaiveDate {
    chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d").expect("date")
}

#[test]
fn an_unknown_day_emits_no_rows() {
    // One captured day. Every day before it is unknown, not zero.
    let at = day("2026-07-03");
    let snapshot = snapshot(vec![(at, "codex", "gpt-5", tokens(5, 5))]);
    let input = build(
        &snapshot,
        NOW_MS,
        &PricingCatalog::bundled(),
        "2026-07-03T12:00:00.000Z".to_owned(),
    )
    .expect("build");

    // Only the observed day produced a daily row.
    assert_eq!(input.daily_rows.len(), 1);
    assert_eq!(input.daily_rows[0].date, "2026-07-03");
    // And the JSON series carries exactly one day.
    assert_eq!(input.document.daily.len(), 1);
    assert_eq!(input.document.daily[0].date, "2026-07-03");
}

#[test]
fn a_day_with_no_usage_inside_the_tracked_span_still_emits_nothing() {
    // Two observed days a gap apart: the day between is `Empty`, which has no
    // rows either — but it is not `Unknown`, so it appears in the series.
    let snapshot = snapshot(vec![
        (day("2026-07-01"), "codex", "gpt-5", tokens(1, 1)),
        (day("2026-07-03"), "codex", "gpt-5", tokens(2, 2)),
    ]);
    let input = build(
        &snapshot,
        NOW_MS,
        &PricingCatalog::bundled(),
        "2026-07-03T12:00:00.000Z".to_owned(),
    )
    .expect("build");
    let dates = input
        .daily_rows
        .iter()
        .map(|row| row.date.as_str())
        .collect::<Vec<_>>();
    assert_eq!(dates, vec!["2026-07-01", "2026-07-03"]);
    // The empty middle day is present in the JSON series with zero tokens.
    let middle = input
        .document
        .daily
        .iter()
        .find(|entry| entry.date == "2026-07-02")
        .expect("the empty day is in the series");
    assert_eq!(middle.tokens, 0);
}

#[test]
fn the_daily_row_carries_the_tool_and_its_tokens() {
    let snapshot = snapshot(vec![
        (day("2026-07-03"), "codex", "gpt-5", tokens(5, 5)),
        (day("2026-07-03"), "claude", "opus", tokens(1, 2)),
    ]);
    let input = build(
        &snapshot,
        NOW_MS,
        &PricingCatalog::bundled(),
        "2026-07-03T12:00:00.000Z".to_owned(),
    )
    .expect("build");
    let mut tools = input
        .daily_rows
        .iter()
        .map(|row| (row.tool.clone(), row.tokens))
        .collect::<Vec<_>>();
    tools.sort();
    assert_eq!(
        tools,
        vec![("claude".to_owned(), 3), ("codex".to_owned(), 10)]
    );
}

#[test]
fn the_daily_model_row_reports_the_remainder_as_unclassified() {
    let snapshot = snapshot(vec![(day("2026-07-03"), "codex", "gpt-5", tokens(5, 5))]);
    let input = build(
        &snapshot,
        NOW_MS,
        &PricingCatalog::bundled(),
        "2026-07-03T12:00:00.000Z".to_owned(),
    )
    .expect("build");
    let row = &input.daily_model_rows[0];
    assert_eq!(row.total_tokens, 10);
    assert_eq!(row.output_tokens, 5);
    // Cache components were zero and the split is not flagged complete, so the
    // input remainder is unclassified rather than assumed.
    assert_eq!(row.unclassified_tokens, 5);
    assert_eq!(row.input_tokens, 0);
}

#[test]
fn a_cost_is_a_plain_usd_decimal() {
    let snapshot = snapshot(vec![(
        day("2026-07-03"),
        "codex",
        "gpt-5",
        tokens(1000, 1000),
    )]);
    let input = build(
        &snapshot,
        NOW_MS,
        &PricingCatalog::bundled(),
        "2026-07-03T12:00:00.000Z".to_owned(),
    )
    .expect("build");
    let cost = &input.daily_rows[0].cost_usd;
    assert!(!cost.contains('e') && !cost.contains('E'), "{cost}");
    assert!(!cost.is_empty());
}

#[test]
fn the_snapshot_periods_appear_in_order_with_tools_before_models() {
    let snapshot = snapshot(vec![(day("2026-07-03"), "codex", "gpt-5", tokens(5, 5))]);
    let input = build(
        &snapshot,
        NOW_MS,
        &PricingCatalog::bundled(),
        "2026-07-03T12:00:00.000Z".to_owned(),
    )
    .expect("build");
    let shape = input
        .snapshot_rows
        .iter()
        .map(|row| (row.period, row.dimension))
        .collect::<Vec<_>>();
    // today's tool then model, and the same for month/allTime if non-empty.
    assert_eq!(shape.first(), Some(&("today", "tool")));
    assert!(shape.contains(&("today", "model")));
    // The ordering contract: the period list never goes backwards.
    let order = ["today", "month", "allTime"];
    let mut last = 0;
    for (period, _) in &shape {
        let index = order
            .iter()
            .position(|value| value == period)
            .expect("known period");
        assert!(index >= last, "periods are out of order: {shape:?}");
        last = index;
    }
}

#[test]
fn monthly_rollups_cover_the_same_days_as_the_daily_series() {
    let snapshot = snapshot(vec![
        (day("2026-06-28"), "codex", "gpt-5", tokens(1, 1)),
        (day("2026-07-03"), "codex", "gpt-5", tokens(2, 2)),
    ]);
    let input = build(
        &snapshot,
        NOW_MS,
        &PricingCatalog::bundled(),
        "2026-07-03T12:00:00.000Z".to_owned(),
    )
    .expect("build");
    let months = input
        .document
        .monthly
        .iter()
        .map(|month| month.month.as_str())
        .collect::<Vec<_>>();
    assert!(months.contains(&"2026-06"), "{months:?}");
    assert!(months.contains(&"2026-07"), "{months:?}");
}

#[test]
fn an_empty_snapshot_still_produces_a_valid_document() {
    let snapshot = snapshot(Vec::new());
    let input = build(
        &snapshot,
        NOW_MS,
        &PricingCatalog::bundled(),
        "2026-07-03T12:00:00.000Z".to_owned(),
    )
    .expect("build");
    assert!(input.daily_rows.is_empty());
    assert!(input.daily_model_rows.is_empty());
    // The conditional files are then absent, which the file set enforces.
    let files = super::super::file_set(
        &input.document,
        &input.snapshot_rows,
        &input.daily_rows,
        &input.daily_model_rows,
    );
    assert_eq!(files.len(), 2);
}

#[test]
fn the_document_never_carries_identity() {
    let snapshot = snapshot(vec![(day("2026-07-03"), "codex", "gpt-5", tokens(5, 5))]);
    let input = build(
        &snapshot,
        NOW_MS,
        &PricingCatalog::bundled(),
        "2026-07-03T12:00:00.000Z".to_owned(),
    )
    .expect("build");
    let rendered = super::super::json::render(&input.document);
    for forbidden in [
        "deviceId",
        "hostname",
        "accountEmail",
        "planLabel",
        "sourceId",
    ] {
        assert!(!rendered.contains(forbidden), "leaked {forbidden}");
    }
}
