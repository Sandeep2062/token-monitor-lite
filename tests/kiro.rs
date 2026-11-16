//! Synthetic Kiro CLI cases adapted from the pinned Tokscale `kiro.rs` reader.
//! The supported subset is the per-turn authoritative counters in a CLI
//! `sessions/cli/<session>.json` document; the hybrid estimation model and the
//! three other Kiro stores are deliberately out of scope.

use token_monitor_lite::accounting::TokenCounts;
use token_monitor_lite::parsers::CostStatus;
use token_monitor_lite::parsers::ReaderError;
use token_monitor_lite::parsers::kiro::{CLIENT, DEFAULT_MODEL, KiroReader, PROVIDER};

fn ingest(reader: &mut KiroReader, document: &str) -> Result<(), ReaderError> {
    reader.ingest(document.as_bytes())
}

fn reader() -> KiroReader {
    KiroReader::new("fallback-session")
}

/// One turn with the given counters, wrapped in a minimal session document.
fn document(turns: &str) -> String {
    format!(
        r#"{{"session_id":"sess-1","cwd":"/work/demo","session_state":{{"rts_model_state":{{"model_info":{{"model_id":"claude-sonnet-4","context_window_tokens":200000}}}},"conversation_metadata":{{"user_turn_metadatas":[{turns}]}}}}}}"#
    )
}

fn turn(input: i64, output: i64) -> String {
    format!(
        r#"{{"input_token_count":{input},"output_token_count":{output},"end_timestamp":1782718000}}"#
    )
}

#[test]
fn one_turn_becomes_one_observation() {
    let mut reader = reader();
    ingest(&mut reader, &document(&turn(100, 50))).unwrap();
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key.client, CLIENT);
    assert_eq!(rows[0].key.session, "sess-1");
    assert_eq!(rows[0].model, "claude-sonnet-4");
    assert_eq!(rows[0].billing_provider_hint.as_deref(), Some(PROVIDER));
    assert_eq!(
        rows[0].tokens,
        TokenCounts {
            input: 100,
            output: 50,
            cache_read: 0,
            cache_write: 0,
            reasoning: 0,
        }
    );
    assert_eq!(rows[0].timestamp_ms, 1_782_718_000_000);
    // The session's absolute `cwd` is never persisted; only its final component
    // survives, because a runtime identity column rejects absolute-path text.
    assert_eq!(rows[0].workspace_key, None);
    assert_eq!(rows[0].workspace_label.as_deref(), Some("demo"));
    assert_eq!(rows[0].dedup_key.as_deref(), Some("sess-1:0"));
}

#[test]
fn several_turns_are_separate_observations() {
    let mut reader = reader();
    let turns = format!("{},{},{}", turn(10, 5), turn(20, 6), turn(30, 7));
    ingest(&mut reader, &document(&turns)).unwrap();
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[1].tokens.input, 20);
    assert_eq!(rows[2].dedup_key.as_deref(), Some("sess-1:2"));
}

#[test]
fn an_all_zero_turn_is_not_estimated() {
    let mut reader = reader();
    // Kiro's Auto agent commonly persists zeros. The pinned reader estimates
    // from the sibling messages.jsonl; the native reader contributes nothing.
    ingest(&mut reader, &document(&turn(0, 0))).unwrap();
    assert!(reader.snapshot().is_empty());
    assert!(!reader.has_supported_usage());
}

#[test]
fn a_missing_model_uses_the_documented_default() {
    let mut reader = reader();
    ingest(
        &mut reader,
        r#"{"session_id":"s","session_state":{"conversation_metadata":{"user_turn_metadatas":[{"input_token_count":5,"output_token_count":1,"end_timestamp":1782718000}]}}}"#,
    )
    .unwrap();
    assert_eq!(reader.snapshot()[0].model, DEFAULT_MODEL);
}

#[test]
fn cache_counters_are_carried_when_reported() {
    let mut reader = reader();
    ingest(
        &mut reader,
        &document(
            r#"{"input_token_count":100,"output_token_count":20,"cache_read_input_token_count":80,"cache_write_input_token_count":5,"end_timestamp":1782718000}"#,
        ),
    )
    .unwrap();
    let tokens = reader.snapshot()[0].tokens;
    assert_eq!(tokens.cache_read, 80);
    assert_eq!(tokens.cache_write, 5);
}

#[test]
fn a_negative_counter_is_rejected_not_clamped() {
    let mut reader = reader();
    let error = ingest(&mut reader, &document(&turn(-1, 5))).unwrap_err();
    assert_eq!(error, ReaderError::InvalidRecord);
}

#[test]
fn a_credit_turn_becomes_provider_reported_cost() {
    let mut reader = reader();
    ingest(
        &mut reader,
        &document(
            r#"{"input_token_count":100,"output_token_count":20,"end_timestamp":1782718000,"metering_usage":[{"value":0.5,"unit":"credit"}]}"#,
        ),
    )
    .unwrap();
    // 0.5 credits at $0.04 per credit is $0.02, i.e. 20,000,000 nano-USD.
    assert_eq!(reader.snapshot()[0].cost, CostStatus::Reported(20_000_000));
}

#[test]
fn non_credit_metering_entries_are_ignored() {
    let mut reader = reader();
    ingest(
        &mut reader,
        &document(
            r#"{"input_token_count":100,"output_token_count":20,"end_timestamp":1782718000,"metering_usage":[{"value":99,"unit":"requests"}]}"#,
        ),
    )
    .unwrap();
    // A unit this reader does not price must not be treated as credits.
    assert_eq!(reader.snapshot()[0].cost, CostStatus::Unavailable);
}

#[test]
fn an_unmetered_turn_is_unpriced_not_free() {
    let mut reader = reader();
    ingest(&mut reader, &document(&turn(100, 20))).unwrap();
    assert_eq!(reader.snapshot()[0].cost, CostStatus::Unavailable);
}

#[test]
fn a_negative_credit_value_is_rejected() {
    let mut reader = reader();
    let error = ingest(
        &mut reader,
        &document(
            r#"{"input_token_count":100,"output_token_count":20,"end_timestamp":1782718000,"metering_usage":[{"value":-1,"unit":"credit"}]}"#,
        ),
    )
    .unwrap_err();
    assert_eq!(error, ReaderError::InvalidRecord);
}

#[test]
fn the_file_session_is_the_fallback_when_the_document_names_none() {
    let mut reader = KiroReader::new("from-filename");
    ingest(
        &mut reader,
        r#"{"session_state":{"conversation_metadata":{"user_turn_metadatas":[{"input_token_count":5,"output_token_count":1,"end_timestamp":1782718000}]}}}"#,
    )
    .unwrap();
    assert_eq!(reader.snapshot()[0].key.session, "from-filename");
}

#[test]
fn a_missing_end_timestamp_falls_back_to_the_file_mtime() {
    let mut reader = reader().with_fallback_timestamp(Some(1_700_000_000_000));
    ingest(
        &mut reader,
        &document(r#"{"input_token_count":5,"output_token_count":1}"#),
    )
    .unwrap();
    assert_eq!(reader.snapshot()[0].timestamp_ms, 1_700_000_000_000);
}

#[test]
fn a_missing_timestamp_without_a_fallback_is_visible_unsupported() {
    let mut reader = reader();
    let error = ingest(
        &mut reader,
        &document(r#"{"input_token_count":5,"output_token_count":1}"#),
    )
    .unwrap_err();
    assert_eq!(error, ReaderError::Declined("Kiro turn timestamp"));
}

#[test]
fn a_document_without_a_turn_array_contributes_nothing() {
    let mut reader = reader();
    ingest(
        &mut reader,
        r#"{"session_id":"s","session_state":{"conversation_metadata":{}}}"#,
    )
    .unwrap();
    assert!(reader.snapshot().is_empty());
}

#[test]
fn a_document_without_session_state_is_visible_unsupported() {
    let mut reader = reader();
    let error = ingest(&mut reader, r#"{"session_id":"s"}"#).unwrap_err();
    assert_eq!(error, ReaderError::Unsupported("Kiro session state"));
}

#[test]
fn a_malformed_document_is_a_visible_candidate_failure() {
    let mut reader = reader();
    assert_eq!(
        ingest(&mut reader, "{not json").unwrap_err(),
        ReaderError::InvalidRecord
    );
}

#[test]
fn a_control_character_in_the_model_is_rejected() {
    let mut reader = reader();
    ingest(
        &mut reader,
        "{\"session_id\":\"s\",\"session_state\":{\"rts_model_state\":{\"model_info\":{\"model_id\":\"claude\\u0007sonnet\"}},\"conversation_metadata\":{\"user_turn_metadatas\":[{\"input_token_count\":5,\"output_token_count\":1,\"end_timestamp\":1782718000}]}}}",
    )
    .unwrap_err();
    // The model is rejected before any turn is emitted.
    assert!(reader.snapshot().is_empty());
}

#[test]
fn a_fractional_counter_is_rejected() {
    let mut reader = reader();
    let error = ingest(
        &mut reader,
        &document(r#"{"input_token_count":5.5,"output_token_count":1,"end_timestamp":1782718000}"#),
    )
    .unwrap_err();
    assert_eq!(error, ReaderError::InvalidRecord);
}
