//! BB thread/event reader contract.

use token_monitor_lite::parsers::bb::{BbReader, BbRow, BbThreadMeta, CLIENT};

fn meta(provider: &str, model: Option<&str>) -> BbThreadMeta {
    BbThreadMeta {
        id: "thr_1".into(),
        project_id: Some("prj_1".into()),
        project_name: Some("tokscale".into()),
        provider_id: provider.into(),
        model_override: model.map(str::to_owned),
        default_model: None,
        service_tier: Some("standard".into()),
        title: Some("Add feature".into()),
        parent_thread_id: None,
    }
}

fn usage_row(thread: &str, turn: Option<&str>, seq: i64, data: serde_json::Value) -> BbRow {
    BbRow {
        thread_id: thread.into(),
        turn_id: turn.map(str::to_owned),
        sequence: seq,
        event_type: "thread/tokenUsage/updated".into(),
        data: data.to_string(),
        created_at_ms: Some(1_787_275_053_000),
        meta: Some(meta("claude-code", Some("claude-3-7-sonnet"))),
    }
}

fn read(rows: Vec<BbRow>) -> Vec<token_monitor_lite::parsers::ResponseUsage> {
    let mut r = BbReader::new(1_000);
    for row in &rows {
        r.ingest(row).expect("row accepted");
    }
    r.snapshot()
}

fn anthropic_event() -> serde_json::Value {
    serde_json::json!({"tokenUsage": {
        "last": {
            "totalTokens": 89409, "inputTokens": 6, "cachedInputTokens": 89145,
            "cacheReadInputTokens": 87563, "cacheWriteInputTokens": 1582,
            "outputTokens": 258, "reasoningOutputTokens": 0
        },
        "promptCacheTtlMs": 3600000
    }})
}

#[test]
fn an_anthropic_turn_keeps_disjoint_buckets() {
    let found = read(vec![usage_row(
        "thr_1",
        Some("turn_1"),
        1,
        anthropic_event(),
    )]);
    assert_eq!(found.len(), 1);
    let m = &found[0];
    assert_eq!(m.key.client, CLIENT);
    assert_eq!(m.key.session, "thr_1");
    assert_eq!(m.model, "claude-3-7-sonnet");
    assert_eq!(m.billing_provider_hint.as_deref(), Some("anthropic"));
    assert_eq!(m.tokens.input, 6);
    assert_eq!(m.tokens.output, 258);
    assert_eq!(m.tokens.cache_read, 87563);
    assert_eq!(m.tokens.cache_write, 1582);
    assert_eq!(m.tokens.reasoning, 0);
}

#[test]
fn a_one_hour_cache_ttl_populates_the_one_hour_tier() {
    let found = read(vec![usage_row(
        "thr_1",
        Some("turn_1"),
        1,
        anthropic_event(),
    )]);
    assert_eq!(found[0].cache_write_1h, 1582);
}

#[test]
fn a_short_cache_ttl_leaves_the_one_hour_tier_empty() {
    let mut event = anthropic_event();
    event["tokenUsage"]["promptCacheTtlMs"] = serde_json::json!(300_000);
    let found = read(vec![usage_row("thr_1", Some("turn_1"), 1, event)]);
    assert_eq!(found[0].cache_write_1h, 0);
    // The ordinary cache_write bucket is unaffected.
    assert_eq!(found[0].tokens.cache_write, 1582);
}

#[test]
fn a_codex_turn_is_normalised_out_of_the_cumulative_convention() {
    let mut row = usage_row(
        "thr_2",
        Some("turn_2"),
        1,
        serde_json::json!({"tokenUsage": {"last": {
            "totalTokens": 20000, "inputTokens": 19000, "cachedInputTokens": 18000,
            "cacheReadInputTokens": 18000, "cacheWriteInputTokens": 0,
            "outputTokens": 1000, "reasoningOutputTokens": 600
        }}}),
    );
    row.meta = Some(meta("codex", Some("o3-mini")));
    let found = read(vec![row]);
    let m = &found[0];
    assert_eq!(m.billing_provider_hint.as_deref(), Some("openai"));
    assert_eq!(m.model, "o3-mini");
    // raw_input 19000 - cached 18000 = 1000 uncached.
    assert_eq!(m.tokens.input, 1000);
    assert_eq!(m.tokens.cache_read, 18000);
    // raw_output 1000 - reasoning 600 = 400.
    assert_eq!(m.tokens.output, 400);
    assert_eq!(m.tokens.reasoning, 600);
}

#[test]
fn a_fallback_model_event_renames_the_following_turns() {
    let mut fallback = usage_row("thr_3", Some("turn_3"), 0, serde_json::json!({}));
    fallback.event_type = "provider/modelFallback".into();
    fallback.data = serde_json::json!({"fallbackModel": "claude-3-5-haiku"}).to_string();

    let found = read(vec![
        fallback,
        usage_row(
            "thr_3",
            Some("turn_3"),
            2,
            serde_json::json!({"tokenUsage": {"last": {
                "totalTokens": 150, "inputTokens": 100, "outputTokens": 50
            }}}),
        ),
    ]);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].model, "claude-3-5-haiku");
}

#[test]
fn a_fork_replaying_a_turn_is_deduplicated() {
    let parent = usage_row(
        "thr_parent",
        Some("turn_1"),
        1,
        serde_json::json!({"tokenUsage": {"last": {
            "totalTokens": 500, "inputTokens": 400, "outputTokens": 100
        }}}),
    );
    // The fork replays turn_1 with a fresh event id but the same turn/sequence.
    let replayed = usage_row(
        "thr_child",
        Some("turn_1"),
        1,
        serde_json::json!({"tokenUsage": {"last": {
            "totalTokens": 500, "inputTokens": 400, "outputTokens": 100
        }}}),
    );
    let new_turn = usage_row(
        "thr_child",
        Some("turn_2"),
        2,
        serde_json::json!({"tokenUsage": {"last": {
            "totalTokens": 600, "inputTokens": 450, "outputTokens": 150
        }}}),
    );

    let found = read(vec![parent, replayed, new_turn]);
    assert_eq!(found.len(), 2, "the replayed turn must collapse");
    assert_eq!(found[0].tokens.input, 400);
    assert_eq!(found[1].tokens.input, 450);
}

#[test]
fn a_cumulative_only_thread_is_turned_into_deltas() {
    let first = usage_row(
        "thr_c",
        Some("turn_c1"),
        1,
        serde_json::json!({"tokenUsage": {"total": {
            "totalTokens": 1200, "inputTokens": 1000, "outputTokens": 200
        }}}),
    );
    let second = usage_row(
        "thr_c",
        Some("turn_c2"),
        2,
        serde_json::json!({"tokenUsage": {"total": {
            "totalTokens": 3000, "inputTokens": 2500, "outputTokens": 500
        }}}),
    );

    let found = read(vec![first, second]);
    assert_eq!(found.len(), 2);
    assert_eq!(found[0].tokens.input, 1000);
    assert_eq!(found[1].tokens.input, 1500);
    let total_input: u64 = found.iter().map(|m| m.tokens.input).sum();
    assert_eq!(total_input, 2500, "deltas must sum to the cumulative total");
}

#[test]
fn an_unmodelled_thread_preserves_unknown() {
    let mut row = usage_row(
        "thr_u",
        Some("turn_u1"),
        1,
        serde_json::json!({"tokenUsage": {"last": {
            "totalTokens": 120, "inputTokens": 100, "outputTokens": 20
        }}}),
    );
    row.meta = Some(meta("claude-code", None));
    let found = read(vec![row]);
    assert_eq!(found[0].model, "unknown");
    assert_eq!(found[0].billing_provider_hint.as_deref(), Some("anthropic"));
}

#[test]
fn a_custom_proxy_is_not_attributed_by_substring() {
    // `myclaudex-1` must not be read as Anthropic, and the raw runner id is
    // normalized rather than dropped.
    let mut row = usage_row(
        "thr_x",
        Some("turn_x"),
        1,
        serde_json::json!({"tokenUsage": {"last": {
            "totalTokens": 120, "inputTokens": 100, "outputTokens": 20
        }}}),
    );
    row.meta = Some(meta("custom-proxy", Some("myclaudex-1")));
    let found = read(vec![row]);
    assert_eq!(found[0].model, "myclaudex-1");
    assert_eq!(
        found[0].billing_provider_hint.as_deref(),
        Some("custom_proxy")
    );
}

#[test]
fn project_metadata_is_carried_as_the_workspace() {
    let found = read(vec![usage_row("thr_1", Some("t"), 1, anthropic_event())]);
    let m = &found[0];
    assert_eq!(m.workspace_key.as_deref(), Some("prj_1"));
    assert_eq!(m.workspace_label.as_deref(), Some("tokscale"));
    assert_eq!(m.title.as_deref(), Some("Add feature"));
    assert_eq!(m.service_tier.as_deref(), Some("standard"));
    assert_eq!(m.agent.as_deref(), Some("BB"));
}

#[test]
fn a_zero_token_event_is_skipped() {
    let found = read(vec![usage_row(
        "thr_z",
        Some("t"),
        1,
        serde_json::json!({"tokenUsage": {"last": {
            "totalTokens": 0, "inputTokens": 0, "outputTokens": 0
        }}}),
    )]);
    assert!(found.is_empty());
}

#[test]
fn an_event_without_token_usage_is_skipped() {
    let found = read(vec![usage_row(
        "thr_n",
        Some("t"),
        1,
        serde_json::json!({}),
    )]);
    assert!(found.is_empty());
}

#[test]
fn a_malformed_event_row_is_declined_not_fatal() {
    let mut r = BbReader::new(1_000);
    let mut row = usage_row("thr_m", Some("t"), 1, serde_json::json!({}));
    row.data = "NOT_JSON_DATA".into();
    assert!(r.ingest(&row).is_err());
}

#[test]
fn an_unknown_thread_uses_the_fallback_model() {
    let mut row = usage_row(
        "thr_orphan",
        Some("t"),
        1,
        serde_json::json!({"tokenUsage": {"last": {"totalTokens": 10, "inputTokens": 8, "outputTokens": 2}}}),
    );
    row.meta = None;
    let found = read(vec![row]);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].model, "default");
    assert_eq!(found[0].billing_provider_hint.as_deref(), Some("unknown"));
}

#[test]
fn there_is_no_cost_claim() {
    // BB records tokens, not money.
    let found = read(vec![usage_row("thr_1", Some("t"), 1, anthropic_event())]);
    assert_eq!(
        found[0].cost,
        token_monitor_lite::parsers::CostStatus::Unavailable
    );
}
