//! Synthetic WorkBuddy JSONL cases adapted from the pinned Tokscale
//! `tencent_buddy.rs`/`workbuddy.rs` readers. The aggregate SQLite fallback is
//! not read.

use serde_json::json;
use token_monitor_lite::parsers::workbuddy::WorkBuddyReader;

fn ingest(reader: &mut WorkBuddyReader, value: serde_json::Value) {
    reader.ingest(&serde_json::to_vec(&value).unwrap()).unwrap();
}

#[test]
fn function_call_usage_proves_inclusive_prompt_tokens() {
    let mut reader = WorkBuddyReader::new("workbuddy", "workbuddy", "fallback");
    ingest(
        &mut reader,
        json!({
            "id": "call-1",
            "timestamp": 1_780_000_000_100_i64,
            "type": "function_call",
            "sessionId": "session-1",
            "providerData": {
                "requestModelId": "glm-5.2",
                "messageId": "msg-1",
                "rawUsage": {
                    "prompt_tokens": 140732,
                    "completion_tokens": 635,
                    "total_tokens": 141367,
                    "prompt_cache_hit_tokens": 76032
                }
            }
        }),
    );
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key.client, "workbuddy");
    assert_eq!(rows[0].model, "glm-5.2");
    assert_eq!(rows[0].tokens.input, 64700);
    assert_eq!(rows[0].tokens.output, 635);
    assert_eq!(rows[0].tokens.cache_read, 76032);
}

#[test]
fn message_usage_without_session_uses_the_file_session() {
    let mut reader = WorkBuddyReader::new("workbuddy", "workbuddy", "file-session");
    ingest(
        &mut reader,
        json!({
            "id": "a1",
            "timestamp": 1_780_000_000_000_i64,
            "type": "message",
            "role": "assistant",
            "message": {
                "model": "deepseek-v4-pro",
                "usage": {"input_tokens": 20, "output_tokens": 4}
            }
        }),
    );
    let rows = reader.snapshot();
    assert_eq!(rows[0].key.session, "file-session");
    assert_eq!(
        rows[0].dedup_key.as_deref(),
        Some("workbuddy:file-session:a1")
    );
}

#[test]
fn cached_miss_tokens_already_exclude_cache_read() {
    let mut reader = WorkBuddyReader::new("workbuddy", "workbuddy", "fallback");
    ingest(
        &mut reader,
        json!({
            "timestamp": 1,
            "type": "message",
            "role": "assistant",
            "sessionId": "s",
            "message": {
                "model": "m",
                "usage": {
                    "inputTokens": 500,
                    "outputTokens": 10,
                    "cacheTokens": 100,
                    "cachedMissTokens": 400
                }
            }
        }),
    );
    let rows = reader.snapshot();
    assert_eq!(rows[0].tokens.input, 400);
    assert_eq!(rows[0].tokens.cache_read, 100);
}
