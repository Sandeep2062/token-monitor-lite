//! Unsloth Studio reader contract.

use token_monitor_lite::parsers::unsloth::{
    API_AGENT, CLIENT, Lane, STUDIO_AGENT, UnslothReader, UnslothRow,
};
use token_monitor_lite::parsers::{CostStatus, ReaderError};

/// A chat row with only the fields a test cares about.
fn chat<'a>(id: &'a str) -> UnslothRow<'a> {
    UnslothRow {
        lane: Lane::Chat,
        id,
        thread_id: Some("thread-1"),
        endpoint: None,
        created_at_secs: 1_782_718_000,
        prompt_tokens: 100,
        completion_tokens: 20,
        total_tokens: 120,
        cached_tokens: 0,
        cache_write_tokens: 0,
        reasoning_tokens: 0,
        requested_model: Some("llama-3"),
        response_model: None,
        provider_type: Some("local"),
        thread_model: None,
    }
}

fn snapshot(rows: Vec<UnslothRow<'_>>) -> Vec<token_monitor_lite::parsers::ResponseUsage> {
    let mut reader = UnslothReader::new();
    for row in rows {
        reader.ingest(row).expect("row is accepted");
    }
    reader.snapshot()
}

#[test]
fn local_inference_is_an_authoritative_zero_cost() {
    let usage = snapshot(vec![chat("m1")]);
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].cost, CostStatus::Reported(0));
    assert_eq!(usage[0].billing_provider_hint.as_deref(), Some("local"));
    assert_eq!(usage[0].agent.as_deref(), Some(STUDIO_AGENT));
    assert_eq!(usage[0].key.client, CLIENT);
}

#[test]
fn a_metered_route_keeps_its_canonical_provider_and_no_cost() {
    // A metered route has a real price, so the reader must not claim zero.
    for (route, canonical) in [
        ("anthropic", "anthropic"),
        ("deepseek", "deepseek"),
        ("gemini", "google"),
        ("huggingface", "huggingface"),
        ("kimi", "moonshotai"),
        ("mistral", "mistralai"),
        ("openai", "openai"),
        ("openrouter", "openrouter"),
        ("qwen", "qwen"),
    ] {
        let usage = snapshot(vec![UnslothRow {
            provider_type: Some(route),
            ..chat("m1")
        }]);
        assert_eq!(
            usage[0].billing_provider_hint.as_deref(),
            Some(canonical),
            "route {route}"
        );
        assert_eq!(usage[0].cost, CostStatus::Unavailable, "route {route}");
    }
}

#[test]
fn a_route_name_is_matched_case_insensitively_and_trimmed() {
    let usage = snapshot(vec![UnslothRow {
        provider_type: Some("  Gemini  "),
        ..chat("m1")
    }]);
    assert_eq!(usage[0].billing_provider_hint.as_deref(), Some("google"));
}

#[test]
fn an_unrecognised_route_carries_no_provider_hint() {
    // Custom and subscription-backed routes have no reliable billing identity,
    // so nothing may be priced against a guess.
    let usage = snapshot(vec![UnslothRow {
        provider_type: Some("my-custom-gateway"),
        ..chat("m1")
    }]);
    assert_eq!(usage[0].billing_provider_hint, None);
    assert_eq!(usage[0].cost, CostStatus::Unavailable);
}

#[test]
fn a_missing_provider_type_is_unpriced() {
    let usage = snapshot(vec![UnslothRow {
        provider_type: None,
        ..chat("m1")
    }]);
    assert_eq!(usage[0].billing_provider_hint, None);
}

#[test]
fn the_responding_model_wins_over_the_requested_one_and_the_thread() {
    let usage = snapshot(vec![UnslothRow {
        response_model: Some("answered-model"),
        requested_model: Some("asked-model"),
        thread_model: Some("thread-model"),
        ..chat("m1")
    }]);
    assert_eq!(usage[0].model, "answered-model");

    let usage = snapshot(vec![UnslothRow {
        response_model: None,
        requested_model: Some("asked-model"),
        thread_model: Some("thread-model"),
        ..chat("m1")
    }]);
    assert_eq!(usage[0].model, "asked-model");

    let usage = snapshot(vec![UnslothRow {
        response_model: None,
        requested_model: None,
        thread_model: Some("thread-model"),
        ..chat("m1")
    }]);
    assert_eq!(usage[0].model, "thread-model");
}

#[test]
fn a_row_with_no_model_anywhere_degrades_to_unknown() {
    let usage = snapshot(vec![UnslothRow {
        response_model: None,
        requested_model: None,
        thread_model: None,
        ..chat("m1")
    }]);
    assert_eq!(usage[0].model, "unknown");
}

#[test]
fn cache_and_reasoning_are_moved_out_of_the_inclusive_counts() {
    let usage = snapshot(vec![UnslothRow {
        prompt_tokens: 100,
        completion_tokens: 40,
        total_tokens: 140,
        cached_tokens: 30,
        cache_write_tokens: 10,
        reasoning_tokens: 15,
        ..chat("m1")
    }]);
    assert_eq!(usage[0].tokens.cache_read, 30);
    assert_eq!(usage[0].tokens.cache_write, 10);
    assert_eq!(usage[0].tokens.reasoning, 15);
    assert_eq!(usage[0].tokens.input, 60);
    assert_eq!(usage[0].tokens.output, 25);
}

#[test]
fn a_larger_reported_total_is_trusted() {
    let usage = snapshot(vec![UnslothRow {
        prompt_tokens: 10,
        completion_tokens: 5,
        total_tokens: 40,
        ..chat("m1")
    }]);
    assert_eq!(usage[0].tokens.input, 35);
}

#[test]
fn cache_larger_than_the_prompt_floors_rather_than_going_negative() {
    let usage = snapshot(vec![UnslothRow {
        prompt_tokens: 10,
        completion_tokens: 20,
        total_tokens: 30,
        cached_tokens: 500,
        ..chat("m1")
    }]);
    // The cache read is capped at the prompt, so input cannot go negative.
    assert_eq!(usage[0].tokens.cache_read, 10);
    assert_eq!(usage[0].tokens.input, 0);
}

#[test]
fn an_all_zero_row_carries_no_usage() {
    let usage = snapshot(vec![UnslothRow {
        prompt_tokens: 0,
        completion_tokens: 0,
        total_tokens: 0,
        ..chat("m1")
    }]);
    assert!(usage.is_empty());
}

#[test]
fn the_chat_session_is_the_thread_id() {
    let usage = snapshot(vec![chat("m1")]);
    assert_eq!(usage[0].key.session, "thread-1");
    assert_eq!(usage[0].dedup_key.as_deref(), Some("unsloth:chat:m1"));
}

#[test]
fn a_blank_thread_id_falls_back_to_an_identity_session() {
    let usage = snapshot(vec![UnslothRow {
        thread_id: Some("   "),
        ..chat("m1")
    }]);
    assert_eq!(usage[0].key.session, "unsloth:chat:m1");
}

#[test]
fn the_external_api_lane_is_local_and_carries_the_endpoint_as_a_title() {
    let usage = snapshot(vec![UnslothRow {
        lane: Lane::Api,
        id: "evt-1",
        thread_id: None,
        endpoint: Some("/v1/chat/completions"),
        created_at_secs: 1_782_718_000,
        prompt_tokens: 50,
        completion_tokens: 10,
        total_tokens: 60,
        cached_tokens: 0,
        cache_write_tokens: 0,
        reasoning_tokens: 0,
        requested_model: Some("local-model"),
        response_model: None,
        provider_type: None,
        thread_model: None,
    }]);
    assert_eq!(usage[0].key.session, "unsloth:api");
    assert_eq!(usage[0].dedup_key.as_deref(), Some("unsloth:api:evt-1"));
    assert_eq!(usage[0].agent.as_deref(), Some(API_AGENT));
    assert_eq!(usage[0].billing_provider_hint.as_deref(), Some("local"));
    assert_eq!(usage[0].cost, CostStatus::Reported(0));
    assert_eq!(usage[0].title.as_deref(), Some("/v1/chat/completions"));
}

#[test]
fn studio_stores_epoch_seconds() {
    let usage = snapshot(vec![UnslothRow {
        created_at_secs: 1_782_718_000,
        ..chat("m1")
    }]);
    assert_eq!(usage[0].timestamp_ms, 1_782_718_000_000);
}

#[test]
fn a_non_positive_timestamp_is_skipped() {
    for seconds in [0, -1] {
        let usage = snapshot(vec![UnslothRow {
            created_at_secs: seconds,
            ..chat("m1")
        }]);
        assert!(usage.is_empty(), "seconds {seconds}");
    }
}

#[test]
fn a_negative_counter_is_a_visible_rejection() {
    let mut reader = UnslothReader::new();
    assert_eq!(
        reader
            .ingest(UnslothRow {
                cached_tokens: -5,
                ..chat("m1")
            })
            .expect_err("negative counter"),
        ReaderError::InvalidRecord
    );
}

#[test]
fn an_empty_row_id_is_a_visible_rejection() {
    let mut reader = UnslothReader::new();
    assert_eq!(
        reader.ingest(chat("")).expect_err("empty id"),
        ReaderError::InvalidRecord
    );
}

#[test]
fn cost_is_never_guessed_and_presentation_fields_stay_empty() {
    let usage = snapshot(vec![UnslothRow {
        provider_type: Some("anthropic"),
        ..chat("m1")
    }]);
    assert_eq!(usage[0].workspace_key, None);
    assert_eq!(usage[0].title, None);
    assert_eq!(usage[0].service_tier, None);
    assert_eq!(usage[0].cache_write_1h, 0);
}

#[test]
fn a_reader_remembers_whether_it_has_supported_usage() {
    let mut reader = UnslothReader::new();
    assert!(!reader.has_supported_usage());
    reader.ingest(chat("m1")).expect("accepted");
    assert!(reader.has_supported_usage());
}
