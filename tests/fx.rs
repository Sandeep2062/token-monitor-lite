//! fx session-snapshot reader contract.

use token_monitor_lite::parsers::fx::{CLIENT, FxReader, UNKNOWN_MODEL};
use token_monitor_lite::parsers::{CostStatus, ReaderError};

/// A snapshot with one per-model entry.
fn snapshot(models: &str, top: &str) -> String {
    format!(
        r#"{{"schema_version":2,"session_id":"sess-1","snapshot":{{"models":[{models}]{top}}}}}"#
    )
}

fn model_entry(model: &str, input: u64, output: u64) -> String {
    format!(r#"{{"model":"{model}","input_tokens":{input},"output_tokens":{output}}}"#)
}

fn read(document: &str) -> Vec<token_monitor_lite::parsers::ResponseUsage> {
    let mut reader = FxReader::new("sess-1").with_fallback_timestamp(Some(1_782_718_000_000));
    reader
        .ingest(document.as_bytes())
        .expect("document is accepted");
    reader.snapshot()
}

#[test]
fn one_observation_is_emitted_per_model_entry() {
    let document = snapshot(
        &format!(
            "{},{}",
            model_entry("zai/glm-5", 100, 20),
            model_entry("anthropic/claude-sonnet-4", 50, 10)
        ),
        "",
    );
    let found = read(&document);
    assert_eq!(found.len(), 2);
    assert_eq!(found[0].model, "glm-5");
    assert_eq!(found[0].tokens.input, 100);
    assert_eq!(found[1].model, "claude-sonnet-4");
    assert_eq!(found[0].key.client, CLIENT);
}

#[test]
fn a_provider_prefixed_model_splits_into_model_and_hint() {
    let found = read(&snapshot(
        &model_entry("anthropic/claude-sonnet-4", 10, 5),
        "",
    ));
    assert_eq!(found[0].model, "claude-sonnet-4");
    assert_eq!(found[0].billing_provider_hint.as_deref(), Some("anthropic"));
}

#[test]
fn an_unprefixed_model_keeps_its_whole_value_and_carries_no_hint() {
    let found = read(&snapshot(&model_entry("glm-5", 10, 5), ""));
    assert_eq!(found[0].model, "glm-5");
    assert_eq!(found[0].billing_provider_hint, None);
}

#[test]
fn a_separator_with_an_empty_side_does_not_split() {
    let found = read(&snapshot(&model_entry("/glm-5", 10, 5), ""));
    assert_eq!(found[0].model, "/glm-5");
    assert_eq!(found[0].billing_provider_hint, None);
}

#[test]
fn the_identity_is_the_session_and_model() {
    let found = read(&snapshot(&model_entry("glm-5", 10, 5), ""));
    assert_eq!(found[0].dedup_key.as_deref(), Some("fx:sess-1:glm-5"));
    assert_eq!(found[0].key.session, "sess-1");
}

#[test]
fn cache_and_reasoning_are_read_as_disjoint_buckets() {
    let entry = r#"{"model":"glm-5","input_tokens":100,"output_tokens":40,"cache_read_tokens":30,"cache_write_tokens":10,"reasoning_tokens":15}"#;
    let found = read(&snapshot(entry, ""));
    assert_eq!(found[0].tokens.cache_read, 30);
    assert_eq!(found[0].tokens.cache_write, 10);
    assert_eq!(found[0].tokens.reasoning, 15);
    assert_eq!(found[0].tokens.input, 100);
    assert_eq!(found[0].tokens.output, 40);
}

#[test]
fn a_zero_token_model_entry_is_skipped() {
    let found = read(&snapshot(&model_entry("glm-5", 0, 0), ""));
    // The empty breakdown falls through to the aggregates, which are absent.
    assert!(found.is_empty());
}

#[test]
fn top_level_aggregates_become_one_unknown_model_observation() {
    // The session total must not be lost when the per-model breakdown is empty.
    let top = r#","input_tokens":200,"output_tokens":50,"request_count":3"#;
    let found = read(&snapshot("", top));
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].model, UNKNOWN_MODEL);
    assert_eq!(found[0].tokens.input, 200);
    assert_eq!(found[0].tokens.output, 50);
    assert_eq!(found[0].dedup_key.as_deref(), Some("fx:sess-1:fx-unknown"));
}

#[test]
fn the_aggregate_fallback_is_used_only_when_the_breakdown_produced_nothing() {
    let top = r#","input_tokens":999,"output_tokens":999"#;
    let found = read(&snapshot(&model_entry("glm-5", 10, 5), top));
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].model, "glm-5");
    assert_eq!(found[0].tokens.input, 10);
}

#[test]
fn an_aggregate_with_nothing_to_report_is_not_emitted() {
    assert!(read(&snapshot("", r#","input_tokens":0,"output_tokens":0"#)).is_empty());
}

#[test]
fn an_aggregate_carrying_only_requests_is_still_reported() {
    let top = r#","input_tokens":0,"output_tokens":0,"request_count":4"#;
    let found = read(&snapshot("", top));
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].model, UNKNOWN_MODEL);
}

#[test]
fn an_aggregate_carrying_only_a_cost_is_still_reported() {
    let top = r#","input_tokens":0,"output_tokens":0,"total_cost":0.5"#;
    let found = read(&snapshot("", top));
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].cost, CostStatus::Reported(500_000_000));
}

#[test]
fn a_reported_cost_is_preserved_as_authoritative() {
    let entry = r#"{"model":"glm-5","input_tokens":10,"output_tokens":5,"total_cost":0.25}"#;
    let found = read(&snapshot(entry, ""));
    assert_eq!(found[0].cost, CostStatus::Reported(250_000_000));
}

#[test]
fn an_absent_cost_is_not_an_authoritative_zero() {
    // A snapshot that never carried a cost must not look like one fx billed
    // nothing for.
    let found = read(&snapshot(&model_entry("glm-5", 10, 5), ""));
    assert_eq!(found[0].cost, CostStatus::Unavailable);
}

#[test]
fn an_explicit_null_cost_is_unreported_not_zero() {
    let entry = r#"{"model":"glm-5","input_tokens":10,"output_tokens":5,"total_cost":null}"#;
    let found = read(&snapshot(entry, ""));
    assert_eq!(found[0].cost, CostStatus::Unavailable);
}

#[test]
fn an_explicit_zero_cost_is_authoritative() {
    let entry = r#"{"model":"glm-5","input_tokens":10,"output_tokens":5,"total_cost":0.0}"#;
    let found = read(&snapshot(entry, ""));
    assert_eq!(found[0].cost, CostStatus::Reported(0));
}

#[test]
fn a_negative_cost_is_unreported_rather_than_clamped_to_zero() {
    // A negative aggregate cannot be a bill fx meant, so it must stay eligible
    // for repricing.
    let entry = r#"{"model":"glm-5","input_tokens":10,"output_tokens":5,"total_cost":-1.5}"#;
    let found = read(&snapshot(entry, ""));
    assert_eq!(found[0].cost, CostStatus::Unavailable);
}

#[test]
fn the_session_id_comes_from_the_snapshot_not_the_caller() {
    let document =
        r#"{"session_id":"from-file","snapshot":{"models":[{"model":"m","input_tokens":10}]}}"#;
    let found = read(document);
    assert_eq!(found[0].key.session, "from-file");
    assert_eq!(found[0].dedup_key.as_deref(), Some("fx:from-file:m"));
}

#[test]
fn an_empty_session_id_keeps_the_callers_session() {
    let document = r#"{"session_id":"  ","snapshot":{"models":[{"model":"m","input_tokens":10}]}}"#;
    let found = read(document);
    assert_eq!(found[0].key.session, "sess-1");
}

#[test]
fn a_document_without_a_snapshot_carries_no_usage() {
    assert!(read(r#"{"schema_version":2}"#).is_empty());
}

#[test]
fn malformed_json_is_a_visible_rejection() {
    let mut reader = FxReader::new("s").with_fallback_timestamp(Some(1_000));
    assert_eq!(
        reader.ingest(b"not json").expect_err("malformed"),
        ReaderError::InvalidRecord
    );
}

#[test]
fn a_negative_counter_is_a_visible_rejection() {
    let mut reader = FxReader::new("s").with_fallback_timestamp(Some(1_000));
    let document = r#"{"snapshot":{"models":[{"model":"m","input_tokens":-5}]}}"#;
    assert_eq!(
        reader
            .ingest(document.as_bytes())
            .expect_err("negative counter"),
        ReaderError::InvalidRecord
    );
}

#[test]
fn the_timestamp_is_the_file_mtime_because_the_sibling_is_not_read() {
    // `session.json` would carry the authoritative timestamps, but a per-source
    // byte reader cannot open it.
    let found = read(&snapshot(&model_entry("m", 10, 5), ""));
    assert_eq!(found[0].timestamp_ms, 1_782_718_000_000);
}

#[test]
fn a_revision_replaces_rather_than_appends() {
    let document = snapshot(&model_entry("m", 10, 5), "");
    let mut reader = FxReader::new("s").with_fallback_timestamp(Some(1_000));
    reader.ingest(document.as_bytes()).expect("accepted");
    reader.ingest(document.as_bytes()).expect("accepted");
    assert_eq!(reader.snapshot().len(), 1);
    assert!(reader.has_supported_usage());
}
