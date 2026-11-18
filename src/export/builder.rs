//! Building an export from a live snapshot.
//!
//! This is the seam between the monitor's canonical data and the pure serializer:
//! it walks the snapshot once and produces the document plus the three row sets.
//! Nothing here touches the filesystem or the UI.

use std::collections::BTreeMap;

use crate::aggregation::CanonicalSnapshot;
use crate::analytics::{
    DayState, UsagePeriod, daily_series, model_breakdown, monthly_rollups, period_totals,
    token_total,
};
use crate::pricing::{CostEstimate, PricingCatalog, estimate};

use super::json::{
    AppIdentity, ExportDocument, JsonDaily, JsonMonthly, JsonSlice, SnapshotDocument,
};
use super::rows::{DailyModelRow, DailyRow, ModelDay, PeriodTotalsRow, SnapshotEntry, SnapshotRow};

/// How much history an export carries.
///
/// The reference caps daily history at "roughly the last 370 days", which is what
/// `MAX_TREND_DAYS` is. Snapshot totals are all-time regardless.
pub const EXPORT_HISTORY_DAYS: usize = crate::analytics::MAX_TREND_DAYS;

/// Everything an export needs, built from one snapshot.
#[derive(Clone, Debug, PartialEq)]
pub struct ExportInput {
    pub document: ExportDocument,
    pub snapshot_rows: Vec<SnapshotRow>,
    pub daily_rows: Vec<DailyRow>,
    pub daily_model_rows: Vec<DailyModelRow>,
}

#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum ExportBuildError {
    #[error("export history could not be built")]
    History,
}

/// Build the export from a canonical snapshot.
///
/// A day whose state is `Unknown` contributes **no rows**: writing a zero row
/// would claim usage was measured and found to be nothing, which is exactly the
/// distinction `DayState` exists to preserve.
pub fn build(
    snapshot: &CanonicalSnapshot,
    now_ms: i64,
    pricing: &PricingCatalog,
    generated_at: String,
) -> Result<ExportInput, ExportBuildError> {
    let series = daily_series(snapshot, EXPORT_HISTORY_DAYS, now_ms)
        .map_err(|_| ExportBuildError::History)?;
    let totals = period_totals(snapshot, now_ms).map_err(|_| ExportBuildError::History)?;

    // Snapshot CSV: per-period tools and models, each with its cost.
    let mut periods: [PeriodTotalsRow; 3] = Default::default();
    for (index, period) in [UsagePeriod::Today, UsagePeriod::Month, UsagePeriod::AllTime]
        .into_iter()
        .enumerate()
    {
        let rows = model_breakdown(snapshot, period, now_ms, pricing).unwrap_or_default();
        let mut tools: BTreeMap<String, (u128, CostEstimate)> = BTreeMap::new();
        let mut models: BTreeMap<String, (u128, CostEstimate)> = BTreeMap::new();
        for row in rows {
            let tool = tools.entry(row.client.clone()).or_default();
            tool.0 = tool.0.saturating_add(row.total);
            tool.1 = tool.1.merge(row.cost);
            let model = models.entry(row.model.clone()).or_default();
            model.0 = model.0.saturating_add(row.total);
            model.1 = model.1.merge(row.cost);
        }
        periods[index] = PeriodTotalsRow {
            tools: tools
                .into_iter()
                .map(|(name, (tokens, cost))| SnapshotEntry { name, tokens, cost })
                .collect(),
            models: models
                .into_iter()
                .map(|(name, (tokens, cost))| SnapshotEntry { name, tokens, cost })
                .collect(),
        };
    }
    let snapshot_rows = super::rows::snapshot_rows(&periods);

    // Day × client and day × model, grouped in one pass over the rollups.
    let mut per_day_tools: BTreeMap<String, BTreeMap<String, (u128, CostEstimate)>> =
        BTreeMap::new();
    for row in snapshot.session_days.iter() {
        let entry = per_day_tools
            .entry(row.day.format("%Y-%m-%d").to_string())
            .or_default()
            .entry(row.key.client.clone())
            .or_default();
        entry.0 = entry.0.saturating_add(token_total(row.tokens));
    }
    let mut per_day_models: BTreeMap<String, BTreeMap<String, ModelDay>> = BTreeMap::new();
    for row in snapshot.day_models.iter() {
        let entry = per_day_models
            .entry(row.day.format("%Y-%m-%d").to_string())
            .or_default()
            .entry(row.model.clone())
            .or_default();
        entry.tokens = entry.tokens.saturating_add(token_total(row.tokens));
        entry.output_tokens = entry
            .output_tokens
            .saturating_add(u128::from(row.tokens.output));
        entry.cache_read_tokens = entry
            .cache_read_tokens
            .saturating_add(u128::from(row.tokens.cache_read));
        entry.cache_write_tokens = entry
            .cache_write_tokens
            .saturating_add(u128::from(row.tokens.cache_write));
        entry.cost = entry.cost.merge(estimate(
            pricing.resolve(Some(row.client.as_str()), &row.model),
            row.tokens,
            row.cache_write_1h,
            None,
        ));
    }

    let mut daily_rows = Vec::new();
    let mut daily_model_rows = Vec::new();
    let mut daily: Vec<JsonDaily> = Vec::new();
    for day in &series {
        // An unknown day carries no evidence, so it contributes no rows at all.
        if day.state == DayState::Unknown {
            continue;
        }
        let date = day.day.format("%Y-%m-%d").to_string();
        let empty_tools = BTreeMap::new();
        let tools = per_day_tools.get(&date).unwrap_or(&empty_tools);
        for (tool, (tokens, cost)) in tools {
            daily_rows.push(DailyRow {
                date: date.clone(),
                tool: tool.clone(),
                tokens: *tokens,
                cost_usd: super::csv::format_usd(cost.nano_usd),
            });
        }

        let empty_models = BTreeMap::new();
        let models = per_day_models.get(&date).unwrap_or(&empty_models);
        for (model, value) in models {
            // The monitor does not record whether a day's component split was
            // complete, so the exporter reports any remainder as unclassified
            // rather than guessing it as input. That is the conservative choice
            // the reference also makes when the flag is absent.
            let split = super::rows::components(false, value);
            daily_model_rows.push(DailyModelRow {
                date: date.clone(),
                model: model.clone(),
                input_tokens: split.input,
                output_tokens: split.output,
                cache_read_tokens: split.cache_read,
                cache_write_tokens: split.cache_write,
                unclassified_tokens: split.unclassified,
                total_tokens: split.total,
                cost_usd: super::csv::format_usd(value.cost.nano_usd),
            });
        }

        let day_cost: i128 = tools.values().map(|(_, cost)| cost.nano_usd).sum();
        daily.push(JsonDaily {
            date: date.clone(),
            tokens: day.total,
            cost: super::json::usd(day_cost),
            per_client: tools
                .iter()
                .map(|(tool, (tokens, cost))| {
                    (
                        tool.clone(),
                        JsonSlice {
                            tokens: *tokens,
                            cost: super::json::usd(cost.nano_usd),
                            ..JsonSlice::default()
                        },
                    )
                })
                .collect(),
            per_model: models
                .iter()
                .map(|(model, value)| {
                    (
                        model.clone(),
                        JsonSlice {
                            tokens: value.tokens,
                            cost: super::json::usd(value.cost.nano_usd),
                            output_tokens: Some(value.output_tokens),
                            cache_read_tokens: Some(value.cache_read_tokens),
                            cache_write_tokens: Some(value.cache_write_tokens),
                            unclassified_tokens: Some(
                                super::rows::components(false, value).unclassified,
                            ),
                        },
                    )
                })
                .collect(),
        });
    }

    // Monthly rollups reuse the same rows, so the CSV and JSON cannot disagree.
    let mut monthly_slices: BTreeMap<String, BTreeMap<String, JsonSlice>> = BTreeMap::new();
    let mut monthly_model_slices: BTreeMap<String, BTreeMap<String, JsonSlice>> = BTreeMap::new();
    let mut monthly_cost: BTreeMap<String, i128> = BTreeMap::new();
    for day in &daily {
        let month = day.date.chars().take(7).collect::<String>();
        *monthly_cost.entry(month.clone()).or_default() += (day.cost * 1_000_000_000.0) as i128;
        for (client, slice) in &day.per_client {
            let entry = monthly_slices
                .entry(month.clone())
                .or_default()
                .entry(client.clone())
                .or_default();
            entry.tokens = entry.tokens.saturating_add(slice.tokens);
            entry.cost += slice.cost;
        }
        for (model, slice) in &day.per_model {
            let entry = monthly_model_slices
                .entry(month.clone())
                .or_default()
                .entry(model.clone())
                .or_default();
            entry.tokens = entry.tokens.saturating_add(slice.tokens);
            entry.cost += slice.cost;
        }
    }
    let monthly: Vec<JsonMonthly> = monthly_rollups(&series)
        .into_iter()
        .map(|month| JsonMonthly {
            per_client: monthly_slices.remove(&month.month).unwrap_or_default(),
            per_model: monthly_model_slices
                .remove(&month.month)
                .unwrap_or_default(),
            cost: super::json::usd(*monthly_cost.get(&month.month).unwrap_or(&0)),
            month: month.month,
            tokens: month.total,
        })
        .collect();

    let document = ExportDocument {
        generated_at,
        app: AppIdentity::default(),
        snapshot: SnapshotDocument {
            today: serde_json::json!({ "total": totals.today.total }),
            month: serde_json::json!({ "total": totals.month.total }),
            all_time: serde_json::json!({ "total": totals.all_time.total }),
        },
        daily,
        monthly,
    };

    Ok(ExportInput {
        document,
        snapshot_rows,
        daily_rows,
        daily_model_rows,
    })
}

#[cfg(test)]
mod tests;
