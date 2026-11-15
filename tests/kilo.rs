//! Synthetic Kilo SQLite message cases adapted from the pinned Tokscale
//! `kilo.rs`/`opencode_schema.rs` readers.

use serde_json::json;
use token_monitor_lite::accounting::TokenCounts;
use token_monitor_lite::parsers::ReaderError;
use token_monitor_lite::parsers::kilo::KiloReader;

fn ingest(reader: &mut KiloReader, row: &str, session: &str, value: serde_json::Value) {
    reader
        .ingest(row, session, &serde_json::to_vec(&value).unwrap())
        .unwrap();
}

fn payload() -> serde_json::Value {
    json!({
        "role": "assistant",
        "modelID": "minimax/m2.5",
        "providerID": "kilo",
        "tokens": {
            "input": 1000,
            "output": 200,
            "reasoning": 40,
            "cache": {"read": 75, "write": 25}
        },
        "time": {"created": 1_794_564_000_000_i64}
    })
}

#[test]
fn assistant_message_splits_reasoning_and_uses_payload_session() {
    let mut reader = KiloReader::new();
    let mut value = payload();
    value["session_id"] = json!("payload-session");
    ingest(&mut reader, "row-1", "row-session", value);
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key.client, "kilo");
    assert_eq!(rows[0].key.session, "payload-session");
    assert_eq!(rows[0].model, "minimax/m2.5");
    assert_eq!(rows[0].billing_provider_hint.as_deref(), Some("kilo"));
    assert_eq!(
        rows[0].dedup_key.as_deref(),
        Some("kilo:payload-session:row-1")
    );
    assert_eq!(
        rows[0].tokens,
        TokenCounts {
            input: 1000,
            output: 160,
            cache_read: 75,
            cache_write: 25,
            reasoning: 40,
        }
    );
}

#[test]
fn fallback_session_and_timestamp_are_used_when_payload_omits_them() {
    let mut reader = KiloReader::new().with_fallback_timestamp(Some(1_700_000_000_000));
    let mut value = payload();
    value.as_object_mut().unwrap().remove("time");
    ingest(&mut reader, "row-2", "row-session", value);
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key.session, "row-session");
    assert_eq!(rows[0].timestamp_ms, 1_700_000_000_000);
}

#[test]
fn zero_rows_and_missing_cache_are_handled() {
    let mut reader = KiloReader::new();
    let zero = json!({
        "role": "assistant",
        "modelID": "legacy",
        "providerID": "legacy",
        "tokens": {
            "input": 0,
            "output": 0,
            "reasoning": 0,
            "cache": {"read": 0, "write": 0}
        },
        "time": {"created": 1_794_564_000_000_i64}
    });
    ingest(&mut reader, "stub", "ses_migrated_x", zero);
    assert!(reader.snapshot().is_empty());

    let mut missing = payload();
    missing["tokens"].as_object_mut().unwrap().remove("cache");
    let result = reader.ingest("row-3", "session", &serde_json::to_vec(&missing).unwrap());
    assert!(matches!(result, Err(ReaderError::Unsupported(_))));
}

#[test]
fn negative_or_reasoning_over_output_is_visible_unsupported() {
    let mut reader = KiloReader::new();
    let mut negative = payload();
    negative["tokens"]["input"] = json!(-1);
    assert!(matches!(
        reader.ingest("row-4", "session", &serde_json::to_vec(&negative).unwrap()),
        Err(ReaderError::InvalidRecord)
    ));

    let mut over = payload();
    over["tokens"]["output"] = json!(10);
    over["tokens"]["reasoning"] = json!(20);
    assert!(matches!(
        reader.ingest("row-5", "session", &serde_json::to_vec(&over).unwrap()),
        Err(ReaderError::Unsupported(_))
    ));
}
