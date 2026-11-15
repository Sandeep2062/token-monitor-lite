//! Prime Agent reader contract.
//!
//! Prime Agent writes the Pi append-only JSONL format, so most of the token
//! extraction lives in `PiReader`. These tests pin the two things this reader
//! adds: the client id it reports, and the `child_usage_attributed` skip.

use serde_json::json;
use token_monitor_lite::parsers::prime_agent::{CLIENT, PrimeAgentReader};

fn header(id: &str) -> serde_json::Value {
    json!({
        "type": "session",
        "version": 3,
        "id": id,
        "timestamp": "2026-07-09T11:00:00.000Z"
    })
}

fn assistant(input: u64, output: u64) -> serde_json::Value {
    json!({
        "type": "message",
        "id": "b1",
        "timestamp": "2026-07-09T11:00:01.000Z",
        "message": {
            "role": "assistant",
            "model": "claude-sonnet-4",
            "provider": "anthropic",
            "usage": {"input": input, "output": output}
        }
    })
}

fn read(values: &[serde_json::Value]) -> Vec<token_monitor_lite::parsers::ResponseUsage> {
    let mut reader =
        PrimeAgentReader::new("dir-session").with_fallback_timestamp(Some(1_782_718_000_000));
    for value in values {
        reader
            .ingest(&serde_json::to_vec(value).unwrap())
            .expect("record is accepted");
    }
    reader.snapshot()
}

#[test]
fn usage_is_reported_under_the_prime_agent_client() {
    // The Pi format is shared, so the client id is what distinguishes this
    // reader from the Pi and Proma ones.
    let found = read(&[header("prime-session-1"), assistant(100, 20)]);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].key.client, CLIENT);
    assert_eq!(found[0].key.session, "prime-session-1");
    assert_eq!(found[0].tokens.input, 100);
    assert_eq!(found[0].tokens.output, 20);
}

#[test]
fn the_header_names_the_session() {
    let found = read(&[header("prime-session-1"), assistant(10, 5)]);
    assert_eq!(found[0].key.session, "prime-session-1");
}

#[test]
fn the_message_carries_its_model_and_provider() {
    let found = read(&[header("s"), assistant(10, 5)]);
    assert_eq!(found[0].model, "claude-sonnet-4");
    assert_eq!(found[0].billing_provider_hint.as_deref(), Some("anthropic"));
}

#[test]
fn a_child_usage_attributed_record_is_never_a_message() {
    // Its usage belongs to the child's own transcript, which is a source in its
    // own right; emitting it here would count the child twice.
    let child = json!({
        "type": "child_usage_attributed",
        "id": "c1",
        "aggregate_usage": {"input": 900, "output": 90}
    });
    let found = read(&[header("s"), assistant(10, 5), child]);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].tokens.input, 10);
}

#[test]
fn a_child_usage_record_before_any_message_is_skipped_too() {
    let child = json!({
        "type": "child_usage_attributed",
        "id": "c1",
        "aggregate_usage": {"input": 900}
    });
    let found = read(&[header("s"), child, assistant(10, 5)]);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].tokens.input, 10);
}

#[test]
fn a_title_line_before_the_header_is_tolerated() {
    let title = json!({"type": "title", "title": "Fixture"});
    let found = read(&[title, header("s"), assistant(10, 5)]);
    assert_eq!(found.len(), 1);
}

#[test]
fn an_assistant_message_without_usage_is_a_visible_rejection() {
    // The Pi format requires a usage block on an assistant message, and this
    // codebase reports a malformed record rather than silently skipping it.
    let no_usage = json!({
        "type": "message",
        "id": "b2",
        "message": {"role": "assistant", "model": "m"}
    });
    let mut reader = PrimeAgentReader::new("s").with_fallback_timestamp(Some(1_000));
    reader
        .ingest(&serde_json::to_vec(&header("s")).unwrap())
        .expect("header accepted");
    assert_eq!(
        reader
            .ingest(&serde_json::to_vec(&no_usage).unwrap())
            .expect_err("usage is required"),
        token_monitor_lite::parsers::ReaderError::Unsupported("Pi assistant usage")
    );
}

#[test]
fn an_empty_transcript_has_no_supported_usage() {
    let mut reader = PrimeAgentReader::new("s").with_fallback_timestamp(Some(1_000));
    reader.finish().expect("finish is infallible");
    assert!(!reader.has_supported_usage());
}

#[test]
fn the_reader_finishes_without_error() {
    let mut reader = PrimeAgentReader::new("s").with_fallback_timestamp(Some(1_000));
    for value in [header("s"), assistant(10, 5)] {
        reader
            .ingest(&serde_json::to_vec(&value).unwrap())
            .expect("accepted");
    }
    reader.finish().expect("finish is infallible");
    assert!(reader.has_supported_usage());
}
