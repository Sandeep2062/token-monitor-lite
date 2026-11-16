//! Synthetic Hermes session-row cases adapted from the pinned Tokscale
//! `hermes.rs` reader. Hermes records per-session aggregates; `reasoning_tokens`
//! is a subset of `output_tokens`, and the native reader refuses a negative or
//! over-large counter rather than clamping it.

use token_monitor_lite::accounting::TokenCounts;
use token_monitor_lite::parsers::ReaderError;
use token_monitor_lite::parsers::hermes::{HermesReader, HermesRow};

#[derive(Clone, Copy)]
struct Counts {
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
    reasoning: i64,
}

const ZERO: Counts = Counts {
    input: 0,
    output: 0,
    cache_read: 0,
    cache_write: 0,
    reasoning: 0,
};

fn counts(input: i64, output: i64, reasoning: i64) -> Counts {
    Counts {
        input,
        output,
        reasoning,
        ..ZERO
    }
}

fn row<'a>(
    session_id: &'a str,
    model: &'a str,
    provider: Option<&'a str>,
    counts: Counts,
) -> HermesRow<'a> {
    HermesRow {
        session_id,
        model,
        billing_provider: provider,
        started_at: 1_794_564_000.0,
        input_tokens: Some(counts.input),
        output_tokens: Some(counts.output),
        cache_read_tokens: Some(counts.cache_read),
        cache_write_tokens: Some(counts.cache_write),
        reasoning_tokens: Some(counts.reasoning),
    }
}

#[test]
fn per_model_row_splits_reasoning_out_of_inclusive_output() {
    let mut reader = HermesReader::new();
    reader
        .ingest(&row(
            "session-a",
            "claude-sonnet-4-5",
            Some("anthropic"),
            Counts {
                input: 100,
                output: 50,
                cache_read: 20,
                cache_write: 10,
                reasoning: 5,
            },
        ))
        .unwrap();
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key.client, "hermes");
    assert_eq!(rows[0].key.session, "session-a");
    assert_eq!(rows[0].model, "claude-sonnet-4-5");
    assert_eq!(rows[0].billing_provider_hint.as_deref(), Some("anthropic"));
    assert_eq!(rows[0].agent.as_deref(), Some("Hermes Agent"));
    assert_eq!(
        rows[0].dedup_key.as_deref(),
        Some("hermes:session-a:claude-sonnet-4-5:anthropic")
    );
    assert_eq!(rows[0].timestamp_ms, 1_794_564_000_000);
    assert_eq!(
        rows[0].tokens,
        TokenCounts {
            input: 100,
            output: 45,
            cache_read: 20,
            cache_write: 10,
            reasoning: 5,
        }
    );
}

#[test]
fn null_and_empty_providers_keep_distinct_identities() {
    let mut reader = HermesReader::new();
    reader
        .ingest(&row("session-b", "gpt-5", None, counts(10, 5, 0)))
        .unwrap();
    reader
        .ingest(&row("session-b", "gpt-5", Some(""), counts(10, 5, 0)))
        .unwrap();
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 2, "SQLite groups NULL and '' separately");
    assert_eq!(
        rows[0].dedup_key.as_deref(),
        Some("hermes:session-b:gpt-5:<null>")
    );
    assert_eq!(
        rows[1].dedup_key.as_deref(),
        Some("hermes:session-b:gpt-5:")
    );
    // Neither provider survives validation, so neither becomes a hint.
    assert!(rows.iter().all(|row| row.billing_provider_hint.is_none()));
}

#[test]
fn zero_usage_rows_are_skipped_without_becoming_observations() {
    let mut reader = HermesReader::new();
    reader
        .ingest(&row("session-c", "gpt-5", None, ZERO))
        .unwrap();
    assert!(reader.snapshot().is_empty());
    assert!(!reader.has_supported_usage());
}

#[test]
fn negative_counters_are_rejected_rather_than_clamped() {
    let mut reader = HermesReader::new();
    assert!(matches!(
        reader.ingest(&row("session-d", "gpt-5", None, counts(-1, 5, 0))),
        Err(ReaderError::InvalidRecord)
    ));
    assert!(matches!(
        reader.ingest(&row("session-d", "gpt-5", None, counts(1, 5, -1))),
        Err(ReaderError::InvalidRecord)
    ));
    assert!(reader.snapshot().is_empty());
}

#[test]
fn reasoning_above_output_is_visible_unsupported() {
    let mut reader = HermesReader::new();
    let result = reader.ingest(&row("session-e", "gpt-5", None, counts(10, 5, 6)));
    assert!(matches!(
        result,
        Err(ReaderError::Unsupported(_) | ReaderError::Declined(_))
    ));
}

#[test]
fn missing_model_and_invalid_identity_are_rejected() {
    let mut reader = HermesReader::new();
    let mut blank_model = row("session-f", "  ", None, counts(10, 5, 0));
    blank_model.model = "   ";
    assert!(matches!(
        reader.ingest(&blank_model),
        Err(ReaderError::InvalidRecord)
    ));
    let empty_session = row("", "gpt-5", None, counts(10, 5, 0));
    assert!(matches!(
        reader.ingest(&empty_session),
        Err(ReaderError::InvalidRecord)
    ));
}

#[test]
fn unsupported_timestamp_falls_back_to_the_file_mtime() {
    let mut reader = HermesReader::new().with_fallback_timestamp(Some(1_700_000_000_000));
    let mut incomplete = row("session-g", "gpt-5", None, counts(10, 5, 0));
    incomplete.started_at = 0.0;
    incomplete.input_tokens = Some(10);
    reader.ingest(&incomplete).unwrap();
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    // A zero second count is a real instant (the epoch), not a missing one.
    assert_eq!(rows[0].timestamp_ms, 0);

    let mut reader = HermesReader::new().with_fallback_timestamp(Some(1_700_000_000_000));
    let mut negative = row("session-h", "gpt-5", None, counts(10, 5, 0));
    negative.started_at = -1.0;
    reader.ingest(&negative).unwrap();
    assert_eq!(reader.snapshot()[0].timestamp_ms, 1_700_000_000_000);
}
