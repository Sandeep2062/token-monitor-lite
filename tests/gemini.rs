//! Synthetic Gemini CLI transcript cases adapted from the pinned Tokscale
//! `gemini.rs` reader. No byte-for-byte Gemini differential fixture is
//! retained, so these are native synthetic cases.

use serde_json::json;
use token_monitor_lite::accounting::TokenCounts;
use token_monitor_lite::parsers::ReaderError;
use token_monitor_lite::parsers::gemini::GeminiReader;

fn ingest(reader: &mut GeminiReader, value: serde_json::Value) {
    reader.ingest(&serde_json::to_vec(&value).unwrap()).unwrap();
}

#[test]
fn session_document_splits_cached_prompt_and_keeps_tool_in_input() {
    let mut reader = GeminiReader::new("fallback");
    ingest(
        &mut reader,
        json!({
            "sessionId": "ses-1",
            "messages": [{
                "id": "m1",
                "timestamp": "2026-11-13T10:00:00Z",
                "model": "gemini-2.5-pro",
                "tokens": {
                    "input": 100,
                    "output": 20,
                    "cached": 10,
                    "thoughts": 4,
                    "tool": 2,
                    "total": 126
                }
            }]
        }),
    );
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key.client, "gemini");
    assert_eq!(rows[0].key.session, "ses-1");
    assert_eq!(rows[0].model, "gemini-2.5-pro");
    assert_eq!(rows[0].billing_provider_hint.as_deref(), Some("google"));
    assert_eq!(rows[0].timestamp_ms, 1_794_564_000_000);
    assert_eq!(rows[0].dedup_key.as_deref(), Some("gemini:ses-1:m1"));
    assert_eq!(
        rows[0].tokens,
        TokenCounts {
            input: 92,
            output: 20,
            cache_read: 10,
            cache_write: 0,
            reasoning: 4,
        }
    );
}

#[test]
fn headless_jsonl_init_and_direct_events_resolve_model_and_identity() {
    let mut reader = GeminiReader::new("fallback");
    for value in [
        json!({"type": "init", "session_id": "ses-2", "model": "gemini-2.5-flash"}),
        json!({
            "type": "gemini",
            "id": "e1",
            "timestamp": "2026-11-13T10:00:00Z",
            "tokens": {"input": 10, "output": 5, "cached": 2, "thoughts": 1, "total": 16}
        }),
        json!({
            "type": "gemini",
            "id": "e2",
            "timestamp": "2026-11-13T10:00:01Z",
            "tokens": {
                "promptTokenCount": 20,
                "candidatesTokenCount": 6,
                "cachedContentTokenCount": 3,
                "thoughtsTokenCount": 2,
                "totalTokenCount": 28
            }
        }),
    ] {
        ingest(&mut reader, value);
    }
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].key.session, "ses-2");
    assert_eq!(rows[0].model, "gemini-2.5-flash");
    assert_eq!(rows[0].dedup_key.as_deref(), Some("gemini:ses-2:e1"));
    assert_eq!(
        rows[0].tokens,
        TokenCounts {
            input: 8,
            output: 5,
            cache_read: 2,
            cache_write: 0,
            reasoning: 1,
        }
    );
    assert_eq!(rows[1].dedup_key.as_deref(), Some("gemini:ses-2:e2"));
    assert_eq!(rows[1].tokens.input, 17);
    assert_eq!(rows[1].tokens.output, 6);
    assert_eq!(rows[1].tokens.cache_read, 3);
    assert_eq!(rows[1].tokens.reasoning, 2);
}

#[test]
fn stats_models_emit_one_source_local_row_per_model() {
    let mut reader = GeminiReader::new("ses-3");
    ingest(
        &mut reader,
        json!({
            "type": "stats",
            "session_id": "ses-3",
            "model": "gemini-2.5-pro",
            "timestamp": "2026-11-13T10:00:00Z",
            "stats": {
                "models": {
                    "gemini-2.5-pro": {
                        "promptTokenCount": 100,
                        "candidatesTokenCount": 20,
                        "cachedContentTokenCount": 10,
                        "thoughtsTokenCount": 5
                    }
                }
            }
        }),
    );
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key.session, "ses-3");
    assert_eq!(rows[0].model, "gemini-2.5-pro");
    assert_eq!(rows[0].dedup_key, None);
    assert_eq!(
        rows[0].tokens,
        TokenCounts {
            input: 90,
            output: 20,
            cache_read: 10,
            cache_write: 0,
            reasoning: 5,
        }
    );
}

#[test]
fn missing_stats_model_is_visible_unsupported() {
    let mut reader = GeminiReader::new("ses-4");
    let result = reader.ingest(
        &serde_json::to_vec(&json!({
            "type": "stats",
            "timestamp": "2026-11-13T10:00:00Z",
            "stats": {"input": 1, "output": 1}
        }))
        .unwrap(),
    );
    assert!(matches!(
        result,
        Err(ReaderError::Unsupported(_) | ReaderError::Declined(_))
    ));
}

#[test]
fn negative_counters_are_not_clamped() {
    let mut reader = GeminiReader::new("ses-5");
    let result = reader.ingest(
        &serde_json::to_vec(&json!({
            "type": "gemini",
            "session_id": "ses-5",
            "model": "gemini-2.5-pro",
            "timestamp": "2026-11-13T10:00:00Z",
            "tokens": {"input": -1, "output": 1}
        }))
        .unwrap(),
    );
    assert!(matches!(result, Err(ReaderError::InvalidRecord)));
}

#[test]
fn non_usage_events_are_ignored() {
    let mut reader = GeminiReader::new("ses-6");
    ingest(&mut reader, json!({"type": "user", "content": "hello"}));
    assert!(reader.snapshot().is_empty());
}
