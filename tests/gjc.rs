//! gajae-code (`gjc`) session-transcript reader contract.

use serde_json::json;
use token_monitor_lite::parsers::CostStatus;
use token_monitor_lite::parsers::gjc::{CLIENT, GjcReader};

fn header(id: &str, cwd: &str) -> serde_json::Value {
    json!({"type": "session", "id": id, "timestamp": "2026-07-09T11:00:00.000Z", "cwd": cwd})
}

fn message(id: &str, usage: serde_json::Value) -> serde_json::Value {
    json!({
        "type": "message",
        "id": id,
        "timestamp": "2026-07-09T11:00:01.000Z",
        "message": {
            "role": "assistant",
            "model": "claude-sonnet-4",
            "provider": "anthropic",
            "api": "anthropic",
            "timestamp": 1_783_594_801_000_i64,
            "usage": usage
        }
    })
}

fn read(values: &[serde_json::Value]) -> Vec<token_monitor_lite::parsers::ResponseUsage> {
    let mut reader =
        GjcReader::new("stem-session").with_fallback_timestamp(Some(1_782_718_000_000));
    for value in values {
        reader
            .ingest(&serde_json::to_vec(value).unwrap())
            .expect("entry is accepted");
    }
    reader.snapshot()
}

#[test]
fn only_assistant_messages_are_emitted() {
    let found = read(&[
        header("gjc_ses_001", "/work/pi"),
        message("msg_001", json!({"input": 100, "output": 50})),
    ]);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].key.client, CLIENT);
    assert_eq!(found[0].key.session, "gjc_ses_001");
}

#[test]
fn a_session_header_emits_nothing_itself() {
    assert!(read(&[header("s", "/work/pi")]).is_empty());
}

#[test]
fn a_user_message_is_skipped() {
    let mut user = message("msg_001", json!({"input": 100}));
    user["message"]["role"] = json!("user");
    assert!(read(&[header("s", "/work/pi"), user]).is_empty());
}

#[test]
fn a_service_tier_change_is_skipped() {
    let tier = json!({"type": "service_tier_change", "tier": "flex"});
    assert!(read(&[header("s", "/w"), tier]).is_empty());
}

#[test]
fn an_unknown_entry_type_is_skipped() {
    let other = json!({"type": "something_else", "payload": 1});
    assert!(read(&[header("s", "/w"), other]).is_empty());
}

#[test]
fn the_counters_come_from_the_usage_block() {
    let found = read(&[
        header("s", "/w"),
        message(
            "m1",
            json!({"input": 100, "output": 50, "cacheRead": 10, "cacheWrite": 5, "reasoning": 7}),
        ),
    ]);
    assert_eq!(found[0].tokens.input, 100);
    assert_eq!(found[0].tokens.output, 50);
    assert_eq!(found[0].tokens.cache_read, 10);
    assert_eq!(found[0].tokens.cache_write, 5);
    assert_eq!(found[0].tokens.reasoning, 7);
}

#[test]
fn an_embedded_cost_is_reported_in_nano_usd() {
    let found = read(&[
        header("s", "/w"),
        message(
            "m1",
            json!({"input": 100, "cost": {"input": 0.1, "output": 0.2, "total": 0.3}}),
        ),
    ]);
    assert_eq!(found[0].cost, CostStatus::Reported(300_000_000));
}

#[test]
fn a_negative_embedded_cost_is_reported_as_unavailable() {
    // Not clamped: a contradictory figure must not become a billable one.
    let found = read(&[
        header("s", "/w"),
        message("m1", json!({"input": 100, "cost": {"total": -1.0}})),
    ]);
    assert_eq!(found[0].cost, CostStatus::Unavailable);
}

#[test]
fn a_usage_without_a_cost_reports_nothing() {
    let found = read(&[header("s", "/w"), message("m1", json!({"input": 100}))]);
    assert_eq!(found[0].cost, CostStatus::Unavailable);
}

#[test]
fn the_message_unix_timestamp_is_preferred() {
    let found = read(&[header("s", "/w"), message("m1", json!({"input": 1}))]);
    assert_eq!(found[0].timestamp_ms, 1_783_594_801_000);
}

#[test]
fn the_entry_timestamp_is_the_fallback() {
    let mut value = message("m1", json!({"input": 1}));
    value["message"]
        .as_object_mut()
        .unwrap()
        .remove("timestamp");
    let found = read(&[header("s", "/w"), value]);
    // The entry-level ISO timestamp is 2026-07-09T11:00:01Z.
    assert_eq!(found[0].timestamp_ms, 1_783_594_801_000);
}

#[test]
fn a_message_without_usage_or_model_is_skipped() {
    let mut no_usage = message("m1", json!({"input": 1}));
    no_usage["message"].as_object_mut().unwrap().remove("usage");
    assert!(read(&[header("s", "/w"), no_usage]).is_empty());

    let mut no_model = message("m1", json!({"input": 1}));
    no_model["message"].as_object_mut().unwrap().remove("model");
    assert!(read(&[header("s", "/w"), no_model]).is_empty());
}

#[test]
fn the_provider_is_the_record_value() {
    let found = read(&[header("s", "/w"), message("m1", json!({"input": 1}))]);
    assert_eq!(found[0].billing_provider_hint.as_deref(), Some("anthropic"));
}

#[test]
fn a_missing_provider_carries_no_hint() {
    // No invented `gjc` pseudo-provider: it would key nothing in the catalog.
    let mut value = message("m1", json!({"input": 1}));
    value["message"].as_object_mut().unwrap().remove("provider");
    let found = read(&[header("s", "/w"), value]);
    assert_eq!(found[0].billing_provider_hint, None);
}

#[test]
fn the_workspace_becomes_a_label_not_a_key() {
    let found = read(&[header("s", "/work/pi"), message("m1", json!({"input": 1}))]);
    assert_eq!(found[0].workspace_label.as_deref(), Some("pi"));
    assert_eq!(found[0].workspace_key, None);
}

#[test]
fn a_headerless_file_uses_the_callers_session() {
    let found = read(&[message("m1", json!({"input": 1}))]);
    assert_eq!(found[0].key.session, "stem-session");
}

#[test]
fn the_identity_uses_the_entry_id_and_names_the_client() {
    let found = read(&[
        header("gjc_ses_001", "/w"),
        message("msg_001", json!({"input": 1})),
    ]);
    assert_eq!(
        found[0].dedup_key.as_deref(),
        Some("gjc:gjc_ses_001:msg_001")
    );
}

#[test]
fn an_id_less_message_gets_a_deterministic_key() {
    let mut value = message("m1", json!({"input": 1}));
    value.as_object_mut().unwrap().remove("id");
    let first = read(&[header("s", "/w"), value.clone()]);
    let second = read(&[header("s", "/w"), value]);
    assert!(first[0].dedup_key.as_deref().unwrap().starts_with("gjc:s:"));
    assert_eq!(first[0].dedup_key, second[0].dedup_key);
}

#[test]
fn a_duplicate_message_id_collapses() {
    // The depth-1 parent and its depth-2 child can replay the same entry.
    let values = [
        header("s", "/w"),
        message("m1", json!({"input": 1})),
        message("m1", json!({"input": 1})),
    ];
    assert_eq!(read(&values).len(), 1);
}

#[test]
fn malformed_json_is_a_visible_decline() {
    let mut reader = GjcReader::new("s").with_fallback_timestamp(Some(1_000));
    assert!(reader.ingest(b"not json").is_err());
}
