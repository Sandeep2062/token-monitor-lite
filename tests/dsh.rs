//! Synthetic DeepSeek Harness (DSH) cases adapted from the pinned Tokscale
//! `dsh.rs` reader. The transcript is an append-only event stream, so each case
//! feeds whole records and then calls `finish` to resolve the fork boundary.

use token_monitor_lite::accounting::TokenCounts;
use token_monitor_lite::parsers::ReaderError;
use token_monitor_lite::parsers::dsh::{CLIENT, DshReader, ForkBoundary};

fn reader() -> DshReader {
    DshReader::new("sess-1")
}

fn ingest(reader: &mut DshReader, line: &str) {
    reader.ingest(line.as_bytes()).unwrap();
}

fn session_header(id: &str, cwd: &str) -> String {
    format!(r#"{{"type":"session","id":"{id}","cwd":"{cwd}"}}"#)
}

#[test]
fn an_assistant_message_with_usage_becomes_one_observation() {
    let mut reader = reader();
    ingest(
        &mut reader,
        r#"{"type":"assistant/message","time":1782718005000,"seq":1,"data":{"message":{"id":"m1","source":{"provider":"deepseek","model":"deepseek-chat"}},"usage":{"inputTokens":100,"outputTokens":50}}}"#,
    );
    reader.finish().unwrap();
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key.client, CLIENT);
    assert_eq!(rows[0].key.session, "sess-1");
    assert_eq!(rows[0].model, "deepseek-chat");
    assert_eq!(rows[0].billing_provider_hint.as_deref(), Some("deepseek"));
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
    assert_eq!(rows[0].timestamp_ms, 1_782_718_005_000);
}

#[test]
fn the_session_header_supplies_the_session_and_workspace() {
    let mut reader = reader();
    ingest(&mut reader, &session_header("from-header", "/work/demo"));
    ingest(
        &mut reader,
        r#"{"type":"assistant/message","time":1782718005000,"data":{"message":{"id":"m1"},"usage":{"inputTokens":10,"outputTokens":5}}}"#,
    );
    reader.finish().unwrap();
    let rows = reader.snapshot();
    assert_eq!(rows[0].key.session, "from-header");
    // An absolute cwd never becomes the workspace key; only its final
    // component survives, because the store rejects absolute-path text.
    assert_eq!(rows[0].workspace_key, None);
    assert_eq!(rows[0].workspace_label.as_deref(), Some("demo"));
}

#[test]
fn reasoning_is_removed_from_output_exactly_once() {
    let mut reader = reader();
    // outputTokens already contains reasoningTokens.
    ingest(
        &mut reader,
        r#"{"type":"assistant/message","time":1782718005000,"data":{"message":{"id":"m1"},"usage":{"inputTokens":10,"outputTokens":80,"reasoningTokens":30}}}"#,
    );
    reader.finish().unwrap();
    let rows = reader.snapshot();
    assert_eq!(rows[0].tokens.output, 50);
    assert_eq!(rows[0].tokens.reasoning, 30);
    // The two buckets still sum to the reported output.
    assert_eq!(rows[0].tokens.output + rows[0].tokens.reasoning, 80);
}

#[test]
fn reasoning_equal_to_output_leaves_a_non_zero_row() {
    let mut reader = reader();
    ingest(
        &mut reader,
        r#"{"type":"assistant/message","time":1782718005000,"data":{"message":{"id":"m1"},"usage":{"inputTokens":10,"outputTokens":40,"reasoningTokens":40}}}"#,
    );
    reader.finish().unwrap();
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].tokens.output, 0);
    assert_eq!(rows[0].tokens.reasoning, 40);
}

#[test]
fn an_all_zero_settlement_contributes_nothing() {
    let mut reader = reader();
    ingest(
        &mut reader,
        r#"{"type":"assistant/message","time":1782718005000,"data":{"message":{"id":"m1"},"usage":{"inputTokens":0,"outputTokens":0}}}"#,
    );
    reader.finish().unwrap();
    assert!(reader.snapshot().is_empty());
    assert!(!reader.has_supported_usage());
}

#[test]
fn a_settlement_without_a_timestamp_contributes_nothing() {
    let mut reader = reader();
    ingest(
        &mut reader,
        r#"{"type":"assistant/message","data":{"message":{"id":"m1"},"usage":{"inputTokens":10,"outputTokens":5}}}"#,
    );
    reader.finish().unwrap();
    assert!(reader.snapshot().is_empty());
}

#[test]
fn a_non_usage_event_contributes_nothing() {
    let mut reader = reader();
    ingest(
        &mut reader,
        r#"{"type":"user/message","time":1782718005000}"#,
    );
    ingest(
        &mut reader,
        r#"{"type":"step/start","time":1782718005000,"data":{"turn":0,"step":0}}"#,
    );
    reader.finish().unwrap();
    assert!(reader.snapshot().is_empty());
}

#[test]
fn usage_in_an_embedded_stream_is_read() {
    let mut reader = reader();
    ingest(
        &mut reader,
        r#"{"type":"assistant/message","time":1782718005000,"data":{"message":{"id":"m1"},"stream":[{"type":"chunk","time":1782718006000,"chunk":{"type":"usage","usage":{"inputTokens":7,"outputTokens":3}}}]}}"#,
    );
    reader.finish().unwrap();
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].tokens.input, 7);
    assert_eq!(rows[0].tokens.output, 3);
}

#[test]
fn the_last_stream_usage_chunk_wins() {
    let mut reader = reader();
    ingest(
        &mut reader,
        r#"{"type":"assistant/attempt","time":1782718005000,"data":{"attemptId":"a1","stream":[{"type":"chunk","time":1782718005500,"chunk":{"type":"usage","usage":{"inputTokens":5,"outputTokens":2}}},{"type":"chunk","time":1782718006000,"chunk":{"type":"usage","usage":{"inputTokens":9,"outputTokens":4}}}]}}"#,
    );
    reader.finish().unwrap();
    let rows = reader.snapshot();
    assert_eq!(rows[0].tokens.input, 9);
    assert_eq!(rows[0].tokens.output, 4);
}

#[test]
fn a_compaction_summary_is_counted_in_addition_to_its_neighbours() {
    let mut reader = reader();
    ingest(
        &mut reader,
        r#"{"type":"assistant/message","time":1782718005000,"seq":1,"data":{"message":{"id":"m1"},"usage":{"inputTokens":10,"outputTokens":5}}}"#,
    );
    // A summary shares no (turn, step) with a loop step, so it is real spend
    // rather than a replacement.
    ingest(
        &mut reader,
        r#"{"type":"compaction/summary","time":1782718006000,"seq":2,"data":{"compactionId":"c1","usage":{"inputTokens":20,"outputTokens":8}}}"#,
    );
    reader.finish().unwrap();
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 2);
    let total: u64 = rows.iter().map(|row| row.tokens.input).sum();
    assert_eq!(total, 30);
}

#[test]
fn a_summary_without_usage_is_skipped() {
    let mut reader = reader();
    ingest(
        &mut reader,
        r#"{"type":"compaction/summary","time":1782718006000,"data":{"compactionId":"c1"}}"#,
    );
    reader.finish().unwrap();
    assert!(reader.snapshot().is_empty());
}

#[test]
fn a_summary_is_namespaced_apart_from_a_loop_step() {
    let mut reader = reader();
    // Identical clock, routing and buckets: without the `summary:` namespace
    // these two would collapse into one.
    ingest(
        &mut reader,
        r#"{"type":"assistant/message","time":1782718005000,"data":{"message":{"id":"m1"},"usage":{"inputTokens":10,"outputTokens":5}}}"#,
    );
    ingest(
        &mut reader,
        r#"{"type":"compaction/summary","time":1782718005000,"data":{"compactionId":"c1","usage":{"inputTokens":10,"outputTokens":5}}}"#,
    );
    reader.finish().unwrap();
    assert_eq!(reader.snapshot().len(), 2);
}

#[test]
fn the_served_response_model_overrides_the_configured_one() {
    let mut reader = reader();
    // source.model is the *requested* model; the provider served another.
    ingest(
        &mut reader,
        r#"{"type":"assistant/message","time":1782718005000,"data":{"message":{"id":"m1","source":{"provider":"deepseek","model":"deepseek/auto","replayState":{"response":{"responseModel":"deepseek-chat-v3"}}}},"usage":{"inputTokens":10,"outputTokens":5}}}"#,
    );
    reader.finish().unwrap();
    assert_eq!(reader.snapshot()[0].model, "deepseek-chat-v3");
}

#[test]
fn a_keepalive_response_model_is_ignored_without_guessing() {
    let mut reader = reader();
    ingest(
        &mut reader,
        r#"{"type":"assistant/message","time":1782718005000,"data":{"message":{"id":"m1","source":{"provider":"deepseek","model":"deepseek-chat","replayState":{"response":{"responseModel":"keepalive"}}}},"usage":{"inputTokens":10,"outputTokens":5}}}"#,
    );
    reader.finish().unwrap();
    // The configured model is kept rather than the placeholder or a guess.
    assert_eq!(reader.snapshot()[0].model, "deepseek-chat");
}

#[test]
fn a_missing_source_falls_back_to_the_request_header() {
    let mut reader = reader();
    ingest(
        &mut reader,
        r#"{"type":"request/header","data":{"header":{"config":{"provider":"openrouter","model":"vendor/model"}}}}"#,
    );
    ingest(
        &mut reader,
        r#"{"type":"assistant/message","time":1782718005000,"data":{"message":{"id":"m1"},"usage":{"inputTokens":10,"outputTokens":5}}}"#,
    );
    reader.finish().unwrap();
    let rows = reader.snapshot();
    assert_eq!(rows[0].model, "vendor/model");
    assert_eq!(rows[0].billing_provider_hint.as_deref(), Some("openrouter"));
}

#[test]
fn a_row_with_no_routing_at_all_is_named_unknown() {
    let mut reader = reader();
    ingest(
        &mut reader,
        r#"{"type":"assistant/message","time":1782718005000,"data":{"message":{"id":"m1"},"usage":{"inputTokens":10,"outputTokens":5}}}"#,
    );
    reader.finish().unwrap();
    assert_eq!(reader.snapshot()[0].model, "unknown");
}

#[test]
fn identical_replayed_rows_are_deduped_within_a_file() {
    let mut reader = reader();
    let line = r#"{"type":"assistant/message","time":1782718005000,"seq":1,"data":{"message":{"id":"m1"},"usage":{"inputTokens":10,"outputTokens":5}}}"#;
    ingest(&mut reader, line);
    ingest(&mut reader, line);
    reader.finish().unwrap();
    assert_eq!(reader.snapshot().len(), 1);
}

#[test]
fn a_repeated_message_id_still_separates_calls_that_differ() {
    let mut reader = reader();
    // Same id, different usage: a non-unique id must not fold two real calls.
    ingest(
        &mut reader,
        r#"{"type":"assistant/message","time":1782718005000,"data":{"message":{"id":"m1"},"usage":{"inputTokens":10,"outputTokens":5}}}"#,
    );
    ingest(
        &mut reader,
        r#"{"type":"assistant/message","time":1782718009000,"data":{"message":{"id":"m1"},"usage":{"inputTokens":30,"outputTokens":9}}}"#,
    );
    reader.finish().unwrap();
    assert_eq!(reader.snapshot().len(), 2);
}

#[test]
fn a_later_sample_for_the_same_step_replaces_the_earlier_one() {
    let mut reader = reader();
    // One call that reports twice must not be summed into two calls.
    ingest(
        &mut reader,
        r#"{"type":"assistant/message","time":1782718005000,"data":{"turn":0,"step":0,"message":{"id":"m1"},"usage":{"inputTokens":10,"outputTokens":5}}}"#,
    );
    ingest(
        &mut reader,
        r#"{"type":"assistant/message","time":1782718006000,"data":{"turn":0,"step":0,"message":{"id":"m2"},"usage":{"inputTokens":40,"outputTokens":20}}}"#,
    );
    reader.finish().unwrap();
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].tokens.input, 40);
}

#[test]
fn a_retried_step_after_a_different_step_is_its_own_call() {
    let mut reader = reader();
    ingest(
        &mut reader,
        r#"{"type":"assistant/message","time":1782718005000,"data":{"turn":0,"step":0,"message":{"id":"m1"},"usage":{"inputTokens":10,"outputTokens":5}}}"#,
    );
    ingest(
        &mut reader,
        r#"{"type":"assistant/message","time":1782718006000,"data":{"turn":0,"step":1,"message":{"id":"m2"},"usage":{"inputTokens":20,"outputTokens":7}}}"#,
    );
    reader.finish().unwrap();
    assert_eq!(reader.snapshot().len(), 2);
}

#[test]
fn a_legacy_seed_length_drops_the_inherited_prefix() {
    let mut reader = reader();
    // A fork copies its parent's prefix; billing those rows again would
    // double-charge the parent.
    ingest(
        &mut reader,
        r#"{"type":"session","id":"child","cwd":"/work","seedLength":2}"#,
    );
    ingest(
        &mut reader,
        r#"{"type":"assistant/message","time":1782718005000,"seq":0,"data":{"message":{"id":"inherited"},"usage":{"inputTokens":100,"outputTokens":50}}}"#,
    );
    ingest(
        &mut reader,
        r#"{"type":"assistant/message","time":1782718006000,"seq":2,"data":{"message":{"id":"own"},"usage":{"inputTokens":10,"outputTokens":5}}}"#,
    );
    reader.finish().unwrap();
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].tokens.input, 10);
}

#[test]
fn a_seeded_header_uses_the_last_tagged_cut() {
    let mut reader = reader();
    ingest(
        &mut reader,
        r#"{"type":"session","id":"child","cwd":"/work","isSeeded":true}"#,
    );
    // A nested fork can inherit an ancestor's marker, so the *last* one wins.
    ingest(
        &mut reader,
        r#"{"type":"session/end-seed","seq":1,"data":{"inherited":true}}"#,
    );
    ingest(
        &mut reader,
        r#"{"type":"session/end-seed","seq":3,"data":{"inherited":true}}"#,
    );
    ingest(
        &mut reader,
        r#"{"type":"assistant/message","time":1782718005000,"seq":2,"data":{"message":{"id":"inherited"},"usage":{"inputTokens":100,"outputTokens":50}}}"#,
    );
    ingest(
        &mut reader,
        r#"{"type":"assistant/message","time":1782718006000,"seq":4,"data":{"message":{"id":"own"},"usage":{"inputTokens":10,"outputTokens":5}}}"#,
    );
    reader.finish().unwrap();
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].tokens.input, 10);
}

#[test]
fn a_seeded_header_without_a_tagged_cut_is_a_visible_failure() {
    let mut reader = reader();
    ingest(
        &mut reader,
        r#"{"type":"session","id":"child","cwd":"/work","isSeeded":true}"#,
    );
    ingest(
        &mut reader,
        r#"{"type":"assistant/message","time":1782718005000,"seq":1,"data":{"message":{"id":"m1"},"usage":{"inputTokens":10,"outputTokens":5}}}"#,
    );
    // Inherited usage cannot be distinguished from the child's own, so charging
    // the whole copied prefix is worse than refusing the transcript.
    assert_eq!(
        reader.finish(),
        Err(ReaderError::Unsupported("DSH seeded fork boundary"))
    );
}

#[test]
fn an_unseeded_session_keeps_every_row() {
    let mut reader = reader();
    ingest(
        &mut reader,
        r#"{"type":"session","id":"solo","cwd":"/work"}"#,
    );
    ingest(
        &mut reader,
        r#"{"type":"assistant/message","time":1782718005000,"seq":1,"data":{"message":{"id":"m1"},"usage":{"inputTokens":10,"outputTokens":5}}}"#,
    );
    reader.finish().unwrap();
    assert_eq!(reader.snapshot().len(), 1);
}

#[test]
fn a_legacy_zero_seed_length_means_no_cut() {
    let mut reader = reader();
    // A present legacy field owns the decision even at zero, so the v3 marker
    // path is never consulted.
    ingest(
        &mut reader,
        r#"{"type":"session","id":"s","cwd":"/work","seedLength":0,"isSeeded":true}"#,
    );
    ingest(
        &mut reader,
        r#"{"type":"assistant/message","time":1782718005000,"seq":1,"data":{"message":{"id":"m1"},"usage":{"inputTokens":10,"outputTokens":5}}}"#,
    );
    reader.finish().unwrap();
    assert_eq!(reader.snapshot().len(), 1);
}

#[test]
fn a_malformed_record_is_a_visible_failure() {
    let mut reader = reader();
    assert_eq!(reader.ingest(b"{not json"), Err(ReaderError::InvalidRecord));
}

#[test]
fn a_negative_counter_is_a_visible_failure() {
    let mut reader = reader();
    assert_eq!(
        reader.ingest(
            br#"{"type":"assistant/message","time":1782718005000,"data":{"message":{"id":"m1"},"usage":{"inputTokens":-5,"outputTokens":5}}}"#
        ),
        Err(ReaderError::InvalidRecord)
    );
}

#[test]
fn cost_is_never_claimed() {
    let mut reader = reader();
    ingest(
        &mut reader,
        r#"{"type":"assistant/message","time":1782718005000,"data":{"message":{"id":"m1"},"usage":{"inputTokens":10,"outputTokens":5}}}"#,
    );
    reader.finish().unwrap();
    assert_eq!(
        reader.snapshot()[0].cost,
        token_monitor_lite::parsers::CostStatus::Unavailable
    );
}

#[test]
fn the_boundary_enum_is_exhaustive_by_construction() {
    // Documents the three states the reader distinguishes.
    assert_ne!(ForkBoundary::None, ForkBoundary::Cut(0));
    assert_ne!(ForkBoundary::Cut(0), ForkBoundary::MissingTaggedCut);
}
