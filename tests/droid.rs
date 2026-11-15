//! Synthetic Droid (Factory.ai) session cases adapted from the pinned Tokscale
//! `droid.rs` reader. Droid records one cumulative `tokenUsage` block per
//! session in `<uuid>.settings.json`; the native reader reports it once,
//! anchored at the last write, instead of apportioning it across transcript
//! replies.

use token_monitor_lite::accounting::TokenCounts;
use token_monitor_lite::parsers::ReaderError;
use token_monitor_lite::parsers::droid::DroidReader;

fn reader() -> DroidReader {
    DroidReader::new("session-a").with_fallback_timestamp(Some(1_700_000_000_000))
}

fn ingest(reader: &mut DroidReader, document: &str) {
    reader.ingest(document.as_bytes()).unwrap();
}

#[test]
fn cumulative_usage_becomes_one_observation() {
    let mut reader = reader();
    ingest(
        &mut reader,
        r#"{
            "model": "claude-sonnet-4-5",
            "providerLock": "anthropic",
            "tokenUsage": {
                "inputTokens": 100,
                "outputTokens": 50,
                "cacheCreationTokens": 10,
                "cacheReadTokens": 20,
                "thinkingTokens": 5
            }
        }"#,
    );
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key.client, "droid");
    assert_eq!(rows[0].key.session, "session-a");
    assert_eq!(rows[0].model, "claude-sonnet-4-5");
    assert_eq!(rows[0].billing_provider_hint.as_deref(), Some("anthropic"));
    assert_eq!(rows[0].dedup_key.as_deref(), Some("droid:session-a"));
    assert_eq!(
        rows[0].tokens,
        TokenCounts {
            input: 100,
            output: 50,
            cache_read: 20,
            cache_write: 10,
            reasoning: 5,
        }
    );
}

#[test]
fn custom_model_spelling_is_normalized() {
    let mut reader = reader();
    ingest(
        &mut reader,
        r#"{
            "model": "custom:Claude-Opus-4.5-Thinking-[Anthropic]-0",
            "tokenUsage": {"inputTokens": 1, "outputTokens": 1}
        }"#,
    );
    let rows = reader.snapshot();
    assert_eq!(rows[0].model, "claude-opus-4-5-thinking-0");
    // No providerLock: the model family names the provider.
    assert_eq!(rows[0].billing_provider_hint.as_deref(), Some("anthropic"));
}

#[test]
fn provider_lock_wins_over_the_model_family() {
    let mut reader = reader();
    ingest(
        &mut reader,
        r#"{
            "model": "gpt-5",
            "providerLock": "factory",
            "tokenUsage": {"inputTokens": 1, "outputTokens": 1}
        }"#,
    );
    assert_eq!(
        reader.snapshot()[0].billing_provider_hint.as_deref(),
        Some("factory")
    );
}

#[test]
fn unknown_model_family_falls_back_to_unknown_provider() {
    let mut reader = reader();
    ingest(
        &mut reader,
        r#"{"model": "mystery-model", "tokenUsage": {"inputTokens": 1, "outputTokens": 1}}"#,
    );
    assert_eq!(
        reader.snapshot()[0].billing_provider_hint.as_deref(),
        Some("unknown")
    );
}

#[test]
fn missing_or_all_zero_usage_is_not_an_observation() {
    let mut reader = reader();
    ingest(&mut reader, r#"{"model": "claude-sonnet-4-5"}"#);
    ingest(
        &mut reader,
        r#"{"model": "claude-sonnet-4-5", "tokenUsage": {"inputTokens": 0, "outputTokens": 0}}"#,
    );
    assert!(reader.snapshot().is_empty());
    assert!(!reader.has_supported_usage());
}

#[test]
fn negative_counters_are_rejected_rather_than_clamped() {
    let mut reader = reader();
    assert!(matches!(
        reader.ingest(br#"{"model": "m", "tokenUsage": {"inputTokens": -1, "outputTokens": 5}}"#),
        Err(ReaderError::InvalidRecord)
    ));
    assert!(reader.snapshot().is_empty());
}

#[test]
fn missing_model_is_visible_unsupported() {
    let mut reader = reader();
    let result = reader.ingest(br#"{"tokenUsage": {"inputTokens": 10, "outputTokens": 5}}"#);
    assert!(matches!(result, Err(ReaderError::Unsupported(_))));
}

#[test]
fn malformed_document_is_rejected() {
    let mut reader = reader();
    assert!(matches!(
        reader.ingest(b"not json"),
        Err(ReaderError::InvalidRecord)
    ));
}

#[test]
fn provider_lock_timestamp_floors_the_file_mtime() {
    // A rewound mtime cannot drag the record earlier than provider selection.
    let mut reader = DroidReader::new("session-b").with_fallback_timestamp(Some(1_000));
    ingest(
        &mut reader,
        r#"{
            "model": "claude-sonnet-4-5",
            "providerLockTimestamp": "2023-11-14T22:13:20Z",
            "tokenUsage": {"inputTokens": 1, "outputTokens": 1}
        }"#,
    );
    assert_eq!(reader.snapshot()[0].timestamp_ms, 1_700_000_000_000);

    // A later mtime wins over an older lock.
    let mut reader = DroidReader::new("session-c").with_fallback_timestamp(Some(2_000_000_000_000));
    ingest(
        &mut reader,
        r#"{
            "model": "claude-sonnet-4-5",
            "providerLockTimestamp": "2023-11-14T22:13:20Z",
            "tokenUsage": {"inputTokens": 1, "outputTokens": 1}
        }"#,
    );
    assert_eq!(reader.snapshot()[0].timestamp_ms, 2_000_000_000_000);
}

#[test]
fn unusable_timestamp_is_visible_unsupported() {
    let mut reader = DroidReader::new("session-d");
    let result =
        reader.ingest(br#"{"model": "m", "tokenUsage": {"inputTokens": 1, "outputTokens": 1}}"#);
    assert!(matches!(result, Err(ReaderError::Unsupported(_))));
}

#[test]
fn prompt_text_never_reaches_the_identity() {
    let mut reader = reader();
    ingest(
        &mut reader,
        r#"{
            "model": "m",
            "tokenUsage": {"inputTokens": 1, "outputTokens": 1},
            "messages": [{"role": "user", "content": "SECRET PROMPT"}]
        }"#,
    );
    let key = reader.snapshot()[0].dedup_key.clone().unwrap();
    assert!(!key.contains("SECRET"));
}
