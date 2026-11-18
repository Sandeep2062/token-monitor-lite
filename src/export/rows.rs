//! Export row construction.
//!
//! Ports `buildSnapshotRows`, `buildDailyRows`, `buildDailyModelRows` and
//! `dailyModelComponents` from `src/shared/exporter.js`. See
//! `docs/migration/export-spec.md` §3.
//!
//! Every input type here is deliberately narrow: there is no device, account,
//! email, plan, or quota field anywhere in this module, so the privacy boundary
//! is enforced by the compiler rather than by a promise. Spec §4.

use std::collections::BTreeMap;

use crate::analytics::{DailyUsage, DayState};
use crate::pricing::CostEstimate;

/// The three periods, in the reference's order.
pub const PERIODS: [&str; 3] = ["today", "month", "allTime"];

pub const SNAPSHOT_COLUMNS: [&str; 5] = ["period", "dimension", "name", "tokens", "cost_usd"];
pub const DAILY_COLUMNS: [&str; 4] = ["date", "tool", "tokens", "cost_usd"];
pub const DAILY_MODEL_COLUMNS: [&str; 9] = [
    "date",
    "model",
    "input_tokens",
    "output_tokens",
    "cache_read_tokens",
    "cache_write_tokens",
    "unclassified_tokens",
    "total_tokens",
    "cost_usd",
];

/// One tool's or model's total for one period.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SnapshotEntry {
    pub name: String,
    pub tokens: u128,
    pub cost: CostEstimate,
}

/// One period's totals, split the way the snapshot CSV wants them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PeriodTotalsRow {
    pub tools: Vec<SnapshotEntry>,
    pub models: Vec<SnapshotEntry>,
}

/// A snapshot row, ready to serialize.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotRow {
    pub period: &'static str,
    pub dimension: &'static str,
    pub name: String,
    pub tokens: u128,
    pub cost_usd: String,
}

/// Build the snapshot rows: for each period, tools then models.
///
/// The period order and the tool-before-model order are the reference's. A period
/// with nothing in it contributes no rows.
pub fn snapshot_rows(periods: &[PeriodTotalsRow; 3]) -> Vec<SnapshotRow> {
    let mut rows = Vec::new();
    for (index, period) in PERIODS.iter().enumerate() {
        let totals = &periods[index];
        for entry in &totals.tools {
            rows.push(SnapshotRow {
                period,
                dimension: "tool",
                name: entry.name.clone(),
                tokens: entry.tokens,
                cost_usd: super::csv::format_usd(entry.cost.nano_usd),
            });
        }
        for entry in &totals.models {
            rows.push(SnapshotRow {
                period,
                dimension: "model",
                name: entry.name.clone(),
                tokens: entry.tokens,
                cost_usd: super::csv::format_usd(entry.cost.nano_usd),
            });
        }
    }
    rows
}

/// One day × one tool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DailyRow {
    pub date: String,
    pub tool: String,
    pub tokens: u128,
    pub cost_usd: String,
}

/// One day × one model, with the component split.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DailyModelRow {
    pub date: String,
    pub model: String,
    pub input_tokens: u128,
    pub output_tokens: u128,
    pub cache_read_tokens: u128,
    pub cache_write_tokens: u128,
    pub unclassified_tokens: u128,
    pub total_tokens: u128,
    pub cost_usd: String,
}

/// A day's usage for one model, as the exporter sees it.
///
/// `components_available` is the reference's `tokenComponentsAvailable`: true only
/// when the source could account for every token, which is what lets a zero
/// remainder mean zero rather than unknown.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelDay {
    pub tokens: u128,
    pub output_tokens: u128,
    pub cache_read_tokens: u128,
    pub cache_write_tokens: u128,
    /// An explicit unknown remainder, when the source reported one.
    pub unclassified_tokens: Option<u128>,
    pub cost: CostEstimate,
}

/// The component split for one day × model.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Components {
    pub input: u128,
    pub output: u128,
    pub cache_read: u128,
    pub cache_write: u128,
    pub unclassified: u128,
    pub total: u128,
}

/// Split one day×model row into components.
///
/// Two rules matter and both exist to avoid inventing a number:
///
/// 1. If the known components exceed the total, the row is malformed or
///    mixed-version. The trusted total is kept whole and reported as
///    `unclassified` rather than fabricating a split.
/// 2. A remainder that the source did not account for is `unclassified`, never
///    input. Guessing it as input would silently inflate the most expensive
///    component.
pub fn components(components_available: bool, value: &ModelDay) -> Components {
    let total = value.tokens;
    let output = value.output_tokens;
    let cache_read = value.cache_read_tokens;
    let cache_write = value.cache_write_tokens;
    let known = output
        .saturating_add(cache_read)
        .saturating_add(cache_write);

    if known > total {
        // Keep the trusted total; refuse the split.
        return Components {
            input: 0,
            output: 0,
            cache_read: 0,
            cache_write: 0,
            unclassified: total,
            total,
        };
    }

    let remaining = total - known;
    let unclassified = match value.unclassified_tokens {
        Some(explicit) => explicit.min(remaining),
        None if components_available => 0,
        None => remaining,
    };
    Components {
        input: remaining.saturating_sub(unclassified),
        output,
        cache_read,
        cache_write,
        unclassified,
        total,
    }
}

/// Every tool's total for one day.
///
/// `None` for an unknown day: an unknown day contributes no rows, because writing
/// a zero row would claim usage was measured and found to be nothing.
pub fn daily_rows_for_day(
    day: &DailyUsage,
    per_tool: &BTreeMap<String, (u128, CostEstimate)>,
) -> Vec<DailyRow> {
    if day.state == DayState::Unknown {
        return Vec::new();
    }
    let date = date_text(day);
    per_tool
        .iter()
        .map(|(tool, (tokens, cost))| DailyRow {
            date: date.clone(),
            tool: tool.clone(),
            tokens: *tokens,
            cost_usd: super::csv::format_usd(cost.nano_usd),
        })
        .collect()
}

/// Every model's components for one day.
pub fn daily_model_rows_for_day(
    day: &DailyUsage,
    components_available: bool,
    per_model: &BTreeMap<String, ModelDay>,
) -> Vec<DailyModelRow> {
    if day.state == DayState::Unknown {
        return Vec::new();
    }
    let date = date_text(day);
    per_model
        .iter()
        .map(|(model, value)| {
            let split = components(components_available, value);
            DailyModelRow {
                date: date.clone(),
                model: model.clone(),
                input_tokens: split.input,
                output_tokens: split.output,
                cache_read_tokens: split.cache_read,
                cache_write_tokens: split.cache_write,
                unclassified_tokens: split.unclassified,
                total_tokens: split.total,
                cost_usd: super::csv::format_usd(value.cost.nano_usd),
            }
        })
        .collect()
}

/// An ISO `YYYY-MM-DD` date.
pub fn date_text(day: &DailyUsage) -> String {
    day.day.format("%Y-%m-%d").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model_day(tokens: u128, output: u128, cache_read: u128, cache_write: u128) -> ModelDay {
        ModelDay {
            tokens,
            output_tokens: output,
            cache_read_tokens: cache_read,
            cache_write_tokens: cache_write,
            ..ModelDay::default()
        }
    }

    #[test]
    fn the_column_sets_match_the_reference_exactly() {
        assert_eq!(
            SNAPSHOT_COLUMNS,
            ["period", "dimension", "name", "tokens", "cost_usd"]
        );
        assert_eq!(DAILY_COLUMNS, ["date", "tool", "tokens", "cost_usd"]);
        assert_eq!(
            DAILY_MODEL_COLUMNS,
            [
                "date",
                "model",
                "input_tokens",
                "output_tokens",
                "cache_read_tokens",
                "cache_write_tokens",
                "unclassified_tokens",
                "total_tokens",
                "cost_usd"
            ]
        );
        assert_eq!(PERIODS, ["today", "month", "allTime"]);
    }

    #[test]
    fn a_complete_split_leaves_no_unclassified_remainder() {
        // 10 = 2 input + 2 output + 1 cache_read + 5 cache_write.
        let split = components(true, &model_day(10, 2, 1, 5));
        assert_eq!(split.input, 2);
        assert_eq!(split.output, 2);
        assert_eq!(split.cache_read, 1);
        assert_eq!(split.cache_write, 5);
        assert_eq!(split.unclassified, 0);
        assert_eq!(split.total, 10);
    }

    #[test]
    fn an_unaccounted_remainder_is_unclassified_not_input() {
        // The documented example: opus, 12 total, output 2, nothing else known.
        let split = components(false, &model_day(12, 2, 0, 0));
        assert_eq!(split.output, 2);
        assert_eq!(split.unclassified, 10);
        assert_eq!(split.input, 0, "the remainder must not be guessed as input");
        assert_eq!(split.total, 12);
    }

    #[test]
    fn a_zero_remainder_means_zero_when_components_are_available() {
        let split = components(true, &model_day(12, 2, 0, 0));
        assert_eq!(split.unclassified, 0);
        assert_eq!(split.input, 10);
    }

    #[test]
    fn an_explicit_unclassified_is_clamped_to_the_remainder() {
        let mut day = model_day(12, 2, 0, 0);
        day.unclassified_tokens = Some(99);
        let split = components(true, &day);
        assert_eq!(split.unclassified, 10, "clamped to the remainder");
        assert_eq!(split.input, 0);
    }

    #[test]
    fn components_exceeding_the_total_keep_the_total_and_refuse_the_split() {
        // Malformed or mixed-version: 5 total but 8 known components.
        let split = components(true, &model_day(5, 3, 3, 2));
        assert_eq!(split.total, 5);
        assert_eq!(split.unclassified, 5, "the whole total is unclassified");
        assert_eq!(split.input, 0);
        assert_eq!(split.output, 0);
        assert_eq!(split.cache_read, 0);
        assert_eq!(split.cache_write, 0);
    }

    #[test]
    fn snapshot_rows_are_period_ordered_then_tools_then_models() {
        let periods = [
            PeriodTotalsRow {
                tools: vec![SnapshotEntry {
                    name: "codex".to_owned(),
                    tokens: 20,
                    cost: CostEstimate::free(),
                }],
                models: vec![SnapshotEntry {
                    name: "gpt-5".to_owned(),
                    tokens: 20,
                    cost: CostEstimate::free(),
                }],
            },
            PeriodTotalsRow::default(),
            PeriodTotalsRow {
                tools: vec![SnapshotEntry {
                    name: "codex".to_owned(),
                    tokens: 100,
                    cost: CostEstimate::free(),
                }],
                models: Vec::new(),
            },
        ];
        let rows = snapshot_rows(&periods);
        let shape = rows
            .iter()
            .map(|row| (row.period, row.dimension, row.name.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(
            shape,
            vec![
                ("today", "tool", "codex"),
                ("today", "model", "gpt-5"),
                ("allTime", "tool", "codex"),
            ]
        );
    }

    #[test]
    fn a_non_integral_cost_is_formatted_as_plain_usd() {
        let periods = [
            PeriodTotalsRow {
                tools: vec![SnapshotEntry {
                    name: "codex".to_owned(),
                    tokens: 20,
                    cost: CostEstimate {
                        nano_usd: 2_000_000_000,
                        unpriced_tokens: 0,
                        provenance: None,
                    },
                }],
                models: Vec::new(),
            },
            PeriodTotalsRow::default(),
            PeriodTotalsRow::default(),
        ];
        let rows = snapshot_rows(&periods);
        assert_eq!(rows[0].cost_usd, "2");
    }
}
