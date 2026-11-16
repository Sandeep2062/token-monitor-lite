//! Synthetic ZCode cases adapted from the pinned Tokscale `zcode.rs` reader.
//! Both stores are covered: the JSONL transcript and the SQLite `model_usage`
//! table.

use token_monitor_lite::accounting::TokenCounts;
use token_monitor_lite::parsers::ReaderError;
use token_monitor_lite::parsers::zcode::{
    CLIENT, DEFAULT_MODEL, PROVIDER, ZcodeReader, ZcodeUsageRow, normalize_row,
};

fn ingest(reader: &mut ZcodeReader, line: &str) -> Result<(), ReaderError> {
    reader.ingest(line.as_bytes())
}

fn reader() -> ZcodeReader {
    ZcodeReader::new("fallback-session")
}

#[test]
fn an_assistant_line_with_usage_becomes_one_observation() {
    let mut reader = reader();
    ingest(
        &mut reader,
        r#"{"role":"assistant","sessionId":"s1","timestamp":"2026-06-20T10:00:05Z","model":"glm-5.2","usage":{"input_tokens":100,"output_tokens":50,"input_cache_read":20}}"#,
    )
    .unwrap();
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key.client, CLIENT);
    assert_eq!(rows[0].key.session, "s1");
    assert_eq!(rows[0].model, "glm-5.2");
    assert_eq!(rows[0].billing_provider_hint.as_deref(), Some(PROVIDER));
    // No reported total, so the shape cannot be detected and the raw counters
    // pass through unchanged rather than being guessed at.
    assert_eq!(
        rows[0].tokens,
        TokenCounts {
            input: 100,
            output: 50,
            cache_read: 20,
            cache_write: 0,
            reasoning: 0,
        }
    );
    assert_eq!(rows[0].timestamp_ms, 1_781_949_605_000);
}

#[test]
fn a_non_assistant_line_contributes_nothing() {
    let mut reader = reader();
    ingest(
        &mut reader,
        r#"{"role":"user","sessionId":"s1","content":"hello"}"#,
    )
    .unwrap();
    assert!(reader.snapshot().is_empty());
    assert!(!reader.has_supported_usage());
}

#[test]
fn an_assistant_line_without_usage_is_not_estimated() {
    let mut reader = reader();
    ingest(
        &mut reader,
        r#"{"role":"assistant","sessionId":"s1","timestamp":"2026-06-20T10:00:05Z","content":"a long reply with no recorded usage"}"#,
    )
    .unwrap();
    // The pinned reader estimates ~4 chars/token here; the native reader
    // contributes nothing rather than fabricating a measurement.
    assert!(reader.snapshot().is_empty());
}

#[test]
fn an_empty_usage_block_does_not_shadow_token_usage() {
    let mut reader = reader();
    ingest(
        &mut reader,
        r#"{"role":"assistant","sessionId":"s","timestamp":"2026-06-20T10:00:05Z","content":"bye","usage":{},"token_usage":{"input_tokens":321,"output_tokens":123,"input_cache_read":7}}"#,
    )
    .unwrap();
    let tokens = reader.snapshot()[0].tokens;
    assert_eq!(tokens.input, 321);
    assert_eq!(tokens.output, 123);
    assert_eq!(tokens.cache_read, 7);
}

#[test]
fn alternative_field_names_are_accepted() {
    let mut reader = reader();
    ingest(
        &mut reader,
        r#"{"role":"assistant","sessionId":"s3","timestamp":"2026-06-20T10:00:05Z","content":"bye","token_usage":{"prompt_tokens":200,"completion_tokens":100}}"#,
    )
    .unwrap();
    let tokens = reader.snapshot()[0].tokens;
    assert_eq!(tokens.input, 200);
    assert_eq!(tokens.output, 100);
}

#[test]
fn a_mid_session_model_switch_prices_later_turns_under_the_new_model() {
    let mut reader = reader();
    for line in [
        r#"{"role":"assistant","sessionId":"s","timestamp":"2026-06-20T10:00:01Z","model":"GLM-5.2","usage":{"input_tokens":10,"output_tokens":5}}"#,
        r#"{"role":"assistant","sessionId":"s","timestamp":"2026-06-20T10:00:02Z","model":"glm-5-turbo","usage":{"input_tokens":10,"output_tokens":5}}"#,
        // No model: inherits the most recently seen one.
        r#"{"role":"assistant","sessionId":"s","timestamp":"2026-06-20T10:00:03Z","usage":{"input_tokens":10,"output_tokens":5}}"#,
    ] {
        ingest(&mut reader, line).unwrap();
    }
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].model, "glm-5.2");
    assert_eq!(rows[1].model, "glm-5-turbo");
    // Documented default model is only used before any model has been seen.
    assert_eq!(rows[2].model, "glm-5-turbo");
}

#[test]
fn the_default_model_applies_before_any_model_is_named() {
    let mut reader = reader();
    ingest(
        &mut reader,
        r#"{"role":"assistant","sessionId":"s","timestamp":"2026-06-20T10:00:01Z","usage":{"input_tokens":10,"output_tokens":5}}"#,
    )
    .unwrap();
    assert_eq!(reader.snapshot()[0].model, DEFAULT_MODEL);
}

#[test]
fn an_inclusive_total_shape_subtracts_the_overlap_once() {
    let mut reader = reader();
    // total == input + output (150) while the additive sum is 245, which proves
    // the row is cache/reasoning-inclusive.
    ingest(
        &mut reader,
        r#"{"role":"assistant","sessionId":"s","timestamp":"2026-06-20T10:00:01Z","model":"glm-5.2","usage":{"input_tokens":100,"output_tokens":50,"reasoning":10,"input_cache_read":80,"input_cache_creation":5,"total":150}}"#,
    )
    .unwrap();
    let tokens = reader.snapshot()[0].tokens;
    assert_eq!(tokens.input, 15);
    assert_eq!(tokens.output, 40);
    assert_eq!(tokens.cache_read, 80);
    assert_eq!(tokens.cache_write, 5);
    assert_eq!(tokens.reasoning, 10);
}

#[test]
fn an_already_exclusive_row_is_left_alone() {
    let mut reader = reader();
    // total equals the fully additive sum, so no subtraction is correct.
    ingest(
        &mut reader,
        r#"{"role":"assistant","sessionId":"s","timestamp":"2026-06-20T10:00:01Z","model":"glm-5.2","usage":{"input_tokens":20,"output_tokens":30,"reasoning":5,"input_cache_read":80,"input_cache_creation":10,"total":145}}"#,
    )
    .unwrap();
    let tokens = reader.snapshot()[0].tokens;
    assert_eq!(tokens.input, 20);
    assert_eq!(tokens.output, 30);
}

#[test]
fn an_all_zero_usage_block_is_not_usage() {
    let mut reader = reader();
    ingest(
        &mut reader,
        r#"{"role":"assistant","sessionId":"s","timestamp":"2026-06-20T10:00:01Z","usage":{"input_tokens":0,"output_tokens":0}}"#,
    )
    .unwrap();
    assert!(reader.snapshot().is_empty());
}

#[test]
fn a_negative_counter_is_rejected_not_clamped() {
    let mut reader = reader();
    let error = ingest(
        &mut reader,
        r#"{"role":"assistant","sessionId":"s","timestamp":"2026-06-20T10:00:01Z","usage":{"input_tokens":-5,"output_tokens":5}}"#,
    )
    .unwrap_err();
    assert_eq!(error, ReaderError::InvalidRecord);
}

#[test]
fn a_missing_timestamp_falls_back_to_the_file_mtime() {
    let mut reader = reader().with_fallback_timestamp(Some(1_700_000_000_000));
    ingest(
        &mut reader,
        r#"{"role":"assistant","sessionId":"s","usage":{"input_tokens":10,"output_tokens":5}}"#,
    )
    .unwrap();
    assert_eq!(reader.snapshot()[0].timestamp_ms, 1_700_000_000_000);
}

#[test]
fn a_missing_timestamp_without_a_fallback_is_visible_unsupported() {
    let mut reader = reader();
    let error = ingest(
        &mut reader,
        r#"{"role":"assistant","sessionId":"s","usage":{"input_tokens":10,"output_tokens":5}}"#,
    )
    .unwrap_err();
    assert_eq!(error, ReaderError::Declined("ZCode line timestamp"));
}

#[test]
fn the_line_session_overrides_the_caller_supplied_one() {
    let mut reader = reader();
    ingest(
        &mut reader,
        r#"{"role":"assistant","sessionId":"from-line","timestamp":"2026-06-20T10:00:01Z","usage":{"input_tokens":10,"output_tokens":5}}"#,
    )
    .unwrap();
    assert_eq!(reader.snapshot()[0].key.session, "from-line");
}

#[test]
fn a_malformed_line_is_a_visible_candidate_failure() {
    let mut reader = reader();
    assert_eq!(
        ingest(&mut reader, "{not json").unwrap_err(),
        ReaderError::InvalidRecord
    );
}

fn usage_row<'a>(id: &'a str) -> ZcodeUsageRow<'a> {
    ZcodeUsageRow {
        id,
        session_id: Some("sess_1"),
        model_id: Some("GLM-5.2"),
        started_at: Some(1_782_718_000_000),
        completed_at: Some(1_782_718_001_000),
        fallback_timestamp_ms: 0,
        input: Some(100),
        output: Some(20),
        reasoning: Some(5),
        cache_read: Some(7),
        cache_write: Some(3),
        computed_total: Some(120),
        legacy_schema: false,
    }
}

#[test]
fn a_sqlite_row_becomes_one_observation() {
    let row = normalize_row(&usage_row("usage_1")).unwrap().unwrap();
    assert_eq!(row.key.client, CLIENT);
    assert_eq!(row.key.session, "sess_1");
    assert_eq!(row.model, "glm-5.2");
    // 120 == 100 + 20, so the row is inclusive and the overlap is removed once.
    assert_eq!(
        row.tokens,
        TokenCounts {
            input: 90,
            output: 15,
            cache_read: 7,
            cache_write: 3,
            reasoning: 5,
        }
    );
    assert_eq!(row.timestamp_ms, 1_782_718_000_000);
    assert_eq!(row.dedup_key.as_deref(), Some("zcode-sqlite:usage_1"));
}

#[test]
fn a_sqlite_row_anchors_on_started_at() {
    let row = normalize_row(&usage_row("usage_1")).unwrap().unwrap();
    assert_eq!(row.timestamp_ms, 1_782_718_000_000);
}

#[test]
fn a_sqlite_row_without_started_at_falls_back_to_completed_at() {
    let mut input = usage_row("usage_1");
    input.started_at = None;
    let row = normalize_row(&input).unwrap().unwrap();
    assert_eq!(row.timestamp_ms, 1_782_718_001_000);
}

#[test]
fn a_legacy_schema_row_subtracts_the_overlap_unconditionally() {
    let mut input = usage_row("usage_legacy");
    input.computed_total = None;
    input.legacy_schema = true;
    let row = normalize_row(&input).unwrap().unwrap();
    // No total column exists to detect the shape, so every legacy row is
    // assumed inclusive.
    assert_eq!(row.tokens.input, 90);
    assert_eq!(row.tokens.output, 15);
}

#[test]
fn a_modern_row_with_a_null_total_passes_through_unchanged() {
    let mut input = usage_row("usage_null_total");
    input.computed_total = None;
    input.legacy_schema = false;
    let row = normalize_row(&input).unwrap().unwrap();
    // Subtracting here would undercount an already-exclusive row.
    assert_eq!(row.tokens.input, 100);
    assert_eq!(row.tokens.output, 20);
}

#[test]
fn a_sqlite_row_with_no_tokens_is_not_usage() {
    let mut input = usage_row("usage_zero");
    for counter in [
        &mut input.input,
        &mut input.output,
        &mut input.reasoning,
        &mut input.cache_read,
        &mut input.cache_write,
    ] {
        *counter = Some(0);
    }
    input.computed_total = Some(0);
    assert!(normalize_row(&input).unwrap().is_none());
}

#[test]
fn a_negative_sqlite_counter_is_visible_unsupported() {
    let mut input = usage_row("usage_negative");
    input.input = Some(-1);
    assert_eq!(
        normalize_row(&input).unwrap_err(),
        ReaderError::Unsupported("ZCode input")
    );
}

#[test]
fn an_overlap_larger_than_the_input_is_rejected_not_saturated() {
    let mut input = usage_row("usage_overlap");
    input.input = Some(5);
    input.cache_read = Some(50);
    input.computed_total = None;
    input.legacy_schema = true;
    // The pinned reader saturates here; an overlap that cannot have fitted
    // inside the value means the row is not interpretable.
    assert_eq!(
        normalize_row(&input).unwrap_err(),
        ReaderError::Declined("ZCode cache overlap")
    );
}

#[test]
fn a_sqlite_row_without_a_model_uses_the_documented_default() {
    let mut input = usage_row("usage_1");
    input.model_id = None;
    assert_eq!(normalize_row(&input).unwrap().unwrap().model, DEFAULT_MODEL);
}

#[test]
fn a_control_character_in_the_row_id_is_rejected() {
    let input = usage_row("usage\u{7}");
    assert_eq!(
        normalize_row(&input).unwrap_err(),
        ReaderError::InvalidRecord
    );
}
