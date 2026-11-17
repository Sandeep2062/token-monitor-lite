//! Roo Code task-log reader contract.
//!
//! The shape is taken from a real `ui_messages.json`: a `ClineMessage[]` where
//! each completed API request is a `say: "api_req_started"` whose `text` is a
//! JSON *string* holding the counters.

use serde_json::{Value, json};
use token_monitor_lite::parsers::roocode::RooCodeReader;
use token_monitor_lite::parsers::roocode::{CLIENT, CLINE_CLIENT, KILO_CLIENT, UNKNOWN_MODEL};
use token_monitor_lite::parsers::{CostStatus, ReaderError};

fn request(ts: i64, payload: Value) -> Value {
    json!({
        "ts": ts,
        "type": "say",
        "say": "api_req_started",
        "text": payload.to_string()
    })
}

fn read(messages: Vec<Value>) -> Vec<token_monitor_lite::parsers::ResponseUsage> {
    let mut reader = RooCodeReader::new("task-1");
    reader
        .ingest(Value::Array(messages).to_string().as_bytes())
        .expect("document is accepted");
    reader.snapshot()
}

#[test]
fn one_observation_is_emitted_per_completed_request() {
    // The real fixture's two requests, verbatim.
    let found = read(vec![
        request(
            1_740_000_000_100,
            json!({"tokensIn": 120, "tokensOut": 30, "cacheWrites": 1, "cacheReads": 2, "cost": 0.01}),
        ),
        request(
            1_740_000_000_300,
            json!({"tokensIn": 180, "tokensOut": 40, "cacheWrites": 0, "cacheReads": 0, "cost": 0.02}),
        ),
    ]);
    assert_eq!(found.len(), 2);
    assert_eq!(found[0].key.client, CLIENT);
    assert_eq!(found[0].timestamp_ms, 1_740_000_000_100);
    assert_eq!(found[1].timestamp_ms, 1_740_000_000_300);
}

#[test]
fn tokens_in_is_gross_of_cache_and_the_overlap_is_removed_once() {
    // `tokensIn` is `input + cacheWrites + cacheReads` under the Anthropic
    // protocol and raw `input_tokens` (cache included) under OpenAI, so in both
    // shapes the cache is already inside it.
    let found = read(vec![request(
        1_740_000_000_100,
        json!({"tokensIn": 120, "tokensOut": 30, "cacheWrites": 1, "cacheReads": 2}),
    )]);
    assert_eq!(found.len(), 1);
    let tokens = found[0].tokens;
    assert_eq!(tokens.cache_write, 1);
    assert_eq!(tokens.cache_read, 2);
    // 120 gross - 1 - 2 = 117 uncached input.
    assert_eq!(tokens.input, 117);
    assert_eq!(tokens.output, 30);
    assert_eq!(tokens.reasoning, 0);
}

#[test]
fn a_reported_cost_is_recorded_verbatim() {
    let found = read(vec![request(
        1_740_000_000_100,
        json!({"tokensIn": 10, "tokensOut": 5, "cost": 0.0125}),
    )]);
    assert_eq!(found[0].cost, CostStatus::Reported(12_500_000));
}

#[test]
fn a_task_with_no_resolvable_model_is_reported_as_unknown() {
    // The model lives in Roo's provider profile settings, not in the task
    // directory, so every row is left visible rather than guessed.
    let found = read(vec![request(
        1_740_000_000_100,
        json!({"tokensIn": 10, "tokensOut": 5}),
    )]);
    assert_eq!(found[0].model, UNKNOWN_MODEL);
    assert_eq!(found[0].billing_provider_hint, None);
}

#[test]
fn the_placeholder_request_written_before_streaming_carries_no_usage() {
    // Roo opens every request with `{"apiProtocol": …}` and only fills the
    // counters once the stream completes, so a cancelled request looks like
    // this and must not be counted.
    let found = read(vec![
        json!({"ts": 1_740_000_000_050_i64, "type": "say", "say": "api_req_started",
               "text": "{\"apiProtocol\":\"anthropic\"}"}),
        request(1_740_000_000_100, json!({"tokensIn": 10, "tokensOut": 5})),
    ]);
    assert_eq!(found.len(), 1);
}

#[test]
fn non_usage_messages_are_ignored() {
    let found = read(vec![
        json!({"ts": 1_i64, "type": "say", "say": "text", "text": "hello"}),
        // An `ask` of the same name is a prompt, not a usage record.
        json!({"ts": 2_i64, "type": "ask", "ask": "api_req_started", "text": "{}"}),
        json!({"ts": 3_i64, "type": "ask", "ask": "tool", "text": "{\"tool\":\"appliedDiff\"}"}),
    ]);
    assert!(found.is_empty());
}

#[test]
fn cache_greater_than_input_is_refused_rather_than_clamped() {
    let mut reader = RooCodeReader::new("task-1");
    let document = Value::Array(vec![request(
        1_740_000_000_100,
        json!({"tokensIn": 5, "tokensOut": 1, "cacheWrites": 4, "cacheReads": 4}),
    )])
    .to_string();
    assert_eq!(
        reader.ingest(document.as_bytes()).expect_err("refused"),
        ReaderError::Declined("task-log cache/input")
    );
}

#[test]
fn a_non_array_document_is_visible_unsupported() {
    let mut reader = RooCodeReader::new("task-1");
    assert_eq!(
        reader
            .ingest(br#"{"messages":[]}"#)
            .expect_err("unsupported"),
        ReaderError::Unsupported("task-log ui_messages")
    );
}

#[test]
fn malformed_json_is_a_visible_rejection() {
    let mut reader = RooCodeReader::new("task-1");
    assert_eq!(
        reader.ingest(b"not json").expect_err("malformed"),
        ReaderError::InvalidRecord
    );
}

#[test]
fn a_negative_counter_is_a_visible_rejection() {
    let mut reader = RooCodeReader::new("task-1");
    let document =
        Value::Array(vec![request(1_740_000_000_100, json!({"tokensIn": -5}))]).to_string();
    assert_eq!(
        reader.ingest(document.as_bytes()).expect_err("negative"),
        ReaderError::InvalidRecord
    );
}

#[test]
fn the_same_log_reads_under_the_cline_and_kilo_clients() {
    // Cline and Kilo Code write the same task log; only the client id and the
    // root differ, so the row must follow the configured client and not the
    // module's default.
    for client in [CLINE_CLIENT, KILO_CLIENT] {
        let mut reader = RooCodeReader::with_client(client, "task-1");
        let document = Value::Array(vec![request(
            1_740_000_000_100,
            json!({"tokensIn": 120, "tokensOut": 30, "cacheWrites": 1, "cacheReads": 2, "cost": 0.01}),
        )])
        .to_string();
        reader.ingest(document.as_bytes()).expect("accepted");
        let found = reader.snapshot();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].key.client, client);
        assert_eq!(found[0].tokens.input, 117);
        assert!(
            found[0]
                .dedup_key
                .as_deref()
                .is_some_and(|key| key.starts_with(&format!("{client}:")))
        );
    }
}
