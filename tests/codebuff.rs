//! Codebuff chat-history reader contract.

use serde_json::json;
use std::path::Path;
use token_monitor_lite::parsers::CostStatus;
use token_monitor_lite::parsers::codebuff::{
    CLIENT, CodebuffReader, parse_chat_id_to_millis, session_from_path,
};

fn read(
    session: &str,
    messages: serde_json::Value,
) -> Vec<token_monitor_lite::parsers::ResponseUsage> {
    let mut reader = CodebuffReader::new(session).with_fallback_timestamp(Some(1_782_718_000_000));
    reader
        .ingest(messages.to_string().as_bytes())
        .expect("chat history is accepted");
    reader.snapshot()
}

fn ai(messages: serde_json::Value) -> serde_json::Value {
    json!([{
        "id": "m1",
        "variant": "ai",
        "variantId": "ai",
        "timestamp": 1_782_700_000_000i64,
        "metadata": { "model": "claude-sonnet-4-5", "usage": messages }
    }])
}

#[test]
fn an_assistant_message_becomes_one_observation() {
    let found = read(
        "manicode/proj/chat-1",
        ai(json!({"inputTokens": 1000, "outputTokens": 500})),
    );
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].key.client, CLIENT);
    assert_eq!(found[0].key.session, "manicode/proj/chat-1");
    assert_eq!(found[0].model, "claude-sonnet-4-5");
}

#[test]
fn snake_case_and_camel_case_counters_are_both_accepted() {
    let camel = read("s", ai(json!({"inputTokens": 10, "outputTokens": 20})));
    assert_eq!(camel[0].tokens.input, 10);
    assert_eq!(camel[0].tokens.output, 20);

    let snake = read("s", ai(json!({"input_tokens": 10, "output_tokens": 20})));
    assert_eq!(snake[0].tokens.input, 10);
    assert_eq!(snake[0].tokens.output, 20);
}

#[test]
fn prompt_and_completion_aliases_are_accepted() {
    let found = read("s", ai(json!({"promptTokens": 7, "completionTokens": 9})));
    assert_eq!(found[0].tokens.input, 7);
    assert_eq!(found[0].tokens.output, 9);
}

#[test]
fn cache_read_falls_back_to_nested_prompt_details() {
    let found = read(
        "s",
        ai(json!({"inputTokens": 10, "promptTokensDetails": {"cachedTokens": 4}})),
    );
    assert_eq!(found[0].tokens.input, 10);
    assert_eq!(found[0].tokens.cache_read, 4);
}

#[test]
fn cache_write_is_carried() {
    let found = read(
        "s",
        ai(json!({"inputTokens": 10, "cacheCreationInputTokens": 3})),
    );
    assert_eq!(found[0].tokens.cache_write, 3);
}

#[test]
fn usage_falls_back_to_the_codebuff_nested_object() {
    let messages = json!([{
        "id": "m1",
        "role": "assistant",
        "metadata": {
            "model": "gpt-5",
            "codebuff": { "usage": { "inputTokens": 42, "outputTokens": 8 } }
        }
    }]);
    let found = read("s", messages);
    assert_eq!(found[0].tokens.input, 42);
    assert_eq!(found[0].tokens.output, 8);
}

#[test]
fn usage_falls_back_to_the_run_state_message_history() {
    // OpenRouter-routed calls land their final counts here rather than on
    // `metadata.usage`.
    let messages = json!([{
        "id": "m1",
        "role": "assistant",
        "metadata": {
            "model": "openrouter/x",
            "runState": { "sessionState": { "mainAgentState": { "messageHistory": [
                { "role": "assistant", "providerOptions": {
                    "usage": { "promptTokens": 30, "completionTokens": 12 } } }
            ] } } }
        }
    }]);
    let found = read("s", messages);
    assert_eq!(found[0].tokens.input, 30);
    assert_eq!(found[0].tokens.output, 12);
}

#[test]
fn the_three_usage_locations_are_fallbacks_not_layers() {
    // Both locations describe the same turn; adding them would double-count.
    let messages = json!([{
        "id": "m1",
        "variant": "ai",
        "metadata": {
            "model": "claude-sonnet-4-5",
            "usage": { "inputTokens": 100, "outputTokens": 50 },
            "codebuff": { "usage": { "inputTokens": 999, "outputTokens": 999 } }
        }
    }]);
    let found = read("s", messages);
    assert_eq!(found[0].tokens.input, 100);
    assert_eq!(found[0].tokens.output, 50);
}

#[test]
fn a_non_assistant_message_is_skipped() {
    let messages = json!([
        {"id": "u1", "role": "user", "metadata": {"usage": {"inputTokens": 5}}},
        {"id": "m1", "variant": "ai", "metadata": {"usage": {"inputTokens": 5}}}
    ]);
    let found = read("s", messages);
    assert_eq!(found.len(), 1);
}

#[test]
fn every_role_alias_counts_as_assistant() {
    for role in ["ai", "agent", "assistant"] {
        let messages = json!([{
            "id": "m", "role": role,
            "metadata": {"usage": {"inputTokens": 3}}
        }]);
        assert_eq!(read("s", messages).len(), 1, "role {role}");
    }
}

#[test]
fn a_message_with_no_usage_signal_is_skipped() {
    let messages = json!([{"id": "m1", "variant": "ai", "metadata": {"model": "x"}}]);
    assert!(read("s", messages).is_empty());
}

#[test]
fn credits_are_carried_in_nano_usd() {
    let found = read("s", ai(json!({"inputTokens": 1, "credits": 0.25})));
    assert_eq!(found[0].cost, CostStatus::Reported(250_000_000));
}

#[test]
fn a_credit_only_message_is_still_an_observation() {
    let found = read("s", ai(json!({"credits": 0.5})));
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].cost, CostStatus::Reported(500_000_000));
}

#[test]
fn a_missing_model_falls_back_to_codebuff_unknown() {
    let messages = json!([{"id": "m", "variant": "ai", "metadata": {"usage": {"inputTokens": 1}}}]);
    assert_eq!(read("s", messages)[0].model, "codebuff-unknown");
}

#[test]
fn the_provider_is_inferred_from_the_model_family() {
    let pairs = [
        ("claude-sonnet-4-5", "anthropic"),
        ("gpt-5", "openai"),
        ("gemini-3-pro", "google"),
        ("glm-5", "zhipu"),
        ("deepseek-v3", "deepseek"),
        ("qwen3-coder", "alibaba"),
        ("kimi-k2", "moonshot"),
    ];
    for (model, provider) in pairs {
        let messages = json!([{
            "id": "m", "variant": "ai",
            "metadata": {"model": model, "usage": {"inputTokens": 1}}
        }]);
        assert_eq!(
            read("s", messages)[0].billing_provider_hint.as_deref(),
            Some(provider),
            "model {model}"
        );
    }
}

#[test]
fn an_unrecognised_model_carries_no_provider_hint() {
    let messages = json!([{
        "id": "m", "variant": "ai",
        "metadata": {"model": "mystery-1", "usage": {"inputTokens": 1}}
    }]);
    assert_eq!(read("s", messages)[0].billing_provider_hint, None);
}

#[test]
fn the_upstream_message_id_is_the_identity() {
    let found = read("s", ai(json!({"inputTokens": 1})));
    assert_eq!(found[0].dedup_key.as_deref(), Some("m1"));
}

#[test]
fn a_message_without_an_id_gets_a_derived_identity() {
    let messages = json!([{
        "variant": "ai",
        "timestamp": 1_782_700_000_000i64,
        "metadata": {"model": "gpt-5", "usage": {"inputTokens": 4, "outputTokens": 2}}
    }]);
    let key = read("manicode/p/c", messages)[0].dedup_key.clone().unwrap();
    assert!(key.starts_with("codebuff:manicode/p/c:1782700000000:gpt-5:0:4:2:0:0"));
}

#[test]
fn an_iso_chat_id_becomes_a_timestamp() {
    // Only the two separators after `T` were flipped; the date keeps its dashes.
    assert_eq!(
        parse_chat_id_to_millis("2025-12-14T10-00-00.000Z"),
        Some(1_765_706_400_000)
    );
    assert_eq!(parse_chat_id_to_millis("not-a-chat-id"), None);
}

#[test]
fn the_timestamp_falls_back_to_the_chat_id_then_the_file_mtime() {
    let messages = json!([{
        "variant": "ai",
        "metadata": {"usage": {"inputTokens": 1}}
    }]);
    // Session's last segment is a chat id, so it supplies the time.
    let found = read("chan/p/2025-12-14T10-00-00.000Z", messages);
    assert_eq!(found[0].timestamp_ms, 1_765_706_400_000);
}

#[test]
fn the_session_identity_comes_from_the_path() {
    let path = Path::new(
        "/home/u/.config/manicode/projects/proj/chats/2025-12-14T10-00-00.000Z/chat-messages.json",
    );
    assert_eq!(
        session_from_path(path),
        "manicode/proj/2025-12-14T10-00-00.000Z"
    );
}

#[test]
fn a_non_array_document_yields_nothing() {
    assert!(read("s", json!({"messages": []})).is_empty());
}

#[test]
fn malformed_json_is_a_visible_rejection() {
    let mut reader = CodebuffReader::new("s").with_fallback_timestamp(Some(1_000));
    assert!(reader.ingest(b"not json").is_err());
}
