//! Synthetic Kimi CLI `wire.jsonl` cases adapted from the pinned Tokscale
//! `kimi.rs` reader. Usage lives on `StatusUpdate` messages only; the wire
//! carries no model id, so the documented default is used.

use token_monitor_lite::accounting::TokenCounts;
use token_monitor_lite::parsers::ReaderError;
use token_monitor_lite::parsers::kimi::{DEFAULT_MODEL, DEFAULT_PROVIDER, KimiReader};

fn new_reader() -> KimiReader {
    KimiReader::new("session-a").with_fallback_timestamp(Some(1_700_000_000_000))
}

fn ingest(reader: &mut KimiReader, line: &str) {
    reader.ingest(line.as_bytes()).unwrap();
}

#[test]
fn status_update_becomes_one_observation() {
    let mut reader = new_reader();
    ingest(
        &mut reader,
        r#"{"timestamp":1770983426.420942,"message":{"type":"StatusUpdate","payload":{"token_usage":{"input_other":1562,"output":2463,"input_cache_read":0,"input_cache_creation":0},"message_id":"chatcmpl-xxx"}}}"#,
    );
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key.client, "kimi");
    assert_eq!(rows[0].key.session, "session-a");
    assert_eq!(rows[0].model, DEFAULT_MODEL);
    assert_eq!(
        rows[0].billing_provider_hint.as_deref(),
        Some(DEFAULT_PROVIDER)
    );
    assert_eq!(rows[0].dedup_key.as_deref(), Some("kimi:chatcmpl-xxx"));
    assert_eq!(rows[0].timestamp_ms, 1_770_983_426_420);
    assert_eq!(
        rows[0].tokens,
        TokenCounts {
            input: 1562,
            output: 2463,
            cache_read: 0,
            cache_write: 0,
            reasoning: 0,
        }
    );
}

#[test]
fn camel_case_aliases_are_accepted_for_kimi_code_records() {
    let mut reader = new_reader();
    ingest(
        &mut reader,
        r#"{"timestamp":1770983426.0,"message":{"type":"StatusUpdate","payload":{"token_usage":{"inputOther":10,"output":5,"inputCacheRead":3,"inputCacheCreation":2}}}}"#,
    );
    let rows = reader.snapshot();
    assert_eq!(
        rows[0].tokens,
        TokenCounts {
            input: 10,
            output: 5,
            cache_read: 3,
            cache_write: 2,
            reasoning: 0,
        }
    );
    // No message_id, so no identity.
    assert!(rows[0].dedup_key.is_none());
}

#[test]
fn non_status_update_lines_carry_no_usage() {
    let mut reader = new_reader();
    ingest(
        &mut reader,
        r#"{"type":"metadata","protocol_version":"1.3"}"#,
    );
    ingest(
        &mut reader,
        r#"{"timestamp":1770983400.0,"message":{"type":"TurnBegin","payload":{"user_input":"hello"}}}"#,
    );
    ingest(
        &mut reader,
        r#"{"timestamp":1770983420.0,"message":{"type":"ToolCall","payload":{"type":"function","id":"tool_1"}}}"#,
    );
    ingest(
        &mut reader,
        r#"{"timestamp":1770983430.0,"message":{"type":"ContentPart","payload":{"type":"text","text":"hi"}}}"#,
    );
    assert!(reader.snapshot().is_empty());
}

#[test]
fn a_duplicate_message_id_keeps_the_larger_total() {
    let mut reader = new_reader();
    ingest(
        &mut reader,
        r#"{"timestamp":1770983410.0,"message":{"type":"StatusUpdate","payload":{"token_usage":{"input_other":100,"output":200},"message_id":"msg-1"}}}"#,
    );
    // The same call reported again with a larger count wins.
    ingest(
        &mut reader,
        r#"{"timestamp":1770983415.0,"message":{"type":"StatusUpdate","payload":{"token_usage":{"input_other":150,"output":250},"message_id":"msg-1"}}}"#,
    );
    assert_eq!(reader.snapshot().len(), 1);
    assert_eq!(reader.snapshot()[0].tokens.input, 150);

    // A smaller duplicate does not replace it.
    ingest(
        &mut reader,
        r#"{"timestamp":1770983419.0,"message":{"type":"StatusUpdate","payload":{"token_usage":{"input_other":1,"output":1},"message_id":"msg-1"}}}"#,
    );
    assert_eq!(reader.snapshot()[0].tokens.input, 150);
}

#[test]
fn distinct_message_ids_stay_separate() {
    let mut reader = new_reader();
    ingest(
        &mut reader,
        r#"{"timestamp":1770983410.0,"message":{"type":"StatusUpdate","payload":{"token_usage":{"input_other":100,"output":200},"message_id":"msg-1"}}}"#,
    );
    ingest(
        &mut reader,
        r#"{"timestamp":1770983430.0,"message":{"type":"StatusUpdate","payload":{"token_usage":{"input_other":300,"output":400,"input_cache_read":50},"message_id":"msg-2"}}}"#,
    );
    assert_eq!(reader.snapshot().len(), 2);
    assert_eq!(reader.snapshot()[1].tokens.input, 300);
    assert_eq!(reader.snapshot()[1].tokens.cache_read, 50);
}

#[test]
fn all_zero_usage_is_skipped() {
    let mut reader = new_reader();
    ingest(
        &mut reader,
        r#"{"timestamp":1770983410.0,"message":{"type":"StatusUpdate","payload":{"token_usage":{"input_other":0,"output":0},"message_id":"msg-0"}}}"#,
    );
    assert!(reader.snapshot().is_empty());
    assert!(!reader.has_supported_usage());
}

#[test]
fn negative_and_non_integer_counters_are_rejected() {
    let mut reader = new_reader();
    assert!(matches!(
        reader.ingest(
            br#"{"timestamp":1770983410.0,"message":{"type":"StatusUpdate","payload":{"token_usage":{"input_other":-1,"output":5}}}}"#
        ),
        Err(ReaderError::InvalidRecord)
    ));
    assert!(matches!(
        reader.ingest(
            br#"{"timestamp":1770983410.0,"message":{"type":"StatusUpdate","payload":{"token_usage":{"input_other":1.5,"output":5}}}}"#
        ),
        Err(ReaderError::InvalidRecord)
    ));
    assert!(reader.snapshot().is_empty());
}

#[test]
fn unusable_wire_timestamp_falls_back_to_the_file_mtime() {
    let mut reader = new_reader();
    ingest(
        &mut reader,
        r#"{"message":{"type":"StatusUpdate","payload":{"token_usage":{"input_other":10,"output":5}}}}"#,
    );
    assert_eq!(reader.snapshot()[0].timestamp_ms, 1_700_000_000_000);

    // A pre-epoch value is not an instant.
    let mut reader = new_reader();
    ingest(
        &mut reader,
        r#"{"timestamp":-1.5,"message":{"type":"StatusUpdate","payload":{"token_usage":{"input_other":10,"output":5}}}}"#,
    );
    assert_eq!(reader.snapshot()[0].timestamp_ms, 1_700_000_000_000);
}

#[test]
fn no_usable_timestamp_anywhere_is_visible_unsupported() {
    let mut reader = KimiReader::new("session-b");
    let result = reader.ingest(
        br#"{"message":{"type":"StatusUpdate","payload":{"token_usage":{"input_other":10,"output":5}}}}"#,
    );
    assert!(matches!(
        result,
        Err(ReaderError::Unsupported(_) | ReaderError::Declined(_))
    ));
}

#[test]
fn malformed_lines_are_rejected() {
    let mut reader = new_reader();
    assert!(matches!(
        reader.ingest(b"not json"),
        Err(ReaderError::InvalidRecord)
    ));
}

#[test]
fn prompt_text_never_reaches_the_identity() {
    let mut reader = new_reader();
    ingest(
        &mut reader,
        r#"{"timestamp":1770983410.0,"message":{"type":"StatusUpdate","payload":{"token_usage":{"input_other":10,"output":5},"message_id":"m1","content":"SECRET PROMPT"}}}"#,
    );
    let key = reader.snapshot()[0].dedup_key.clone().unwrap();
    assert!(!key.contains("SECRET"));
}
