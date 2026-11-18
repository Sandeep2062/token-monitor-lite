//! The Claude and Codex quota mappers.
//!
//! Ports `mapClaudeUsageToProvider` (`providers/claude/limits.js`) and the Codex
//! rate-limit normalization (`providers/codex/limits.js`). Only these two
//! providers exist: spec §2.1.1.
//!
//! Both functions take an already-fetched payload. They perform no IO, hold no
//! credentials and cannot start a probe, which is what keeps the mapping layer
//! testable against recorded fixtures.

use serde_json::Value;

use super::provider::{LimitSource, LimitsProvider};
use super::status::LimitStatus;
use super::store::Attempt;
use super::window::{Window, WindowKind, WindowMetric, parse_instant_ms};

/// Identity and provenance for one probe result.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProviderMeta {
    pub account_key: String,
    pub account_label: String,
    pub account_name: String,
    pub account_email: String,
    pub plan_label: String,
    pub source: Option<LimitSource>,
    pub updated_at: Option<String>,
}

impl ProviderMeta {
    fn apply(&self, mut row: LimitsProvider) -> LimitsProvider {
        row.account_key = self.account_key.clone();
        row.account_label = self.account_label.clone();
        row.account_name = self.account_name.clone();
        row.account_email = self.account_email.clone();
        row.plan_label = self.plan_label.clone();
        row.source = self.source.unwrap_or(LimitSource::Oauth);
        row.updated_at = self.updated_at.clone();
        row
    }
}

/// Read the first present alias. An explicit `null` is skipped, matching
/// `valueFromAliases`.
fn alias<'a>(value: &'a Value, names: &[&str]) -> Option<&'a Value> {
    let object = value.as_object()?;
    names
        .iter()
        .find_map(|name| object.get(*name).filter(|found| !found.is_null()))
}

fn number(value: &Value, names: &[&str]) -> Option<f64> {
    match alias(value, names)? {
        Value::Number(number) => number.as_f64(),
        // The reference accepts numeric strings only where it calls `Number()`;
        // the wire sends numbers, so a string here is not coerced.
        _ => None,
    }
}

fn text(value: &Value, names: &[&str]) -> Option<String> {
    alias(value, names)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|found| !found.trim().is_empty())
}

/// `percentFromWindow`: an explicit percentage wins, else derive it from
/// `used`/`limit`, and clamp to 0..100 either way.
fn percent_from(value: &Value, used: Option<f64>, limit: Option<f64>) -> Option<f64> {
    if let Some(explicit) = number(
        value,
        &["usedPercent", "used_percent", "utilization", "percent"],
    ) {
        return explicit.is_finite().then(|| explicit.clamp(0.0, 100.0));
    }
    match (used, limit) {
        (Some(used), Some(limit)) if limit > 0.0 && used.is_finite() && limit.is_finite() => {
            Some((used / limit * 100.0).clamp(0.0, 100.0))
        }
        _ => None,
    }
}

/// Claude's usage window: `{ utilization | percent, resets_at }`.
fn claude_window(kind: WindowKind, value: &Value) -> Option<Window> {
    let used = number(value, &["used"]);
    let limit = number(value, &["limit"]);
    let used_percent = percent_from(value, used, limit)?;
    let resets_at =
        text(value, &["resets_at", "resetsAt"]).and_then(|instant| normalise_instant(&instant));
    let mut window = Window::new(kind);
    window.used = used;
    window.limit = limit;
    window.used_percent = Some(used_percent);
    window.resets_at = resets_at;
    Some(window.normalize())
}

/// Keep only instants the reference would accept: RFC 3339, and nothing else.
fn normalise_instant(value: &str) -> Option<String> {
    parse_instant_ms(value).map(|_| value.trim().to_owned())
}

/// Claude: `five_hour`/`fiveHour` is the session window, `seven_day`/`sevenDay`
/// the weekly one.
///
/// The reference also surfaces a promo "Fable only" `weekly_scoped` entry and a
/// credits window. Both are deliberately not ported: the promo is time-limited
/// and self-removing, and credits are money rather than quota, reported by
/// `spend`/`extra_usage` in two spellings. Spec §2.1.3 records why.
pub fn claude_from_usage(usage: &Value, meta: &ProviderMeta) -> Option<Attempt> {
    let session = alias(usage, &["five_hour", "fiveHour"])
        .and_then(|value| claude_window(WindowKind::Session, value));
    let weekly = alias(usage, &["seven_day", "sevenDay"])
        .and_then(|value| claude_window(WindowKind::Weekly, value));
    let windows: Vec<Window> = session.into_iter().chain(weekly).collect();
    if windows.is_empty() {
        return None;
    }
    let mut row = LimitsProvider::new("claude", LimitStatus::Ok, LimitSource::Oauth);
    row.windows = windows;
    Some(Attempt::new(meta.apply(row)))
}

/// Codex decides a window's cadence from its declared duration, then falls back
/// to the slot name: `secondary` is weekly, anything else is a session.
fn codex_window_kind(slot: &str, window: &Value) -> WindowKind {
    let minutes = number(window, &["windowDurationMins", "window_duration_mins"]).or_else(|| {
        number(window, &["limitWindowSeconds", "limit_window_seconds"])
            .map(|seconds| seconds / 60.0)
    });
    match minutes {
        Some(minutes) if minutes == 30.0 * 24.0 * 60.0 => WindowKind::Billing,
        Some(minutes) if minutes >= 7.0 * 24.0 * 60.0 => WindowKind::Weekly,
        Some(minutes) if minutes >= 24.0 * 60.0 => WindowKind::Daily,
        Some(minutes) if minutes == 5.0 * 60.0 => WindowKind::Session,
        _ if slot.eq_ignore_ascii_case("secondary") => WindowKind::Weekly,
        _ => WindowKind::Session,
    }
}

fn codex_window(slot: &str, value: &Value) -> Option<Window> {
    let kind = codex_window_kind(slot, value);
    let used = number(value, &["used"]);
    let limit = number(value, &["limit"]);
    let used_percent = percent_from(value, used, limit)?;
    let resets_at = text(value, &["resetsAt", "resets_at", "resetAt", "reset_at"])
        .and_then(|instant| normalise_instant(&instant));
    let minutes = number(value, &["windowDurationMins", "window_duration_mins"]).or_else(|| {
        number(value, &["limitWindowSeconds", "limit_window_seconds"]).map(|seconds| seconds / 60.0)
    });
    let mut window = Window::new(kind);
    window.used = used;
    window.limit = limit;
    window.used_percent = Some(used_percent);
    window.resets_at = resets_at;
    window.window_minutes = minutes
        .filter(|value| *value >= 0.0)
        .map(|value| value as u64);
    Some(window.normalize())
}

/// Codex: `primary`/`primaryWindow`/`primary_window` and the `secondary` pair.
///
/// The `rateLimitsByLimitId` side channel and the `wham` wrapper are not ported:
/// they only add extra named buckets beside a canonical quota, and the CLI lane
/// that produces them is deliberately out of scope (spec §2.1.4).
pub fn codex_from_rate_limits(payload: &Value, meta: &ProviderMeta) -> Option<Attempt> {
    let primary = alias(payload, &["primary", "primaryWindow", "primary_window"])
        .and_then(|value| codex_window("primary", value));
    let secondary = alias(
        payload,
        &["secondary", "secondaryWindow", "secondary_window"],
    )
    .and_then(|value| codex_window("secondary", value));
    let windows: Vec<Window> = primary.into_iter().chain(secondary).collect();
    if windows.is_empty() {
        return None;
    }
    let mut row = LimitsProvider::new("codex", LimitStatus::Ok, LimitSource::Oauth);
    row.windows = windows;
    let plan = text(payload, &["planType", "plan_type"]);
    let mut meta = meta.clone();
    if meta.plan_label.is_empty()
        && let Some(plan) = plan
    {
        meta.plan_label = plan;
    }
    Some(Attempt::new(meta.apply(row)))
}

/// A row that carries no windows, for a probe that failed before mapping.
pub fn failure(
    provider: &str,
    status: LimitStatus,
    source: LimitSource,
    meta: &ProviderMeta,
) -> Attempt {
    let row = LimitsProvider::new(provider, status, source);
    Attempt::new(meta.apply(row))
}

/// Money windows are not produced for either provider, but the metric field is
/// part of the wire contract and is exercised by tests.
pub fn is_spend(window: &Window) -> bool {
    window.metric == Some(WindowMetric::Spend)
}

#[cfg(test)]
mod tests;
