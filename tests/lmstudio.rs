//! LM Studio server-log reader contract.

use token_monitor_lite::parsers::lmstudio::{CLIENT, LmstudioReader};
use token_monitor_lite::parsers::{CostStatus, ReaderError};

/// One pretty-printed response, shaped like a real server log.
///
/// The usage object deliberately spans several lines, because that is what LM
/// Studio writes and a line-oriented parser would miss it.
fn record(id: &str, model: &str, prompt: u64, completion: u64) -> String {
    format!(
        "[2026-07-09 11:00:00][INFO][{model}]\n\
         Final response: {{\n\
         \x20 \"id\": \"{id}\",\n\
         \x20 \"model\": \"{model}\",\n\
         \x20 \"usage\": {{\n\
         \x20   \"prompt_tokens\": {prompt},\n\
         \x20   \"completion_tokens\": {completion},\n\
         \x20   \"total_tokens\": {}\n\
         \x20 }}\n\
         }}\n",
        prompt + completion
    )
}

fn messages(log: &str) -> Vec<token_monitor_lite::parsers::ResponseUsage> {
    let mut reader = LmstudioReader::new("log").with_fallback_timestamp(Some(1_000));
    reader.ingest(log.as_bytes()).expect("log is accepted");
    reader.snapshot()
}

/// The epoch milliseconds of a naive wall clock in the host's own zone.
///
/// A log header carries no zone designator, so the only defensible reading is
/// the zone of the machine that wrote it. Tests must therefore compute the
/// expected value in the local zone rather than hardcoding a UTC instant, or
/// they only pass on a host that happens to run at UTC.
fn local_wall_clock_ms(text: &str) -> i64 {
    use chrono::TimeZone;
    let naive = chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S").unwrap();
    match chrono::Local.from_local_datetime(&naive) {
        chrono::LocalResult::Single(local) => local.timestamp_millis(),
        chrono::LocalResult::Ambiguous(earlier, _) => earlier.timestamp_millis(),
        chrono::LocalResult::None => naive.and_utc().timestamp_millis(),
    }
}

#[test]
fn a_pretty_printed_usage_object_is_found_across_lines() {
    // The whole point of the balanced scan: this object is not on one line.
    let found = messages(&record("chatcmpl-a", "llama-3", 50, 12));
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].model, "llama-3");
    assert_eq!(found[0].tokens.input, 50);
    assert_eq!(found[0].tokens.output, 12);
    assert_eq!(found[0].key.client, CLIENT);
}

#[test]
fn the_response_id_is_the_dedup_identity() {
    let found = messages(&record("chatcmpl-abc", "m", 10, 5));
    assert_eq!(found[0].dedup_key.as_deref(), Some("lmstudio:chatcmpl-abc"));
}

#[test]
fn a_responses_api_id_prefix_is_recognised() {
    let found = messages(&record("resp_xyz", "m", 10, 5));
    assert_eq!(found[0].dedup_key.as_deref(), Some("lmstudio:resp_xyz"));
}

#[test]
fn an_id_without_a_known_prefix_is_not_used_as_identity() {
    // The log is full of request ids and model-file ids; only a response id may
    // key the record.
    let found = messages(&record("msg-plain", "m", 10, 5));
    let key = found[0]
        .dedup_key
        .as_deref()
        .expect("a fallback key exists");
    assert!(key.starts_with("lmstudio:log:"), "got {key}");
}

#[test]
fn cache_and_reasoning_are_moved_out_of_the_inclusive_counts() {
    let log = concat!(
        "[2026-07-09 11:00:00][INFO][m]\n",
        r#"{"id":"chatcmpl-c","model":"m","usage":{"prompt_tokens":100,"completion_tokens":40,"#,
        r#""total_tokens":140,"prompt_tokens_details":{"cached_tokens":30,"#,
        r#""cache_creation_input_tokens":10},"#,
        r#""output_tokens_details":{"reasoning_tokens":15}}}"#,
        "\n"
    );
    let found = messages(log);
    assert_eq!(found[0].tokens.cache_read, 30);
    assert_eq!(found[0].tokens.cache_write, 10);
    assert_eq!(found[0].tokens.reasoning, 15);
    assert_eq!(found[0].tokens.input, 60);
    assert_eq!(found[0].tokens.output, 25);
    // The five disjoint buckets sum to the reported total.
    let tokens = found[0].tokens;
    assert_eq!(
        tokens.input + tokens.output + tokens.cache_read + tokens.cache_write + tokens.reasoning,
        140
    );
}

#[test]
fn flat_detail_spellings_are_read_when_the_nested_block_is_absent() {
    let log = "[2026-07-09 11:00:00][INFO][m]\n\
               {\"id\":\"chatcmpl-d\",\"model\":\"m\",\"usage\":{\
               \"prompt_tokens\":100,\"completion_tokens\":40,\"total_tokens\":140,\
               \"cached_tokens\":25,\"reasoning_tokens\":10}}\n";
    let found = messages(log);
    assert_eq!(found[0].tokens.cache_read, 25);
    assert_eq!(found[0].tokens.reasoning, 10);
}

#[test]
fn a_larger_reported_total_is_trusted_over_the_bucket_sum() {
    // A server that counts differently must not have its usage shortened.
    let log = "[2026-07-09 11:00:00][INFO][m]\n\
               {\"id\":\"chatcmpl-e\",\"model\":\"m\",\"usage\":{\
               \"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":40}}\n";
    let found = messages(log);
    assert_eq!(found[0].tokens.input, 35);
}

#[test]
fn cache_larger_than_the_prompt_floors_at_zero() {
    let log = "[2026-07-09 11:00:00][INFO][m]\n\
               {\"id\":\"chatcmpl-f\",\"model\":\"m\",\"usage\":{\
               \"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15,\
               \"cached_tokens\":500}}\n";
    let found = messages(log);
    assert_eq!(found[0].tokens.cache_read, 10);
    assert_eq!(found[0].tokens.input, 0);
}

#[test]
fn a_zero_usage_object_carries_no_observation() {
    let log = "[2026-07-09 11:00:00][INFO][m]\n\
               {\"id\":\"chatcmpl-g\",\"model\":\"m\",\"usage\":{\
               \"prompt_tokens\":0,\"completion_tokens\":0,\"total_tokens\":0}}\n";
    assert!(messages(log).is_empty());
}

#[test]
fn a_partial_trailing_object_is_not_charged() {
    // The object never closes, so it is an incomplete write.
    let log = "[2026-07-09 11:00:00][INFO][m]\n\
               {\"id\":\"chatcmpl-h\",\"model\":\"m\",\"usage\":{\
               \"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15\n";
    assert!(messages(log).is_empty());
}

#[test]
fn each_response_in_one_log_is_its_own_record() {
    let log = format!(
        "{}{}",
        record("chatcmpl-1", "m", 10, 5),
        record("chatcmpl-2", "m", 20, 6)
    );
    let found = messages(&log);
    assert_eq!(found.len(), 2);
    assert_eq!(found[0].tokens.input, 10);
    assert_eq!(found[1].tokens.input, 20);
}

#[test]
fn a_model_in_the_response_body_is_not_taken_for_the_record_model() {
    // A model that echoes a `[date][...]` header or an `"id"` in its answer must
    // not be dated or identified from its own output.
    let log = "[2026-07-09 11:00:00][INFO][real-model]\n\
               {\"id\":\"chatcmpl-i\",\"model\":\"real-model\",\
               \"choices\":[{\"message\":{\"content\":\"[2020-01-01 00:00:00][INFO]\"}}],\
               \"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}\n";
    let found = messages(log);
    assert_eq!(found[0].model, "real-model");
    // The real header's wall clock, not the 2020 date inside the answer.
    assert_eq!(
        found[0].timestamp_ms,
        local_wall_clock_ms("2026-07-09 11:00:00")
    );
}

#[test]
fn the_log_header_is_the_timestamp_source_and_is_read_in_the_host_zone() {
    // A naive header is the writing host's wall clock, so it is resolved in
    // the local zone. Reading it as UTC instead would shift every record by the
    // host's offset and move records across midnight, corrupting day buckets.
    let log = record("chatcmpl-j", "m", 10, 5);
    let found = messages(&log);
    assert_eq!(
        found[0].timestamp_ms,
        local_wall_clock_ms("2026-07-09 11:00:00")
    );
    // Record ordering is unaffected by the zone either way.
    assert!(found[0].timestamp_ms > 0);
}

#[test]
fn a_missing_header_falls_back_to_the_file_mtime() {
    let log = "{\"id\":\"chatcmpl-k\",\"model\":\"m\",\"usage\":{\
               \"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}\n";
    let mut reader = LmstudioReader::new("log").with_fallback_timestamp(Some(1_700_000_000_000));
    reader.ingest(log.as_bytes()).expect("accepted");
    assert_eq!(reader.snapshot()[0].timestamp_ms, 1_700_000_000_000);
}

#[test]
fn a_record_with_no_usable_timestamp_and_no_mtime_is_rejected() {
    let log = "{\"id\":\"chatcmpl-l\",\"model\":\"m\",\"usage\":{\
               \"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}\n";
    let mut reader = LmstudioReader::new("log");
    assert_eq!(
        reader.ingest(log.as_bytes()).expect_err("no timestamp"),
        ReaderError::InvalidTimestamp
    );
}

#[test]
fn the_model_runs_locally_so_the_cost_is_an_authoritative_zero() {
    let found = messages(&record("chatcmpl-m", "m", 10, 5));
    assert_eq!(found[0].cost, CostStatus::Reported(0));
    assert_eq!(found[0].billing_provider_hint.as_deref(), Some("lmstudio"));
}

#[test]
fn a_missing_model_degrades_to_unknown() {
    let log = "[2026-07-09 11:00:00][INFO][m]\n\
               {\"id\":\"chatcmpl-n\",\"usage\":{\
               \"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}\n";
    let found = messages(log);
    assert_eq!(found[0].model, "unknown");
}

#[test]
fn a_negative_counter_is_a_visible_rejection() {
    let log = "[2026-07-09 11:00:00][INFO][m]\n\
               {\"id\":\"chatcmpl-o\",\"model\":\"m\",\"usage\":{\
               \"prompt_tokens\":-5,\"completion_tokens\":5,\"total_tokens\":15}}\n";
    let mut reader = LmstudioReader::new("log").with_fallback_timestamp(Some(1_000));
    assert_eq!(
        reader.ingest(log.as_bytes()).expect_err("negative counter"),
        ReaderError::InvalidRecord
    );
}

#[test]
fn an_escaped_usage_marker_is_not_a_record() {
    // A prompt that literally contains `\"usage\"` must not start a record.
    let log = "[2026-07-09 11:00:00][INFO][m]\n\
               {\"id\":\"chatcmpl-p\",\"model\":\"m\",\
               \"prompt\":\"text with \\\"usage\\\": {}\",\
               \"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}\n";
    let found = messages(log);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].tokens.input, 10);
}

#[test]
fn a_revision_replaces_rather_than_appends() {
    // The document input re-delivers a whole revision on change, so the same
    // bytes must not be counted twice.
    let log = record("chatcmpl-q", "m", 10, 5);
    let mut reader = LmstudioReader::new("log").with_fallback_timestamp(Some(1_000));
    reader.ingest(log.as_bytes()).expect("accepted");
    reader.ingest(log.as_bytes()).expect("accepted");
    assert_eq!(reader.snapshot().len(), 1);
    assert!(reader.has_supported_usage());
}

#[test]
fn an_empty_log_has_no_supported_usage() {
    let mut reader = LmstudioReader::new("log").with_fallback_timestamp(Some(1_000));
    reader.ingest(b"").expect("accepted");
    assert!(!reader.has_supported_usage());
}
