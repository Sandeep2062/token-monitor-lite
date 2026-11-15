//! Synthetic Zed hosted-thread cases adapted from the pinned Tokscale
//! `zed.rs` reader.

use serde_json::json;
use token_monitor_lite::accounting::TokenCounts;
use token_monitor_lite::parsers::ReaderError;
use token_monitor_lite::parsers::zed::ZedReader;

fn thread() -> serde_json::Value {
    json!({
        "model": {"provider": "zed.dev", "model": "claude-sonnet-4-6"},
        "request_token_usage": {
            "0": {
                "input_tokens": 10,
                "output_tokens": 5,
                "cache_read_input_tokens": 2,
                "cache_creation_input_tokens": 1
            },
            "1": {
                "input_tokens": 4,
                "output_tokens": 2,
                "cache_read_input_tokens": 0,
                "cache_creation_input_tokens": 0
            }
        }
    })
}

#[test]
fn request_usage_sums_and_hosted_provider_is_carried() {
    let mut reader = ZedReader::new();
    reader
        .ingest_thread(
            "thread-1",
            "2026-11-13T10:00:00Z",
            Some("2026-11-13T09:00:00Z"),
            &thread(),
        )
        .unwrap();
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key.client, "zed");
    assert_eq!(rows[0].key.session, "thread-1");
    assert_eq!(rows[0].model, "claude-sonnet-4-6");
    assert_eq!(rows[0].dedup_key.as_deref(), Some("zed:thread-1"));
    assert_eq!(
        rows[0].tokens,
        TokenCounts {
            input: 14,
            output: 7,
            cache_read: 2,
            cache_write: 1,
            reasoning: 0,
        }
    );
}

#[test]
fn cumulative_usage_is_the_fallback() {
    let mut reader = ZedReader::new();
    reader
        .ingest_thread(
            "thread-2",
            "2026-11-13T10:00:00Z",
            None,
            &json!({
                "model": {"provider": "zed.dev", "model": "m"},
                "cumulative_token_usage": {"input_tokens": 3, "output_tokens": 2}
            }),
        )
        .unwrap();
    assert_eq!(reader.snapshot()[0].tokens.input, 3);
}

#[test]
fn external_and_imported_threads_are_ignored() {
    let mut reader = ZedReader::new();
    reader
        .ingest_thread(
            "external",
            "2026-11-13T10:00:00Z",
            None,
            &json!({
                "model": {"provider": "anthropic", "model": "m"},
                "request_token_usage": {"0": {"input_tokens": 3}}
            }),
        )
        .unwrap();
    reader
        .ingest_thread(
            "imported",
            "2026-11-13T10:00:00Z",
            None,
            &json!({
                "imported": true,
                "model": {"provider": "zed.dev", "model": "m"},
                "request_token_usage": {"0": {"input_tokens": 3}}
            }),
        )
        .unwrap();
    assert!(reader.snapshot().is_empty());
}

#[test]
fn missing_usage_is_visible_unsupported() {
    let mut reader = ZedReader::new();
    let result = reader.ingest_thread(
        "thread-3",
        "2026-11-13T10:00:00Z",
        None,
        &json!({"model": {"provider": "zed.dev", "model": "m"}}),
    );
    assert!(matches!(result, Err(ReaderError::Unsupported(_))));
}
