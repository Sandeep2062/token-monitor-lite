//! Copilot CLI session reader contract.
//!
//! Shape taken from a real `~/.copilot/session-state/<id>/events.jsonl`: one
//! `session.usage_record` per billed call, with the counters under
//! `data.usage`.

use serde_json::{Value, json};
use token_monitor_lite::parsers::copilot_cli::{CLIENT, CopilotCliReader, UNKNOWN_MODEL};
use token_monitor_lite::parsers::{CostStatus, ReaderError};

fn usage(overrides: Value) -> Value {
    let mut base = json!({
        "model": "gpt-5.6-luna",
        "provider": {"id": "copilot", "kind": "copilot"},
        "inputTokens": 20_347,
        "outputTokens": 61,
        "cacheReadTokens": 0,
        "cacheWriteTokens": 20_344,
        "reasoningTokens": 40,
        "cost": 1.0,
        "accounting": {"sourceSessionId": "session-1", "sequence": 1}
    });
    for (key, value) in overrides.as_object().unwrap() {
        base[key] = value.clone();
    }
    json!({"type": "session.usage_record", "timestamp": "2026-10-24T15:36:45.326Z",
           "data": {"usage": base}})
}

fn read(events: Vec<Value>) -> Vec<token_monitor_lite::parsers::ResponseUsage> {
    let mut reader = CopilotCliReader::new("session-1");
    for event in events {
        reader
            .ingest(event.to_string().as_bytes())
            .expect("event is accepted");
    }
    reader.snapshot()
}

#[test]
fn one_usage_record_is_one_observation() {
    let found = read(vec![usage(json!({}))]);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].key.client, CLIENT);
    assert_eq!(found[0].model, "gpt-5.6-luna");
    // 2026-10-24T15:36:45.326Z
    assert_eq!(found[0].timestamp_ms, 1_792_856_205_326);
}

#[test]
fn input_tokens_is_gross_of_cache() {
    // Verified against the record's own `tokenDetails`: 20347 = 3 + 0 + 20344.
    let found = read(vec![usage(json!({}))]);
    assert_eq!(found[0].tokens.input, 3);
    assert_eq!(found[0].tokens.cache_write, 20_344);
    assert_eq!(found[0].tokens.cache_read, 0);
}

#[test]
fn output_tokens_is_not_reduced_by_reasoning() {
    // `tokenDetails` has no reasoning row and its `output` entry equals
    // `outputTokens`, so reasoning is already outside output.
    let found = read(vec![usage(json!({}))]);
    assert_eq!(found[0].tokens.output, 61);
    assert_eq!(found[0].tokens.reasoning, 40);
}

#[test]
fn a_premium_request_count_is_not_a_cost() {
    let found = read(vec![usage(json!({}))]);
    assert_eq!(found[0].cost, CostStatus::Unavailable);
}

#[test]
fn identity_is_the_cli_accounting_pair() {
    let found = read(vec![usage(json!({}))]);
    assert_eq!(
        found[0].dedup_key.as_deref(),
        Some("copilot:session-1:usage:1")
    );
}

#[test]
fn a_forked_session_keeps_its_own_identity() {
    // The directory name is the fork's id, but the CLI accounting id is the
    // original call's, so a fork cannot collide with the session it came from.
    let mut reader = CopilotCliReader::new("fork-directory");
    reader
        .ingest(
            usage(json!({"accounting": {"sourceSessionId": "original", "sequence": 7}}))
                .to_string()
                .as_bytes(),
        )
        .expect("accepted");
    let found = reader.snapshot();
    assert_eq!(
        found[0].dedup_key.as_deref(),
        Some("copilot:original:usage:7")
    );
}

#[test]
fn non_usage_events_are_ignored() {
    let found = read(vec![
        json!({"type": "assistant.turn_start", "timestamp": "2026-10-24T15:36:45.326Z"}),
        json!({"type": "session.usage_checkpoint", "data": {"totalNanoAiu": 1}}),
        json!({"type": "user.message", "data": {"content": "hi"}}),
    ]);
    assert!(found.is_empty());
}

#[test]
fn a_record_that_bills_nothing_is_skipped() {
    let found = read(vec![usage(json!({
        "inputTokens": 0,
        "outputTokens": 0,
        "cacheReadTokens": 0,
        "cacheWriteTokens": 0,
        "reasoningTokens": 0
    }))]);
    assert!(found.is_empty());
}

#[test]
fn cache_greater_than_input_is_refused_rather_than_clamped() {
    let mut reader = CopilotCliReader::new("session-1");
    let event = usage(json!({
        "inputTokens": 5,
        "cacheReadTokens": 4,
        "cacheWriteTokens": 4
    }));
    assert_eq!(
        reader
            .ingest(event.to_string().as_bytes())
            .expect_err("refused"),
        ReaderError::Declined("Copilot cache/input")
    );
}

#[test]
fn a_record_without_accounting_is_refused() {
    let mut reader = CopilotCliReader::new("session-1");
    let event = json!({"type": "session.usage_record", "timestamp": "2026-10-24T15:36:45.326Z",
                       "data": {"usage": {"inputTokens": 10, "outputTokens": 1}}});
    assert_eq!(
        reader
            .ingest(event.to_string().as_bytes())
            .expect_err("refused"),
        ReaderError::Declined("Copilot accounting")
    );
}

#[test]
fn a_missing_usage_object_is_visible_unsupported() {
    let mut reader = CopilotCliReader::new("session-1");
    let event = json!({"type": "session.usage_record", "timestamp": "2026-10-24T15:36:45.326Z"});
    assert_eq!(
        reader
            .ingest(event.to_string().as_bytes())
            .expect_err("unsupported"),
        ReaderError::Unsupported("Copilot usage record")
    );
}

#[test]
fn a_missing_timestamp_is_a_visible_rejection() {
    let mut reader = CopilotCliReader::new("session-1");
    let event = json!({"type": "session.usage_record", "data": {"usage": {
        "inputTokens": 10, "outputTokens": 1,
        "accounting": {"sourceSessionId": "session-1", "sequence": 1}}}});
    assert_eq!(
        reader
            .ingest(event.to_string().as_bytes())
            .expect_err("timestamp"),
        ReaderError::Declined("Copilot session timestamp")
    );
}

#[test]
fn malformed_json_is_a_visible_rejection() {
    assert_eq!(
        CopilotCliReader::new("session-1")
            .ingest(b"not json")
            .expect_err("malformed"),
        ReaderError::InvalidRecord
    );
}

#[test]
fn a_model_is_never_invented() {
    let found = read(vec![usage(json!({"model": null}))]);
    assert_eq!(found[0].model, UNKNOWN_MODEL);
}
