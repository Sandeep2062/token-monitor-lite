//! Period analytics over committed canonical rollups.
//!
//! Live canonical observations take precedence over retained archive periods
//! for the same session and calendar window. Unmatched retained identities are
//! added; they are never guessed into a different block or window.

use crate::accounting::{SessionKey, TokenCounts};
use crate::aggregation::{BucketTimezone, CanonicalSnapshot, SessionDayRollup};
use crate::sources::{RetainedPeriod, RetainedPeriodUsage};
use chrono::{DateTime, Datelike, Days, NaiveDate};
use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;

pub const MAX_TREND_DAYS: usize = 370;
pub const MAX_MODEL_ROWS: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelUsage {
    pub client: String,
    pub model: String,
    pub tokens: TokenCounts,
    pub total: u128,
}

/// Period-scoped model totals. Native session usage replaces a retained
/// session's models only when that session has native usage in the period
/// window; retained model remainders stay explicitly unclassified.
pub fn model_breakdown(
    snapshot: &CanonicalSnapshot,
    period: UsagePeriod,
    now_ms: i64,
) -> Result<Vec<ModelUsage>, AnalyticsError> {
    let timezone = BucketTimezone::from_str(&snapshot.timezone_name)
        .map_err(|_| AnalyticsError::InvalidTimezone)?;
    let now = DateTime::from_timestamp_millis(now_ms).ok_or(AnalyticsError::InvalidTimestamp)?;
    let local = now.with_timezone(&timezone.tz());
    let today = local.date_naive();
    let today_key = today.format("%Y-%m-%d").to_string();
    let month_key = format!("{:04}-{:02}", local.year(), local.month());
    let in_window = |day: NaiveDate| match period {
        UsagePeriod::Today => day == today,
        UsagePeriod::Month => day.year() == local.year() && day.month() == local.month(),
        UsagePeriod::AllTime => true,
    };

    let mut native_sessions = BTreeSet::new();
    for row in snapshot.session_days.iter() {
        if in_window(row.day) {
            native_sessions.insert(row.key.clone());
        }
    }

    let mut totals: BTreeMap<(String, String), (TokenCounts, u128)> = BTreeMap::new();
    for row in snapshot.day_models.iter() {
        if !in_window(row.day) {
            continue;
        }
        let value = totals
            .entry((row.client.clone(), row.model.clone()))
            .or_default();
        value.0 = add_tokens(value.0, row.tokens)?;
        value.1 = value
            .1
            .checked_add(token_total(row.tokens))
            .ok_or(AnalyticsError::Overflow)?;
    }

    for row in snapshot.retained_periods.iter() {
        if retained_period(row.period) != period
            || !window_matches(row, period, &today_key, &month_key)
        {
            continue;
        }
        let key = SessionKey::new(&row.client, &row.session);
        if native_sessions.contains(&key) {
            continue;
        }
        let mut covered = 0_u128;
        for model in row.models.iter() {
            let value = totals
                .entry((row.client.clone(), model.model.clone()))
                .or_default();
            value.0 = add_tokens(
                value.0,
                TokenCounts {
                    input: model.input,
                    output: model.output,
                    cache_read: model.cache_read,
                    cache_write: model.cache_write,
                    reasoning: 0,
                },
            )?;
            value.1 = value
                .1
                .checked_add(u128::from(model.total))
                .ok_or(AnalyticsError::Overflow)?;
            covered = covered
                .checked_add(u128::from(model.total))
                .ok_or(AnalyticsError::Overflow)?;
        }
        let remainder = u128::from(row.total).saturating_sub(covered);
        if remainder > 0 {
            let value = totals
                .entry((row.client.clone(), "Unclassified".to_owned()))
                .or_default();
            value.1 = value
                .1
                .checked_add(remainder)
                .ok_or(AnalyticsError::Overflow)?;
        }
    }

    let mut rows = totals
        .into_iter()
        .filter(|(_, (tokens, total))| *total > 0 || token_total(*tokens) > 0)
        .map(|((client, model), (tokens, total))| ModelUsage {
            client,
            model,
            tokens,
            total,
        })
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| {
        right
            .total
            .cmp(&left.total)
            .then_with(|| left.client.cmp(&right.client))
            .then_with(|| left.model.cmp(&right.model))
    });
    if rows.len() > MAX_MODEL_ROWS {
        let overflow = rows.split_off(MAX_MODEL_ROWS - 1);
        let mut other = ModelUsage {
            client: "all".to_owned(),
            model: "Other models".to_owned(),
            tokens: TokenCounts::default(),
            total: 0,
        };
        for row in overflow {
            other.tokens = add_tokens(other.tokens, row.tokens)?;
            other.total = other
                .total
                .checked_add(row.total)
                .ok_or(AnalyticsError::Overflow)?;
        }
        rows.push(other);
    }
    Ok(rows)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum UsagePeriod {
    #[default]
    Today,
    Month,
    AllTime,
}

impl UsagePeriod {
    pub const ALL: [Self; 3] = [Self::Today, Self::Month, Self::AllTime];

    pub fn index(self) -> usize {
        match self {
            Self::Today => 0,
            Self::Month => 1,
            Self::AllTime => 2,
        }
    }

    pub fn from_index(index: usize) -> Option<Self> {
        Self::ALL.get(index).copied()
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Today => "Today",
            Self::Month => "Month",
            Self::AllTime => "All time",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PeriodUsage {
    /// Canonical components plus unmatched retained components. Reasoning is
    /// reported separately because provider semantics decide whether it is a
    /// subset of output.
    pub tokens: TokenCounts,
    pub cache_write_1h: u64,
    /// Retained totals that the archive did not classify into components.
    pub unclassified: u128,
    /// Authoritative displayed total: live canonical components plus each
    /// unmatched retained period total.
    pub total: u128,
    pub sessions: usize,
    pub retained_sessions: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PeriodTotals {
    pub today: PeriodUsage,
    pub month: PeriodUsage,
    pub all_time: PeriodUsage,
}

impl PeriodTotals {
    pub fn get(&self, period: UsagePeriod) -> &PeriodUsage {
        match period {
            UsagePeriod::Today => &self.today,
            UsagePeriod::Month => &self.month,
            UsagePeriod::AllTime => &self.all_time,
        }
    }
}

#[derive(Clone, Copy, Debug, thiserror::Error, PartialEq, Eq)]
pub enum AnalyticsError {
    #[error("analytics timestamp is invalid")]
    InvalidTimestamp,
    #[error("analytics timezone is invalid")]
    InvalidTimezone,
    #[error("analytics accumulation overflow")]
    Overflow,
    #[error("analytics capacity exceeded: {0}")]
    Capacity(&'static str),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DailyUsage {
    pub day: NaiveDate,
    pub tokens: TokenCounts,
    pub total: u128,
}

/// Returns one entry per local calendar day, oldest first, including explicit
/// zero days. Retained Today totals apply only to the current day; retained
/// Month/All Time aggregates cannot be assigned to individual days.
pub fn daily_series(
    snapshot: &CanonicalSnapshot,
    days: usize,
    now_ms: i64,
) -> Result<Vec<DailyUsage>, AnalyticsError> {
    if days == 0 || days > MAX_TREND_DAYS {
        return Err(AnalyticsError::Capacity("trend days"));
    }
    let timezone = BucketTimezone::from_str(&snapshot.timezone_name)
        .map_err(|_| AnalyticsError::InvalidTimezone)?;
    let now = DateTime::from_timestamp_millis(now_ms).ok_or(AnalyticsError::InvalidTimestamp)?;
    let today = now.with_timezone(&timezone.tz()).date_naive();
    let offset = i64::try_from(days - 1).map_err(|_| AnalyticsError::Capacity("trend days"))?;
    let start = today
        .checked_sub_days(Days::new(offset.unsigned_abs()))
        .ok_or(AnalyticsError::InvalidTimestamp)?;

    let mut native: BTreeMap<NaiveDate, TokenCounts> = BTreeMap::new();
    for row in snapshot.session_days.iter() {
        if row.day < start || row.day > today {
            continue;
        }
        let value = native.entry(row.day).or_default();
        *value = add_tokens(*value, row.tokens)?;
    }

    let period_today = period_totals(snapshot, now_ms)?.today;
    let mut series = Vec::with_capacity(days);
    for index in 0..days {
        let shift = u64::try_from(index).map_err(|_| AnalyticsError::Capacity("trend days"))?;
        let day = start
            .checked_add_days(Days::new(shift))
            .ok_or(AnalyticsError::InvalidTimestamp)?;
        if day == today {
            series.push(DailyUsage {
                day,
                tokens: period_today.tokens,
                total: period_today.total,
            });
        } else {
            let tokens = native.remove(&day).unwrap_or_default();
            series.push(DailyUsage {
                day,
                tokens,
                total: token_total(tokens),
            });
        }
    }
    Ok(series)
}

/// Computes the three native periods at `now_ms`.
pub fn period_totals(
    snapshot: &CanonicalSnapshot,
    now_ms: i64,
) -> Result<PeriodTotals, AnalyticsError> {
    let timezone = BucketTimezone::from_str(&snapshot.timezone_name)
        .map_err(|_| AnalyticsError::InvalidTimezone)?;
    let now = DateTime::from_timestamp_millis(now_ms).ok_or(AnalyticsError::InvalidTimestamp)?;
    let local = now.with_timezone(&timezone.tz());
    let today = local.date_naive();
    let today_key = today.format("%Y-%m-%d").to_string();
    let month_key = format!("{:04}-{:02}", local.year(), local.month());

    let mut native: [BTreeMap<SessionKey, TokenAccumulator>; 3] =
        std::array::from_fn(|_| BTreeMap::new());
    for row in snapshot.session_days.iter() {
        add_native(&mut native[UsagePeriod::AllTime.index()], row)?;
        if row.day == today {
            add_native(&mut native[UsagePeriod::Today.index()], row)?;
        }
        if row.day.year() == local.year() && row.day.month() == local.month() {
            add_native(&mut native[UsagePeriod::Month.index()], row)?;
        }
    }

    let mut totals = PeriodTotals::default();
    for period in UsagePeriod::ALL {
        let native_usage = &native[period.index()];
        let mut usage = PeriodUsage {
            sessions: native_usage.len(),
            ..PeriodUsage::default()
        };
        for value in native_usage.values() {
            usage.tokens = add_tokens(usage.tokens, value.tokens)?;
            usage.cache_write_1h = usage
                .cache_write_1h
                .checked_add(value.cache_write_1h)
                .ok_or(AnalyticsError::Overflow)?;
        }
        usage.total = token_total(usage.tokens);

        let mut retained_seen = BTreeSet::new();
        for row in snapshot.retained_periods.iter() {
            if retained_period(row.period) != period
                || !window_matches(row, period, &today_key, &month_key)
            {
                continue;
            }
            let key = SessionKey::new(&row.client, &row.session);
            if native_usage.contains_key(&key) || !retained_seen.insert(key) {
                continue;
            }
            usage.tokens = add_tokens(usage.tokens, retained_components(row))?;
            usage.unclassified = usage
                .unclassified
                .checked_add(u128::from(row.unclassified))
                .ok_or(AnalyticsError::Overflow)?;
            usage.total = usage
                .total
                .checked_add(u128::from(row.total))
                .ok_or(AnalyticsError::Overflow)?;
            usage.retained_sessions += 1;
        }
        usage.sessions = usage.sessions.saturating_add(usage.retained_sessions);
        match period {
            UsagePeriod::Today => totals.today = usage,
            UsagePeriod::Month => totals.month = usage,
            UsagePeriod::AllTime => totals.all_time = usage,
        }
    }
    Ok(totals)
}

#[derive(Clone, Copy)]
struct TokenAccumulator {
    tokens: TokenCounts,
    cache_write_1h: u64,
}

fn add_native(
    target: &mut BTreeMap<SessionKey, TokenAccumulator>,
    row: &SessionDayRollup,
) -> Result<(), AnalyticsError> {
    let value = target.entry(row.key.clone()).or_insert(TokenAccumulator {
        tokens: TokenCounts::default(),
        cache_write_1h: 0,
    });
    value.tokens = add_tokens(value.tokens, row.tokens)?;
    value.cache_write_1h = value
        .cache_write_1h
        .checked_add(row.cache_write_1h)
        .ok_or(AnalyticsError::Overflow)?;
    Ok(())
}

fn add_tokens(current: TokenCounts, value: TokenCounts) -> Result<TokenCounts, AnalyticsError> {
    current
        .checked_add(value)
        .map_err(|_| AnalyticsError::Overflow)
}

fn token_total(tokens: TokenCounts) -> u128 {
    u128::from(tokens.input)
        + u128::from(tokens.output)
        + u128::from(tokens.cache_read)
        + u128::from(tokens.cache_write)
        + u128::from(tokens.reasoning)
}

fn retained_period(period: RetainedPeriod) -> UsagePeriod {
    match period {
        RetainedPeriod::Today => UsagePeriod::Today,
        RetainedPeriod::Month => UsagePeriod::Month,
        RetainedPeriod::AllTime => UsagePeriod::AllTime,
    }
}

fn window_matches(
    row: &RetainedPeriodUsage,
    period: UsagePeriod,
    today: &str,
    month: &str,
) -> bool {
    match period {
        UsagePeriod::Today => row.day.as_deref() == Some(today),
        UsagePeriod::Month => row.month.as_deref() == Some(month),
        UsagePeriod::AllTime => true,
    }
}

fn retained_components(row: &RetainedPeriodUsage) -> TokenCounts {
    TokenCounts {
        input: row.input,
        output: row.output,
        cache_read: row.cache_read,
        cache_write: row.cache_write,
        reasoning: row.reasoning,
    }
}

#[cfg(test)]
mod tests {
    use super::{UsagePeriod, period_totals};
    use crate::accounting::{SessionKey, TokenCounts};
    use crate::aggregation::{CanonicalSnapshot, DayModelRollup, SessionDayRollup, SourceRef};
    use crate::sources::{RetainedModelUsage, RetainedPeriod, RetainedPeriodUsage};
    use std::sync::Arc;

    fn at(day: &str) -> i64 {
        chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d")
            .expect("test date")
            .and_hms_opt(12, 0, 0)
            .expect("test time")
            .and_utc()
            .timestamp_millis()
    }

    fn snapshot(
        session_days: Vec<SessionDayRollup>,
        retained: Vec<RetainedPeriodUsage>,
    ) -> CanonicalSnapshot {
        CanonicalSnapshot {
            generation: 1,
            timezone_name: "UTC".to_owned(),
            totals: TokenCounts::default(),
            cache_write_1h: 0,
            day_models: Arc::from([]),
            session_models: Arc::from([]),
            session_days: session_days.into(),
            retained_periods: retained.into(),
            sources: Arc::from([]),
        }
    }

    fn day(day: &str, client: &str, session: &str, input: u64) -> SessionDayRollup {
        SessionDayRollup {
            day: chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d").expect("test day"),
            key: SessionKey::new(client, session),
            tokens: TokenCounts {
                input,
                ..TokenCounts::default()
            },
            cache_write_1h: 0,
        }
    }

    fn retained(
        period: RetainedPeriod,
        client: &str,
        session: &str,
        day: &str,
        month: &str,
        total: u64,
    ) -> RetainedPeriodUsage {
        RetainedPeriodUsage {
            client: client.to_owned(),
            session: session.to_owned(),
            period,
            captured_at_ms: 1,
            day: Some(day.to_owned()),
            month: Some(month.to_owned()),
            total,
            input: total,
            output: 0,
            cache_read: 0,
            cache_write: 0,
            reasoning: 0,
            unclassified: 0,
            components_complete: true,
            models_complete: true,
            models: Arc::from([]),
        }
    }

    fn retained_model(
        period: RetainedPeriod,
        client: &str,
        session: &str,
        model: &str,
        total: u64,
    ) -> RetainedPeriodUsage {
        let mut row = retained(period, client, session, "2026-11-13", "2026-11", total);
        row.models = vec![RetainedModelUsage {
            model: model.to_owned(),
            total,
            input: total,
            cache_read: 0,
            cache_write: 0,
            output: 0,
        }]
        .into();
        row
    }

    fn model_day(day: &str, client: &str, model: &str, input: u64) -> DayModelRollup {
        DayModelRollup {
            day: chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d").expect("test day"),
            client: client.to_owned(),
            model: model.to_owned(),
            tokens: TokenCounts {
                input,
                ..TokenCounts::default()
            },
            cache_write_1h: 0,
            first_seen_ms: 0,
            last_seen_ms: 0,
            sources: Arc::<[SourceRef]>::from([]),
        }
    }

    #[test]
    fn periods_split_live_rows_by_local_calendar_window() {
        let totals = period_totals(
            &snapshot(
                vec![
                    day("2026-11-13", "claude", "today", 7),
                    day("2026-11-01", "claude", "month", 11),
                    day("2026-10-31", "claude", "previous", 13),
                ],
                Vec::new(),
            ),
            at("2026-11-13"),
        )
        .expect("period totals");
        assert_eq!(totals.today.tokens.input, 7);
        assert_eq!(totals.month.tokens.input, 18);
        assert_eq!(totals.all_time.tokens.input, 31);
        assert_eq!(totals.today.sessions, 1);
        assert_eq!(totals.month.sessions, 2);
        assert_eq!(totals.all_time.sessions, 3);
    }

    #[test]
    fn live_session_replaces_retained_row_for_the_same_window_only() {
        let totals = period_totals(
            &snapshot(
                vec![day("2026-11-13", "claude", "shared", 5)],
                vec![
                    retained(
                        RetainedPeriod::Today,
                        "claude",
                        "shared",
                        "2026-11-13",
                        "2026-11",
                        90,
                    ),
                    retained(
                        RetainedPeriod::Month,
                        "claude",
                        "shared",
                        "2026-11-13",
                        "2026-11",
                        90,
                    ),
                    retained(
                        RetainedPeriod::AllTime,
                        "claude",
                        "shared",
                        "2026-11-13",
                        "2026-11",
                        90,
                    ),
                    retained(
                        RetainedPeriod::Month,
                        "claude",
                        "archived",
                        "2026-11-13",
                        "2026-11",
                        40,
                    ),
                ],
            ),
            at("2026-11-13"),
        )
        .expect("period totals");
        assert_eq!(totals.today.tokens.input, 5);
        assert_eq!(totals.today.total, 5);
        assert_eq!(totals.month.tokens.input, 45);
        assert_eq!(totals.month.total, 45);
        assert_eq!(totals.all_time.total, 5);
    }

    #[test]
    fn retained_rows_from_another_window_do_not_enter_today_or_month() {
        let totals = period_totals(
            &snapshot(
                Vec::new(),
                vec![
                    retained(
                        RetainedPeriod::Today,
                        "codex",
                        "old",
                        "2026-11-12",
                        "2026-11",
                        3,
                    ),
                    retained(
                        RetainedPeriod::Month,
                        "codex",
                        "old",
                        "2026-11-12",
                        "2026-10",
                        4,
                    ),
                    retained(
                        RetainedPeriod::AllTime,
                        "codex",
                        "old",
                        "2026-11-12",
                        "2026-10",
                        5,
                    ),
                ],
            ),
            at("2026-11-13"),
        )
        .expect("period totals");
        assert_eq!(totals.today.total, 0);
        assert_eq!(totals.month.total, 0);
        assert_eq!(totals.all_time.total, 5);
    }

    #[test]
    fn unclassified_retained_totals_remain_visible() {
        let mut row = retained(
            RetainedPeriod::AllTime,
            "codex",
            "a",
            "2026-11-13",
            "2026-11",
            10,
        );
        row.input = 4;
        row.unclassified = 6;
        let totals = period_totals(&snapshot(Vec::new(), vec![row]), at("2026-11-13"))
            .expect("period totals");
        assert_eq!(totals.all_time.tokens.input, 4);
        assert_eq!(totals.all_time.unclassified, 6);
        assert_eq!(totals.all_time.total, 10);
    }

    #[test]
    fn daily_series_is_contiguous_and_includes_retained_today() {
        let mut retained_today = retained(
            RetainedPeriod::Today,
            "codex",
            "archived",
            "2026-11-13",
            "2026-11",
            40,
        );
        retained_today.input = 40;
        let series = super::daily_series(
            &snapshot(
                vec![
                    day("2026-11-13", "claude", "today", 7),
                    day("2026-11-11", "claude", "earlier", 11),
                ],
                vec![retained_today],
            ),
            4,
            at("2026-11-13"),
        )
        .expect("daily series");
        assert_eq!(series.len(), 4);
        assert_eq!(series[0].day.to_string(), "2026-11-10");
        assert_eq!(series[0].total, 0);
        assert_eq!(series[1].day.to_string(), "2026-11-11");
        assert_eq!(series[1].total, 11);
        assert_eq!(series[2].total, 0);
        assert_eq!(series[3].day.to_string(), "2026-11-13");
        assert_eq!(series[3].total, 47);
    }

    #[test]
    fn daily_series_rejects_an_out_of_range_window() {
        assert_eq!(
            super::daily_series(&snapshot(Vec::new(), Vec::new()), 0, at("2026-11-13")),
            Err(super::AnalyticsError::Capacity("trend days"))
        );
        assert_eq!(
            super::daily_series(
                &snapshot(Vec::new(), Vec::new()),
                super::MAX_TREND_DAYS + 1,
                at("2026-11-13")
            ),
            Err(super::AnalyticsError::Capacity("trend days"))
        );
    }

    #[test]
    fn model_breakdown_combines_periods_and_replaces_retained_sessions() {
        let mut canonical = snapshot(
            vec![
                day("2026-11-13", "claude", "shared", 5),
                day("2026-11-02", "claude", "archived", 2),
            ],
            vec![
                retained(
                    RetainedPeriod::Month,
                    "claude",
                    "shared",
                    "2026-11-13",
                    "2026-11",
                    90,
                ),
                retained(
                    RetainedPeriod::Month,
                    "claude",
                    "archived",
                    "2026-11-13",
                    "2026-11",
                    40,
                ),
                retained(
                    RetainedPeriod::Month,
                    "claude",
                    "old",
                    "2026-11-13",
                    "2026-11",
                    30,
                ),
            ],
        );
        canonical.day_models = vec![
            model_day("2026-11-13", "claude", "sonnet", 5),
            model_day("2026-11-02", "claude", "sonnet", 2),
            model_day("2026-10-20", "claude", "opus", 13),
        ]
        .into();
        canonical.retained_periods = vec![
            retained_model(RetainedPeriod::Month, "claude", "shared", "sonnet", 90),
            retained_model(RetainedPeriod::Month, "claude", "archived", "sonnet", 40),
            retained_model(RetainedPeriod::Month, "claude", "old", "haiku", 30),
            retained_model(RetainedPeriod::AllTime, "claude", "old", "haiku", 30),
        ]
        .into();

        let month = super::model_breakdown(&canonical, UsagePeriod::Month, at("2026-11-13"))
            .expect("month models");
        assert_eq!(
            month
                .iter()
                .map(|row| (row.model.as_str(), row.total))
                .collect::<Vec<_>>(),
            vec![("haiku", 30), ("sonnet", 7)]
        );
        let all = super::model_breakdown(&canonical, UsagePeriod::AllTime, at("2026-11-13"))
            .expect("all models");
        assert_eq!(
            all.iter()
                .map(|row| (row.model.as_str(), row.total))
                .collect::<Vec<_>>(),
            vec![("haiku", 30), ("opus", 13), ("sonnet", 7)]
        );
    }

    #[test]
    fn period_index_round_trips() {
        for period in UsagePeriod::ALL {
            assert_eq!(UsagePeriod::from_index(period.index()), Some(period));
        }
        assert_eq!(UsagePeriod::from_index(3), None);
    }
}
