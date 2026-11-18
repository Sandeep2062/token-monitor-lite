//! Provider mapping tests.
//!
//! Every payload here is a fixture: a hand-built representation of the documented
//! wire contract, not a capture from a live account. Spec §5 states which claims
//! these tests do and do not support.

use serde_json::json;

use super::*;
use crate::limits::window::WindowKind;

fn meta() -> ProviderMeta {
    ProviderMeta {
        account_key: "account-1".to_owned(),
        account_label: "Primary".to_owned(),
        updated_at: Some("2026-11-15T12:00:00Z".to_owned()),
        ..ProviderMeta::default()
    }
}

#[test]
fn claude_maps_the_session_and_weekly_windows() {
    let usage = json!({
        "five_hour": { "utilization": 42.5, "resets_at": "2026-11-15T17:00:00Z" },
        "seven_day": { "utilization": 12.0, "resets_at": "2026-11-20T00:00:00Z" }
    });
    let attempt = claude_from_usage(&usage, &meta()).expect("attempt");
    assert_eq!(attempt.provider.provider, "claude");
    assert_eq!(attempt.provider.status, LimitStatus::Ok);
    assert_eq!(attempt.provider.windows.len(), 2);
    let session = &attempt.provider.windows[0];
    assert_eq!(session.kind, WindowKind::Session);
    assert_eq!(session.used_percent, Some(42.5));
    assert_eq!(session.remaining_percent(), Some(57.5));
    assert_eq!(session.resets_at.as_deref(), Some("2026-11-15T17:00:00Z"));
    assert_eq!(attempt.provider.windows[1].kind, WindowKind::Weekly);
}

#[test]
fn claude_accepts_the_camel_case_spellings() {
    let usage = json!({ "fiveHour": { "percent": 10.0 }, "sevenDay": { "percent": 20.0 } });
    let attempt = claude_from_usage(&usage, &meta()).expect("attempt");
    assert_eq!(attempt.provider.windows.len(), 2);
    assert_eq!(attempt.provider.windows[0].used_percent, Some(10.0));
}

#[test]
fn claude_derives_percent_from_used_and_limit() {
    let usage = json!({ "five_hour": { "used": 25, "limit": 200 } });
    let attempt = claude_from_usage(&usage, &meta()).expect("attempt");
    assert_eq!(attempt.provider.windows[0].used_percent, Some(12.5));
}

#[test]
fn a_percentage_is_clamped_rather_than_displayed_raw() {
    let usage = json!({ "five_hour": { "utilization": 140.0 } });
    let attempt = claude_from_usage(&usage, &meta()).expect("attempt");
    assert_eq!(attempt.provider.windows[0].used_percent, Some(100.0));
}

#[test]
fn a_window_without_any_percentage_is_not_invented() {
    // No utilisation, and no used/limit pair to derive one from: the reference
    // refuses the window rather than rendering a zero bar.
    let usage = json!({ "five_hour": { "resets_at": "2026-11-15T17:00:00Z" } });
    assert!(claude_from_usage(&usage, &meta()).is_none());
}

#[test]
fn a_usage_payload_with_no_windows_yields_no_row() {
    assert!(claude_from_usage(&json!({}), &meta()).is_none());
    assert!(claude_from_usage(&json!({ "unexpected": true }), &meta()).is_none());
}

#[test]
fn a_non_rfc3339_reset_is_dropped_not_passed_through() {
    let usage = json!({ "five_hour": { "utilization": 5.0, "resets_at": "in 3 hours" } });
    let attempt = claude_from_usage(&usage, &meta()).expect("attempt");
    assert_eq!(attempt.provider.windows[0].resets_at, None);
}

#[test]
fn codex_maps_primary_and_secondary_to_session_and_weekly() {
    let payload = json!({
        "planType": "plus",
        "primary": { "usedPercent": 31.0, "resetsAt": "2026-11-15T15:00:00Z" },
        "secondary": { "usedPercent": 8.0, "resetsAt": "2026-11-20T00:00:00Z" }
    });
    let attempt = codex_from_rate_limits(&payload, &meta()).expect("attempt");
    assert_eq!(attempt.provider.provider, "codex");
    assert_eq!(attempt.provider.plan_label, "plus");
    assert_eq!(attempt.provider.windows[0].kind, WindowKind::Session);
    assert_eq!(attempt.provider.windows[1].kind, WindowKind::Weekly);
    assert_eq!(attempt.provider.windows[0].used_percent, Some(31.0));
}

#[test]
fn codex_reads_the_cadence_from_the_declared_duration() {
    // A 24-hour primary is daily, not a session: the duration outranks the slot.
    let payload = json!({
        "primary": { "usedPercent": 5.0, "windowDurationMins": 1440 },
        "secondary": { "usedPercent": 6.0, "windowDurationMins": 10080 }
    });
    let attempt = codex_from_rate_limits(&payload, &meta()).expect("attempt");
    assert_eq!(attempt.provider.windows[0].kind, WindowKind::Daily);
    assert_eq!(attempt.provider.windows[0].window_minutes, Some(1440));
    assert_eq!(attempt.provider.windows[1].kind, WindowKind::Weekly);
}

#[test]
fn codex_reads_a_duration_in_seconds() {
    let payload = json!({ "primary": { "usedPercent": 5.0, "limitWindowSeconds": 300 } });
    let attempt = codex_from_rate_limits(&payload, &meta()).expect("attempt");
    assert_eq!(attempt.provider.windows[0].kind, WindowKind::Session);
    assert_eq!(attempt.provider.windows[0].window_minutes, Some(5));
}

#[test]
fn a_monthly_codex_quota_is_a_billing_window() {
    let payload = json!({ "primary": { "usedPercent": 5.0, "windowDurationMins": 43200 } });
    let attempt = codex_from_rate_limits(&payload, &meta()).expect("attempt");
    assert_eq!(attempt.provider.windows[0].kind, WindowKind::Billing);
}

#[test]
fn codex_ignores_the_named_bucket_side_channel() {
    // `rateLimitsByLimitId` only adds buckets beside a canonical quota, and the
    // lane that produces it is out of scope; it must not fabricate windows.
    let payload = json!({
        "rateLimitsByLimitId": { "codex": { "primary": { "usedPercent": 90.0 } } }
    });
    assert!(codex_from_rate_limits(&payload, &meta()).is_none());
}

#[test]
fn the_source_defaults_to_oauth_and_is_never_guessed() {
    let attempt = claude_from_usage(&json!({ "five_hour": { "utilization": 1.0 } }), &meta())
        .expect("attempt");
    assert_eq!(attempt.provider.source, LimitSource::Oauth);
}

#[test]
fn a_failure_row_carries_no_windows() {
    let attempt = failure(
        "claude",
        LimitStatus::Unauthorized,
        LimitSource::Oauth,
        &meta(),
    );
    assert!(attempt.provider.windows.is_empty());
    assert_eq!(attempt.provider.status, LimitStatus::Unauthorized);
    assert_eq!(attempt.provider.account_key, "account-1");
}

#[test]
fn a_row_never_carries_a_credential() {
    let attempt = claude_from_usage(&json!({ "five_hour": { "utilization": 1.0 } }), &meta())
        .expect("attempt");
    let rendered = serde_json::to_string(&attempt.provider).expect("json");
    for forbidden in ["Bearer ", "access_token", "refresh_token", "sk-"] {
        assert!(!rendered.contains(forbidden), "row leaked {forbidden}");
    }
}
