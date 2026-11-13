use token_monitor_lite::parsers::claude::ClaudeReader;

#[test]
fn streaming_duplicates_keep_completeness_and_cache_split_units() {
    let mut reader = ClaudeReader::new("session-a");
    reader.ingest(br#"{"type":"assistant","timestamp":"2026-11-09T10:00:00Z","requestId":"req","message":{"id":"msg","model":"claude-sonnet-4-5","usage":{"input_tokens":1000,"output_tokens":2}}}"#).unwrap();
    reader.ingest(br#"{"type":"assistant","timestamp":"2026-11-09T10:00:01Z","requestId":"req","message":{"id":"msg","model":"claude-sonnet-4-5","usage":{"input_tokens":100,"output_tokens":10,"cache_read_input_tokens":900,"cache_creation_input_tokens":20,"cache_creation":{"ephemeral_1h_input_tokens":50}}}}"#).unwrap();
    reader.ingest(br#"{"type":"assistant","timestamp":"2026-11-09T10:00:02Z","requestId":"req","message":{"id":"msg","usage":{"input_tokens":1000,"output_tokens":8,"cache_creation_input_tokens":60}}}"#).unwrap();
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key.client, "claude");
    assert_eq!(rows[0].key.session, "session-a");
    assert_eq!(rows[0].dedup_key.as_deref(), Some("msg:req"));
    assert_eq!(rows[0].tokens.input, 1000); // both latter rows explicitly describe split
    assert_eq!(rows[0].tokens.output, 10);
    assert_eq!(rows[0].tokens.cache_read, 900);
    assert_eq!(rows[0].tokens.cache_write, 60);
    assert_eq!(rows[0].cache_write_1h, 50);
    assert_eq!(rows[0].tokens.reasoning, 0);
    assert_eq!(rows[0].timestamp_ms, 1_794_218_400_000);
}

#[test]
fn explicit_zero_cache_keys_override_bare_prompt_and_silence_is_not_zero() {
    let mut reader = ClaudeReader::new("a");
    for line in [
        br#"{"type":"assistant","timestamp":"2026-11-09T10:00:00Z","message":{"id":"a","model":"claude","usage":{"input_tokens":100}}}"#.as_slice(),
        br#"{"type":"assistant","message":{"id":"a","usage":{"cache_read_input_tokens":0}}}"#,
        br#"{"type":"assistant","message":{"id":"a","usage":{"input_tokens":0,"cache_read_input_tokens":0}}}"#,
        br#"{"type":"assistant","message":{"id":"a","usage":{"input_tokens":100}}}"#,
    ] { reader.ingest(line).unwrap(); }
    // The silent cache-bearing row marks the response as split while retaining
    // the prior input. A later split zero is then a same-unit max, not a reset.
    assert_eq!(reader.snapshot()[0].tokens.input, 100);
    let mut reader = ClaudeReader::new("a");
    for line in [
        br#"{"type":"assistant","timestamp":"2026-11-09T10:00:00Z","message":{"id":"a","model":"claude","usage":{"input_tokens":100}}}"#.as_slice(),
        br#"{"type":"assistant","message":{"id":"a","usage":{"input_tokens":0,"cache_read_input_tokens":0}}}"#,
        br#"{"type":"assistant","message":{"id":"a","usage":{"input_tokens":100}}}"#,
    ] { reader.ingest(line).unwrap(); }
    assert_eq!(reader.snapshot()[0].tokens.input, 0);
}

#[test]
fn distinct_requests_and_models_do_not_collapse() {
    let mut reader = ClaudeReader::new("a");
    for (request, model) in [("one", "claude-one"), ("two", "claude-two")] {
        reader.ingest(format!(r#"{{"type":"assistant","timestamp":"2026-11-09T10:00:00Z","requestId":"{request}","message":{{"id":"same","model":"{model}","usage":{{"input_tokens":3,"output_tokens":4}}}}}}"#).as_bytes()).unwrap();
    }
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].model, "claude-one");
    assert_eq!(rows[1].model, "claude-two");
}

#[test]
fn invalid_usage_and_unimplemented_billable_shapes_are_visible_without_mutation() {
    let mut reader = ClaudeReader::new("a");
    for line in [
        br#"{"type":"assistant","timestamp":"bad","message":{"model":"claude","usage":{"input_tokens":2}}}"#.as_slice(),
        br#"{"type":"assistant","timestamp":"2026-11-09T10:00:00Z","message":{"model":"claude","usage":{"input_tokens":-1}}}"#,
        br#"{"type":"assistant","timestamp":"2026-11-09T10:00:00Z","message":{"model":"claude","usage":{"input_tokens":2.5}}}"#,
        br#"{"type":"result","timestamp":"bad","model":"claude","usage":{"input_tokens":8}}"#,
        br#"{"type":"user","toolUseResult":{"usage":{"input_tokens":2}}}"#,
    ] {
        assert!(reader.ingest(line).is_err());
        assert!(reader.snapshot().is_empty());
    }
}

#[test]
fn advisor_iterations_emit_stable_supplementary_rows_and_merge_without_duplication() {
    let mut reader = ClaudeReader::new("session-a");
    reader.ingest(br#"{"type":"assistant","timestamp":"2026-11-09T10:00:00Z","requestId":"req","message":{"id":"msg","model":"claude-main","providerId":"gateway","usage":{"input_tokens":100,"output_tokens":10,"iterations":[{"type":"advisor_message","model":"claude-advisor","input_tokens":5,"output_tokens":2,"cache_read_input_tokens":1,"cache_creation_input_tokens":3,"cache_creation":{"ephemeral_1h_input_tokens":2}},{"type":"other","model":"ignored"},{"type":"advisor_message","model":"<synthetic>","input_tokens":999},{"type":"advisor_message","model":"malformed","input_tokens":-1}]}}}"#).unwrap();
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].dedup_key.as_deref(), Some("msg:req"));
    assert_eq!(rows[1].dedup_key.as_deref(), Some("msg:req:advisor:0"));
    assert_eq!(rows[1].model, "claude-advisor");
    assert_eq!(rows[1].key.session, "session-a");
    assert_eq!(rows[1].billing_provider_hint.as_deref(), Some("gateway"));
    assert_eq!(rows[1].timestamp_ms, rows[0].timestamp_ms);
    assert_eq!(rows[1].tokens.input, 5);
    assert_eq!(rows[1].tokens.output, 2);
    assert_eq!(rows[1].tokens.cache_read, 1);
    assert_eq!(rows[1].tokens.cache_write, 3);
    assert_eq!(rows[1].cache_write_1h, 2);

    reader.ingest(br#"{"type":"assistant","timestamp":"2026-11-09T10:00:01Z","requestId":"req","message":{"id":"msg","model":"claude-main","usage":{"input_tokens":100,"output_tokens":12,"iterations":[{"type":"advisor_message","model":"claude-advisor","input_tokens":7,"output_tokens":4,"cache_read_input_tokens":2,"cache_creation_input_tokens":3,"cache_creation":{"ephemeral_1h_input_tokens":3}}]}}}"#).unwrap();
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].tokens.input, 7);
    assert_eq!(rows[1].tokens.output, 4);
    assert_eq!(rows[1].tokens.cache_read, 2);
    assert_eq!(rows[1].cache_write_1h, 3);
}

#[test]
fn explicit_tool_result_usage_uses_transcript_identity_and_prior_ownership() {
    let mut reader = ClaudeReader::new("session-a");
    reader.ingest(br#"{"type":"assistant","timestamp":"2026-11-09T10:00:00Z","message":{"id":"msg","model":"claude-main","providerId":"gateway","usage":{"input_tokens":10}}}"#).unwrap();
    reader.ingest(br#"{"type":"user","timestamp":"2026-11-09T10:00:01Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tool-1","input_tokens":7}]}}"#).unwrap();
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[1].dedup_key.as_deref(),
        Some("tool_result:session-a:tool-1")
    );
    assert_eq!(rows[1].model, "claude-main");
    assert_eq!(rows[1].billing_provider_hint.as_deref(), Some("gateway"));
    assert_eq!(rows[1].tokens.input, 7);
    assert_eq!(rows[1].tokens.output, 0);
    assert_eq!(rows[1].tokens.cache_read, 0);
    assert_eq!(rows[1].tokens.cache_write, 0);

    reader.ingest(br#"{"type":"user","timestamp":"2026-11-09T10:00:02Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tool-1","input_tokens":9}]}}"#).unwrap();
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].tokens.input, 9);
}

#[test]
fn tool_result_without_explicit_tokens_is_ignored_and_synthetic_carrier_is_unsupported() {
    let mut reader = ClaudeReader::new("session-a");
    reader.ingest(br#"{"type":"assistant","timestamp":"2026-11-09T10:00:00Z","message":{"id":"msg","model":"claude-main","usage":{"input_tokens":10}}}"#).unwrap();
    reader.ingest(br#"{"type":"user","timestamp":"2026-11-09T10:00:01Z","message":{"role":"user","content":[{"type":"tool_result","text":"no explicit tokens"}]}}"#).unwrap();
    assert_eq!(reader.snapshot().len(), 1);

    reader.ingest(br#"{"type":"assistant","timestamp":"2026-11-09T10:00:02Z","message":{"id":"synthetic","model":"<synthetic>","usage":{"input_tokens":1}}}"#).unwrap();
    let before = reader.snapshot();
    let error = reader
        .ingest(br#"{"type":"tool_result","timestamp":"2026-11-09T10:00:03Z","input_tokens":4}"#)
        .unwrap_err();
    assert_eq!(
        error,
        token_monitor_lite::parsers::ReaderError::Unsupported("model-less tool result")
    );
    assert_eq!(reader.snapshot(), before);
}

#[test]
fn headless_stream_accumulates_completed_usage_and_incomplete_streams_stay_unpublished() {
    let mut reader = ClaudeReader::new("session-a");
    reader.ingest(br#"{"type":"message_start","timestamp":"2026-11-09T10:00:00Z","providerId":"gateway","message":{"model":"claude-headless","usage":{"input_tokens":10,"output_tokens":2,"cache_read_input_tokens":3,"cache_creation_input_tokens":4,"cache_creation":{"ephemeral_1h_input_tokens":1}}}}"#).unwrap();
    assert!(reader.snapshot().is_empty());
    reader.ingest(br#"{"type":"message_delta","usage":{"input_tokens":12,"output_tokens":5,"cache_read_input_tokens":4}}"#).unwrap();
    assert!(reader.snapshot().is_empty());
    reader.ingest(br#"{"type":"message_stop"}"#).unwrap();
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].model, "claude-headless");
    assert_eq!(rows[0].billing_provider_hint.as_deref(), Some("gateway"));
    assert_eq!(rows[0].dedup_key, None);
    assert_eq!(rows[0].tokens.input, 12);
    assert_eq!(rows[0].tokens.output, 5);
    assert_eq!(rows[0].tokens.cache_read, 4);
    assert_eq!(rows[0].tokens.cache_write, 4);
    assert_eq!(rows[0].cache_write_1h, 1);
    assert_eq!(rows[0].timestamp_ms, 1_794_218_400_000);

    reader.ingest(br#"{"type":"message_start","timestamp":"2026-11-09T10:01:00Z","message":{"model":"claude-incomplete","usage":{"input_tokens":99}}}"#).unwrap();
    assert_eq!(reader.snapshot().len(), 1);
}

#[test]
fn headless_result_events_emit_direct_usage_and_synthetic_streams_are_skipped() {
    let mut reader = ClaudeReader::new("session-a");
    reader.ingest(br#"{"type":"message_start","timestamp":"2026-11-09T10:00:00Z","message":{"model":"<synthetic>","usage":{"input_tokens":10}}}"#).unwrap();
    reader
        .ingest(br#"{"type":"message_delta","usage":{"input_tokens":99,"output_tokens":99}}"#)
        .unwrap();
    reader.ingest(br#"{"type":"message_stop"}"#).unwrap();
    assert!(reader.snapshot().is_empty());

    reader.ingest(br#"{"type":"result","timestamp":"2026-11-09T10:02:00Z","model":"claude-result","providerId":"gateway","usage":{"input_tokens":20,"output_tokens":6,"cache_creation_input_tokens":3}}"#).unwrap();
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].model, "claude-result");
    assert_eq!(rows[0].tokens.input, 20);
    assert_eq!(rows[0].tokens.output, 6);
    assert_eq!(rows[0].tokens.cache_write, 3);
    assert_eq!(rows[0].dedup_key, None);
    assert_eq!(rows[0].timestamp_ms, 1_794_218_520_000);
}

#[test]
fn sidechain_usage_stays_on_parent_session_and_preserves_agent_identity() {
    let mut reader = ClaudeReader::new("session-a");
    reader.ingest(br#"{"type":"assistant","isSidechain":true,"agentId":"agent-1","timestamp":"2026-11-09T10:00:00Z","requestId":"req","message":{"id":"msg","model":"anthropic/claude-sonnet-4-5","usage":{"input_tokens":5,"output_tokens":2}}}"#).unwrap();
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key.session, "session-a");
    assert_eq!(rows[0].key.client, "claude");
    assert_eq!(rows[0].model, "claude-sonnet-4-5");
    assert_eq!(rows[0].agent.as_deref(), Some("agent-1"));
    assert_eq!(rows[0].dedup_key.as_deref(), Some("msg:req"));

    let mut mirrored = ClaudeReader::with_client("cc-mirror/variant", "session-a")
        .with_fallback_timestamp(Some(1_794_218_400_000));
    mirrored
        .ingest(
            br#"{"type":"assistant","message":{"id":"msg","model":"claude","usage":{"input_tokens":7}}}"#,
        )
        .unwrap();
    let rows = mirrored.snapshot();
    assert_eq!(rows[0].key.client, "cc-mirror/variant");
    assert_eq!(rows[0].timestamp_ms, 1_794_218_400_000);
}

#[test]
fn counters_above_float_precision_stay_exact_and_unidentified_rows_stay_separate() {
    let mut reader = ClaudeReader::new("a");
    let line = br#"{"type":"assistant","timestamp":"2026-11-09T10:00:00Z","message":{"model":"claude","usage":{"input_tokens":9007199254740993}}}"#;
    reader.ingest(line).unwrap();
    reader.ingest(line).unwrap();
    assert_eq!(reader.snapshot().len(), 2);
    assert_eq!(reader.snapshot()[0].tokens.input, 9_007_199_254_740_993);
}

#[test]
fn request_start_time_offsets_and_one_hour_subset_survive_assembly() {
    let mut reader = ClaudeReader::new("a");
    reader
        .ingest(
            br#"{"type":"user","timestamp":"2026-11-09T15:29:59+05:30","message":{"role":"user"}}"#,
        )
        .unwrap();
    reader.ingest(br#"{"type":"assistant","timestamp":"2026-11-09T10:00:01Z","message":{"id":"a","model":"claude","usage":{"input_tokens":1,"cache_creation":{"ephemeral_1h_input_tokens":30}}}}"#).unwrap();
    assert_eq!(reader.snapshot()[0].cache_write_1h, 0);
    reader.ingest(br#"{"type":"assistant","message":{"id":"a","usage":{"cache_creation_input_tokens":40}}}"#).unwrap();
    let rows = reader.snapshot();
    assert_eq!(rows[0].cache_write_1h, 30);
    assert_eq!(rows[0].timestamp_ms, 1_794_218_399_000);
}

#[test]
fn errors_preserve_existing_responses_and_diagnostics_are_value_redacted() {
    let mut reader = ClaudeReader::new("a");
    reader.ingest(br#"{"type":"assistant","timestamp":"2026-11-09T10:00:00Z","message":{"id":"a","model":"claude","usage":{"input_tokens":7}}}"#).unwrap();
    for line in [
        br#"{"type":"assistant","message":{"id":"a","usage":{"input_tokens":"secret-marker"}}}"#.as_slice(),
        br#"{"type":"assistant","isSidechain":true,"agentId":"agent-x","message":{"id":"sidechain-new","usage":{"input_tokens":8}}}"#,
        br#"{"type":"result","usage":{"input_tokens":8}}"#,
        b"{",
    ] {
        let error = reader.ingest(line).unwrap_err();
        assert!(!format!("{error:?} {error}").contains("secret-marker"));
        assert_eq!(reader.snapshot()[0].tokens.input, 7);
    }
    reader.ingest(br#"{"type":"assistant","timestamp":"bad","message":{"model":" <SyNtHeTiC> ","usage":{"input_tokens":999}}}"#).unwrap();
    assert_eq!(reader.snapshot().len(), 1);
}

#[test]
fn response_capacity_is_visible_and_duplicates_still_fit_at_capacity() {
    use token_monitor_lite::parsers::claude::MAX_RESPONSES;
    let mut reader = ClaudeReader::new("a");
    for id in 0..MAX_RESPONSES {
        reader.ingest(format!(r#"{{"type":"assistant","timestamp":"2026-11-09T10:00:00Z","message":{{"id":"{id}","model":"claude","usage":{{"input_tokens":1}}}}}}"#).as_bytes()).unwrap();
    }
    assert!(reader.ingest(br#"{"type":"assistant","timestamp":"2026-11-09T10:00:00Z","message":{"model":"claude","usage":{"input_tokens":1}}}"#).is_err());
    reader
        .ingest(br#"{"type":"assistant","message":{"id":"0","usage":{"output_tokens":2}}}"#)
        .unwrap();
    let rows = reader.snapshot();
    assert_eq!(rows.len(), MAX_RESPONSES);
    assert_eq!(rows[0].tokens.output, 2);
}
