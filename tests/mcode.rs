//! Synthetic MiniMax Code headless stream cases adapted from the pinned
//! Tokscale `mcode.rs` reader.

use serde_json::json;
use token_monitor_lite::parsers::mcode::McodeReader;

fn ingest(reader: &mut McodeReader, value: serde_json::Value) {
    reader.ingest(&serde_json::to_vec(&value).unwrap()).unwrap();
}

fn message(turn: &str, timestamp: i64, input: u64, output: u64) -> serde_json::Value {
    json!({
        "type": "message",
        "message": {
            "role": "assistant",
            "turnId": turn,
            "timestamp": timestamp,
            "usage": {
                "inputTokens": input,
                "outputTokens": output,
                "cacheReadTokens": 0,
                "cacheWriteTokens": 0
            }
        }
    })
}

#[test]
fn legacy_messages_wait_for_a_model_bearing_result() {
    let mut reader = McodeReader::new();
    ingest(&mut reader, message("turn-1", 1_780_000_000_000, 10, 2));
    assert!(reader.snapshot().is_empty());

    ingest(
        &mut reader,
        json!({
            "type": "exec.result",
            "turnId": "turn-1",
            "sessionId": "mcode-session",
            "model": {"providerId": "minimax", "modelId": "MiniMax-M2"}
        }),
    );
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key.client, "mcode");
    assert_eq!(rows[0].key.session, "mcode-session");
    assert_eq!(rows[0].model, "MiniMax-M2");
    assert_eq!(rows[0].billing_provider_hint.as_deref(), Some("minimax"));
    assert_eq!(rows[0].tokens.input, 10);
    assert_eq!(rows[0].tokens.output, 2);
}

#[test]
fn completed_envelope_carries_its_own_usage() {
    let mut reader = McodeReader::new();
    ingest(
        &mut reader,
        json!({
            "type": "exec.completed",
            "turnId": "turn-2",
            "sessionId": "mcode-session",
            "timestampMs": 1_780_000_000_000_i64,
            "result": {
                "model": {"providerId": "minimax", "modelId": "MiniMax-M2"},
                "usage": {"inputTokens": 5, "outputTokens": 1}
            }
        }),
    );
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].tokens.input, 5);
    assert_eq!(rows[0].agent.as_deref(), Some("headless"));
}

#[test]
fn result_without_pending_usage_emits_nothing() {
    let mut reader = McodeReader::new();
    ingest(
        &mut reader,
        json!({
            "type": "exec.result",
            "turnId": "missing",
            "sessionId": "mcode-session",
            "model": {"providerId": "minimax", "modelId": "MiniMax-M2"}
        }),
    );
    assert!(reader.snapshot().is_empty());
}
