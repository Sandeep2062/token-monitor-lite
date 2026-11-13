use serde_json::{Value, json};
use token_monitor_lite::accounting::TokenCounts;
use token_monitor_lite::parsers::ReaderError;
use token_monitor_lite::parsers::codex::CodexReader;

fn ingest(reader: &mut CodexReader, value: Value) {
    reader.ingest(&serde_json::to_vec(&value).unwrap()).unwrap();
}

fn start() -> CodexReader {
    let mut reader = CodexReader::new("session-a");
    ingest(
        &mut reader,
        json!({"type":"session_meta","payload":{"id":"session-a","model_provider":"openai","source":"cli"}}),
    );
    ingest(
        &mut reader,
        json!({"type":"turn_context","timestamp":"2026-11-09T10:00:00Z","payload":{"model":"gpt-5"}}),
    );
    reader
}

fn count(last: Value, total: Value, seconds: u32) -> Value {
    json!({"type":"event_msg","timestamp":format!("2026-11-09T10:00:{seconds:02}Z"),"payload":{"type":"token_count","info":{"last_token_usage":last,"total_token_usage":total}}})
}

#[test]
fn last_usage_is_primary_and_inclusive_cache_reasoning_are_split_once() {
    let mut reader = start();
    let last = json!({"input_tokens":100,"output_tokens":40,"cached_input_tokens":20,"cache_read_input_tokens":30,"reasoning_output_tokens":10});
    let total = json!({"input_tokens":1000,"output_tokens":400,"cached_input_tokens":300,"reasoning_output_tokens":100});
    ingest(&mut reader, count(last.clone(), total.clone(), 1));
    ingest(&mut reader, count(last, total, 2));
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].tokens,
        TokenCounts {
            input: 70,
            output: 30,
            cache_read: 30,
            cache_write: 0,
            reasoning: 10
        }
    );
    assert_eq!(rows[0].key.client, "codex");
    assert_eq!(rows[0].key.session, "session-a");
    assert_eq!(rows[0].timestamp_ms, 1_794_218_400_000);
    assert_eq!(
        rows[0].dedup_key.as_deref(),
        Some("codex:token_count-total:session-a:openai:gpt-5:1000:400:300:100")
    );
    assert_eq!(rows[0].billing_provider_hint.as_deref(), Some("openai"));
    assert!(reader.has_supported_usage());
}

#[test]
fn timestampless_last_only_usage_has_copy_stable_fallback_identity() {
    let mut reader = start();
    ingest(
        &mut reader,
        json!({"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":10},"total_token_usage":null}}}),
    );

    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].dedup_key.as_deref(),
        Some("codex:usage-fallback:9:session-a:0:5:gpt-5:10:0:0:0:0:0")
    );
}

#[test]
fn stale_regression_is_suppressed_but_hard_reset_counts_last() {
    let mut reader = start();
    for (last, total, time) in [
        (100, 1000, 1),
        (10, 990, 2),
        (10, 1010, 3),
        (5, 50, 4),
        (5, 55, 5),
    ] {
        ingest(
            &mut reader,
            count(
                json!({"input_tokens":last}),
                json!({"input_tokens":total}),
                time,
            ),
        );
    }
    let rows = reader.snapshot();
    assert_eq!(
        rows.iter().map(|r| r.tokens.input).collect::<Vec<_>>(),
        [100, 10, 5, 5]
    );
    // Suppressed duplicate/regression doesn't advance the duration cursor.
    assert_eq!(rows[1].timestamp_ms, 1_794_218_401_000);
}

#[test]
fn total_only_decrease_rebaselines_and_zero_last_does_not_advance_baseline() {
    let mut reader = start();
    for (last, total, time) in [
        (Value::Null, 100, 1),
        (Value::Null, 110, 2),
        (Value::Null, 20, 3),
        (Value::Null, 25, 4),
        (json!({"input_tokens":0}), 0, 5),
        (Value::Null, 30, 6),
    ] {
        ingest(
            &mut reader,
            count(last, json!({"input_tokens":total}), time),
        );
    }
    assert_eq!(
        reader
            .snapshot()
            .iter()
            .map(|r| r.tokens.input)
            .collect::<Vec<_>>(),
        [100, 10, 5, 5]
    );
}

#[test]
fn last_only_exact_arithmetic_and_cache_reasoning_clamps_are_bounded() {
    let mut reader = start();
    ingest(
        &mut reader,
        count(json!({"input_tokens":9007199254740993_u64}), Value::Null, 1),
    );
    ingest(
        &mut reader,
        count(
            json!({"input_tokens":2,"output_tokens":3,"cached_input_tokens":5,"reasoning_output_tokens":9}),
            Value::Null,
            2,
        ),
    );
    let rows = reader.snapshot();
    assert_eq!(rows[0].tokens.input, 9_007_199_254_740_993);
    assert_eq!(
        rows[1].tokens,
        TokenCounts {
            input: 0,
            output: 0,
            cache_read: 2,
            cache_write: 0,
            reasoning: 3
        }
    );
}

#[test]
fn invalid_signs_fractional_and_unclassified_counters_are_visible() {
    let mut reader = start();
    for value in [
        count(json!({"input_tokens":-1}), Value::Null, 1),
        count(json!({"input_tokens":1.5}), Value::Null, 1),
        count(json!({"total_tokens":7}), Value::Null, 1),
    ] {
        assert!(reader.ingest(&serde_json::to_vec(&value).unwrap()).is_err());
        assert!(reader.snapshot().is_empty());
    }
    ingest(
        &mut reader,
        count(json!({"input_tokens":7}), Value::Null, 1),
    );
    assert_eq!(reader.snapshot()[0].tokens.input, 7);
}

#[test]
fn usage_record_confirmed_by_token_count_is_not_double_counted() {
    let mut reader = start();
    ingest(
        &mut reader,
        json!({"type":"token_usage_record","timestamp":"2026-11-09T10:00:01Z","payload":{"thread_id":"session-a","turn_id":"turn-1","response_id":"response-1","model":"gpt-5","usage":{"input_tokens":200}}}),
    );
    assert!(reader.snapshot().is_empty());
    ingest(
        &mut reader,
        count(json!({"input_tokens":200}), json!({"input_tokens":200}), 1),
    );
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].tokens.input, 200);
    assert_eq!(
        rows[0].dedup_key.as_deref(),
        Some("codex:token_count-total:session-a:openai:gpt-5:200:0:0:0")
    );
}

#[test]
fn pending_usage_record_is_flushed_at_a_new_turn_and_compaction_is_non_usage() {
    let mut reader = start();
    ingest(
        &mut reader,
        json!({"type":"token_usage_record","timestamp":"2026-11-09T10:00:01Z","payload":{"thread_id":"session-a","turn_id":"turn-1","response_id":"response-1","model":"gpt-5","usage":{"input_tokens":200}}}),
    );
    ingest(&mut reader, json!({"type":"compacted","payload":{}}));
    ingest(
        &mut reader,
        json!({"type":"event_msg","timestamp":"2026-11-09T10:00:02Z","payload":{"type":"token_count","turn_id":"turn-2","info":{"last_token_usage":{"input_tokens":50},"total_token_usage":{"input_tokens":50}}}}),
    );
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].tokens.input, 200);
    assert_eq!(rows[1].tokens.input, 50);
}

#[test]
fn usage_record_confirmation_ignores_cache_split_differences() {
    let mut reader = start();
    ingest(
        &mut reader,
        json!({"type":"token_usage_record","timestamp":"2026-11-09T10:00:01Z","payload":{"thread_id":"session-a","turn_id":"turn-1","response_id":"response-1","model":"gpt-5","usage":{"input_tokens":200,"cached_input_tokens":50,"output_tokens":20}}}),
    );
    assert!(reader.snapshot().is_empty());
    ingest(
        &mut reader,
        count(
            json!({"input_tokens":200,"cached_input_tokens":10,"output_tokens":20}),
            json!({"input_tokens":200,"cached_input_tokens":10,"output_tokens":20}),
            1,
        ),
    );
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].tokens,
        TokenCounts {
            input: 190,
            output: 20,
            cache_read: 10,
            cache_write: 0,
            reasoning: 0
        }
    );
}

#[test]
fn compaction_settles_a_pending_record_only_when_token_count_does_not_advance() {
    let mut reader = start();
    ingest(
        &mut reader,
        json!({"type":"token_usage_record","timestamp":"2026-11-09T10:00:01Z","payload":{"thread_id":"session-a","turn_id":"turn-1","response_id":"compacted-response","model":"gpt-5","usage":{"input_tokens":200,"output_tokens":20}}}),
    );
    ingest(&mut reader, json!({"type":"compacted","payload":{}}));
    ingest(
        &mut reader,
        count(json!({"input_tokens":0}), json!({"input_tokens":0}), 2),
    );
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].tokens.input, 200);
    assert_eq!(
        rows[0].dedup_key.as_deref(),
        Some("codex:usage-record:session-a:compacted-response")
    );
}

#[test]
fn compaction_counts_both_the_record_and_a_different_token_count_response() {
    let mut reader = start();
    ingest(
        &mut reader,
        json!({"type":"token_usage_record","timestamp":"2026-11-09T10:00:01Z","payload":{"thread_id":"session-a","turn_id":"turn-1","response_id":"compacted-response","model":"gpt-5","usage":{"input_tokens":200,"output_tokens":20}}}),
    );
    ingest(&mut reader, json!({"type":"compacted","payload":{}}));
    ingest(
        &mut reader,
        count(json!({"input_tokens":50}), json!({"input_tokens":50}), 2),
    );
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].tokens.input, 200);
    assert_eq!(rows[1].tokens.input, 50);
}

#[test]
fn late_token_count_for_an_already_emitted_record_is_not_double_counted() {
    let mut reader = start();
    ingest(
        &mut reader,
        json!({"type":"token_usage_record","timestamp":"2026-11-09T10:00:01Z","payload":{"thread_id":"session-a","turn_id":"turn-1","response_id":"settled-response","model":"gpt-5","usage":{"input_tokens":200,"output_tokens":20}}}),
    );
    ingest(
        &mut reader,
        json!({"type":"event_msg","timestamp":"2026-11-09T10:00:02Z","payload":{"type":"task_complete","turn_id":"turn-1"}}),
    );
    assert_eq!(reader.snapshot().len(), 1);
    ingest(
        &mut reader,
        count(
            json!({"input_tokens":200,"output_tokens":20}),
            Value::Null,
            3,
        ),
    );
    assert_eq!(reader.snapshot().len(), 1);
}

#[test]
fn usage_record_for_another_thread_is_ignored() {
    let mut reader = start();
    ingest(
        &mut reader,
        json!({"type":"token_usage_record","timestamp":"2026-11-09T10:00:01Z","payload":{"thread_id":"session-b","turn_id":"turn-1","response_id":"other","model":"gpt-5","usage":{"input_tokens":200}}}),
    );
    assert!(reader.finish().is_ok());
    assert!(reader.snapshot().is_empty());
}

#[test]
fn model_less_usage_buffers_until_a_later_model_arrives() {
    let mut reader = CodexReader::new("session-a");
    ingest(
        &mut reader,
        json!({"type":"session_meta","payload":{"id":"session-a","model_provider":"openai"}}),
    );
    ingest(
        &mut reader,
        json!({"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":10},"total_token_usage":{"input_tokens":10}}}}),
    );
    assert!(reader.snapshot().is_empty());
    ingest(
        &mut reader,
        json!({"type":"turn_context","timestamp":"2026-11-09T10:00:01Z","payload":{"model":"gpt-5-late"}}),
    );
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].model, "gpt-5-late");
    assert_eq!(
        rows[0].dedup_key.as_deref(),
        Some("codex:token_count-total:session-a:openai:gpt-5-late:10:0:0:0")
    );
    assert_eq!(rows[0].timestamp_ms, 1_794_218_401_000);
}

#[test]
fn forked_user_child_suppresses_replayed_parent_history_until_its_own_turn() {
    let child = "0199aaaa-bbbb-7ccc-8ddd-eeeeeeeeeeee";
    let parent = "0198aaaa-0000-7ccc-8ddd-eeeeeeeeeeee";
    let mut reader = CodexReader::new(child);
    ingest(
        &mut reader,
        json!({"type":"session_meta","payload":{"id":child,"forked_from_id":parent,"thread_source":"user"}}),
    );
    ingest(
        &mut reader,
        json!({"type":"session_meta","payload":{"id":parent}}),
    );
    ingest(
        &mut reader,
        json!({"type":"turn_context","timestamp":"2026-11-09T10:00:00Z","payload":{"turn_id":"0198aaaa-0000-7ccc-8ddd-eeeeeeeeeeee","model":"gpt-5"}}),
    );
    ingest(
        &mut reader,
        count(json!({"input_tokens":100}), json!({"input_tokens":100}), 0),
    );
    assert!(reader.snapshot().is_empty());
    ingest(
        &mut reader,
        json!({"type":"turn_context","timestamp":"2026-11-09T10:00:05Z","payload":{"turn_id":child,"model":"gpt-5"}}),
    );
    ingest(
        &mut reader,
        count(json!({"input_tokens":50}), json!({"input_tokens":150}), 5),
    );
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].tokens.input, 50);
    assert_eq!(
        rows[0].dedup_key.as_deref(),
        Some("codex:token_count-total:0198aaaa-0000-7ccc-8ddd-eeeeeeeeeeee:openai:gpt-5:150:0:0:0")
    );
    assert_eq!(rows[0].agent.as_deref(), Some("Codex"));
}

#[test]
fn codex_subagent_and_headless_threads_retain_agent_provenance() {
    let mut subagent = CodexReader::new("subagent-session");
    ingest(
        &mut subagent,
        json!({"type":"session_meta","payload":{"id":"subagent-session","model_provider":"openai","thread_source":"subagent","parent_thread_id":"parent-thread","source":{"subagent":{"thread_spawn":{"parent_thread_id":"parent-thread"}}}}}),
    );
    ingest(
        &mut subagent,
        json!({"type":"turn_context","timestamp":"2026-11-09T10:00:00Z","payload":{"turn_id":"0199aaaa-bbbb-7ccc-8ddd-eeeeeeeeeeee","model":"gpt-5"}}),
    );
    ingest(
        &mut subagent,
        count(json!({"input_tokens":5}), json!({"input_tokens":5}), 0),
    );
    let rows = subagent.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].agent.as_deref(), Some("Codex Subagent"));
    assert_eq!(
        rows[0].dedup_key.as_deref(),
        Some("codex:token_count-total:parent-thread:openai:gpt-5:5:0:0:0")
    );

    let mut headless = CodexReader::new("headless-session");
    ingest(
        &mut headless,
        json!({"type":"session_meta","payload":{"id":"headless-session","source":"exec"}}),
    );
    ingest(
        &mut headless,
        json!({"type":"turn_context","timestamp":"2026-11-09T10:00:00Z","payload":{"model":"gpt-5"}}),
    );
    ingest(
        &mut headless,
        count(json!({"input_tokens":3}), json!({"input_tokens":3}), 0),
    );
    let rows = headless.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].agent.as_deref(), Some("Codex Headless"));
}

#[test]
fn pending_record_is_settled_by_a_new_turn_boundary() {
    let mut reader = start();
    ingest(
        &mut reader,
        json!({"type":"token_usage_record","timestamp":"2026-11-09T10:00:01Z","payload":{"thread_id":"session-a","turn_id":"turn-1","response_id":"abandoned","model":"gpt-5","usage":{"input_tokens":200}}}),
    );
    assert!(reader.snapshot().is_empty());
    ingest(
        &mut reader,
        json!({"type":"event_msg","timestamp":"2026-11-09T10:00:02Z","payload":{"type":"task_started","turn_id":"turn-2"}}),
    );
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].tokens.input, 200);
    assert_eq!(
        rows[0].dedup_key.as_deref(),
        Some("codex:usage-record:session-a:abandoned")
    );
}

#[test]
fn file_mtime_is_the_bounded_timestamp_fallback() {
    let mut reader = CodexReader::new("s").with_fallback_timestamp(Some(1_234));
    ingest(
        &mut reader,
        json!({"type":"session_meta","payload":{"id":"s"}}),
    );
    ingest(
        &mut reader,
        json!({"type":"turn_context","payload":{"model":"gpt-5"}}),
    );
    ingest(
        &mut reader,
        json!({"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":10},"total_token_usage":null}}}),
    );
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].timestamp_ms, 1_234);
    assert_eq!(
        rows[0].dedup_key.as_deref(),
        Some("codex:usage-fallback:1:s:0:5:gpt-5:10:0:0:0:0:0")
    );
}

#[test]
fn non_usage_items_are_ignored_and_provider_changes_apply_to_later_rows() {
    let mut reader = start();
    ingest(
        &mut reader,
        count(json!({"input_tokens":10}), json!({"input_tokens":10}), 1),
    );
    for ignored in [
        json!({"type":"response_item","payload":{"type":"message","content":[]}}),
        json!({"type":"response_item","payload":{"type":"function_call","name":"x"}}),
        json!({"type":"event_msg","payload":{"type":"turn_aborted"}}),
    ] {
        ingest(&mut reader, ignored);
    }
    ingest(
        &mut reader,
        json!({"type":"event_msg","payload":{"type":"thread_settings_applied","thread_settings":{"model_provider_id":"azure"}}}),
    );
    ingest(
        &mut reader,
        count(json!({"input_tokens":5}), json!({"input_tokens":15}), 2),
    );
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].billing_provider_hint.as_deref(), Some("openai"));
    assert_eq!(rows[1].billing_provider_hint.as_deref(), Some("azure"));
    assert_eq!(
        rows[1].dedup_key.as_deref(),
        Some("codex:token_count-total:session-a:azure:gpt-5:15:0:0:0")
    );
}

#[test]
fn openclaw_owned_rollouts_remain_visible_unsupported() {
    let mut reader = start();
    let before = reader.snapshot();
    assert_eq!(
        reader
            .ingest(
                br#"{"type":"session_meta","payload":{"id":"session-a","originator":"openclaw"}}"#
            )
            .unwrap_err(),
        ReaderError::Unsupported("non-Codex ownership")
    );
    assert_eq!(reader.snapshot(), before);
}

#[test]
fn known_zero_ledger_and_model_changes_retain_raw_service_tier_and_partition() {
    let mut reader = start();
    ingest(
        &mut reader,
        count(json!({"input_tokens":0}), json!({"input_tokens":0}), 1),
    );
    assert!(reader.has_supported_usage());
    assert!(reader.snapshot().is_empty());
    ingest(
        &mut reader,
        json!({"type":"event_msg","payload":{"type":"thread_settings_applied","thread_settings":{"model_provider_id":"openai","service_tier":"priority"}}}),
    );
    ingest(
        &mut reader,
        count(json!({"input_tokens":3}), json!({"input_tokens":3}), 2),
    );
    ingest(
        &mut reader,
        json!({"type":"turn_context","timestamp":"2026-11-09T10:01:00Z","payload":{"model":"gpt-5-mini"}}),
    );
    ingest(
        &mut reader,
        count(json!({"input_tokens":4}), json!({"input_tokens":7}), 3),
    );
    let rows = reader.snapshot();
    assert_eq!(rows[0].model, "gpt-5");
    assert_eq!(rows[1].model, "gpt-5-mini");
    assert_eq!(rows[1].service_tier.as_deref(), Some("priority"));
}

#[test]
fn failed_baseline_addition_and_invalid_records_retain_last_good_state() {
    let mut reader = start();
    ingest(
        &mut reader,
        count(
            json!({"input_tokens":u64::MAX}),
            json!({"input_tokens":u64::MAX}),
            1,
        ),
    );
    assert!(
        reader
            .ingest(&serde_json::to_vec(&count(json!({"input_tokens":1}), Value::Null, 2)).unwrap())
            .is_err()
    );
    assert!(reader.ingest(b"{secret-marker").is_err());
    // Duplicate max baseline is still recognized after the failed addition.
    ingest(
        &mut reader,
        count(
            json!({"input_tokens":7}),
            json!({"input_tokens":u64::MAX}),
            3,
        ),
    );
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].tokens.input, u64::MAX);
}

#[test]
fn missing_metadata_and_unresolved_models_are_not_complete_usage_observations() {
    let mut reader = CodexReader::new("a");
    assert!(
        reader
            .ingest(&serde_json::to_vec(&count(json!({"input_tokens":7}), Value::Null, 1)).unwrap())
            .is_err()
    );
    ingest(
        &mut reader,
        json!({"type":"session_meta","payload":{"id":"a"}}),
    );
    ingest(
        &mut reader,
        count(json!({"input_tokens":7}), Value::Null, 1),
    );
    assert!(reader.has_supported_usage());
    assert!(reader.snapshot().is_empty());
    assert_eq!(
        reader.finish().unwrap_err(),
        ReaderError::Unsupported("missing model")
    );
}
