//! Trae usage-API cache reader contract.

use serde_json::json;
use token_monitor_lite::parsers::CostStatus;
use token_monitor_lite::parsers::trae::{CLIENT, TraeReader};

fn read(value: serde_json::Value) -> Vec<token_monitor_lite::parsers::ResponseUsage> {
    let mut reader = TraeReader::new("file").with_fallback_timestamp(Some(1_782_718_000_000));
    reader
        .ingest(value.to_string().as_bytes())
        .expect("cache is accepted");
    reader.snapshot()
}

fn session(model: &str, usage_time: i64) -> serde_json::Value {
    json!({
        "model_name": model,
        "session_id": "test-session-1",
        "usage_time": usage_time,
        "dollar_float": 0.5,
        "extra_info": {
            "input_token": 1000,
            "output_token": 500,
            "cache_read_token": 200,
            "cache_write_token": 100
        }
    })
}

#[test]
fn one_observation_is_emitted_per_session_object() {
    let found = read(json!([session("GPT-5.4", 1_776_000_000)]));
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].key.client, CLIENT);
    assert_eq!(found[0].key.session, "test-session-1");
}

#[test]
fn the_counters_come_from_extra_info() {
    let found = read(json!([session("GPT-5.4", 1_776_000_000)]));
    assert_eq!(found[0].tokens.input, 1000);
    assert_eq!(found[0].tokens.output, 500);
    assert_eq!(found[0].tokens.cache_read, 200);
    assert_eq!(found[0].tokens.cache_write, 100);
}

#[test]
fn usage_time_is_epoch_seconds_converted_to_milliseconds() {
    let found = read(json!([session("GPT-5.4", 1_776_000_000)]));
    assert_eq!(found[0].timestamp_ms, 1_776_000_000_000);
}

#[test]
fn a_display_name_is_normalised_to_a_model_id() {
    assert_eq!(
        read(json!([session("GPT-5.3-Codex", 1)]))[0].model,
        "gpt-5.3-codex"
    );
    assert_eq!(read(json!([session("GLM 5.1", 1)]))[0].model, "glm-5.1");
    assert_eq!(
        read(json!([session("Unknown Model", 1)]))[0].model,
        "Unknown Model"
    );
}

#[test]
fn the_provider_comes_from_the_model_family() {
    assert_eq!(
        read(json!([session("GPT-5.4", 1)]))[0]
            .billing_provider_hint
            .as_deref(),
        Some("openai")
    );
    assert_eq!(
        read(json!([session("Claude Sonnet 4.6", 1)]))[0]
            .billing_provider_hint
            .as_deref(),
        Some("anthropic")
    );
    assert_eq!(
        read(json!([session("Gemini 3.1 Pro", 1)]))[0]
            .billing_provider_hint
            .as_deref(),
        Some("google")
    );
    assert_eq!(
        read(json!([session("GLM 5.1", 1)]))[0]
            .billing_provider_hint
            .as_deref(),
        Some("zhipu")
    );
}

#[test]
fn auto_mode_is_bucketed_under_its_mode() {
    // Auto mode returns an empty `model_name`; the cost must still be attributed.
    let mut value = session("", 1_776_000_000);
    value["mode"] = json!("Auto");
    let found = read(json!([value]));
    assert_eq!(found[0].model, "trae-auto");
    assert_eq!(found[0].billing_provider_hint.as_deref(), Some("trae"));
}

#[test]
fn no_model_and_no_mode_falls_back_to_trae_unknown() {
    let found = read(json!([session("", 1_776_000_000)]));
    assert_eq!(found[0].model, "trae-unknown");
}

#[test]
fn a_zero_token_session_is_skipped() {
    let mut value = session("GPT-5.4", 1_776_000_000);
    value["extra_info"] = json!({
        "input_token": 0, "output_token": 0,
        "cache_read_token": 0, "cache_write_token": 0
    });
    assert!(read(json!([value])).is_empty());
}

#[test]
fn a_session_without_an_id_is_dropped() {
    // Every such record would collide on one dedup key.
    let mut value = session("GPT-5.4", 1_776_000_000);
    value.as_object_mut().unwrap().remove("session_id");
    assert!(read(json!([value])).is_empty());
}

#[test]
fn a_session_without_a_positive_usage_time_is_dropped() {
    let mut absent = session("GPT-5.4", 1_776_000_000);
    absent.as_object_mut().unwrap().remove("usage_time");
    assert!(read(json!([absent])).is_empty());
    // Zero would land at epoch 0.
    assert!(read(json!([session("GPT-5.4", 0)])).is_empty());
    assert!(read(json!([session("GPT-5.4", -5)])).is_empty());
}

#[test]
fn an_overflowing_usage_time_is_rejected() {
    // The cache is untrusted: near `i64::MAX` the multiply would panic in debug
    // and wrap negative in release.
    assert!(read(json!([session("GPT-5.4", i64::MAX)])).is_empty());
}

#[test]
fn the_cost_is_carried_in_nano_usd() {
    let found = read(json!([session("GPT-5.4", 1)]));
    assert_eq!(found[0].cost, CostStatus::Reported(500_000_000));
}

#[test]
fn a_zero_cost_is_not_claimed() {
    let mut value = session("GPT-5.4", 1);
    value["dollar_float"] = json!(0.0);
    assert_eq!(read(json!([value]))[0].cost, CostStatus::Unavailable);
}

#[test]
fn the_identity_is_the_session_and_usage_time() {
    let found = read(json!([session("GPT-5.4", 1_776_000_000)]));
    assert_eq!(
        found[0].dedup_key.as_deref(),
        Some("trae:test-session-1:1776000000")
    );
}

#[test]
fn an_empty_or_non_array_cache_yields_nothing() {
    assert!(read(json!([])).is_empty());
    assert!(read(json!({"sessions": []})).is_empty());
}

#[test]
fn malformed_json_is_a_visible_rejection() {
    let mut reader = TraeReader::new("f").with_fallback_timestamp(Some(1_000));
    assert!(reader.ingest(b"not json").is_err());
}
