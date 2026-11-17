//! Copilot chat-session reader contract.
//!
//! The shape is taken from a real `chatSessions/<id>.jsonl`: an append-only
//! operation log where `kind: 2` *appends* to the array at `k` and `kind: 1`
//! writes a leaf, so a request is appended when it starts and its token
//! counters arrive afterwards.

use serde_json::{Value, json};
use token_monitor_lite::parsers::copilot_chat::{CLIENT, CopilotChatReader, UNKNOWN_MODEL};
use token_monitor_lite::parsers::{CostStatus, ReaderError};

fn reader() -> CopilotChatReader {
    CopilotChatReader::new("session-1")
}

fn apply(reader: &mut CopilotChatReader, operations: Vec<Value>) {
    for operation in operations {
        reader
            .ingest(operation.to_string().as_bytes())
            .expect("operation is accepted");
    }
}

fn document() -> Value {
    json!({"version": 3, "creationDate": 1_789_802_708_000_i64, "responderUsername": "copilot"})
}

fn append(requests: Vec<Value>) -> Value {
    json!({"kind": 2, "k": ["requests"], "v": requests})
}

fn patch(index: usize, field: &str, value: Value) -> Value {
    json!({"kind": 1, "k": ["requests", index, field], "v": value})
}

#[test]
fn a_request_is_appended_before_its_counters_arrive() {
    let mut reader = reader();
    apply(
        &mut reader,
        vec![
            json!({"kind": 0, "k": null, "v": document()}),
            append(vec![json!({
                "requestId": "request-1",
                "timestamp": 1_789_802_708_095_i64,
                "modelId": "copilot/auto"
            })]),
            patch(0, "promptTokens", json!(57_404)),
            patch(0, "completionTokens", json!(5_274)),
        ],
    );
    let found = reader.snapshot();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].key.client, CLIENT);
    assert_eq!(found[0].tokens.input, 57_404);
    assert_eq!(found[0].tokens.output, 5_274);
    assert_eq!(found[0].timestamp_ms, 1_789_802_708_095);
}

#[test]
fn an_append_carries_one_row_per_request() {
    let mut reader = reader();
    apply(
        &mut reader,
        vec![
            json!({"kind": 0, "k": null, "v": document()}),
            append(vec![json!({
                "requestId": "request-1",
                "timestamp": 1_000_i64,
                "modelId": "copilot/auto",
                "promptTokens": 10,
                "completionTokens": 2
            })]),
            append(vec![json!({
                "requestId": "request-2",
                "timestamp": 2_000_i64,
                "modelId": "copilot/auto",
                "promptTokens": 20,
                "completionTokens": 3
            })]),
        ],
    );
    let found = reader.snapshot();
    assert_eq!(found.len(), 2);
    assert_eq!(found[0].tokens.input, 10);
    assert_eq!(found[1].tokens.input, 20);
    assert_eq!(found[1].timestamp_ms, 2_000);
}

#[test]
fn append_is_not_replacement() {
    // The whole point of the format: a second `kind: 2` must add, not replace.
    let mut reader = reader();
    apply(
        &mut reader,
        vec![
            json!({"kind": 0, "k": null, "v": document()}),
            append(vec![json!({
                "requestId": "request-1",
                "timestamp": 1_000_i64,
                "promptTokens": 10,
                "completionTokens": 1
            })]),
            append(vec![json!({
                "requestId": "request-2",
                "timestamp": 2_000_i64,
                "promptTokens": 20,
                "completionTokens": 1
            })]),
            append(vec![json!({
                "requestId": "request-3",
                "timestamp": 3_000_i64,
                "promptTokens": 30,
                "completionTokens": 1
            })]),
        ],
    );
    assert_eq!(reader.snapshot().len(), 3);
}

#[test]
fn a_patched_request_keeps_one_row() {
    let mut reader = reader();
    apply(
        &mut reader,
        vec![
            json!({"kind": 0, "k": null, "v": document()}),
            append(vec![json!({
                "requestId": "request-1",
                "timestamp": 1_000_i64,
                "promptTokens": 10,
                "completionTokens": 1
            })]),
            patch(0, "promptTokens", json!(99)),
            patch(0, "completionTokens", json!(9)),
        ],
    );
    let found = reader.snapshot();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].tokens.input, 99);
    assert_eq!(found[0].tokens.output, 9);
}

#[test]
fn a_request_with_no_counters_yet_is_not_a_row() {
    // Copilot appends a request before the stream produces usage, so an
    // interrupted request must not become a zero-token row.
    let mut reader = reader();
    apply(
        &mut reader,
        vec![
            json!({"kind": 0, "k": null, "v": document()}),
            append(vec![json!({
                "requestId": "request-1",
                "timestamp": 1_000_i64,
                "modelId": "copilot/auto"
            })]),
        ],
    );
    assert!(reader.snapshot().is_empty());
    assert!(!reader.has_supported_usage());
}

#[test]
fn the_model_comes_from_the_request() {
    let mut reader = reader();
    apply(
        &mut reader,
        vec![
            json!({"kind": 0, "k": null, "v": document()}),
            append(vec![json!({
                "requestId": "request-1",
                "timestamp": 1_000_i64,
                "modelId": "copilot/auto",
                "promptTokens": 10,
                "completionTokens": 1
            })]),
            append(vec![json!({
                "requestId": "request-2",
                "timestamp": 2_000_i64,
                "promptTokens": 10,
                "completionTokens": 1
            })]),
        ],
    );
    let found = reader.snapshot();
    assert_eq!(found[0].model, "copilot/auto");
    assert_eq!(found[1].model, UNKNOWN_MODEL);
}

#[test]
fn no_cost_is_invented_from_credits() {
    let mut reader = reader();
    apply(
        &mut reader,
        vec![
            json!({"kind": 0, "k": null, "v": document()}),
            append(vec![json!({
                "requestId": "request-1",
                "timestamp": 1_000_i64,
                "promptTokens": 10,
                "completionTokens": 1,
                "copilotCredits": 12
            })]),
        ],
    );
    assert_eq!(reader.snapshot()[0].cost, CostStatus::Unavailable);
}

#[test]
fn the_document_resets_state() {
    let mut reader = reader();
    apply(
        &mut reader,
        vec![
            json!({"kind": 0, "k": null, "v": document()}),
            append(vec![json!({
                "requestId": "request-1",
                "timestamp": 1_000_i64,
                "promptTokens": 10,
                "completionTokens": 1
            })]),
        ],
    );
    assert_eq!(reader.snapshot().len(), 1);
    // A rewritten document is the new truth.
    apply(
        &mut reader,
        vec![json!({"kind": 0, "k": null, "v": document()})],
    );
    assert!(reader.snapshot().is_empty());
}

#[test]
fn an_operation_before_the_document_is_ignored() {
    let mut reader = reader();
    apply(
        &mut reader,
        vec![append(vec![json!({
            "requestId": "request-1",
            "timestamp": 1_000_i64,
            "promptTokens": 10,
            "completionTokens": 1
        })])],
    );
    assert!(reader.snapshot().is_empty());
}

#[test]
fn an_unknown_operation_kind_is_ignored() {
    let mut reader = reader();
    apply(
        &mut reader,
        vec![
            json!({"kind": 0, "k": null, "v": document()}),
            json!({"kind": 7, "k": ["requests"], "v": []}),
        ],
    );
    assert!(reader.snapshot().is_empty());
}

#[test]
fn malformed_json_is_a_visible_rejection() {
    assert_eq!(
        reader().ingest(b"not json").expect_err("malformed"),
        ReaderError::InvalidRecord
    );
}

#[test]
fn a_non_object_document_is_visible_unsupported() {
    assert_eq!(
        reader()
            .ingest(br#"{"kind":0,"k":null,"v":[]}"#)
            .expect_err("unsupported"),
        ReaderError::Unsupported("Copilot chat session")
    );
}

#[test]
fn a_negative_counter_is_a_visible_rejection() {
    let mut reader = reader();
    reader
        .ingest(
            json!({"kind": 0, "k": null, "v": document()})
                .to_string()
                .as_bytes(),
        )
        .expect("document");
    let document_with_bad_counter = append(vec![json!({
        "requestId": "request-1",
        "timestamp": 1_000_i64,
        "promptTokens": -1,
        "completionTokens": 1
    })]);
    assert_eq!(
        reader
            .ingest(document_with_bad_counter.to_string().as_bytes())
            .expect_err("negative"),
        ReaderError::InvalidRecord
    );
}
