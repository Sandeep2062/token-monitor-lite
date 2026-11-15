//! Synthetic Reasonix statistics cases adapted from the pinned Tokscale
//! `reasonix.rs` reader.

use serde_json::json;
use token_monitor_lite::accounting::TokenCounts;
use token_monitor_lite::parsers::ReaderError;
use token_monitor_lite::parsers::reasonix::ReasonixReader;

fn ingest(reader: &mut ReasonixReader, value: serde_json::Value) {
    reader.ingest(&serde_json::to_vec(&value).unwrap()).unwrap();
}

#[test]
fn authoritative_cache_miss_and_reasoning_split_are_preserved() {
    let mut reader = ReasonixReader::new("2026-08-04");
    ingest(
        &mut reader,
        json!({
            "ts": "2026-08-04T09:10:11Z",
            "model": "deepseek/chat",
            "prompt": 100,
            "completion": 20,
            "reasoning": 5,
            "cache_hit": 30,
            "cache_miss": 70,
            "total": 120,
            "requests": 1
        }),
    );
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key.client, "reasonix");
    assert_eq!(rows[0].key.session, "2026-08-04");
    assert_eq!(rows[0].model, "chat");
    assert_eq!(rows[0].billing_provider_hint.as_deref(), Some("deepseek"));
    assert_eq!(
        rows[0].tokens,
        TokenCounts {
            input: 70,
            output: 15,
            cache_read: 30,
            cache_write: 0,
            reasoning: 5,
        }
    );
}

#[test]
fn older_record_without_cache_miss_derives_uncached_input() {
    let mut reader = ReasonixReader::new("stats");
    ingest(
        &mut reader,
        json!({
            "ts": "2026-08-04T09:11:11Z",
            "model": "qwen3-coder",
            "prompt": 10,
            "completion": 2,
            "cache_hit": 3,
            "total": 12
        }),
    );
    assert_eq!(reader.snapshot()[0].tokens.input, 7);
}

#[test]
fn turn_markers_blank_models_and_zero_rows_are_ignored() {
    let mut reader = ReasonixReader::new("stats");
    ingest(
        &mut reader,
        json!({"ts": "2026-08-04T09:10:11Z", "model": "m", "total": 10, "turn": true}),
    );
    ingest(
        &mut reader,
        json!({"ts": "2026-08-04T09:10:11Z", "total": 10}),
    );
    ingest(
        &mut reader,
        json!({"ts": "2026-08-04T09:10:11Z", "model": "m", "total": 0}),
    );
    assert!(reader.snapshot().is_empty());
}

#[test]
fn negative_counters_and_missing_timestamps_are_visible_unsupported() {
    let mut reader = ReasonixReader::new("stats");
    let result = reader.ingest(
        &serde_json::to_vec(&json!({
            "ts": "2026-08-04T09:10:11Z",
            "model": "m",
            "prompt": -1,
            "total": 10
        }))
        .unwrap(),
    );
    assert!(matches!(result, Err(ReaderError::InvalidRecord)));

    let result = reader.ingest(&serde_json::to_vec(&json!({"model": "m", "total": 10})).unwrap());
    assert!(matches!(result, Err(ReaderError::InvalidTimestamp)));
}
