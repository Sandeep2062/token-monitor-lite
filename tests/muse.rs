//! Synthetic Muse Code event cases adapted from the pinned Tokscale `muse.rs`
//! reader.

use serde_json::json;
use token_monitor_lite::accounting::TokenCounts;
use token_monitor_lite::parsers::ReaderError;
use token_monitor_lite::parsers::muse::MuseReader;

fn ingest(reader: &mut MuseReader, value: serde_json::Value) {
    reader.ingest(&serde_json::to_vec(&value).unwrap()).unwrap();
}

fn completion(
    sequence: i64,
    input: u64,
    output: u64,
    cached: u64,
    reasoning: u64,
) -> serde_json::Value {
    json!({
        "recorded_at": 1_789_790_455_896_395_i64,
        "sequence": sequence,
        "stream": {"id": "muse-session"},
        "payload_type": "runtime.session",
        "payload": {
            "kind": "run",
            "event": {
                "kind": "model_completed",
                "model": "muse-spark-1.3-contributor",
                "duration_ms": 5819,
                "usage": {
                    "input_tokens": input,
                    "output_tokens": output,
                    "cached_tokens": cached,
                    "cache_read_tokens": cached,
                    "cache_write_tokens": 0,
                    "reasoning_tokens": reasoning
                }
            }
        }
    })
}

#[test]
fn completion_events_split_cached_input_and_reasoning() {
    let mut reader = MuseReader::new("fallback");
    ingest(&mut reader, completion(7, 26964, 379, 5105, 278));
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key.client, "muse");
    assert_eq!(rows[0].key.session, "muse-session");
    assert_eq!(rows[0].model, "muse-spark-1.3-contributor");
    assert_eq!(
        rows[0].tokens,
        TokenCounts {
            input: 21859,
            output: 101,
            cache_read: 5105,
            cache_write: 0,
            reasoning: 278,
        }
    );
}

#[test]
fn non_completion_events_are_ignored() {
    let mut reader = MuseReader::new("fallback");
    ingest(
        &mut reader,
        json!({
            "recorded_at": 1_789_790_455_896_395_i64,
            "payload_type": "runtime.session.metadata",
            "payload": {"record": {"model_id": "muse-spark"}}
        }),
    );
    ingest(
        &mut reader,
        json!({
            "recorded_at": 1_789_790_455_896_395_i64,
            "payload": {"event": {"kind": "tool_call", "usage": {"input_tokens": 5}}}
        }),
    );
    assert!(reader.snapshot().is_empty());
}

#[test]
fn missing_model_or_timestamp_is_visible_unsupported() {
    let mut reader = MuseReader::new("fallback");
    let result = reader.ingest(
        &serde_json::to_vec(&json!({
            "payload": {"event": {"kind": "model_completed", "usage": {"input_tokens": 2}}}
        }))
        .unwrap(),
    );
    assert!(matches!(result, Err(ReaderError::Unsupported(_))));

    let mut reader = MuseReader::new("fallback");
    let result = reader.ingest(
        &serde_json::to_vec(&json!({
            "payload": {"event": {
                "kind": "model_completed",
                "model": "m",
                "usage": {"input_tokens": 2}
            }}
        }))
        .unwrap(),
    );
    assert!(matches!(result, Err(ReaderError::InvalidTimestamp)));
}
