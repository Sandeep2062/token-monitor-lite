//! OpenCodeReview session-transcript reader contract.

use serde_json::json;
use token_monitor_lite::parsers::CostStatus;
use token_monitor_lite::parsers::opencodereview::{CLIENT, OpenCodeReviewReader};

fn start(cwd: &str) -> serde_json::Value {
    json!({"type": "session_start", "cwd": cwd})
}

fn response(usage: serde_json::Value) -> serde_json::Value {
    json!({
        "type": "llm_response",
        "model": "claude-sonnet-4",
        "timestamp": "2026-07-09T11:00:00.000Z",
        "usage": usage
    })
}

fn read(values: &[serde_json::Value]) -> Vec<token_monitor_lite::parsers::ResponseUsage> {
    let mut reader =
        OpenCodeReviewReader::new("session-abc").with_fallback_timestamp(Some(1_782_718_000_000));
    for value in values {
        reader
            .ingest(&serde_json::to_vec(value).unwrap())
            .expect("record is accepted");
    }
    reader.snapshot()
}

#[test]
fn one_observation_is_emitted_per_llm_response() {
    let found = read(&[
        start("/work/repo"),
        response(json!({"prompt_tokens": 100, "completion_tokens": 20})),
    ]);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].key.client, CLIENT);
    assert_eq!(found[0].key.session, "session-abc");
}

#[test]
fn a_session_start_emits_nothing_but_names_the_workspace() {
    let found = read(&[start("/work/repo"), response(json!({"prompt_tokens": 1}))]);
    assert_eq!(found[0].workspace_label.as_deref(), Some("repo"));
    assert_eq!(found[0].workspace_key, None);
}

#[test]
fn the_counters_use_the_prompt_completion_names() {
    let found = read(&[response(json!({
        "prompt_tokens": 100,
        "completion_tokens": 20,
        "cache_read_tokens": 300,
        "cache_write_tokens": 40
    }))]);
    assert_eq!(found[0].tokens.input, 100);
    assert_eq!(found[0].tokens.output, 20);
    assert_eq!(found[0].tokens.cache_read, 300);
    assert_eq!(found[0].tokens.cache_write, 40);
}

#[test]
fn a_zero_usage_response_is_not_an_observation() {
    assert!(
        read(&[response(
            json!({"prompt_tokens": 0, "completion_tokens": 0})
        )])
        .is_empty()
    );
}

#[test]
fn a_response_without_usage_is_skipped() {
    let mut value = response(json!({"prompt_tokens": 1}));
    value.as_object_mut().unwrap().remove("usage");
    assert!(read(&[value]).is_empty());
}

#[test]
fn an_unrelated_record_type_is_skipped() {
    let other = json!({"type": "tool_call", "name": "read"});
    assert!(read(&[other]).is_empty());
}

#[test]
fn the_start_is_back_anchored_from_the_recorded_end() {
    let mut value = response(json!({"prompt_tokens": 10}));
    value["duration_ms"] = json!(1500);
    let found = read(&[value]);
    assert_eq!(found[0].timestamp_ms, 1_783_594_800_000 - 1500);
}

#[test]
fn the_back_anchor_does_not_apply_to_the_mtime_fallback() {
    // Without a recorded timestamp the value is the file mtime; shifting it by a
    // call duration would land on the wrong day.
    let mut value = response(json!({"prompt_tokens": 10}));
    value.as_object_mut().unwrap().remove("timestamp");
    value["duration_ms"] = json!(1500);
    let found = read(&[value]);
    assert_eq!(found[0].timestamp_ms, 1_782_718_000_000);
}

#[test]
fn the_identity_collapses_a_replayed_write() {
    // Two records agreeing on end timestamp, model and usage are one call.
    let value = response(json!({"prompt_tokens": 10, "completion_tokens": 2}));
    assert_eq!(read(&[value.clone(), value]).len(), 1);
}

#[test]
fn without_a_timestamp_distinct_calls_stay_distinct() {
    // The fallback is the same mtime for every record, so the line number is the
    // only thing that can separate two distinct calls sharing a model and counts.
    let mut value = response(json!({"prompt_tokens": 10, "completion_tokens": 2}));
    value.as_object_mut().unwrap().remove("timestamp");
    let found = read(&[value.clone(), value]);
    assert_eq!(found.len(), 2);
    assert_ne!(found[0].dedup_key, found[1].dedup_key);
}

#[test]
fn the_identity_carries_the_client_and_session() {
    let found = read(&[response(json!({"prompt_tokens": 10}))]);
    let key = found[0].dedup_key.as_deref().expect("a key is set");
    assert!(key.starts_with("opencodereview:session-abc:"), "{key}");
}

#[test]
fn no_provider_hint_is_invented() {
    let found = read(&[response(json!({"prompt_tokens": 10}))]);
    assert_eq!(found[0].billing_provider_hint, None);
    assert_eq!(found[0].cost, CostStatus::Unavailable);
}

#[test]
fn a_response_without_a_model_reports_unknown() {
    let mut value = response(json!({"prompt_tokens": 10}));
    value.as_object_mut().unwrap().remove("model");
    assert_eq!(read(&[value])[0].model, "unknown");
}

#[test]
fn malformed_json_is_a_visible_decline() {
    let mut reader = OpenCodeReviewReader::new("s").with_fallback_timestamp(Some(1_000));
    assert!(reader.ingest(br#"{"type":"llm_response" broken"#).is_err());
}
