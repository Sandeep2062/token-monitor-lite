//! Synthetic Qwen CLI cases adapted from the pinned Tokscale `qwen.rs` reader.

use serde_json::json;
use token_monitor_lite::accounting::TokenCounts;
use token_monitor_lite::parsers::qwen::QwenReader;

fn ingest(reader: &mut QwenReader, value: serde_json::Value) {
    reader.ingest(&serde_json::to_vec(&value).unwrap()).unwrap();
}

#[test]
fn prompt_cache_is_split_and_reported_total_splits_reasoning() {
    let mut reader = QwenReader::new("fallback");
    ingest(
        &mut reader,
        json!({
            "type": "assistant",
            "sessionId": "qwen-session",
            "model": "qwen3-coder",
            "timestamp": "2026-11-13T10:00:00Z",
            "usageMetadata": {
                "promptTokenCount": 100,
                "candidatesTokenCount": 20,
                "thoughtsTokenCount": 5,
                "cachedContentTokenCount": 30,
                "totalTokenCount": 120
            }
        }),
    );
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key.client, "qwen");
    assert_eq!(rows[0].model, "qwen3-coder");
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
fn separate_thoughts_stay_additive_when_the_total_proves_it() {
    let mut reader = QwenReader::new("fallback");
    ingest(
        &mut reader,
        json!({
            "type": "assistant",
            "timestamp": "2026-11-13T10:00:00Z",
            "usageMetadata": {
                "promptTokenCount": 10,
                "candidatesTokenCount": 4,
                "thoughtsTokenCount": 2,
                "totalTokenCount": 16
            }
        }),
    );
    let tokens = reader.snapshot()[0].tokens;
    assert_eq!(tokens.output, 4);
    assert_eq!(tokens.reasoning, 2);
}

#[test]
fn non_assistant_and_zero_rows_are_ignored() {
    let mut reader = QwenReader::new("fallback");
    ingest(
        &mut reader,
        json!({
            "type": "user",
            "usageMetadata": {"promptTokenCount": 10}
        }),
    );
    ingest(
        &mut reader,
        json!({
            "type": "assistant",
            "usageMetadata": {"promptTokenCount": 0}
        }),
    );
    assert!(reader.snapshot().is_empty());
}

#[test]
fn cached_content_above_prompt_is_kept_as_reported() {
    let mut reader = QwenReader::new("fallback");
    ingest(
        &mut reader,
        json!({
            "type": "assistant",
            "timestamp": "2026-11-13T10:00:00Z",
            "usageMetadata": {
                "promptTokenCount": 5,
                "candidatesTokenCount": 1,
                "cachedContentTokenCount": 9
            }
        }),
    );
    let tokens = reader.snapshot()[0].tokens;
    assert_eq!(tokens.input, 5);
    assert_eq!(tokens.cache_read, 9);
}
