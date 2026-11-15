//! Synthetic CodeBuddy JSONL cases adapted from the pinned Tokscale
//! `tencent_buddy.rs`/`codebuddy.rs` readers. Extension log sinks are not read.

use serde_json::json;
use token_monitor_lite::accounting::TokenCounts;
use token_monitor_lite::parsers::ReaderError;
use token_monitor_lite::parsers::codebuddy::{CodeBuddyLogReader, CodeBuddyReader};

fn ingest(reader: &mut CodeBuddyReader, value: serde_json::Value) {
    reader.ingest(&serde_json::to_vec(&value).unwrap()).unwrap();
}

#[test]
fn assistant_message_splits_cache_inclusive_input() {
    let mut reader = CodeBuddyReader::new("codebuddy", "codebuddy", "fallback");
    ingest(
        &mut reader,
        json!({
            "id": "assistant-1",
            "timestamp": 1_780_000_000_100_i64,
            "type": "message",
            "role": "assistant",
            "status": "completed",
            "sessionId": "session-1",
            "providerData": {"model": "glm-5.2", "messageId": "msg-1"},
            "message": {"usage": {
                "input_tokens": 24486,
                "output_tokens": 3,
                "total_tokens": 24489,
                "cache_read_input_tokens": 14720
            }}
        }),
    );
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key.client, "codebuddy");
    assert_eq!(rows[0].key.session, "session-1");
    assert_eq!(rows[0].model, "glm-5.2");
    assert_eq!(
        rows[0].dedup_key.as_deref(),
        Some("codebuddy:session-1:msg-1")
    );
    assert_eq!(
        rows[0].tokens,
        TokenCounts {
            input: 9766,
            output: 3,
            cache_read: 14720,
            cache_write: 0,
            reasoning: 0,
        }
    );
}

#[test]
fn function_call_usage_uses_request_model_and_raw_total() {
    let mut reader = CodeBuddyReader::new("codebuddy", "codebuddy", "fallback");
    ingest(
        &mut reader,
        json!({
            "id": "call-1",
            "timestamp": 1_780_000_000_100_i64,
            "type": "function_call",
            "sessionId": "session-2",
            "providerData": {
                "requestModelId": "minimax-m3-pay",
                "messageId": "msg-2",
                "rawUsage": {
                    "prompt_tokens": 10,
                    "completion_tokens": 2,
                    "prompt_cache_hit_tokens": 3
                }
            }
        }),
    );
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].model, "minimax-m3-pay");
    assert_eq!(rows[0].tokens.input, 10);
    assert_eq!(rows[0].tokens.output, 2);
    assert_eq!(rows[0].tokens.cache_read, 3);
}

#[test]
fn incomplete_status_and_negative_counters_are_not_usage() {
    let mut reader = CodeBuddyReader::new("codebuddy", "codebuddy", "fallback");
    ingest(
        &mut reader,
        json!({
            "type": "message",
            "role": "assistant",
            "status": "streaming",
            "sessionId": "session-1",
            "message": {"usage": {"input_tokens": 5}}
        }),
    );
    assert!(reader.snapshot().is_empty());

    let result = reader.ingest(
        &serde_json::to_vec(&json!({
            "type": "message",
            "role": "assistant",
            "timestamp": 1,
            "sessionId": "session-1",
            "message": {
                "model": "m",
                "usage": {"input_tokens": -1, "output_tokens": 2}
            }
        }))
        .unwrap(),
    );
    assert!(matches!(result, Err(ReaderError::InvalidRecord)));
}

#[test]
fn duplicate_message_identity_keeps_the_largest_total() {
    let mut reader = CodeBuddyReader::new("codebuddy", "codebuddy", "fallback");
    for input in [4, 9] {
        ingest(
            &mut reader,
            json!({
                "timestamp": 1,
                "type": "message",
                "role": "assistant",
                "sessionId": "session-1",
                "providerData": {"messageId": "same"},
                "message": {"model": "m", "usage": {"input_tokens": input}}
            }),
        );
    }
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].tokens.input, 9);
}

#[test]
fn extension_log_settlement_uses_the_prepared_model() {
    let mut reader = CodeBuddyLogReader::new("codebuddy", "codebuddy", "log");
    let log = concat!(
        "[2026/7/1 16:56:01.100] [Info] [CraftInvokableAgent] [agent-1]  Model prepared: Kimi-K2.7-Code (kimi-k2.7)
",
        "[2026/7/1 16:56:02.200] [Info] [AgentReporter] [agent-1]  Agent execution successful with usage: {\"inputTokens\":140732,\"outputTokens\":635,\"totalTokens\":141367,\"cacheTokens\":76032,\"cachedWriteTokens\":0,\"cachedMissTokens\":64700}
"
    );
    reader.ingest(log.as_bytes()).unwrap();
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key.client, "codebuddy");
    assert_eq!(rows[0].key.session, "agent-1");
    assert_eq!(rows[0].model, "kimi-k2.7");
    assert_eq!(rows[0].tokens.input, 64700);
    assert_eq!(rows[0].tokens.output, 635);
    assert_eq!(rows[0].tokens.cache_read, 76032);
    assert_eq!(rows[0].agent.as_deref(), Some("agent-1"));
}
