//! Synthetic Cline CLI/Desktop session-message cases adapted from the pinned
//! Tokscale `cline.rs` reader. The VS Code task-log format is not implemented.

use serde_json::json;
use token_monitor_lite::accounting::TokenCounts;
use token_monitor_lite::parsers::ReaderError;
use token_monitor_lite::parsers::cline::ClineReader;

fn ingest(reader: &mut ClineReader, value: serde_json::Value) {
    reader.ingest(&serde_json::to_vec(&value).unwrap()).unwrap();
}

#[test]
fn assistant_metrics_are_split_from_gross_input_and_keep_model_and_agent() {
    let mut reader = ClineReader::new("fallback");
    ingest(
        &mut reader,
        json!({
            "sessionId": "cline-session",
            "agent": "main",
            "messages": [
                {"id": "u1", "role": "user", "ts": 1_763_000_000_000_i64},
                {
                    "id": "a1",
                    "role": "assistant",
                    "ts": 1_763_000_001_000_i64,
                    "modelInfo": {"id": "claude-sonnet-4-5", "provider": "anthropic"},
                    "metrics": {
                        "inputTokens": 100,
                        "outputTokens": 20,
                        "cacheReadTokens": 10,
                        "cacheWriteTokens": 5
                    }
                },
                {
                    "id": "a2",
                    "role": "assistant",
                    "ts": "2026-11-13T10:00:02Z",
                    "metrics": {"inputTokens": 50, "outputTokens": 10}
                }
            ]
        }),
    );
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].key.client, "cline");
    assert_eq!(rows[0].key.session, "cline-session");
    assert_eq!(rows[0].model, "claude-sonnet-4-5");
    assert_eq!(rows[0].billing_provider_hint.as_deref(), Some("anthropic"));
    assert_eq!(rows[0].agent.as_deref(), Some("main"));
    assert_eq!(
        rows[0].dedup_key.as_deref(),
        Some("cline-cli:cline-session:a1")
    );
    assert_eq!(
        rows[0].tokens,
        TokenCounts {
            input: 85,
            output: 20,
            cache_read: 10,
            cache_write: 5,
            reasoning: 0,
        }
    );
    assert_eq!(
        rows[1].dedup_key.as_deref(),
        Some("cline-cli:cline-session:a2")
    );
    assert_eq!(rows[1].tokens.input, 50);
    assert_eq!(rows[1].tokens.output, 10);
}

#[test]
fn zero_usage_and_non_assistant_rows_are_ignored() {
    let mut reader = ClineReader::new("cline-session");
    ingest(
        &mut reader,
        json!({
            "messages": [
                {"id": "u1", "role": "user", "metrics": {"inputTokens": 9}},
                {
                    "id": "a1",
                    "role": "assistant",
                    "ts": 1_763_000_000_000_i64,
                    "modelInfo": {"id": "m"},
                    "metrics": {"inputTokens": 0, "outputTokens": 0}
                }
            ]
        }),
    );
    assert!(reader.snapshot().is_empty());
}

#[test]
fn cache_greater_than_input_is_visible_unsupported() {
    let mut reader = ClineReader::new("cline-session");
    let result = reader.ingest(
        &serde_json::to_vec(&json!({
            "messages": [{
                "id": "a1",
                "role": "assistant",
                "ts": 1_763_000_000_000_i64,
                "modelInfo": {"id": "m"},
                "metrics": {
                    "inputTokens": 5,
                    "outputTokens": 1,
                    "cacheReadTokens": 10,
                    "cacheWriteTokens": 0
                }
            }]
        }))
        .unwrap(),
    );
    assert!(matches!(result, Err(ReaderError::Unsupported(_))));
}

#[test]
fn missing_messages_is_visible_unsupported() {
    let mut reader = ClineReader::new("cline-session");
    let result = reader.ingest(&serde_json::to_vec(&json!({"sessionId": "x"})).unwrap());
    assert!(matches!(result, Err(ReaderError::Unsupported(_))));
}
