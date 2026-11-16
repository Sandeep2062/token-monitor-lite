//! Augment Code session-snapshot reader contract.

use serde_json::json;
use token_monitor_lite::parsers::CostStatus;
use token_monitor_lite::parsers::augment::{AugmentReader, CLIENT};

fn read(value: serde_json::Value) -> Vec<token_monitor_lite::parsers::ResponseUsage> {
    let mut reader =
        AugmentReader::new("stem-session").with_fallback_timestamp(Some(1_782_718_000_000));
    reader
        .ingest(value.to_string().as_bytes())
        .expect("snapshot is accepted");
    reader.snapshot()
}

/// A completed turn with the given usage node.
fn turn(usage: serde_json::Value) -> serde_json::Value {
    json!({
        "finishedAt": "2026-07-09T11:00:00.000Z",
        "completed": true,
        "exchange": {
            "modelId": "claude-sonnet-4",
            "requestId": "req-1",
            "responseNodes": [{"tokenUsage": usage}]
        }
    })
}

fn session(turns: Vec<serde_json::Value>) -> serde_json::Value {
    json!({
        "sessionId": "augment-session-1",
        "agentState": {"modelId": "default-model"},
        "chatHistory": turns
    })
}

#[test]
fn one_observation_is_emitted_per_completed_turn() {
    let found = read(session(vec![
        turn(json!({"inputTokens": 100, "outputTokens": 20})),
        turn(json!({"inputTokens": 50, "outputTokens": 10})),
    ]));
    assert_eq!(found.len(), 2);
    assert!(found.iter().all(|row| row.key.client == CLIENT));
}

#[test]
fn an_incomplete_turn_is_not_counted() {
    // Snapshots retain aborted turns that already carry a partial usage.
    let mut partial = turn(json!({"inputTokens": 100}));
    partial["completed"] = json!(false);
    assert!(read(session(vec![partial])).is_empty());
}

#[test]
fn a_turn_with_no_completed_flag_is_not_counted() {
    let mut turn = turn(json!({"inputTokens": 100}));
    turn.as_object_mut().unwrap().remove("completed");
    assert!(read(session(vec![turn])).is_empty());
}

#[test]
fn the_last_non_empty_usage_node_wins_rather_than_summing() {
    // Summing repeated cumulative values would double-count; the final streamed
    // total is authoritative.
    let mut turn = turn(json!({"inputTokens": 10}));
    turn["exchange"]["responseNodes"] = json!([
        {"tokenUsage": {"inputTokens": 10}},
        {"tokenUsage": {"inputTokens": 100, "outputTokens": 20}}
    ]);
    let found = read(session(vec![turn]));
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].tokens.input, 100);
}

#[test]
fn the_cache_buckets_are_independent_of_the_input() {
    // Anthropic-style split accounting: cache reads are not subtracted from the
    // input count.
    let found = read(session(vec![turn(json!({
        "inputTokens": 100,
        "outputTokens": 20,
        "cacheReadInputTokens": 300,
        "cacheCreationInputTokens": 40
    }))]));
    assert_eq!(found[0].tokens.input, 100);
    assert_eq!(found[0].tokens.cache_read, 300);
    assert_eq!(found[0].tokens.cache_write, 40);
}

#[test]
fn a_turn_without_an_exchange_is_skipped() {
    let mut turn = turn(json!({"inputTokens": 1}));
    turn.as_object_mut().unwrap().remove("exchange");
    assert!(read(session(vec![turn])).is_empty());
}

#[test]
fn a_turn_without_a_usage_node_is_skipped() {
    let mut turn = turn(json!({"inputTokens": 1}));
    turn["exchange"]["responseNodes"] = json!([]);
    assert!(read(session(vec![turn])).is_empty());
}

#[test]
fn the_exchange_model_wins_over_the_agent_state_model() {
    let found = read(session(vec![turn(json!({"inputTokens": 1}))]));
    assert_eq!(found[0].model, "claude-sonnet-4");
}

#[test]
fn a_turn_without_an_exchange_model_uses_the_agent_state_model() {
    let mut turn = turn(json!({"inputTokens": 1}));
    turn["exchange"].as_object_mut().unwrap().remove("modelId");
    let found = read(session(vec![turn]));
    assert_eq!(found[0].model, "default-model");
}

#[test]
fn the_snapshot_session_id_wins_over_the_stem() {
    let found = read(session(vec![turn(json!({"inputTokens": 1}))]));
    assert_eq!(found[0].key.session, "augment-session-1");
}

#[test]
fn a_blank_session_id_keeps_the_callers_session() {
    let mut value = session(vec![turn(json!({"inputTokens": 1}))]);
    value["sessionId"] = json!("   ");
    assert_eq!(read(value)[0].key.session, "stem-session");
}

#[test]
fn the_observation_is_anchored_at_the_turn_end() {
    // Auggie records `finishedAt` only; there is no start or duration.
    let found = read(session(vec![turn(json!({"inputTokens": 1}))]));
    assert_eq!(found[0].timestamp_ms, 1_783_594_800_000);
}

#[test]
fn a_turn_without_finished_at_uses_the_fallback() {
    let mut turn = turn(json!({"inputTokens": 1}));
    turn.as_object_mut().unwrap().remove("finishedAt");
    assert_eq!(read(session(vec![turn]))[0].timestamp_ms, 1_782_718_000_000);
}

#[test]
fn cost_is_never_claimed() {
    // The pinned reader emits cost 0 and lets downstream pricing estimate from
    // the model id; Augment credits are deliberately ignored.
    let found = read(session(vec![turn(json!({"inputTokens": 1}))]));
    assert_eq!(found[0].cost, CostStatus::Unavailable);
    assert_eq!(found[0].billing_provider_hint, None);
    assert_eq!(found[0].cache_write_1h, 0);
}

#[test]
fn the_identity_prefers_the_request_id() {
    let found = read(session(vec![turn(json!({"inputTokens": 1}))]));
    assert_eq!(
        found[0].dedup_key.as_deref(),
        Some("augment:augment-session-1:req:req-1")
    );
}

#[test]
fn the_identity_falls_back_to_the_sequence_id() {
    let mut turn = turn(json!({"inputTokens": 1}));
    turn["exchange"]
        .as_object_mut()
        .unwrap()
        .remove("requestId");
    turn["sequenceId"] = json!(7);
    let found = read(session(vec![turn]));
    assert_eq!(
        found[0].dedup_key.as_deref(),
        Some("augment:augment-session-1:seq:7")
    );
}

#[test]
fn the_identity_falls_back_to_the_turn_index() {
    let mut turn = turn(json!({"inputTokens": 1}));
    turn["exchange"]
        .as_object_mut()
        .unwrap()
        .remove("requestId");
    let found = read(session(vec![turn]));
    assert_eq!(
        found[0].dedup_key.as_deref(),
        Some("augment:augment-session-1:turn:0")
    );
}

#[test]
fn malformed_json_is_a_visible_rejection() {
    let mut reader = AugmentReader::new("s").with_fallback_timestamp(Some(1_000));
    assert!(reader.ingest(b"not json").is_err());
}

#[test]
fn a_revision_replaces_rather_than_appends() {
    let value = session(vec![turn(json!({"inputTokens": 1}))]);
    let mut reader = AugmentReader::new("s").with_fallback_timestamp(Some(1_000));
    reader
        .ingest(value.to_string().as_bytes())
        .expect("accepted");
    reader
        .ingest(value.to_string().as_bytes())
        .expect("accepted");
    assert_eq!(reader.snapshot().len(), 1);
    assert!(reader.has_supported_usage());
}
