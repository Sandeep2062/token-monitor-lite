//! Mux workspace-usage reader contract.

use token_monitor_lite::parsers::mux::{CLIENT, MuxReader};
use token_monitor_lite::parsers::CostStatus;

fn read(document: &str) -> Vec<token_monitor_lite::parsers::ResponseUsage> {
    let mut reader = MuxReader::new("ws_alpha").with_fallback_timestamp(Some(1_700_000_000_000));
    reader.ingest(document.as_bytes()).expect("document is accepted");
    reader.snapshot()
}

const TWO_MODELS: &str = r#"{
  "version": 1,
  "byModel": {
    "anthropic:claude-opus-4-6": {
      "input": {"tokens": 100, "cost_usd": 0.01},
      "cached": {"tokens": 5000, "cost_usd": 0.05},
      "cacheCreate": {"tokens": 200, "cost_usd": 0.02},
      "output": {"tokens": 300, "cost_usd": 0.03},
      "reasoning": {"tokens": 0, "cost_usd": 0}
    },
    "openai:gpt-4o": {
      "input": {"tokens": 50, "cost_usd": 0.005},
      "output": {"tokens": 150, "cost_usd": 0.015}
    }
  },
  "lastRequest": {"model": "anthropic:claude-opus-4-6", "timestamp": 1700000000000}
}"#;

#[test]
fn one_observation_is_emitted_per_model_entry() {
    let found = read(TWO_MODELS);
    assert_eq!(found.len(), 2);
    assert!(found.iter().all(|row| row.key.client == CLIENT));
}

#[test]
fn the_counters_map_to_the_workspace_buckets() {
    let found = read(TWO_MODELS);
    let claude = found
        .iter()
        .find(|row| row.model == "claude-opus-4-6")
        .expect("the claude entry is present");
    assert_eq!(claude.tokens.input, 100);
    assert_eq!(claude.tokens.cache_read, 5000);
    assert_eq!(claude.tokens.cache_write, 200);
    assert_eq!(claude.tokens.output, 300);
    assert_eq!(claude.tokens.reasoning, 0);
    assert_eq!(claude.cache_write_1h, 0);
}

#[test]
fn the_provider_prefix_becomes_the_billing_hint() {
    let found = read(TWO_MODELS);
    let claude = found.iter().find(|row| row.model == "claude-opus-4-6").unwrap();
    assert_eq!(claude.billing_provider_hint.as_deref(), Some("anthropic"));
    let gpt = found.iter().find(|row| row.model == "gpt-4o").unwrap();
    assert_eq!(gpt.billing_provider_hint.as_deref(), Some("openai"));
}

#[test]
fn an_unprefixed_model_carries_no_provider_hint() {
    // An empty provider string would be a fabricated key, so the absence of a
    // prefix must mean no hint rather than a blank one.
    let document = r#"{"byModel":{"claude-opus-4-6":{"input":{"tokens":100}}}}"#;
    let found = read(document);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].model, "claude-opus-4-6");
    assert_eq!(found[0].billing_provider_hint, None);
}

#[test]
fn a_model_name_containing_colons_keeps_them() {
    let document = r#"{"byModel":{"provider:sub:model-name":{"input":{"tokens":100}}}}"#;
    let found = read(document);
    assert_eq!(found[0].billing_provider_hint.as_deref(), Some("provider"));
    assert_eq!(found[0].model, "sub:model-name");
}

#[test]
fn the_reported_cost_is_the_sum_of_the_buckets() {
    let found = read(TWO_MODELS);
    let claude = found.iter().find(|row| row.model == "claude-opus-4-6").unwrap();
    // 0.01 + 0.05 + 0.02 + 0.03 = 0.11 USD, reported in nano-USD.
    assert_eq!(claude.cost, CostStatus::Reported(110_000_000));
}

#[test]
fn a_document_with_no_cost_reports_nothing() {
    let document = r#"{"byModel":{"m":{"input":{"tokens":10}}}}"#;
    assert_eq!(read(document)[0].cost, CostStatus::Unavailable);
}

#[test]
fn an_entry_with_no_tokens_is_not_an_observation() {
    let document = r#"{
      "byModel": {
        "anthropic:m": {
          "input": {"tokens": 0}, "cached": {"tokens": 0},
          "cacheCreate": {"tokens": 0}, "output": {"tokens": 0},
          "reasoning": {"tokens": 0}
        }
      }
    }"#;
    assert!(read(document).is_empty());
}

#[test]
fn a_negative_counter_is_clamped_not_rejected() {
    // This is an aggregate document: one bad bucket must not discard a
    // workspace's otherwise valid usage.
    let document = r#"{"byModel":{"anthropic:m":{"input":{"tokens":-50},"output":{"tokens":100}}}}"#;
    let found = read(document);
    assert_eq!(found[0].tokens.input, 0);
    assert_eq!(found[0].tokens.output, 100);
}

#[test]
fn an_empty_or_absent_by_model_yields_nothing() {
    assert!(read(r#"{"version":1,"byModel":{}}"#).is_empty());
    assert!(read(r#"{"version":1}"#).is_empty());
}

#[test]
fn the_session_is_the_workspace_and_the_key_is_workspace_scoped() {
    let found = read(TWO_MODELS);
    assert!(found.iter().all(|row| row.key.session == "ws_alpha"));
    let claude = found.iter().find(|row| row.model == "claude-opus-4-6").unwrap();
    assert_eq!(claude.dedup_key.as_deref(), Some("mux:ws_alpha:anthropic:claude-opus-4-6"));
}

#[test]
fn two_workspaces_using_the_same_model_do_not_collide() {
    // A positional key gave the first model in every workspace the same key, so
    // one workspace's usage was dropped in a cross-file dedup set.
    let alpha = read(TWO_MODELS);
    let mut beta_reader = MuxReader::new("ws_beta").with_fallback_timestamp(Some(1_000));
    beta_reader.ingest(TWO_MODELS.as_bytes()).expect("accepted");
    let beta = beta_reader.snapshot();
    let alpha_key = alpha
        .iter()
        .find(|row| row.model == "claude-opus-4-6")
        .and_then(|row| row.dedup_key.clone());
    let beta_key = beta
        .iter()
        .find(|row| row.model == "claude-opus-4-6")
        .and_then(|row| row.dedup_key.clone());
    assert!(alpha_key.is_some());
    assert_ne!(alpha_key, beta_key);
}

#[test]
fn the_last_request_timestamp_wins_over_the_fallback() {
    let found = read(TWO_MODELS);
    assert_eq!(found[0].timestamp_ms, 1_700_000_000_000);
}

#[test]
fn a_document_without_a_timestamp_uses_the_fallback() {
    let document = r#"{"byModel":{"anthropic:m":{"input":{"tokens":10}}}}"#;
    assert_eq!(read(document)[0].timestamp_ms, 1_700_000_000_000);
}

#[test]
fn malformed_json_is_a_visible_rejection() {
    let mut reader = MuxReader::new("ws").with_fallback_timestamp(Some(1_000));
    assert!(reader.ingest(b"not json").is_err());
}

#[test]
fn a_revision_replaces_rather_than_appends() {
    let mut reader = MuxReader::new("ws").with_fallback_timestamp(Some(1_000));
    reader.ingest(TWO_MODELS.as_bytes()).expect("accepted");
    reader.ingest(TWO_MODELS.as_bytes()).expect("accepted");
    assert_eq!(reader.snapshot().len(), 2);
    assert!(reader.has_supported_usage());
}
