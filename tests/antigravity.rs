//! Synthetic Antigravity usage-cache cases adapted from the pinned Tokscale
//! `antigravity.rs` reader.

use serde_json::json;
use token_monitor_lite::accounting::TokenCounts;
use token_monitor_lite::parsers::ReaderError;
use token_monitor_lite::parsers::antigravity::{AntigravityDbReader, AntigravityReader};

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

fn varint(mut value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            return out;
        }
    }
}

fn field_varint(field: u64, value: u64) -> Vec<u8> {
    let mut out = varint(field << 3);
    out.extend(varint(value));
    out
}

fn field_bytes(field: u64, value: &[u8]) -> Vec<u8> {
    let mut out = varint((field << 3) | 2);
    out.extend(varint(value.len() as u64));
    out.extend_from_slice(value);
    out
}

#[test]
fn generation_database_protobuf_usage_is_decoded() {
    let mut usage = Vec::new();
    usage.extend(field_varint(1, 100));
    usage.extend(field_varint(2, 50));
    usage.extend(field_varint(5, 5));
    usage.extend(field_varint(9, 20));
    usage.extend(field_varint(10, 3));
    usage.extend(field_bytes(11, b"resp-1"));
    let mut chat = Vec::new();
    chat.extend(field_bytes(19, b"gemini-3.5-flash-high"));
    chat.extend(field_bytes(4, &usage));
    let mut blob = Vec::new();
    blob.extend(field_bytes(1, &chat));

    let mut reader = AntigravityDbReader::new("conversation-1", 1_780_000_000_000);
    reader.ingest_blob(&blob).unwrap();
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key.client, "antigravity");
    assert_eq!(rows[0].key.session, "conversation-1");
    assert_eq!(rows[0].model, "gemini-3.5-flash-high");
    assert_eq!(rows[0].tokens.input, 150);
    assert_eq!(rows[0].tokens.output, 20);
    assert_eq!(rows[0].tokens.cache_read, 5);
    assert_eq!(rows[0].tokens.reasoning, 3);
    assert_eq!(
        rows[0].dedup_key.as_deref(),
        Some("antigravity:conversation-1:resp-1")
    );
}
