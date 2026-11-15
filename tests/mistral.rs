//! Mistral Vibe session-metadata reader contract.

use token_monitor_lite::parsers::mistral::{CLIENT, DEFAULT_MODEL_ALIAS, VibeReader};
use token_monitor_lite::parsers::{CostStatus, ReaderError};

fn read(document: &str) -> Vec<token_monitor_lite::parsers::ResponseUsage> {
    let mut reader =
        VibeReader::new("dir-session").with_fallback_timestamp(Some(1_782_718_000_000));
    reader
        .ingest(document.as_bytes())
        .expect("document is accepted");
    reader.snapshot()
}

/// A Vibe 2.x `meta.json` with the given stats object.
fn meta(stats: &str) -> String {
    format!(r#"{{"session_id":"sess-1","stats":{{{stats}}}}}"#)
}

#[test]
fn the_cached_count_is_split_out_of_the_prompt_not_added() {
    // `session_cached_tokens` is a subset of `session_prompt_tokens`: upstream's
    // own cost function subtracts it before charging the input rate.
    let found = read(&meta(
        r#""session_prompt_tokens":1000,"session_completion_tokens":200,"session_cached_tokens":300"#,
    ));
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].tokens.input, 700);
    assert_eq!(found[0].tokens.cache_read, 300);
    assert_eq!(found[0].tokens.output, 200);
    // The buckets sum back to the reported prompt + completion.
    assert_eq!(found[0].tokens.input + found[0].tokens.cache_read, 1000);
    assert_eq!(found[0].key.client, CLIENT);
}

#[test]
fn cached_larger_than_the_prompt_floors_at_zero() {
    let found = read(&meta(
        r#""session_prompt_tokens":100,"session_completion_tokens":10,"session_cached_tokens":500"#,
    ));
    assert_eq!(found[0].tokens.input, 0);
    assert_eq!(found[0].tokens.cache_read, 500);
}

#[test]
fn a_legacy_flat_file_wraps_its_metadata() {
    // Vibe 1.x puts the record under a `metadata` key; Vibe 2.x has it at the
    // root.
    let document = r#"{"metadata":{"session_id":"legacy-1","stats":{"session_prompt_tokens":100,"session_completion_tokens":20}}}"#;
    let found = read(document);
    assert_eq!(found[0].key.session, "legacy-1");
    assert_eq!(found[0].tokens.input, 100);
}

#[test]
fn a_document_without_stats_carries_no_usage() {
    assert!(read(r#"{"session_id":"s"}"#).is_empty());
}

#[test]
fn an_all_zero_session_carries_no_usage() {
    assert!(
        read(&meta(
            r#""session_prompt_tokens":0,"session_completion_tokens":0,"session_cached_tokens":0"#,
        ))
        .is_empty()
    );
}

#[test]
fn the_pinned_alias_wins() {
    let document = r#"{"session_id":"s","config":{"active_model":"mistral-large-latest"},"stats":{"session_prompt_tokens":10}}"#;
    let found = read(document);
    assert_eq!(found[0].model, "mistral-large-latest");
}

#[test]
fn a_blank_active_model_is_the_unpinned_sentinel_and_falls_through() {
    // Upstream documents `""` as "follow the configured default", not as
    // missing data. Accepting it would leave the model blank and price the
    // session at zero.
    let document = r#"{"session_id":"s","config":{"active_model":"","models":{"m":{"alias":"mistral-small-latest"}}},"stats":{"session_prompt_tokens":10}}"#;
    let found = read(document);
    assert_eq!(found[0].model, "mistral-small-latest");
}

#[test]
fn a_single_configured_model_is_used_when_unpinned() {
    let document = r#"{"session_id":"s","config":{"models":{"key-1":{"alias":"mistral-small-latest"}}},"stats":{"session_prompt_tokens":10}}"#;
    let found = read(document);
    assert_eq!(found[0].model, "mistral-small-latest");
}

#[test]
fn a_configured_model_without_an_alias_uses_its_map_key() {
    let document = r#"{"session_id":"s","config":{"models":{"mistral-small-latest":{}}},"stats":{"session_prompt_tokens":10}}"#;
    let found = read(document);
    assert_eq!(found[0].model, "mistral-small-latest");
}

#[test]
fn several_configured_models_resolve_deterministically() {
    // The transcript cannot say which one ran, so the choice must not depend on
    // map iteration order.
    let document = r#"{"session_id":"s","config":{"models":{"a":{"alias":"zeta-model"},"b":{"alias":"alpha-model"}}},"stats":{"session_prompt_tokens":10}}"#;
    let first = read(document);
    let second = read(document);
    assert_eq!(first[0].model, "alpha-model");
    assert_eq!(first[0].model, second[0].model);
}

#[test]
fn several_models_prefer_the_published_default_alias() {
    let document = format!(
        r#"{{"session_id":"s","config":{{"models":{{"a":{{"alias":"zeta-model"}},"b":{{"alias":"{DEFAULT_MODEL_ALIAS}"}}}}}},"stats":{{"session_prompt_tokens":10}}}}"#
    );
    let found = read(&document);
    assert_eq!(found[0].model, DEFAULT_MODEL_ALIAS);
}

#[test]
fn an_unpinned_session_with_no_configured_models_uses_the_published_default() {
    let found = read(&meta(r#""session_prompt_tokens":10"#));
    assert_eq!(found[0].model, DEFAULT_MODEL_ALIAS);
}

#[test]
fn the_session_id_comes_from_the_record() {
    let found = read(r#"{"session_id":"record-session","stats":{"session_prompt_tokens":10}}"#);
    assert_eq!(found[0].key.session, "record-session");
    assert_eq!(found[0].dedup_key.as_deref(), Some("vibe:record-session"));
}

#[test]
fn a_blank_session_id_keeps_the_callers_session() {
    let found = read(r#"{"session_id":"   ","stats":{"session_prompt_tokens":10}}"#);
    assert_eq!(found[0].key.session, "dir-session");
}

#[test]
fn the_end_time_is_preferred_over_the_start_time() {
    let document = r#"{"session_id":"s","start_time":"2026-07-09T10:00:00Z","end_time":"2026-07-09T11:00:00Z","stats":{"session_prompt_tokens":10}}"#;
    let found = read(document);
    assert_eq!(found[0].timestamp_ms, 1_783_594_800_000);
}

#[test]
fn the_start_time_is_used_when_the_end_time_is_absent() {
    let document = r#"{"session_id":"s","start_time":"2026-07-09T11:00:00Z","stats":{"session_prompt_tokens":10}}"#;
    let found = read(document);
    assert_eq!(found[0].timestamp_ms, 1_783_594_800_000);
}

#[test]
fn a_session_with_no_timestamps_falls_back_to_the_file_mtime() {
    let found = read(&meta(r#""session_prompt_tokens":10"#));
    assert_eq!(found[0].timestamp_ms, 1_782_718_000_000);
}

#[test]
fn an_origin_directory_becomes_a_label_never_a_workspace_key() {
    // An absolute path is refused in every runtime identity column, so only the
    // final component survives.
    let document = r#"{"session_id":"s","origin_directory":"/work/demo","stats":{"session_prompt_tokens":10}}"#;
    let found = read(document);
    assert_eq!(found[0].workspace_key, None);
    assert_eq!(found[0].workspace_label.as_deref(), Some("demo"));
}

#[test]
fn the_working_directory_environment_key_is_an_alternate_spelling() {
    let document = r#"{"session_id":"s","environment":{"working_directory":"/work/demo/"},"stats":{"session_prompt_tokens":10}}"#;
    let found = read(document);
    assert_eq!(found[0].workspace_label.as_deref(), Some("demo"));
}

#[test]
fn the_title_is_carried_as_presentation_metadata() {
    let document = r#"{"session_id":"s","title":"  Refactor the reader  ","stats":{"session_prompt_tokens":10}}"#;
    let found = read(document);
    assert_eq!(found[0].title.as_deref(), Some("Refactor the reader"));
}

#[test]
fn a_blank_title_is_not_carried() {
    let document = r#"{"session_id":"s","title":"   ","stats":{"session_prompt_tokens":10}}"#;
    let found = read(document);
    assert_eq!(found[0].title, None);
}

#[test]
fn cost_is_never_claimed_and_cache_write_stays_zero() {
    let found = read(&meta(
        r#""session_prompt_tokens":10,"session_completion_tokens":5"#,
    ));
    assert_eq!(found[0].cost, CostStatus::Unavailable);
    assert_eq!(found[0].billing_provider_hint, None);
    assert_eq!(found[0].tokens.cache_write, 0);
    assert_eq!(found[0].tokens.reasoning, 0);
    assert_eq!(found[0].cache_write_1h, 0);
}

#[test]
fn malformed_json_is_a_visible_rejection() {
    let mut reader = VibeReader::new("s").with_fallback_timestamp(Some(1_000));
    assert_eq!(
        reader.ingest(b"not json").expect_err("malformed"),
        ReaderError::InvalidRecord
    );
}

#[test]
fn a_negative_counter_is_a_visible_rejection() {
    let mut reader = VibeReader::new("s").with_fallback_timestamp(Some(1_000));
    let document = meta(r#""session_prompt_tokens":-5"#);
    assert_eq!(
        reader
            .ingest(document.as_bytes())
            .expect_err("negative counter"),
        ReaderError::InvalidRecord
    );
}

#[test]
fn a_revision_replaces_rather_than_appends() {
    let document = meta(r#""session_prompt_tokens":10"#);
    let mut reader = VibeReader::new("s").with_fallback_timestamp(Some(1_000));
    reader.ingest(document.as_bytes()).expect("accepted");
    reader.ingest(document.as_bytes()).expect("accepted");
    assert_eq!(reader.snapshot().len(), 1);
    assert!(reader.has_supported_usage());
}
