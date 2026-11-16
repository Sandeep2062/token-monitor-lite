//! Synthetic Proma cases adapted from the pinned Tokscale `proma.rs` reader.
//! Both supported shapes are covered: native Pi session files and legacy SDK
//! `assistant` transcripts.

use token_monitor_lite::accounting::TokenCounts;
use token_monitor_lite::parsers::ReaderError;
use token_monitor_lite::parsers::proma::{CLIENT, DEFAULT_PROVIDER, PromaReader};

fn ingest(reader: &mut PromaReader, line: &str) -> Result<(), ReaderError> {
    reader.ingest(line.as_bytes())
}

fn reader() -> PromaReader {
    PromaReader::new("fallback-session")
}

fn sdk_assistant(usage: &str) -> String {
    format!(
        r#"{{"type":"assistant","session_id":"sess-1","_createdAt":"2026-06-20T10:00:05Z","message":{{"id":"resp-1","model":"claude-sonnet-4","usage":{{{usage}}}}}}}"#
    )
}

#[test]
fn an_sdk_assistant_record_becomes_one_observation() {
    let mut reader = reader();
    ingest(
        &mut reader,
        &sdk_assistant(
            r#""input_tokens":100,"output_tokens":50,"cache_read_input_tokens":20,"cache_creation_input_tokens":5"#,
        ),
    )
    .unwrap();
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key.client, CLIENT);
    assert_eq!(rows[0].key.session, "sess-1");
    assert_eq!(rows[0].model, "claude-sonnet-4");
    assert_eq!(
        rows[0].tokens,
        TokenCounts {
            input: 100,
            output: 50,
            cache_read: 20,
            cache_write: 5,
            reasoning: 0,
        }
    );
    assert_eq!(
        rows[0].billing_provider_hint.as_deref(),
        Some(DEFAULT_PROVIDER)
    );
}

#[test]
fn a_result_record_is_not_usage() {
    let mut reader = reader();
    // A `result` line is a session aggregate, not another API request.
    ingest(
        &mut reader,
        r#"{"type":"result","session_id":"s","message":{"model":"claude-sonnet-4","usage":{"input_tokens":100}}}"#,
    )
    .unwrap();
    assert!(reader.snapshot().is_empty());
}

#[test]
fn a_synthetic_model_is_not_usage() {
    let mut reader = reader();
    ingest(
        &mut reader,
        r#"{"type":"assistant","session_id":"s","timestamp":"2026-06-20T10:00:05Z","message":{"model":"<synthetic>","usage":{"input_tokens":100,"output_tokens":1}}}"#,
    )
    .unwrap();
    assert!(reader.snapshot().is_empty());
}

#[test]
fn an_all_zero_usage_record_is_not_usage() {
    let mut reader = reader();
    ingest(
        &mut reader,
        &sdk_assistant(r#""input_tokens":0,"output_tokens":0"#),
    )
    .unwrap();
    assert!(reader.snapshot().is_empty());
}

#[test]
fn a_missing_model_is_reported_as_unknown() {
    let mut reader = reader();
    ingest(
        &mut reader,
        r#"{"type":"assistant","session_id":"s","timestamp":"2026-06-20T10:00:05Z","message":{"usage":{"input_tokens":10,"output_tokens":2}}}"#,
    )
    .unwrap();
    assert_eq!(reader.snapshot()[0].model, "unknown");
}

#[test]
fn the_session_field_overrides_the_file_stem() {
    let mut reader = reader();
    ingest(
        &mut reader,
        &sdk_assistant(r#""input_tokens":10,"output_tokens":2"#),
    )
    .unwrap();
    assert_eq!(reader.snapshot()[0].key.session, "sess-1");
}

#[test]
fn the_file_stem_is_used_when_no_session_is_recorded() {
    let mut reader = PromaReader::new("from-stem");
    ingest(
        &mut reader,
        r#"{"type":"assistant","timestamp":"2026-06-20T10:00:05Z","message":{"usage":{"input_tokens":10,"output_tokens":2}}}"#,
    )
    .unwrap();
    assert_eq!(reader.snapshot()[0].key.session, "from-stem");
}

#[test]
fn a_missing_timestamp_falls_back_to_the_file_mtime() {
    let mut reader = reader().with_fallback_timestamp(Some(1_700_000_000_000));
    ingest(
        &mut reader,
        r#"{"type":"assistant","session_id":"s","message":{"usage":{"input_tokens":10,"output_tokens":2}}}"#,
    )
    .unwrap();
    assert_eq!(reader.snapshot()[0].timestamp_ms, 1_700_000_000_000);
}

#[test]
fn a_missing_timestamp_without_a_fallback_is_visible_unsupported() {
    let mut reader = reader();
    let error = ingest(
        &mut reader,
        r#"{"type":"assistant","session_id":"s","message":{"usage":{"input_tokens":10,"output_tokens":2}}}"#,
    )
    .unwrap_err();
    assert_eq!(error, ReaderError::Declined("Proma record timestamp"));
}

#[test]
fn a_negative_counter_is_rejected_not_clamped() {
    let mut reader = reader();
    let error = ingest(
        &mut reader,
        &sdk_assistant(r#""input_tokens":-5,"output_tokens":2"#),
    )
    .unwrap_err();
    assert_eq!(error, ReaderError::Declined("Proma counter"));
}

#[test]
fn an_explicit_service_tier_is_carried() {
    let mut reader = reader();
    ingest(
        &mut reader,
        r#"{"type":"assistant","session_id":"s","timestamp":"2026-06-20T10:00:05Z","message":{"model":"claude-sonnet-4","service_tier":"standard","usage":{"input_tokens":10,"output_tokens":2}}}"#,
    )
    .unwrap();
    assert_eq!(
        reader.snapshot()[0].service_tier.as_deref(),
        Some("standard")
    );
}

#[test]
fn a_pi_session_file_is_delegated_to_the_pi_reader() {
    let mut reader = reader();
    let lines = [
        r#"{"type":"session","id":"pi-session","version":3,"timestamp":"2026-08-20T15:27:35.811Z","cwd":"/tmp/workspace"}"#,
        r#"{"type":"message","id":"b2","timestamp":"2026-08-20T15:27:40.000Z","message":{"role":"assistant","model":"pi-model","provider":"pi-provider","usage":{"input":11,"output":3,"cacheRead":2,"cacheWrite":1}}}"#,
    ];
    for line in lines {
        ingest(&mut reader, line).unwrap();
    }
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    // Proma's own client identity, not `pi`.
    assert_eq!(rows[0].key.client, CLIENT);
    assert_eq!(rows[0].key.session, "pi-session");
    assert_eq!(rows[0].model, "pi-model");
}

#[test]
fn a_pi_file_with_no_usage_is_not_a_source() {
    let mut reader = reader();
    ingest(
        &mut reader,
        r#"{"type":"session","id":"pi-session","version":1}"#,
    )
    .unwrap();
    assert!(!reader.has_supported_usage());
}

#[test]
fn a_malformed_line_is_a_visible_candidate_failure() {
    let mut reader = reader();
    assert_eq!(
        ingest(&mut reader, "{not json").unwrap_err(),
        ReaderError::InvalidRecord
    );
}

#[test]
fn an_identity_is_stable_across_distinct_records() {
    let mut reader = reader();
    let mut second = sdk_assistant(r#""input_tokens":10,"output_tokens":2"#);
    second = second.replace("resp-1", "resp-2");
    ingest(
        &mut reader,
        &sdk_assistant(r#""input_tokens":10,"output_tokens":2"#),
    )
    .unwrap();
    ingest(&mut reader, &second).unwrap();
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 2);
    // Two different response ids are two distinct observations.
    assert_ne!(rows[0].dedup_key, rows[1].dedup_key);
}

#[test]
fn an_identical_record_is_kept_separate_by_ordinal() {
    let mut reader = reader();
    let record = sdk_assistant(r#""input_tokens":10,"output_tokens":2"#);
    ingest(&mut reader, &record).unwrap();
    ingest(&mut reader, &record).unwrap();
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 2);
    // No content or token-count fingerprint is invented as an identity, so a
    // byte-identical repeat stays a separate row rather than being merged.
    assert_ne!(rows[0].dedup_key, rows[1].dedup_key);
}
