//! Synthetic Cursor usage-cache cases adapted from the pinned Tokscale
//! `cursor.rs` reader. The source is the read-only tokscale cache; native
//! Cursor cloud sync and CSV exports are not implemented.

use serde_json::json;
use token_monitor_lite::accounting::TokenCounts;
use token_monitor_lite::parsers::ReaderError;
use token_monitor_lite::parsers::cursor::CursorReader;

fn ingest(reader: &mut CursorReader, value: serde_json::Value) {
    reader.ingest(&serde_json::to_vec(&value).unwrap()).unwrap();
}

#[test]
fn repeated_event_snapshots_collapse_to_the_largest_token_total() {
    let mut reader = CursorReader::new("active");
    ingest(
        &mut reader,
        json!({
            "usageEventsDisplay": [
                {
                    "usageEventId": "e1",
                    "conversationId": "conv-1",
                    "timestamp": "1794564000000",
                    "model": "claude-sonnet-4-5",
                    "tokenUsage": {
                        "inputTokens": 10,
                        "outputTokens": 5,
                        "cacheReadTokens": 2,
                        "cacheWriteTokens": 1
                    }
                },
                {
                    "usageEventId": "e1",
                    "conversationId": "conv-1",
                    "timestamp": "1794564000000",
                    "model": "claude-sonnet-4-5",
                    "tokenUsage": {
                        "inputTokens": 20,
                        "outputTokens": 10,
                        "cacheReadTokens": 4,
                        "cacheWriteTokens": 1
                    }
                },
                {
                    "usageEventId": "e2",
                    "conversationId": "conv-2",
                    "timestamp": 1_794_564_001_000_i64,
                    "model": "gpt-5-mini",
                    "tokenUsage": {
                        "inputTokens": 8,
                        "outputTokens": 3,
                        "cacheReadTokens": 0,
                        "cacheWriteTokens": 0
                    }
                }
            ]
        }),
    );
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].key.client, "cursor");
    assert_eq!(rows[0].key.session, "conv-1");
    assert_eq!(rows[0].model, "claude-sonnet-4-5");
    assert_eq!(rows[0].billing_provider_hint.as_deref(), Some("cursor"));
    assert_eq!(rows[0].dedup_key.as_deref(), Some("cursor-event:e1"));
    assert_eq!(rows[0].timestamp_ms, 1_794_564_000_000);
    assert_eq!(
        rows[0].tokens,
        TokenCounts {
            input: 20,
            output: 10,
            cache_read: 4,
            cache_write: 1,
            reasoning: 0,
        }
    );
    assert_eq!(rows[1].key.session, "conv-2");
    assert_eq!(rows[1].dedup_key.as_deref(), Some("cursor-event:e2"));
}

#[test]
fn idless_legacy_row_joins_a_matching_identified_event() {
    let mut reader = CursorReader::new("active");
    ingest(
        &mut reader,
        json!({
            "usageEventsDisplay": [
                {
                    "usageEventId": "e3",
                    "conversationId": "conv-3",
                    "timestamp": 1_794_564_000_000_i64,
                    "model": "claude-sonnet-4-5",
                    "tokenUsage": {"inputTokens": 10, "outputTokens": 2}
                },
                {
                    "conversationId": "conv-3",
                    "timestamp": 1_794_564_000_000_i64,
                    "model": "claude-sonnet-4-5",
                    "tokenUsage": {"inputTokens": 20, "outputTokens": 4}
                }
            ]
        }),
    );
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].dedup_key.as_deref(), Some("cursor-event:e3"));
    assert_eq!(rows[0].tokens.input, 20);
    assert_eq!(rows[0].tokens.output, 4);
}

#[test]
fn missing_conversation_uses_the_account_day_fallback() {
    let mut reader = CursorReader::new("active");
    ingest(
        &mut reader,
        json!({
            "usageEventsDisplay": [{
                "timestamp": 1_794_564_000_000_i64,
                "model": "claude-sonnet-4-5",
                "tokenUsage": {"inputTokens": 1, "outputTokens": 1}
            }]
        }),
    );
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key.session, "cursor-active-2026-11-13");
    assert_eq!(rows[0].dedup_key, None);
}

#[test]
fn missing_event_array_and_negative_counters_are_visible_unsupported() {
    let mut reader = CursorReader::new("active");
    let missing = reader.ingest(&serde_json::to_vec(&json!({"usage": []})).unwrap());
    assert!(matches!(
        missing,
        Err(ReaderError::Unsupported(_) | ReaderError::Declined(_))
    ));

    let negative = reader.ingest(
        &serde_json::to_vec(&json!({
            "usageEventsDisplay": [{
                "conversationId": "conv-4",
                "timestamp": 1_794_564_000_000_i64,
                "model": "claude-sonnet-4-5",
                "tokenUsage": {"inputTokens": -1, "outputTokens": 1}
            }]
        }))
        .unwrap(),
    );
    assert!(matches!(negative, Err(ReaderError::InvalidRecord)));
}
