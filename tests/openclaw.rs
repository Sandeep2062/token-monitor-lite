//! Synthetic OpenClaw transcript cases adapted from the pinned Tokscale
//! `openclaw.rs` reader. OpenClaw writes assistant events whose
//! `reasoningTokens` is a subset of `output`, and its own transcript-bookkeeping
//! rows are not model output.

use serde_json::Value;
use token_monitor_lite::accounting::TokenCounts;
use token_monitor_lite::parsers::ReaderError;
use token_monitor_lite::parsers::openclaw::OpenClawReader;

fn reader() -> OpenClawReader {
    OpenClawReader::new("sess-a").with_fallback_timestamp(Some(1_700_000_000_000))
}

fn ingest(reader: &mut OpenClawReader, line: &str) {
    reader.ingest(line.as_bytes()).unwrap();
}

fn text(value: &Value) -> String {
    value.to_string()
}

#[test]
fn assistant_usage_splits_reasoning_out_of_inclusive_output() {
    let mut reader = reader();
    ingest(
        &mut reader,
        r#"{"type":"model_change","provider":"anthropic","modelId":"claude-sonnet-4-5"}"#,
    );
    ingest(
        &mut reader,
        r#"{"type":"message","id":"msg1","message":{"role":"assistant","usage":{"input":100,"output":50,"cacheRead":200,"cacheWrite":10,"reasoningTokens":20},"timestamp":1700000000000}}"#,
    );
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key.client, "openclaw");
    assert_eq!(rows[0].key.session, "sess-a");
    assert_eq!(rows[0].model, "claude-sonnet-4-5");
    assert_eq!(rows[0].billing_provider_hint.as_deref(), Some("anthropic"));
    assert_eq!(rows[0].timestamp_ms, 1_700_000_000_000);
    assert_eq!(
        rows[0].tokens,
        TokenCounts {
            input: 100,
            output: 30,
            cache_read: 200,
            cache_write: 10,
            reasoning: 20,
        }
    );
    assert_eq!(
        rows[0].dedup_key.as_deref(),
        Some("openclaw:msg1:1700000000000:100:30")
    );
}

#[test]
fn model_snapshot_events_feed_the_carried_forward_model() {
    let mut reader = reader();
    ingest(
        &mut reader,
        r#"{"type":"custom","customType":"model-snapshot","data":{"provider":"openai","modelId":"gpt-5.2"}}"#,
    );
    ingest(
        &mut reader,
        r#"{"type":"message","id":"m1","message":{"role":"assistant","usage":{"input":10,"output":5},"timestamp":1700000000000}}"#,
    );
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].model, "gpt-5.2");
    assert_eq!(rows[0].billing_provider_hint.as_deref(), Some("openai"));

    // Other `custom` subtypes are not model bookkeeping.
    ingest(
        &mut reader,
        r#"{"type":"custom","customType":"something-else","data":{"modelId":"ignored"}}"#,
    );
    assert_eq!(reader.snapshot().len(), 1);
}

#[test]
fn transcript_bookkeeping_rows_are_not_usage() {
    let mut reader = reader();
    ingest(
        &mut reader,
        r#"{"type":"message","id":"d1","message":{"role":"assistant","api":"openclaw-transcript","provider":"openclaw","model":"delivery-mirror","usage":{"input":0,"output":0},"timestamp":1700000000000}}"#,
    );
    ingest(
        &mut reader,
        r#"{"type":"message","id":"g1","message":{"role":"assistant","provider":"openclaw","model":"gateway-injected","usage":{"input":0,"output":0},"timestamp":1700000000000}}"#,
    );
    assert!(reader.snapshot().is_empty());
}

#[test]
fn user_and_tool_rows_are_ignored() {
    let mut reader = reader();
    ingest(
        &mut reader,
        r#"{"type":"message","id":"u1","message":{"role":"user","usage":{"input":9,"output":9}}}"#,
    );
    ingest(
        &mut reader,
        r#"{"type":"message","id":"t1","message":{"role":"toolResult","usage":{"input":5,"output":5}}}"#,
    );
    assert!(reader.snapshot().is_empty());
}

#[test]
fn assistant_row_without_a_model_is_visible_unsupported() {
    let mut reader = OpenClawReader::new("sess-a");
    let result = reader.ingest(
        br#"{"type":"message","id":"m1","message":{"role":"assistant","usage":{"input":10,"output":5},"timestamp":1700000000000}}"#,
    );
    assert!(matches!(result, Err(ReaderError::Unsupported(_))));
}

#[test]
fn missing_provider_falls_back_to_unknown() {
    let mut reader = reader();
    ingest(
        &mut reader,
        r#"{"type":"model_change","modelId":"claude-sonnet-4-5"}"#,
    );
    ingest(
        &mut reader,
        r#"{"type":"message","id":"m1","message":{"role":"assistant","usage":{"input":10,"output":5},"timestamp":1700000000000}}"#,
    );
    assert_eq!(
        reader.snapshot()[0].billing_provider_hint.as_deref(),
        Some("unknown")
    );
}

#[test]
fn negative_and_fractional_counters_are_rejected() {
    let mut reader = reader();
    assert!(matches!(
        reader.ingest(
            br#"{"type":"message","id":"m1","message":{"role":"assistant","model":"m","usage":{"input":-1,"output":5},"timestamp":1700000000000}}"#
        ),
        Err(ReaderError::InvalidRecord)
    ));
    assert!(matches!(
        reader.ingest(
            br#"{"type":"message","id":"m2","message":{"role":"assistant","model":"m","usage":{"input":1.5,"output":5},"timestamp":1700000000000}}"#
        ),
        Err(ReaderError::InvalidRecord)
    ));
    assert!(reader.snapshot().is_empty());
}

#[test]
fn reasoning_above_output_is_visible_unsupported() {
    let mut reader = reader();
    let result = reader.ingest(
        br#"{"type":"message","id":"m1","message":{"role":"assistant","model":"m","usage":{"input":10,"output":5,"reasoningTokens":6},"timestamp":1700000000000}}"#,
    );
    assert!(matches!(result, Err(ReaderError::Unsupported(_))));
}

#[test]
fn all_zero_usage_rows_are_skipped() {
    let mut reader = reader();
    ingest(
        &mut reader,
        r#"{"type":"message","id":"m1","message":{"role":"assistant","model":"m","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},"timestamp":1700000000000}}"#,
    );
    assert!(reader.snapshot().is_empty());
}

#[test]
fn event_without_a_timestamp_falls_back_to_the_caller_supplied_one() {
    let mut reader = reader();
    ingest(
        &mut reader,
        r#"{"type":"message","id":"m1","message":{"role":"assistant","model":"m","usage":{"input":10,"output":5}}}"#,
    );
    let rows = reader.snapshot();
    assert_eq!(rows[0].timestamp_ms, 1_700_000_000_000);
    // Keyed by id and usage alone: the supplied timestamp differs per store.
    assert_eq!(rows[0].dedup_key.as_deref(), Some("openclaw:m1:10:5"));
}

#[test]
fn codex_mirror_rows_are_keyed_by_thread_and_turn() {
    let mut reader = reader();
    ingest(
        &mut reader,
        r#"{"type":"message","id":"m1","message":{"role":"assistant","provider":"openai","model":"gpt-5.2-codex","usage":{"input":400,"output":300},"idempotencyKey":"codex-app-server:thread-1:turn-7:assistant","timestamp":1700000000000}}"#,
    );
    assert_eq!(
        reader.snapshot()[0].dedup_key.as_deref(),
        Some("openclaw:codex-mirror:thread-1:turn-7:m1:1700000000000:400:300")
    );

    // A mirror naming only the thread keeps an empty turn segment.
    let mut reader = OpenClawReader::new("sess-b");
    ingest(
        &mut reader,
        r#"{"type":"message","id":"m2","message":{"role":"assistant","provider":"openai","model":"gpt-5.2-codex","usage":{"input":7,"output":3},"idempotencyKey":"codex-app-server:thread-2","timestamp":1700000000000}}"#,
    );
    assert_eq!(
        reader.snapshot()[0].dedup_key.as_deref(),
        Some("openclaw:codex-mirror:thread-2::m2:1700000000000:7:3")
    );
}

#[test]
fn text_never_leaks_into_the_dedup_identity() {
    let mut reader = reader();
    ingest(
        &mut reader,
        &text(&serde_json::json!({
            "type": "message",
            "id": "m1",
            "message": {
                "role": "assistant",
                "model": "m",
                "content": [{"type": "text", "text": "SECRET PROMPT TEXT"}],
                "usage": {"input": 10, "output": 5},
                "timestamp": 1_700_000_000_000_i64,
            }
        })),
    );
    let key = reader.snapshot()[0].dedup_key.clone().unwrap();
    assert!(!key.contains("SECRET"));
}

#[test]
fn sqlite_session_events_reset_carried_model_on_a_session_change() {
    let mut reader = OpenClawReader::new("sess-a");
    let first: Value = serde_json::from_str(
        r#"{"type":"model_change","provider":"anthropic","modelId":"claude-opus-4-6"}"#,
    )
    .unwrap();
    reader
        .ingest_session_event("sess-a", Some(1), first.as_object().unwrap())
        .unwrap();
    let usage: Value = serde_json::from_str(
        r#"{"type":"message","id":"m1","message":{"role":"assistant","usage":{"input":10,"output":5}}}"#,
    )
    .unwrap();
    reader
        .ingest_session_event("sess-a", Some(1_000), usage.as_object().unwrap())
        .unwrap();
    assert_eq!(reader.snapshot()[0].model, "claude-opus-4-6");
    assert_eq!(reader.snapshot()[0].timestamp_ms, 1_000);

    // A second session must not inherit the first session's model.
    let result = reader.ingest_session_event("sess-b", Some(2_000), usage.as_object().unwrap());
    assert!(matches!(result, Err(ReaderError::Unsupported(_))));
}
