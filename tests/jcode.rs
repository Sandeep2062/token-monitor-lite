//! Jcode session-snapshot reader contract.

use serde_json::json;
use token_monitor_lite::parsers::jcode::{CLIENT, JcodeReader};
use token_monitor_lite::parsers::{CostStatus, ReaderError};

fn snapshot(messages: Vec<serde_json::Value>) -> String {
    json!({
        "id": "jcode-session-1",
        "provider_key": "anthropic",
        "model": "claude-sonnet-4",
        "messages": messages
    })
    .to_string()
}

fn assistant(id: &str, usage: serde_json::Value) -> serde_json::Value {
    json!({
        "id": id,
        "role": "assistant",
        "timestamp": "2026-07-09T11:00:00.000Z",
        "token_usage": usage
    })
}

fn read(document: &str) -> Vec<token_monitor_lite::parsers::ResponseUsage> {
    let mut reader =
        JcodeReader::new("dir-session").with_fallback_timestamp(Some(1_782_718_000_000));
    reader
        .ingest(document.as_bytes())
        .expect("document is accepted");
    reader.snapshot()
}

#[test]
fn one_observation_is_emitted_per_message_with_usage() {
    let document = snapshot(vec![
        assistant("m1", json!({"input_tokens": 100, "output_tokens": 20})),
        assistant("m2", json!({"input_tokens": 50, "output_tokens": 10})),
    ]);
    let found = read(&document);
    assert_eq!(found.len(), 2);
    assert_eq!(found[0].tokens.input, 100);
    assert_eq!(found[1].tokens.input, 50);
    assert_eq!(found[0].key.client, CLIENT);
}

#[test]
fn a_message_without_token_usage_is_skipped() {
    // User and tool messages carry no usage.
    let user = json!({"id": "u1", "role": "user", "content": "hello"});
    let found = read(&snapshot(vec![user]));
    assert!(found.is_empty());
}

#[test]
fn an_anthropic_style_usage_already_splits_its_cache_buckets() {
    // The creation field is present, so `input_tokens` is already the uncached
    // share and nothing is subtracted.
    let usage = json!({
        "input_tokens": 100,
        "output_tokens": 20,
        "cache_read_input_tokens": 300,
        "cache_creation_input_tokens": 0
    });
    let found = read(&snapshot(vec![assistant("m1", usage)]));
    assert_eq!(found[0].tokens.input, 100);
    assert_eq!(found[0].tokens.cache_read, 300);
}

#[test]
fn an_openai_style_usage_counts_cache_reads_as_a_subset_of_input() {
    // No creation field and cache reads inside the input count, so the overlap
    // is removed rather than charged twice.
    let usage = json!({
        "input_tokens": 100,
        "output_tokens": 20,
        "cache_read_input_tokens": 40
    });
    let found = read(&snapshot(vec![assistant("m1", usage)]));
    assert_eq!(found[0].tokens.input, 60);
    assert_eq!(found[0].tokens.cache_read, 40);
    assert_eq!(
        found[0].tokens.input + found[0].tokens.cache_read,
        100,
        "the two buckets must sum to the reported input"
    );
}

#[test]
fn cache_reads_above_the_input_prove_the_split_shape() {
    // Impossible under subset accounting, so the creation field is not needed
    // to know the buckets are already separate.
    let usage = json!({
        "input_tokens": 10,
        "output_tokens": 5,
        "cache_read_input_tokens": 900
    });
    let found = read(&snapshot(vec![assistant("m1", usage)]));
    assert_eq!(found[0].tokens.input, 10);
    assert_eq!(found[0].tokens.cache_read, 900);
}

#[test]
fn reasoning_output_tokens_are_reported() {
    let usage = json!({
        "input_tokens": 10,
        "output_tokens": 50,
        "reasoning_output_tokens": 30
    });
    let found = read(&snapshot(vec![assistant("m1", usage)]));
    assert_eq!(found[0].tokens.reasoning, 30);
}

#[test]
fn an_all_zero_usage_message_is_skipped() {
    let usage = json!({"input_tokens": 0, "output_tokens": 0});
    assert!(read(&snapshot(vec![assistant("m1", usage)])).is_empty());
}

#[test]
fn the_identity_is_the_session_and_message() {
    let usage = json!({"input_tokens": 10});
    let found = read(&snapshot(vec![assistant("m1", usage)]));
    assert_eq!(found[0].key.session, "jcode-session-1");
    assert_eq!(
        found[0].dedup_key.as_deref(),
        Some("jcode:jcode-session-1:m1")
    );
}

#[test]
fn the_model_and_provider_come_from_the_session() {
    let usage = json!({"input_tokens": 10});
    let found = read(&snapshot(vec![assistant("m1", usage)]));
    assert_eq!(found[0].model, "claude-sonnet-4");
    assert_eq!(found[0].billing_provider_hint.as_deref(), Some("anthropic"));
}

#[test]
fn a_blank_session_model_and_provider_fall_back() {
    let document = json!({
        "id": "s",
        "model": "  ",
        "provider_key": "",
        "messages": [assistant("m1", json!({"input_tokens": 10}))]
    })
    .to_string();
    let found = read(&document);
    assert_eq!(found[0].model, "unknown");
    assert_eq!(found[0].billing_provider_hint.as_deref(), Some("jcode"));
}

#[test]
fn a_blank_session_id_keeps_the_callers_session() {
    let document = json!({
        "id": "   ",
        "messages": [assistant("m1", json!({"input_tokens": 10}))]
    })
    .to_string();
    let found = read(&document);
    assert_eq!(found[0].key.session, "dir-session");
}

#[test]
fn the_turn_start_is_back_anchored_from_its_recorded_end() {
    let message = json!({
        "id": "m1",
        "role": "assistant",
        "timestamp": "2026-07-09T11:00:00.000Z",
        "tool_duration_ms": 5000,
        "token_usage": {"input_tokens": 10}
    });
    let found = read(&snapshot(vec![message]));
    assert_eq!(found[0].timestamp_ms, 1_783_594_800_000 - 5000);
}

#[test]
fn the_back_anchor_does_not_apply_to_the_fallback_timestamp() {
    // With no own timestamp the value is the file fallback, and shifting it by a
    // turn duration would land on the wrong day.
    let message = json!({
        "id": "m1",
        "role": "assistant",
        "tool_duration_ms": 5000,
        "token_usage": {"input_tokens": 10}
    });
    let found = read(&snapshot(vec![message]));
    assert_eq!(found[0].timestamp_ms, 1_782_718_000_000);
}

#[test]
fn the_working_directory_becomes_a_label_never_a_workspace_key() {
    let document = json!({
        "id": "s",
        "working_dir": "/work/demo",
        "messages": [assistant("m1", json!({"input_tokens": 10}))]
    })
    .to_string();
    let found = read(&document);
    assert_eq!(found[0].workspace_key, None);
    assert_eq!(found[0].workspace_label.as_deref(), Some("demo"));
}

#[test]
fn a_message_without_an_id_gets_an_ordinal_fallback() {
    let message = json!({
        "role": "assistant",
        "token_usage": {"input_tokens": 10}
    });
    let found = read(&snapshot(vec![message]));
    assert_eq!(
        found[0].dedup_key.as_deref(),
        Some("jcode:jcode-session-1:snapshot:0")
    );
}

#[test]
fn cost_is_never_claimed() {
    let found = read(&snapshot(vec![assistant(
        "m1",
        json!({"input_tokens": 10}),
    )]));
    assert_eq!(found[0].cost, CostStatus::Unavailable);
    assert_eq!(found[0].cache_write_1h, 0);
}

#[test]
fn malformed_json_is_a_visible_rejection() {
    let mut reader = JcodeReader::new("s").with_fallback_timestamp(Some(1_000));
    assert_eq!(
        reader.ingest(b"not json").expect_err("malformed"),
        ReaderError::InvalidRecord
    );
}

#[test]
fn a_negative_counter_is_a_visible_rejection() {
    let document = snapshot(vec![assistant("m1", json!({"input_tokens": -5}))]);
    let mut reader = JcodeReader::new("s").with_fallback_timestamp(Some(1_000));
    assert_eq!(
        reader
            .ingest(document.as_bytes())
            .expect_err("negative counter"),
        ReaderError::InvalidRecord
    );
}

#[test]
fn a_revision_replaces_rather_than_appends() {
    let document = snapshot(vec![assistant("m1", json!({"input_tokens": 10}))]);
    let mut reader = JcodeReader::new("s").with_fallback_timestamp(Some(1_000));
    reader.ingest(document.as_bytes()).expect("accepted");
    reader.ingest(document.as_bytes()).expect("accepted");
    assert_eq!(reader.snapshot().len(), 1);
    assert!(reader.has_supported_usage());
}

// A journal is JSONL: one append batch per line, read into the same reader
// after the snapshot. See docs/migration/source-groups.md.

#[test]
fn a_journal_appends_the_unsaved_suffix() {
    let mut reader = JcodeReader::new("dir-session").with_fallback_timestamp(Some(1_000));
    reader
        .ingest(snapshot(vec![assistant("m1", json!({"input_tokens": 10}))]).as_bytes())
        .expect("snapshot accepted");
    let batch = json!({
        "messages": [assistant("m2", json!({"input_tokens": 7}))]
    });
    reader
        .ingest(format!("{batch}\n").as_bytes())
        .expect("journal accepted");

    let found = reader.snapshot();
    assert_eq!(found.len(), 2);
    assert_eq!(found[0].tokens.input, 10);
    assert_eq!(found[1].tokens.input, 7);
}

#[test]
fn a_journal_repeated_message_replaces_the_snapshot_entry() {
    let mut reader = JcodeReader::new("dir-session").with_fallback_timestamp(Some(1_000));
    reader
        .ingest(snapshot(vec![assistant("m1", json!({"input_tokens": 10}))]).as_bytes())
        .expect("snapshot accepted");
    // The journal corrects the same message: the journal wins, and the message
    // is still counted once.
    let batch = json!({
        "messages": [assistant("m1", json!({"input_tokens": 999}))]
    });
    reader
        .ingest(format!("{batch}\n").as_bytes())
        .expect("journal accepted");

    let found = reader.snapshot();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].tokens.input, 999);
}

#[test]
fn a_journal_batch_rescopes_the_provider_for_its_own_messages() {
    let mut reader = JcodeReader::new("dir-session").with_fallback_timestamp(Some(1_000));
    reader
        .ingest(snapshot(vec![assistant("m1", json!({"input_tokens": 10}))]).as_bytes())
        .expect("snapshot accepted");
    let batch = json!({
        "provider_key": "openrouter",
        "messages": [assistant("m2", json!({"input_tokens": 5}))]
    });
    reader
        .ingest(format!("{batch}\n").as_bytes())
        .expect("journal accepted");

    let found = reader.snapshot();
    assert_eq!(found.len(), 2);
    // A route change must not re-authenticate the message recorded before it.
    assert_eq!(found[0].billing_provider_hint.as_deref(), Some("anthropic"));
    assert_eq!(
        found[1].billing_provider_hint.as_deref(),
        Some("openrouter")
    );
}

#[test]
fn a_journal_written_one_message_per_line_is_accepted() {
    let mut reader = JcodeReader::new("dir-session").with_fallback_timestamp(Some(1_000));
    reader
        .ingest(snapshot(vec![assistant("m1", json!({"input_tokens": 10}))]).as_bytes())
        .expect("snapshot accepted");
    // No enclosing `messages` array: the line is the message.
    let line = assistant("m2", json!({"input_tokens": 3}));
    reader
        .ingest(format!("{line}\n").as_bytes())
        .expect("journal accepted");

    assert_eq!(reader.snapshot().len(), 2);
}
