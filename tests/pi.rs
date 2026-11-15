//! Synthetic Pi-format cases adapted from the pinned Tokscale `pi.rs`,
//! `omp.rs` and `senpi.rs` readers.

use serde_json::json;
use token_monitor_lite::accounting::TokenCounts;
use token_monitor_lite::parsers::ReaderError;
use token_monitor_lite::parsers::pi::PiReader;

fn header(id: &str) -> serde_json::Value {
    json!({
        "type": "session",
        "version": 3,
        "id": id,
        "timestamp": "2026-08-20T15:27:35.811Z",
        "cwd": "/tmp/workspace"
    })
}

fn ingest(reader: &mut PiReader, value: serde_json::Value) {
    reader.ingest(&serde_json::to_vec(&value).unwrap()).unwrap();
}

#[test]
fn assistant_usage_keeps_reasoning_informational_and_carries_cache_write_1h() {
    let mut reader = PiReader::new("pi", "fallback");
    ingest(&mut reader, header("pi-session"));
    ingest(
        &mut reader,
        json!({
            "type": "message",
            "id": "b2",
            "timestamp": "2026-08-20T15:27:40.000Z",
            "message": {
                "role": "assistant",
                "model": "claude-opus-5",
                "provider": "anthropic",
                "usage": {
                    "input": 2,
                    "output": 49,
                    "cacheRead": 40625,
                    "cacheWrite": 332,
                    "cacheWrite1h": 12,
                    "reasoning": 16,
                    "totalTokens": 41008
                }
            }
        }),
    );
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key.client, "pi");
    assert_eq!(rows[0].key.session, "pi-session");
    assert_eq!(rows[0].model, "claude-opus-5");
    assert_eq!(rows[0].billing_provider_hint.as_deref(), Some("anthropic"));
    assert_eq!(rows[0].cache_write_1h, 12);
    assert_eq!(
        rows[0].tokens,
        TokenCounts {
            input: 2,
            output: 49,
            cache_read: 40625,
            cache_write: 332,
            reasoning: 0,
        }
    );
}

#[test]
fn model_change_attributes_a_model_less_usage_entry() {
    let mut reader = PiReader::new("pi", "fallback");
    ingest(&mut reader, header("pi-session"));
    ingest(
        &mut reader,
        json!({
            "type": "model_change",
            "id": "a1",
            "provider": "anthropic",
            "modelId": "claude-opus-5"
        }),
    );
    ingest(
        &mut reader,
        json!({
            "type": "usage",
            "id": "u1",
            "timestamp": "2026-08-20T15:28:00.000Z",
            "usage": {"input": 3, "output": 1}
        }),
    );
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].model, "claude-opus-5");
    assert!(rows[0].dedup_key.is_some());
}

#[test]
fn model_less_operations_use_the_tools_summaries_bucket() {
    let mut reader = PiReader::new("pi", "fallback");
    ingest(&mut reader, header("pi-session"));
    ingest(
        &mut reader,
        json!({
            "type": "compaction",
            "id": "c1",
            "timestamp": "2026-08-20T15:29:00.000Z",
            "usage": {"input": 10, "output": 2}
        }),
    );
    let rows = reader.snapshot();
    assert_eq!(rows[0].model, "tools/summaries");
}

#[test]
fn tool_result_without_a_model_is_separate_from_the_conversation_model() {
    let mut reader = PiReader::new("pi", "fallback");
    ingest(&mut reader, header("pi-session"));
    ingest(
        &mut reader,
        json!({
            "type": "message",
            "id": "t1",
            "timestamp": "2026-08-20T15:30:00.000Z",
            "message": {
                "role": "toolResult",
                "usage": {"input": 4, "output": 1}
            }
        }),
    );
    let rows = reader.snapshot();
    assert_eq!(rows[0].model, "tools/summaries");
}

#[test]
fn missing_header_and_bad_cache_write_1h_are_visible_unsupported() {
    let mut reader = PiReader::new("pi", "fallback");
    let result = reader.ingest(
        &serde_json::to_vec(&json!({
            "type": "message",
            "message": {"role": "assistant", "model": "m", "usage": {"input": 1}}
        }))
        .unwrap(),
    );
    assert!(matches!(result, Err(ReaderError::Unsupported(_))));

    ingest(&mut reader, header("pi-session"));
    let result = reader.ingest(
        &serde_json::to_vec(&json!({
            "type": "message",
            "id": "b1",
            "timestamp": "2026-08-20T15:31:00.000Z",
            "message": {
                "role": "assistant",
                "model": "m",
                "usage": {
                    "input": 1,
                    "output": 1,
                    "cacheWrite": 2,
                    "cacheWrite1h": 3
                }
            }
        }))
        .unwrap(),
    );
    assert!(matches!(result, Err(ReaderError::Unsupported(_))));
}

#[test]
fn copied_assistant_records_share_a_cross_session_identity() {
    let mut parent = PiReader::new("pi", "parent");
    ingest(&mut parent, header("parent"));
    ingest(
        &mut parent,
        json!({
            "type": "message",
            "id": "b2",
            "timestamp": "2026-08-20T15:27:40.000Z",
            "message": {
                "role": "assistant",
                "model": "m",
                "usage": {"input": 2, "output": 3}
            }
        }),
    );
    let mut fork = PiReader::new("pi", "fork");
    ingest(&mut fork, header("fork"));
    ingest(
        &mut fork,
        json!({
            "type": "message",
            "id": "b2",
            "timestamp": "2026-08-20T15:27:40.000Z",
            "message": {
                "role": "assistant",
                "model": "m",
                "usage": {"input": 2, "output": 3}
            }
        }),
    );
    assert_eq!(parent.snapshot()[0].dedup_key, fork.snapshot()[0].dedup_key);
}
