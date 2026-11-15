//! Cherry Studio chat-ledger reader contract.

use token_monitor_lite::parsers::CostStatus;
use token_monitor_lite::parsers::cherrystudio::{
    CHAT_MESSAGE_KIND, CLIENT, CherryReader, CherryRow,
};

/// A row with only the fields a test cares about; the rest are neutral.
fn row<'a>(id: &'a str) -> CherryRow<'a> {
    CherryRow {
        id,
        message_id: None,
        session_title: None,
        provider_id: None,
        model_id: Some("claude-sonnet-4"),
        input_tokens: Some(100),
        output_tokens: Some(20),
        cache_read_tokens: None,
        cache_write_tokens: None,
        reasoning_tokens: None,
        no_cache_tokens: None,
        cost: None,
        cost_currency: None,
        created_at: Some(1_782_718_000_000),
        message_kind: Some(CHAT_MESSAGE_KIND),
    }
}

fn snapshot(rows: Vec<CherryRow<'_>>) -> Vec<token_monitor_lite::parsers::ResponseUsage> {
    let mut reader = CherryReader::new();
    for row in rows {
        reader.ingest(row).expect("row is accepted");
    }
    reader.snapshot()
}

#[test]
fn inclusive_prompt_and_completion_are_split_into_disjoint_buckets() {
    // `input_tokens` is the whole prompt and `output_tokens` the whole
    // completion, with cache and reasoning counted again in their own columns.
    // Adding them on top would count every cache write and reasoning token
    // twice in the total.
    let usage = snapshot(vec![CherryRow {
        input_tokens: Some(1_000),
        output_tokens: Some(400),
        cache_read_tokens: Some(300),
        cache_write_tokens: Some(100),
        reasoning_tokens: Some(50),
        ..row("r1")
    }]);
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].tokens.input, 600);
    assert_eq!(usage[0].tokens.cache_read, 300);
    assert_eq!(usage[0].tokens.cache_write, 100);
    assert_eq!(usage[0].tokens.output, 350);
    assert_eq!(usage[0].tokens.reasoning, 50);
    // The five buckets sum to the ledger's reported 1_000 prompt + 400
    // completion, so no cache write or reasoning token is counted twice.
    assert_eq!(
        usage[0].tokens.input
            + usage[0].tokens.output
            + usage[0].tokens.cache_read
            + usage[0].tokens.cache_write
            + usage[0].tokens.reasoning,
        1_400
    );
}

#[test]
fn the_ledger_own_uncached_count_is_preferred_over_subtraction() {
    // The app's own `no_cache_tokens` is exactly the bucket modelled, so it
    // wins even when subtraction would produce a different figure.
    let usage = snapshot(vec![CherryRow {
        input_tokens: Some(1_000),
        cache_read_tokens: Some(300),
        cache_write_tokens: Some(100),
        no_cache_tokens: Some(555),
        ..row("r1")
    }]);
    assert_eq!(usage[0].tokens.input, 555);
}

#[test]
fn cache_larger_than_the_prompt_floors_at_zero_rather_than_wrapping() {
    let usage = snapshot(vec![CherryRow {
        input_tokens: Some(100),
        cache_read_tokens: Some(500),
        cache_write_tokens: Some(500),
        ..row("r1")
    }]);
    assert_eq!(usage[0].tokens.input, 0);
    assert_eq!(usage[0].tokens.cache_read, 500);
}

#[test]
fn reasoning_larger_than_output_floors_at_zero() {
    let usage = snapshot(vec![CherryRow {
        output_tokens: Some(10),
        reasoning_tokens: Some(90),
        ..row("r1")
    }]);
    assert_eq!(usage[0].tokens.output, 0);
    assert_eq!(usage[0].tokens.reasoning, 90);
}

#[test]
fn an_all_zero_row_carries_no_usage() {
    let usage = snapshot(vec![CherryRow {
        input_tokens: Some(0),
        output_tokens: Some(0),
        cache_read_tokens: Some(0),
        ..row("r1")
    }]);
    assert!(usage.is_empty());
}

#[test]
fn an_all_zero_row_with_a_positive_usd_cost_is_retained() {
    // The pinned reader keeps such a row: it is a real charge that happened to
    // record no tokens, and dropping it would hide the spend.
    let usage = snapshot(vec![CherryRow {
        input_tokens: Some(0),
        output_tokens: Some(0),
        cost: Some(0.25),
        cost_currency: Some("USD"),
        ..row("r1")
    }]);
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].cost, CostStatus::Reported(250_000_000));
}

#[test]
fn a_non_usd_nonzero_cost_is_left_to_pricing() {
    // The ledger's nullable currency has no USD default, so a nonzero amount in
    // another currency must not be copied as USD.
    let usage = snapshot(vec![CherryRow {
        input_tokens: Some(0),
        output_tokens: Some(0),
        cost: Some(1.5),
        cost_currency: Some("CNY"),
        ..row("r1")
    }]);
    // No tokens and no authoritative USD cost: the row carries nothing.
    assert!(usage.is_empty());
}

#[test]
fn a_non_usd_cost_still_keeps_the_tokens_it_reports() {
    let usage = snapshot(vec![CherryRow {
        cost: Some(1.5),
        cost_currency: Some("CNY"),
        ..row("r1")
    }]);
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].cost, CostStatus::Unavailable);
}

#[test]
fn an_explicit_usd_zero_cost_is_reported_not_estimated() {
    let usage = snapshot(vec![CherryRow {
        cost: Some(0.0),
        cost_currency: Some("USD"),
        ..row("r1")
    }]);
    assert_eq!(usage[0].cost, CostStatus::Reported(0));
}

#[test]
fn an_agent_session_row_is_skipped() {
    // Those are the same billed calls the `claude` transcripts already report,
    // so reading them here would count Agent mode twice.
    let usage = snapshot(vec![CherryRow {
        message_kind: Some("agent-session"),
        ..row("r1")
    }]);
    assert!(usage.is_empty());
}

#[test]
fn a_row_predating_the_message_kind_column_is_read() {
    // Older builds have no `message_kind`; those rows carry no transcript, so
    // they are exactly the ones this reader adds.
    let usage = snapshot(vec![CherryRow {
        message_kind: None,
        ..row("r1")
    }]);
    assert_eq!(usage.len(), 1);
}

#[test]
fn the_provider_is_the_session_and_the_provider_hint() {
    let usage = snapshot(vec![CherryRow {
        provider_id: Some("anthropic"),
        ..row("r1")
    }]);
    assert_eq!(usage[0].key.session, "anthropic");
    assert_eq!(usage[0].billing_provider_hint.as_deref(), Some("anthropic"));
}

#[test]
fn a_row_without_a_provider_falls_back_to_an_unknown_session() {
    let usage = snapshot(vec![row("r1")]);
    assert_eq!(usage[0].key.session, "unknown");
    assert_eq!(usage[0].billing_provider_hint, None);
}

#[test]
fn a_missing_model_degrades_to_unknown_rather_than_dropping_the_row() {
    let usage = snapshot(vec![CherryRow {
        model_id: None,
        ..row("r1")
    }]);
    assert_eq!(usage[0].model, "unknown");
}

#[test]
fn a_synthetic_model_is_skipped() {
    let usage = snapshot(vec![CherryRow {
        model_id: Some("<synthetic>"),
        ..row("r1")
    }]);
    assert!(usage.is_empty());
}

#[test]
fn a_blank_model_degrades_to_unknown() {
    let usage = snapshot(vec![CherryRow {
        model_id: Some("   "),
        ..row("r1")
    }]);
    assert_eq!(usage[0].model, "unknown");
}

#[test]
fn the_ledger_row_id_is_the_dedup_identity() {
    // `ai_usage_record.id` is the ledger's own primary key, so it stays stable
    // even when `created_at` collides.
    let usage = snapshot(vec![row("row-abc")]);
    assert_eq!(
        usage[0].dedup_key.as_deref(),
        Some("cherrystudio-sqlite:row-abc")
    );
    assert_eq!(usage[0].key.client, CLIENT);
}

#[test]
fn a_missing_or_non_positive_created_at_falls_back_to_zero() {
    let usage = snapshot(vec![CherryRow {
        created_at: Some(0),
        ..row("r1")
    }]);
    assert_eq!(usage[0].timestamp_ms, 0);
    let usage = snapshot(vec![CherryRow {
        created_at: None,
        ..row("r1")
    }]);
    assert_eq!(usage[0].timestamp_ms, 0);
}

#[test]
fn the_conversation_title_is_carried_as_presentation_metadata() {
    let usage = snapshot(vec![CherryRow {
        message_id: Some("m1"),
        session_title: Some("Refactor the parser"),
        ..row("r1")
    }]);
    assert_eq!(usage[0].title.as_deref(), Some("Refactor the parser"));
}

#[test]
fn a_negative_counter_is_a_visible_rejection_not_a_clamp() {
    let mut reader = CherryReader::new();
    let error = reader
        .ingest(CherryRow {
            cache_read_tokens: Some(-5),
            ..row("r1")
        })
        .expect_err("negative counters are rejected");
    assert_eq!(
        error,
        token_monitor_lite::parsers::ReaderError::InvalidRecord
    );
}

#[test]
fn an_empty_row_id_is_a_visible_rejection() {
    let mut reader = CherryReader::new();
    assert_eq!(
        reader.ingest(row("")).expect_err("empty id rejected"),
        token_monitor_lite::parsers::ReaderError::InvalidRecord
    );
}

#[test]
fn cost_is_never_claimed_when_the_ledger_reports_none() {
    let usage = snapshot(vec![row("r1")]);
    assert_eq!(usage[0].cost, CostStatus::Unavailable);
    assert_eq!(usage[0].cache_write_1h, 0);
    assert_eq!(usage[0].service_tier, None);
    assert_eq!(usage[0].agent, None);
    assert_eq!(usage[0].workspace_key, None);
}

#[test]
fn a_row_remembers_whether_it_has_supported_usage() {
    let mut reader = CherryReader::new();
    assert!(!reader.has_supported_usage());
    reader.ingest(row("r1")).expect("row is accepted");
    assert!(reader.has_supported_usage());
}
