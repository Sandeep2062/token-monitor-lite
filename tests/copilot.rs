//! Synthetic Copilot OTel JSONL cases adapted from the pinned Tokscale
//! `copilot.rs` reader. Flat records with direct attributes are supported;
//! cross-line trace enrichment and VS Code session stores are not.

use serde_json::json;
use token_monitor_lite::accounting::TokenCounts;
use token_monitor_lite::parsers::ReaderError;
use token_monitor_lite::parsers::copilot::CopilotReader;

fn ingest(reader: &mut CopilotReader, value: serde_json::Value) {
    reader.ingest(&serde_json::to_vec(&value).unwrap()).unwrap();
}

#[test]
fn chat_span_normalizes_cache_inclusive_input_and_identity() {
    let mut reader = CopilotReader::new("fallback");
    ingest(
        &mut reader,
        json!({
            "type": "span",
            "traceId": "trace-1",
            "spanId": "span-1",
            "name": "chat gpt-5",
            "startTime": "2026-11-13T10:00:00Z",
            "attributes": {
                "gen_ai.operation.name": "chat",
                "gen_ai.response.model": "gpt-5",
                "gen_ai.conversation.id": "conv-1",
                "gen_ai.response.id": "resp-1",
                "gen_ai.usage.input_tokens": 100,
                "gen_ai.usage.output_tokens": 20,
                "gen_ai.usage.cache_read.input_tokens": 30,
                "gen_ai.usage.reasoning.output_tokens": 5
            }
        }),
    );
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key.client, "copilot");
    assert_eq!(rows[0].key.session, "conv-1");
    assert_eq!(rows[0].model, "gpt-5");
    assert_eq!(
        rows[0].billing_provider_hint.as_deref(),
        Some("github-copilot")
    );
    assert_eq!(
        rows[0].dedup_key.as_deref(),
        Some("copilot:conv-1:response:resp-1")
    );
    assert_eq!(rows[0].timestamp_ms, 1_794_564_000_000);
    assert_eq!(
        rows[0].tokens,
        TokenCounts {
            input: 70,
            output: 20,
            cache_read: 30,
            cache_write: 0,
            reasoning: 5,
        }
    );
}

#[test]
fn inference_log_record_is_supported_without_span_fields() {
    let mut reader = CopilotReader::new("fallback");
    ingest(
        &mut reader,
        json!({
            "timeUnixNano": 1_794_564_000_000_000_000_u64,
            "timeUnixNano": 1_794_564_000_000_000_000_u64,
            "body": "GenAI inference: gpt-5",
            "attributes": {
                "event.name": "gen_ai.client.inference.operation.details",
                "gen_ai.request.model": "gpt-5",
                "gen_ai.conversation.id": "conv-2",
                "gen_ai.usage.input_tokens": 10,
                "gen_ai.usage.output_tokens": 5
            }
        }),
    );
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key.session, "conv-2");
    assert_eq!(rows[0].tokens.input, 10);
    assert_eq!(rows[0].tokens.output, 5);
}

#[test]
fn mixed_sources_prefer_chat_spans() {
    let mut reader = CopilotReader::new("fallback");
    ingest(
        &mut reader,
        json!({
            "type": "span",
            "traceId": "trace-1",
            "spanId": "span-1",
            "name": "chat gpt-5",
            "startTime": "2026-11-13T10:00:00Z",
            "attributes": {
                "gen_ai.operation.name": "chat",
                "gen_ai.response.model": "gpt-5",
                "gen_ai.conversation.id": "conv-1",
                "gen_ai.response.id": "resp-1",
                "gen_ai.usage.input_tokens": 100,
                "gen_ai.usage.output_tokens": 20
            }
        }),
    );
    ingest(
        &mut reader,
        json!({
            "timeUnixNano": 1_794_564_000_000_000_000_u64,
            "body": "GenAI inference: gpt-5",
            "attributes": {
                "event.name": "gen_ai.client.inference.operation.details",
                "gen_ai.request.model": "gpt-5",
                "gen_ai.conversation.id": "conv-2",
                "gen_ai.usage.input_tokens": 10,
                "gen_ai.usage.output_tokens": 5
            }
        }),
    );
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key.session, "conv-1");
}

#[test]
fn missing_model_and_negative_counters_are_visible_unsupported() {
    let mut reader = CopilotReader::new("fallback");
    let missing = reader.ingest(
        &serde_json::to_vec(&json!({
            "type": "span",
            "name": "chat",
            "attributes": {
                "gen_ai.operation.name": "chat",
                "gen_ai.usage.input_tokens": 1,
                "gen_ai.usage.output_tokens": 1
            }
        }))
        .unwrap(),
    );
    assert!(matches!(
        missing,
        Err(ReaderError::Unsupported(_) | ReaderError::Declined(_))
    ));

    let negative = reader.ingest(
        &serde_json::to_vec(&json!({
            "type": "span",
            "name": "chat",
            "attributes": {
                "gen_ai.operation.name": "chat",
                "gen_ai.request.model": "gpt-5",
                "gen_ai.usage.input_tokens": -1,
                "gen_ai.usage.output_tokens": 1
            }
        }))
        .unwrap(),
    );
    assert!(matches!(negative, Err(ReaderError::InvalidRecord)));
}
