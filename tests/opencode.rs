//! Synthetic OpenCode SQLite message cases. No byte-for-byte OpenCode
//! differential fixture is retained at the Tokscale pin, so these tests use
//! payload shapes adapted (not executed) from the pinned
//! `opencode_schema.rs` unit fixtures plus native edge cases.

use serde_json::json;
use token_monitor_lite::accounting::TokenCounts;
use token_monitor_lite::parsers::ReaderError;
use token_monitor_lite::parsers::opencode::{
    OpenCodeKind, OpenCodeReader, OpenCodeRecord, normalize_record,
};

fn record<'a>(
    message_id: &'a str,
    session_id: &'a str,
    kind: OpenCodeKind,
    data: &'a [u8],
    fallback_timestamp_ms: Option<i64>,
) -> OpenCodeRecord<'a> {
    OpenCodeRecord {
        message_id,
        session_id,
        kind,
        data,
        fallback_timestamp_ms,
    }
}

fn payload(value: serde_json::Value) -> Vec<u8> {
    serde_json::to_vec(&value).unwrap()
}

#[test]
fn current_v2_message_splits_reasoning_from_inclusive_output() {
    let data = payload(json!({
        "role": "assistant",
        "model": {"id": "claude-sonnet-4-5", "providerID": "anthropic"},
        "time": {"created": 1_763_000_000_000_i64},
        "tokens": {
            "input": 100,
            "output": 40,
            "reasoning": 10,
            "cache": {"read": 20, "write": 5}
        }
    }));
    let row = normalize_record(&record(
        "message-a",
        "session-a",
        OpenCodeKind::Assistant,
        &data,
        None,
    ))
    .unwrap()
    .expect("supported usage");

    assert_eq!(row.key.client, "opencode");
    assert_eq!(row.key.session, "session-a");
    assert_eq!(row.model, "claude-sonnet-4-5");
    assert_eq!(row.billing_provider_hint.as_deref(), Some("anthropic"));
    assert_eq!(
        row.tokens,
        TokenCounts {
            input: 100,
            output: 30,
            cache_read: 20,
            cache_write: 5,
            reasoning: 10,
        }
    );
    assert_eq!(row.timestamp_ms, 1_763_000_000_000);
    assert_eq!(
        row.dedup_key.as_deref(),
        Some("opencode:session-a:message-a")
    );
    assert_eq!(row.cache_write_1h, 0);
    assert_eq!(row.service_tier, None);
    assert_eq!(row.agent, None);
}

#[test]
fn legacy_top_level_model_and_column_timestamp_are_supported() {
    let data = payload(json!({
        "role": "assistant",
        "modelID": "qwen3-coder",
        "providerID": "qwen",
        "tokens": {
            "input": 7,
            "output": 3,
            "reasoning": 1,
            "cache": {"read": 2, "write": 1}
        }
    }));
    let row = normalize_record(&record(
        "message-legacy",
        "session-legacy",
        OpenCodeKind::Assistant,
        &data,
        Some(1_700_000_000_000),
    ))
    .unwrap()
    .expect("supported usage");

    assert_eq!(row.model, "qwen3-coder");
    assert_eq!(row.billing_provider_hint.as_deref(), Some("qwen"));
    assert_eq!(row.timestamp_ms, 1_700_000_000_000);
}

#[test]
fn pinned_example_shape_prefers_top_level_model_and_provider() {
    // Field set adapted from the pinned opencode_schema.rs fixture that proves
    // top-level modelID/providerID win over the nested model object.
    let data = payload(json!({
        "modelID": "top-level-model",
        "providerID": "top-level-provider",
        "model": {"id": "nested-model", "providerID": "nested-provider"},
        "time": {"created": 1_763_000_000_000_i64},
        "tokens": {
            "input": 1,
            "output": 1,
            "reasoning": 0,
            "cache": {"read": 0, "write": 0}
        }
    }));
    let row = normalize_record(&record(
        "message-pinned",
        "session-pinned",
        OpenCodeKind::Assistant,
        &data,
        None,
    ))
    .unwrap()
    .expect("supported usage");
    assert_eq!(row.model, "top-level-model");
    assert_eq!(
        row.billing_provider_hint.as_deref(),
        Some("top-level-provider")
    );
}

#[test]
fn compaction_row_with_usage_is_supported() {
    let data = payload(json!({
        "model": {"id": "claude-sonnet-4-5"},
        "time": {"created": 1_763_000_000_000_i64},
        "tokens": {
            "input": 10,
            "output": 4,
            "reasoning": 0,
            "cache": {"read": 1, "write": 0}
        }
    }));
    let row = normalize_record(&record(
        "message-compaction",
        "session-a",
        OpenCodeKind::Compaction,
        &data,
        None,
    ))
    .unwrap()
    .expect("compaction usage");
    assert_eq!(row.key.session, "session-a");
}

#[test]
fn contradictory_role_is_unsupported_and_tokenless_assistant_is_ignored() {
    let data = payload(json!({
        "role": "user",
        "time": {"created": 1_763_000_000_000_i64}
    }));
    let row = normalize_record(&record(
        "message-user",
        "session-a",
        OpenCodeKind::Assistant,
        &data,
        None,
    ));
    assert!(matches!(
        row,
        Err(ReaderError::Unsupported(_) | ReaderError::Declined(_))
    ));

    let assistant = payload(json!({
        "role": "assistant",
        "time": {"created": 1_763_000_000_000_i64}
    }));
    assert!(
        normalize_record(&record(
            "message-clean",
            "session-a",
            OpenCodeKind::Assistant,
            &assistant,
            None,
        ))
        .unwrap()
        .is_none()
    );
}

#[test]
fn missing_or_partial_cache_is_visible_unsupported() {
    for cache in [json!(null), json!({"read": 1})] {
        let data = payload(json!({
            "role": "assistant",
            "model": {"id": "m"},
            "time": {"created": 1_763_000_000_000_i64},
            "tokens": {"input": 1, "output": 1, "reasoning": 0, "cache": cache}
        }));
        let result = normalize_record(&record(
            "message-cache",
            "session-a",
            OpenCodeKind::Assistant,
            &data,
            None,
        ));
        assert!(matches!(
            result,
            Err(ReaderError::Unsupported(_) | ReaderError::Declined(_))
        ));
    }
}

#[test]
fn negative_and_fractional_values_are_not_clamped_or_truncated() {
    let negative = payload(json!({
        "role": "assistant",
        "model": {"id": "m"},
        "time": {"created": 1_763_000_000_000_i64},
        "tokens": {
            "input": -1,
            "output": 1,
            "reasoning": 0,
            "cache": {"read": 0, "write": 0}
        }
    }));
    assert!(matches!(
        normalize_record(&record(
            "message-negative",
            "session-a",
            OpenCodeKind::Assistant,
            &negative,
            None,
        )),
        Err(ReaderError::InvalidRecord)
    ));

    let fractional = payload(json!({
        "role": "assistant",
        "model": {"id": "m"},
        "time": {"created": 1_763_000_000_000.5_f64},
        "tokens": {
            "input": 1,
            "output": 1,
            "reasoning": 0,
            "cache": {"read": 0, "write": 0}
        }
    }));
    assert!(matches!(
        normalize_record(&record(
            "message-fractional",
            "session-a",
            OpenCodeKind::Assistant,
            &fractional,
            None,
        )),
        Err(ReaderError::InvalidTimestamp)
    ));
}

#[test]
fn reasoning_greater_than_inclusive_output_is_visible_unsupported() {
    let data = payload(json!({
        "role": "assistant",
        "model": {"id": "m"},
        "time": {"created": 1_763_000_000_000_i64},
        "tokens": {
            "input": 1,
            "output": 2,
            "reasoning": 3,
            "cache": {"read": 0, "write": 0}
        }
    }));
    assert!(matches!(
        normalize_record(&record(
            "message-reasoning",
            "session-a",
            OpenCodeKind::Assistant,
            &data,
            None,
        )),
        Err(ReaderError::Unsupported(_) | ReaderError::Declined(_))
    ));
}

#[test]
fn missing_model_or_timestamp_is_visible_unsupported() {
    let no_model = payload(json!({
        "role": "assistant",
        "time": {"created": 1_763_000_000_000_i64},
        "tokens": {"input": 1, "output": 1, "reasoning": 0, "cache": {"read": 0, "write": 0}}
    }));
    assert!(matches!(
        normalize_record(&record(
            "message-model",
            "session-a",
            OpenCodeKind::Assistant,
            &no_model,
            None,
        )),
        Err(ReaderError::Unsupported(_) | ReaderError::Declined(_))
    ));

    let no_time = payload(json!({
        "role": "assistant",
        "model": {"id": "m"},
        "tokens": {"input": 1, "output": 1, "reasoning": 0, "cache": {"read": 0, "write": 0}}
    }));
    assert!(matches!(
        normalize_record(&record(
            "message-time",
            "session-a",
            OpenCodeKind::Assistant,
            &no_time,
            None,
        )),
        Err(ReaderError::Unsupported(_) | ReaderError::Declined(_))
    ));
}

#[test]
fn reader_collects_only_supported_rows() {
    let supported = payload(json!({
        "role": "assistant",
        "model": {"id": "m"},
        "time": {"created": 1_763_000_000_000_i64},
        "tokens": {"input": 1, "output": 1, "reasoning": 0, "cache": {"read": 0, "write": 0}}
    }));
    let ignored = payload(json!({"role": "assistant"}));
    let mut reader = OpenCodeReader::new();
    reader
        .ingest(record(
            "message-supported",
            "session-a",
            OpenCodeKind::Assistant,
            &supported,
            None,
        ))
        .unwrap();
    reader
        .ingest(record(
            "message-ignored",
            "session-a",
            OpenCodeKind::Assistant,
            &ignored,
            None,
        ))
        .unwrap();
    assert_eq!(reader.snapshot().len(), 1);
    assert!(reader.has_supported_usage());
}
