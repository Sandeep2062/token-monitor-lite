//! Synthetic Goose session-row cases adapted from the pinned Tokscale
//! `goose.rs` reader. Goose has no reasoning column; a non-zero remainder is
//! refused rather than inferred.

use token_monitor_lite::accounting::TokenCounts;
use token_monitor_lite::parsers::ReaderError;
use token_monitor_lite::parsers::goose::{GooseReader, GooseRow};

fn row<'a>(
    id: &'a str,
    input: Option<i64>,
    output: Option<i64>,
    total: Option<i64>,
    accumulated_input: Option<i64>,
    accumulated_output: Option<i64>,
    accumulated_total: Option<i64>,
) -> GooseRow<'a> {
    GooseRow {
        id,
        model_config_json: r#"{"model_name":"claude-sonnet-4-5","context_limit":200000}"#,
        provider_name: Some("anthropic"),
        created_at: "2026-11-13T10:00:00Z",
        total_tokens: total,
        input_tokens: input,
        output_tokens: output,
        accumulated_total_tokens: accumulated_total,
        accumulated_input_tokens: accumulated_input,
        accumulated_output_tokens: accumulated_output,
    }
}

#[test]
fn accumulated_counters_win_and_produce_one_session_observation() {
    let mut reader = GooseReader::new();
    reader
        .ingest(&row(
            "session-a",
            Some(10),
            Some(5),
            Some(15),
            Some(100),
            Some(50),
            Some(150),
        ))
        .unwrap();
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key.client, "goose");
    assert_eq!(rows[0].key.session, "session-a");
    assert_eq!(rows[0].model, "claude-sonnet-4-5");
    assert_eq!(rows[0].billing_provider_hint.as_deref(), Some("anthropic"));
    assert_eq!(rows[0].dedup_key.as_deref(), Some("goose:session-a"));
    assert_eq!(rows[0].timestamp_ms, 1_794_564_000_000);
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
}

#[test]
fn accumulated_zero_shadows_non_accumulated_values() {
    let mut reader = GooseReader::new();
    reader
        .ingest(&row(
            "session-b",
            Some(100),
            Some(50),
            Some(150),
            Some(0),
            Some(0),
            Some(0),
        ))
        .unwrap();
    assert!(reader.snapshot().is_empty());
}

#[test]
fn unclassified_total_remainder_is_visible_unsupported() {
    let mut reader = GooseReader::new();
    let result = reader.ingest(&row(
        "session-c",
        Some(100),
        Some(50),
        Some(200),
        None,
        None,
        None,
    ));
    assert!(matches!(result, Err(ReaderError::Unsupported(_))));
}

#[test]
fn invalid_model_and_negative_counters_are_rejected() {
    let mut reader = GooseReader::new();
    let mut invalid_model = row("session-d", Some(1), Some(1), Some(2), None, None, None);
    invalid_model.model_config_json = "not json";
    reader.ingest(&invalid_model).unwrap();
    assert!(reader.snapshot().is_empty());

    let mut negative = row("session-e", Some(-1), Some(1), Some(0), None, None, None);
    negative.created_at = "2026-11-13T10:00:00Z";
    assert!(matches!(
        reader.ingest(&negative),
        Err(ReaderError::InvalidRecord)
    ));
}
