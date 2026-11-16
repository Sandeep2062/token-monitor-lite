//! Junie session-events reader contract.

use serde_json::json;
use token_monitor_lite::parsers::CostStatus;
use token_monitor_lite::parsers::junie::{CLIENT, JunieReader};

fn usage_event(usages: Vec<serde_json::Value>) -> serde_json::Value {
    json!({
        "kind": "AgentEvent",
        "timestampMs": 1_785_594_801_000_i64,
        "event": {
            "agentEvent": {
                "kind": "LlmResponseMetadataEvent",
                "agent": {"name": "Junie"},
                "modelUsage": usages
            }
        }
    })
}

fn read(events: &[serde_json::Value]) -> Vec<token_monitor_lite::parsers::ResponseUsage> {
    let mut reader =
        JunieReader::new("session-260709-110000").with_fallback_timestamp(Some(1_782_718_000_000));
    for event in events {
        reader
            .ingest(&serde_json::to_vec(event).unwrap())
            .expect("event is accepted");
    }
    reader.snapshot()
}

#[test]
fn one_observation_is_emitted_per_usage_row() {
    let event = usage_event(vec![
        json!({"model": "golden-goose", "inputTokens": 100, "outputTokens": 20}),
        json!({"model": "golden-goose-mini", "inputTokens": 50, "outputTokens": 10}),
    ]);
    let found = read(&[event]);
    assert_eq!(found.len(), 2);
    assert!(found.iter().all(|row| row.key.client == CLIENT));
}

#[test]
fn the_session_is_the_caller_supplied_one() {
    let event = usage_event(vec![json!({"model": "m", "inputTokens": 1})]);
    assert_eq!(read(&[event])[0].key.session, "session-260709-110000");
}

#[test]
fn the_counters_read_the_first_present_spelling() {
    let event = usage_event(vec![json!({
        "model": "m",
        "input": 100,
        "output": 20,
        "cacheReadInputTokens": 300,
        "cacheCreationInputTokens": 40,
        "reasoningOutputTokens": 15
    })]);
    let found = read(&[event]);
    assert_eq!(found[0].tokens.input, 100);
    assert_eq!(found[0].tokens.output, 20);
    assert_eq!(found[0].tokens.cache_read, 300);
    assert_eq!(found[0].tokens.cache_write, 40);
    assert_eq!(found[0].tokens.reasoning, 15);
}

#[test]
fn the_primary_spelling_wins_over_the_alternate() {
    let event = usage_event(vec![json!({
        "model": "m",
        "inputTokens": 100,
        "input": 999,
        "cacheInputTokens": 7,
        "cacheReadInputTokens": 999
    })]);
    let found = read(&[event]);
    assert_eq!(found[0].tokens.input, 100);
    assert_eq!(found[0].tokens.cache_read, 7);
}

#[test]
fn a_prompt_event_carries_no_usage() {
    let prompt = json!({"kind": "UserPromptEvent", "timestampMs": 1_785_594_800_000_i64});
    assert!(read(&[prompt]).is_empty());
}

#[test]
fn a_state_snapshot_is_skipped_without_parsing() {
    // The kind is not one of the two the reader looks for.
    let snapshot = json!({"kind": "SomeStateSnapshot", "payload": "x".repeat(64)});
    assert!(read(&[snapshot]).is_empty());
}

#[test]
fn an_event_with_no_model_usage_carries_nothing() {
    let event = json!({
        "kind": "AgentEvent",
        "event": {"agentEvent": {"kind": "LlmResponseMetadataEvent"}}
    });
    assert!(read(&[event]).is_empty());
}

#[test]
fn a_usage_row_with_no_model_is_skipped() {
    let event = usage_event(vec![json!({"inputTokens": 100})]);
    assert!(read(&[event]).is_empty());
}

#[test]
fn a_row_with_no_tokens_and_no_cost_is_not_an_observation() {
    let event = usage_event(vec![json!({"model": "m", "inputTokens": 0, "cost": 0})]);
    assert!(read(&[event]).is_empty());
}

#[test]
fn the_start_is_back_anchored_from_the_recorded_end() {
    // `timestampMs` is the call's end and `time` is its latency in ms.
    let event = usage_event(vec![json!({"model": "m", "inputTokens": 10, "time": 1500})]);
    let found = read(&[event]);
    assert_eq!(found[0].timestamp_ms, 1_785_594_801_000 - 1500);
}

#[test]
fn the_back_anchor_does_not_apply_to_the_fallback() {
    // With no recorded `timestampMs` the value is the file fallback, and
    // shifting it by a call latency would land on the wrong day.
    let mut event = usage_event(vec![json!({"model": "m", "inputTokens": 10, "time": 1500})]);
    event.as_object_mut().unwrap().remove("timestampMs");
    let found = read(&[event]);
    assert_eq!(found[0].timestamp_ms, 1_782_718_000_000);
}

#[test]
fn a_reported_cost_is_carried_in_nano_usd() {
    let event = usage_event(vec![json!({"model": "m", "inputTokens": 10, "cost": 0.25})]);
    assert_eq!(read(&[event])[0].cost, CostStatus::Reported(250_000_000));
}

#[test]
fn a_zero_cost_is_not_claimed() {
    // Zero here means "none recorded", not "billed nothing".
    let event = usage_event(vec![json!({"model": "m", "inputTokens": 10, "cost": 0})]);
    assert_eq!(read(&[event])[0].cost, CostStatus::Unavailable);
}

#[test]
fn the_provider_is_the_record_value_or_the_client_id() {
    let with_provider = usage_event(vec![
        json!({"model": "m", "inputTokens": 1, "provider": "openai"}),
    ]);
    assert_eq!(
        read(&[with_provider])[0].billing_provider_hint.as_deref(),
        Some("openai")
    );
    let without = usage_event(vec![json!({"model": "m", "inputTokens": 1})]);
    assert_eq!(
        read(&[without])[0].billing_provider_hint.as_deref(),
        Some("junie")
    );
}

#[test]
fn the_agent_name_is_carried() {
    let event = usage_event(vec![json!({"model": "m", "inputTokens": 1})]);
    assert_eq!(read(&[event])[0].agent.as_deref(), Some("Junie"));
}

#[test]
fn a_replayed_event_collapses_to_one_row() {
    // Copies `/fork` or a re-log produce the same key.
    let event = usage_event(vec![json!({"model": "m", "inputTokens": 10})]);
    let found = read(&[event.clone(), event]);
    assert_eq!(found.len(), 1);
}

#[test]
fn two_rows_in_one_event_stay_distinct() {
    // The dedup suffix is the row's position within the event, not a
    // file-global counter.
    let event = usage_event(vec![
        json!({"model": "m", "inputTokens": 10, "outputTokens": 1}),
        json!({"model": "m", "inputTokens": 10, "outputTokens": 1}),
    ]);
    assert_eq!(read(&[event]).len(), 2);
}

#[test]
fn malformed_json_is_a_visible_rejection() {
    let mut reader = JunieReader::new("s").with_fallback_timestamp(Some(1_000));
    let line = br#"{"kind":"LlmResponseMetadataEvent" not json"#;
    assert!(reader.ingest(line).is_err());
}
