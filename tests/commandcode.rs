//! Synthetic Command Code v3 transcript cases adapted from the pinned Tokscale
//! `commandcode.rs` reader. The transcript is a typed JSONL stream whose
//! assistant lines carry authoritative, disjoint usage buckets plus a reported
//! cost, and whose entry ids form a rewound tree rather than a flat log.

use token_monitor_lite::accounting::TokenCounts;
use token_monitor_lite::parsers::CostStatus;
use token_monitor_lite::parsers::ReaderError;
use token_monitor_lite::parsers::commandcode::{
    CLIENT, CommandCodeReader, PROVIDER, UNKNOWN_MODEL,
};

fn new_reader() -> CommandCodeReader {
    CommandCodeReader::new("session-a").with_fallback_timestamp(Some(1_700_000_000_000))
}

fn header(id: &str) -> Vec<u8> {
    format!(r#"{{"type":"session","version":3,"id":"{id}"}}"#).into_bytes()
}

fn assistant(id: &str, parent: &str, model: &str, usage: &str) -> Vec<u8> {
    format!(
        r#"{{"type":"message","id":"{id}","parentId":"{parent}","timestamp":"2026-02-13T10:30:26.420Z","model":"{model}","message":{{"role":"assistant","content":[{{"type":"text","text":"hi"}}]}},"usage":{usage}}}"#
    )
    .into_bytes()
}

#[test]
fn an_assistant_line_carries_authoritative_disjoint_buckets() {
    let mut reader = new_reader();
    reader.ingest(&header("session-a")).unwrap();
    reader
        .ingest(&assistant(
            "a1",
            "root",
            "deepseek/deepseek-v4-flash",
            r#"{"inputTokens":28534,"outputTokens":205,"cacheReadTokens":7424,"cacheWriteTokens":11,"costUsd":0.006464748}"#,
        ))
        .unwrap();

    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key.client, CLIENT);
    assert_eq!(rows[0].key.session, "session-a");
    // The org segment is not a pricing key, so it is dropped.
    assert_eq!(rows[0].model, "deepseek-v4-flash");
    assert_eq!(rows[0].timestamp_ms, 1_770_978_626_420);
    // The buckets are disjoint: `inputTokens` is cache-exclusive and the cache
    // buckets are billed on top, so nothing is subtracted.
    assert_eq!(
        rows[0].tokens,
        TokenCounts {
            input: 28_534,
            output: 205,
            cache_read: 7_424,
            cache_write: 11,
            reasoning: 0,
        }
    );
    assert_eq!(rows[0].cost, CostStatus::Reported(6_464_748));
}

#[test]
fn the_session_header_names_the_session_not_the_path_stem() {
    let mut reader = CommandCodeReader::new("path-stem");
    reader.ingest(&header("header-id")).unwrap();
    reader
        .ingest(&assistant(
            "a1",
            "root",
            "openai/gpt-5",
            r#"{"inputTokens":5}"#,
        ))
        .unwrap();
    assert_eq!(reader.snapshot()[0].key.session, "header-id");
}

#[test]
fn a_message_id_is_never_mistaken_for_the_session_id() {
    let mut reader = CommandCodeReader::new("path-stem");
    // No `session` header at all: the path stem must stand.
    reader
        .ingest(&assistant(
            "message-uuid",
            "root",
            "openai/gpt-5",
            r#"{"inputTokens":5}"#,
        ))
        .unwrap();
    assert_eq!(reader.snapshot()[0].key.session, "path-stem");
}

#[test]
fn a_model_change_record_updates_the_model_and_emits_nothing() {
    let mut reader = new_reader();
    reader.ingest(&header("session-a")).unwrap();
    reader
        .ingest(br#"{"type":"model_change","id":"m1","parentId":"root","timestamp":"2026-02-13T10:20:00.000Z","model":"anthropic/claude-opus-4-5"}"#)
        .unwrap();
    assert!(reader.snapshot().is_empty());
    // A message that names no model of its own runs on the last `model_change`.
    reader
        .ingest(br#"{"type":"message","id":"a1","parentId":"m1","timestamp":"2026-02-13T10:30:26.420Z","message":{"role":"assistant","content":[]},"usage":{"inputTokens":7}}"#)
        .unwrap();
    assert_eq!(reader.snapshot()[0].model, "claude-opus-4-5");
}

#[test]
fn a_messages_own_model_wins_over_an_earlier_model_change() {
    let mut reader = new_reader();
    reader
        .ingest(br#"{"type":"model_change","id":"m1","parentId":"root","timestamp":"2026-02-13T10:20:00.000Z","model":"anthropic/claude-opus-4-5"}"#)
        .unwrap();
    // The line's own model is the one in effect for that call.
    reader
        .ingest(&assistant(
            "a1",
            "m1",
            "openai/gpt-5",
            r#"{"inputTokens":7}"#,
        ))
        .unwrap();
    assert_eq!(reader.snapshot()[0].model, "gpt-5");
}

#[test]
fn the_configured_agent_model_is_the_fallback_then_unknown() {
    let mut with_config =
        CommandCodeReader::new("s").with_config_model(Some("meta/muse-spark-1.1".into()));
    with_config
        .ingest(br#"{"type":"message","id":"a1","parentId":"","message":{"role":"assistant","content":[]},"usage":{"inputTokens":3},"timestamp":"2026-02-13T10:30:26.420Z"}"#)
        .unwrap();
    assert_eq!(with_config.snapshot()[0].model, "muse-spark-1.1");
    assert_eq!(
        with_config.snapshot()[0].billing_provider_hint.as_deref(),
        Some("meta")
    );

    let mut without = new_reader();
    without
        .ingest(br#"{"type":"message","id":"a1","parentId":"","message":{"role":"assistant","content":[]},"usage":{"inputTokens":3},"timestamp":"2026-02-13T10:30:26.420Z"}"#)
        .unwrap();
    assert_eq!(without.snapshot()[0].model, UNKNOWN_MODEL);
    assert_eq!(
        without.snapshot()[0].billing_provider_hint.as_deref(),
        Some(PROVIDER)
    );
}

#[test]
fn a_non_assistant_message_contributes_nothing() {
    let mut reader = new_reader();
    reader
        .ingest(br#"{"type":"message","id":"u1","parentId":"","timestamp":"2026-02-13T10:30:26.420Z","message":{"role":"user","content":[{"type":"text","text":"go"}]}}"#)
        .unwrap();
    reader
        .ingest(br#"{"type":"message","id":"t1","parentId":"","timestamp":"2026-02-13T10:30:26.420Z","message":{"role":"user","content":[{"type":"tool_result","content":"x"}]}}"#)
        .unwrap();
    assert!(reader.snapshot().is_empty());
}

#[test]
fn an_all_zero_block_with_no_cost_records_nothing() {
    let mut reader = new_reader();
    reader
        .ingest(&assistant(
            "a1",
            "root",
            "openai/gpt-5",
            r#"{"inputTokens":0,"outputTokens":0,"cacheReadTokens":0,"cacheWriteTokens":0,"costUsd":0}"#,
        ))
        .unwrap();
    assert!(reader.snapshot().is_empty());
}

#[test]
fn a_positive_cost_beside_zero_buckets_is_still_recorded() {
    let mut reader = new_reader();
    reader
        .ingest(&assistant(
            "a1",
            "root",
            "openai/gpt-5",
            r#"{"inputTokens":0,"outputTokens":0,"costUsd":0.02}"#,
        ))
        .unwrap();
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].tokens, TokenCounts::default());
    assert_eq!(rows[0].cost, CostStatus::Reported(20_000_000));
}

#[test]
fn a_negative_counter_is_rejected_not_clamped() {
    let mut reader = new_reader();
    let error = reader
        .ingest(&assistant(
            "a1",
            "root",
            "openai/gpt-5",
            r#"{"inputTokens":-5}"#,
        ))
        .unwrap_err();
    assert_eq!(error, ReaderError::InvalidRecord);
}

#[test]
fn an_implausible_counter_is_rejected_not_clamped() {
    let mut reader = new_reader();
    let error = reader
        .ingest(&assistant(
            "a1",
            "root",
            "openai/gpt-5",
            r#"{"inputTokens":1000000000001}"#,
        ))
        .unwrap_err();
    assert_eq!(error, ReaderError::InvalidRecord);
}

#[test]
fn a_negative_or_absurd_cost_is_rejected() {
    for cost in ["-0.01", "1000001.0"] {
        let mut reader = new_reader();
        let error = reader
            .ingest(&assistant(
                "a1",
                "root",
                "openai/gpt-5",
                &format!(r#"{{"inputTokens":5,"costUsd":{cost}}}"#),
            ))
            .unwrap_err();
        assert_eq!(error, ReaderError::InvalidRecord);
    }
}

#[test]
fn an_abandoned_rewind_branch_is_not_counted() {
    let mut reader = new_reader();
    reader.ingest(&header("session-a")).unwrap();
    // The active chain is root -> a1 -> u1 -> a2, so the sibling `dead` that
    // was abandoned by `/rewind` must not contribute even though its usage is
    // still on disk.
    reader
        .ingest(&assistant(
            "a1",
            "",
            "openai/gpt-5",
            r#"{"inputTokens":100}"#,
        ))
        .unwrap();
    reader
        .ingest(&assistant(
            "dead",
            "a1",
            "openai/gpt-5",
            r#"{"inputTokens":999}"#,
        ))
        .unwrap();
    reader
        .ingest(br#"{"type":"message","id":"u1","parentId":"a1","timestamp":"2026-02-13T10:31:00.000Z","message":{"role":"user","content":[{"type":"text","text":"next"}]}}"#)
        .unwrap();
    reader
        .ingest(&assistant(
            "a2",
            "u1",
            "openai/gpt-5",
            r#"{"inputTokens":200}"#,
        ))
        .unwrap();

    let rows = reader.snapshot();
    assert_eq!(rows.len(), 2);
    let total = rows.iter().map(|row| row.tokens.input).sum::<u64>();
    assert_eq!(total, 300);
}

#[test]
fn a_broken_branch_chain_keeps_every_message() {
    let mut reader = new_reader();
    reader.ingest(&header("session-a")).unwrap();
    reader
        .ingest(&assistant(
            "a1",
            "",
            "openai/gpt-5",
            r#"{"inputTokens":100}"#,
        ))
        .unwrap();
    // `a2` names an ancestor this file never contained, so the chain is broken.
    // Truncating there would silently discard the already-billed `a1`.
    reader
        .ingest(&assistant(
            "a2",
            "missing",
            "openai/gpt-5",
            r#"{"inputTokens":200}"#,
        ))
        .unwrap();
    assert_eq!(reader.snapshot().len(), 2);
}

#[test]
fn a_fork_copy_collapses_on_entry_id_and_timestamp() {
    let line = assistant("a1", "", "openai/gpt-5", r#"{"inputTokens":100}"#);
    let mut original = new_reader();
    original.ingest(&header("session-a")).unwrap();
    original.ingest(&line).unwrap();

    let mut fork = CommandCodeReader::new("session-b");
    fork.ingest(&header("session-b")).unwrap();
    fork.ingest(&line).unwrap();

    // Both files emit the row, but with an identical key so aggregation
    // collapses the copy rather than double counting it.
    assert_eq!(
        original.snapshot()[0].dedup_key,
        fork.snapshot()[0].dedup_key
    );
}

#[test]
fn a_missing_timestamp_uses_the_file_mtime_fallback() {
    let mut reader = new_reader();
    reader
        .ingest(br#"{"type":"message","id":"a1","parentId":"","message":{"role":"assistant","content":[]},"usage":{"inputTokens":5}}"#)
        .unwrap();
    assert_eq!(reader.snapshot()[0].timestamp_ms, 1_700_000_000_000);
}

#[test]
fn an_uncounted_usage_block_records_nothing_rather_than_estimating() {
    let mut reader = new_reader();
    // No bucket fields at all: the pinned reader estimates ~4 chars/token from
    // the message text here, which the native reader refuses to fabricate.
    reader
        .ingest(&assistant(
            "a1",
            "root",
            "openai/gpt-5",
            r#"{"costUsd":null}"#,
        ))
        .unwrap();
    assert!(reader.snapshot().is_empty());
    assert!(!reader.has_supported_usage());
}

#[test]
fn a_user_only_transcript_reports_no_supported_usage() {
    let mut reader = new_reader();
    reader
        .ingest(br#"{"type":"message","id":"u1","parentId":"","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}"#)
        .unwrap();
    assert!(!reader.has_supported_usage());
    assert!(reader.snapshot().is_empty());
}

#[test]
fn a_malformed_line_is_a_visible_candidate_failure() {
    let mut reader = new_reader();
    assert_eq!(
        reader.ingest(b"{not json").unwrap_err(),
        ReaderError::InvalidRecord
    );
}

#[test]
fn a_control_character_in_the_model_is_rejected() {
    let mut reader = new_reader();
    let error = reader
        .ingest(&assistant(
            "a1",
            "root",
            "openai/gpt-5\u{0007}",
            r#"{"inputTokens":5}"#,
        ))
        .unwrap_err();
    assert_eq!(error, ReaderError::InvalidRecord);
}
