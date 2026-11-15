//! Synthetic Amp thread cases adapted from the pinned Tokscale `amp.rs`
//! reader. Ledger-first and message-fallback shapes are supported; ledger and
//! message reconciliation is not.

use serde_json::json;
use token_monitor_lite::accounting::TokenCounts;
use token_monitor_lite::parsers::ReaderError;
use token_monitor_lite::parsers::amp::AmpReader;

fn ingest(reader: &mut AmpReader, value: serde_json::Value) {
    reader.ingest(&serde_json::to_vec(&value).unwrap()).unwrap();
}

#[test]
fn usage_ledger_is_authoritative_when_it_has_token_rows() {
    let mut reader = AmpReader::new("fallback");
    ingest(
        &mut reader,
        json!({
            "id": "thread-a",
            "created": 1_794_564_000_000_i64,
            "usageLedger": {"events": [
                {
                    "model": "claude-sonnet-4-5",
                    "timestamp": "2026-11-13T10:00:00Z",
                    "tokens": {
                        "input": 10,
                        "output": 5,
                        "cacheReadInputTokens": 2,
                        "cacheCreationInputTokens": 1
                    }
                },
                {
                    "model": "gpt-5",
                    "timestamp": "2026-11-13T10:00:01Z",
                    "tokens": {"input": 4, "output": 2}
                }
            ]}
        }),
    );
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].key.client, "amp");
    assert_eq!(rows[0].key.session, "thread-a");
    assert_eq!(rows[0].model, "claude-sonnet-4-5");
    assert_eq!(rows[0].dedup_key.as_deref(), Some("amp:thread-a:ledger:0"));
    assert_eq!(
        rows[0].tokens,
        TokenCounts {
            input: 10,
            output: 5,
            cache_read: 2,
            cache_write: 1,
            reasoning: 0,
        }
    );
    assert_eq!(rows[1].dedup_key.as_deref(), Some("amp:thread-a:ledger:1"));
    assert_eq!(rows[1].model, "gpt-5");
}

#[test]
fn assistant_message_usage_is_the_fallback_without_a_ledger() {
    let mut reader = AmpReader::new("thread-b");
    ingest(
        &mut reader,
        json!({
            "created": 1_794_564_000_000_i64,
            "messages": [
                {"role": "user"},
                {
                    "role": "assistant",
                    "messageId": 7,
                    "usage": {
                        "model": "claude-sonnet-4-5",
                        "inputTokens": 10,
                        "outputTokens": 5,
                        "cacheReadInputTokens": 2,
                        "cacheCreationInputTokens": 1,
                        "timestamp": "2026-11-13T10:00:00Z"
                    }
                }
            ]
        }),
    );
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key.session, "thread-b");
    assert_eq!(rows[0].dedup_key.as_deref(), Some("amp:thread-b:message:7"));
    assert_eq!(
        rows[0].tokens,
        TokenCounts {
            input: 10,
            output: 5,
            cache_read: 2,
            cache_write: 1,
            reasoning: 0,
        }
    );
}

#[test]
fn zero_ledger_rows_fall_back_to_messages() {
    let mut reader = AmpReader::new("thread-c");
    ingest(
        &mut reader,
        json!({
            "created": 1_794_564_000_000_i64,
            "usageLedger": {"events": [{"model": "m", "tokens": {}}]},
            "messages": [{
                "role": "assistant",
                "messageId": 3,
                "usage": {"model": "m", "inputTokens": 2, "outputTokens": 1}
            }]
        }),
    );
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].dedup_key.as_deref(), Some("amp:thread-c:message:3"));
}

#[test]
fn negative_counters_and_invalid_shapes_are_visible_unsupported() {
    let mut reader = AmpReader::new("thread-d");
    let negative = reader.ingest(
        &serde_json::to_vec(&json!({
            "id": "thread-d",
            "created": 1_794_564_000_000_i64,
            "usageLedger": {"events": [{
                "model": "m",
                "timestamp": "2026-11-13T10:00:00Z",
                "tokens": {"input": -1}
            }]}
        }))
        .unwrap(),
    );
    assert!(matches!(negative, Err(ReaderError::InvalidRecord)));

    let missing_messages = reader.ingest(&serde_json::to_vec(&json!({"id": "x"})).unwrap());
    assert!(missing_messages.is_ok());
    assert!(reader.snapshot().is_empty());
}
