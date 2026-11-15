//! Synthetic Antigravity usage-cache cases adapted from the pinned Tokscale
//! `antigravity.rs` reader.

use serde_json::json;
use token_monitor_lite::accounting::TokenCounts;
use token_monitor_lite::parsers::ReaderError;
use token_monitor_lite::parsers::antigravity::AntigravityReader;

fn ingest(reader: &mut AntigravityReader, value: serde_json::Value) {
    reader.ingest(&serde_json::to_vec(&value).unwrap()).unwrap();
}

#[test]
fn usage_uses_session_meta_model_and_direct_counters() {
    let mut reader = AntigravityReader::new("fallback");
    ingest(
        &mut reader,
        json!({"type": "session_meta", "sessionId": "ag-session", "modelId": "claude-sonnet-4-6"}),
    );
    ingest(
        &mut reader,
        json!({
            "type": "usage",
            "sessionId": "ag-session",
            "timestamp": 1_711_200_000_000_i64,
            "input": 12,
            "output": 4,
            "cacheRead": 2,
            "cacheWrite": 1,
            "reasoning": 1,
            "responseId": "resp-1"
        }),
    );
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].model, "claude-sonnet-4-6");
    assert_eq!(
        rows[0].dedup_key.as_deref(),
        Some("antigravity:ag-session:resp-1")
    );
    assert_eq!(
        rows[0].tokens,
        TokenCounts {
            input: 12,
            output: 4,
            cache_read: 2,
            cache_write: 1,
            reasoning: 1,
        }
    );
}

#[test]
fn zero_rows_and_unknown_record_types_are_ignored() {
    let mut reader = AntigravityReader::new("fallback");
    ingest(&mut reader, json!({"type": "other"}));
    ingest(
        &mut reader,
        json!({
            "type": "usage",
            "sessionId": "s",
            "timestamp": 1,
            "input": 0,
            "output": 0
        }),
    );
    assert!(reader.snapshot().is_empty());
}

#[test]
fn negative_counters_are_visible_unsupported() {
    let mut reader = AntigravityReader::new("fallback");
    let result = reader.ingest(
        &serde_json::to_vec(&json!({
            "type": "usage",
            "sessionId": "s",
            "timestamp": 1,
            "input": -1
        }))
        .unwrap(),
    );
    assert!(matches!(result, Err(ReaderError::InvalidRecord)));
}
