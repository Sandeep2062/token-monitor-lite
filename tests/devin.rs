//! Devin CLI reader contract.

use token_monitor_lite::parsers::devin::{CLIENT, DevinReader, DevinRow};
use token_monitor_lite::parsers::{CostStatus, ReaderError};

/// An assistant chat message carrying metrics.
fn assistant(request_id: Option<&str>, model: Option<&str>) -> String {
    let mut metadata = serde_json::json!({
        "metrics": {
            "input_tokens": 100,
            "output_tokens": 20,
            "cache_read_tokens": 30,
            "cache_creation_tokens": 10,
        }
    });
    if let Some(request_id) = request_id {
        metadata["request_id"] = serde_json::json!(request_id);
    }
    if let Some(model) = model {
        metadata["generation_model"] = serde_json::json!(model);
    }
    serde_json::json!({"role": "assistant", "metadata": metadata}).to_string()
}

fn row<'a>(session: &'a str, message: &'a str) -> DevinRow<'a> {
    DevinRow {
        row_id: 1,
        session_id: session,
        chat_message: message.as_bytes(),
        created_at_ms: Some(1_782_718_000_000),
        session_model: Some("claude-sonnet-4"),
        working_directory: None,
    }
}

fn read(rows: Vec<DevinRow<'_>>) -> Vec<token_monitor_lite::parsers::ResponseUsage> {
    let mut reader = DevinReader::new();
    for row in rows {
        reader.ingest(row, 1_000).expect("row is accepted");
    }
    reader.snapshot()
}

#[test]
fn metrics_are_read_from_the_chat_message_json_blob() {
    let found = read(vec![row("s1", &assistant(None, Some("claude-sonnet-4")))]);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].tokens.input, 100);
    assert_eq!(found[0].tokens.output, 20);
    assert_eq!(found[0].tokens.cache_read, 30);
    assert_eq!(found[0].tokens.cache_write, 10);
    assert_eq!(found[0].tokens.reasoning, 0);
    assert_eq!(found[0].key.client, CLIENT);
}

#[test]
fn a_non_assistant_row_is_not_a_call() {
    let message = serde_json::json!({"role": "user"}).to_string();
    assert!(read(vec![row("s1", &message)]).is_empty());
}

#[test]
fn the_generation_model_wins_over_the_session_model() {
    let found = read(vec![DevinRow {
        session_model: Some("session-model"),
        ..row("s1", &assistant(None, Some("generation-model")))
    }]);
    assert_eq!(found[0].model, "generation-model");
}

#[test]
fn the_session_model_is_the_fallback_when_no_generation_model_exists() {
    let found = read(vec![DevinRow {
        session_model: Some("session-model"),
        ..row("s1", &assistant(None, None))
    }]);
    assert_eq!(found[0].model, "session-model");
}

#[test]
fn an_adaptive_session_model_is_a_routing_mode_not_a_model() {
    // `adaptive` names a Devin routing mode; reporting it as a model would
    // invent one.
    let found = read(vec![DevinRow {
        session_model: Some("adaptive"),
        ..row("s1", &assistant(None, None))
    }]);
    assert!(found.is_empty());
}

#[test]
fn a_row_with_no_real_model_is_skipped() {
    let found = read(vec![DevinRow {
        session_model: None,
        ..row("s1", &assistant(None, None))
    }]);
    assert!(found.is_empty());
}

#[test]
fn the_request_id_is_the_dedup_identity_so_replayed_rows_collapse() {
    // Devin can persist one response in several rows; `request_id` identifies
    // the call while `row_id` differs between copies.
    let message = assistant(Some("req-1"), Some("claude-sonnet-4"));
    let found = read(vec![
        DevinRow {
            row_id: 1,
            ..row("s1", &message)
        },
        DevinRow {
            row_id: 2,
            ..row("s1", &message)
        },
    ]);
    assert_eq!(found.len(), 2);
    assert_eq!(found[0].dedup_key, found[1].dedup_key);
    assert_eq!(
        found[0].dedup_key.as_deref(),
        Some("devin-cli:s1:request:req-1")
    );
}

#[test]
fn a_row_without_a_request_id_falls_back_to_its_row_id() {
    let message = assistant(None, Some("m"));
    let found = read(vec![DevinRow {
        row_id: 42,
        ..row("s1", &message)
    }]);
    assert_eq!(found[0].dedup_key.as_deref(), Some("devin-cli:s1:row:42"));
}

#[test]
fn the_turn_start_is_back_anchored_from_its_recorded_end() {
    // `created_at` is stamped when the row is written, i.e. the turn's end.
    let message = serde_json::json!({
        "role": "assistant",
        "metadata": {
            "generation_model": "m",
            "metrics": {"input_tokens": 10, "total_time_ms": 5000}
        }
    })
    .to_string();
    let found = read(vec![DevinRow {
        created_at_ms: Some(1_782_718_000_000),
        ..row("s1", &message)
    }]);
    assert_eq!(found[0].timestamp_ms, 1_782_718_000_000 - 5000);
}

#[test]
fn the_back_anchor_does_not_apply_to_the_mtime_fallback() {
    // With no own `created_at`, the timestamp is the database mtime; shifting it
    // by a turn's duration would land on the wrong day.
    let message = serde_json::json!({
        "role": "assistant",
        "metadata": {
            "generation_model": "m",
            "metrics": {"input_tokens": 10, "total_time_ms": 5000}
        }
    })
    .to_string();
    let mut reader = DevinReader::new();
    reader
        .ingest(
            DevinRow {
                created_at_ms: None,
                ..row("s1", &message)
            },
            1_700_000_000_000,
        )
        .expect("accepted");
    assert_eq!(reader.snapshot()[0].timestamp_ms, 1_700_000_000_000);
}

#[test]
fn num_tokens_is_the_output_fallback_when_metrics_are_empty() {
    // Older rows carry only `num_tokens`; attributing it to output keeps the
    // message counted rather than dropping it.
    let message = serde_json::json!({
        "role": "assistant",
        "metadata": {"generation_model": "m", "num_tokens": 77}
    })
    .to_string();
    let found = read(vec![row("s1", &message)]);
    assert_eq!(found[0].tokens.output, 77);
    assert_eq!(found[0].tokens.input, 0);
}

#[test]
fn an_assistant_row_with_no_usage_at_all_is_skipped() {
    let message =
        serde_json::json!({"role": "assistant", "metadata": {"generation_model": "m"}}).to_string();
    assert!(read(vec![row("s1", &message)]).is_empty());
}

#[test]
fn cache_and_reasoning_are_not_invented() {
    let found = read(vec![row("s1", &assistant(None, Some("m")))]);
    assert_eq!(found[0].cache_write_1h, 0);
    assert_eq!(found[0].cost, CostStatus::Unavailable);
    assert_eq!(found[0].billing_provider_hint, None);
}

#[test]
fn the_session_is_the_cli_session_id() {
    let found = read(vec![row("session-abc", &assistant(None, Some("m")))]);
    assert_eq!(found[0].key.session, "session-abc");
}

#[test]
fn a_working_directory_becomes_a_label_never_a_workspace_key() {
    // An absolute path is refused in every runtime identity column, so only the
    // final component survives.
    let found = read(vec![DevinRow {
        working_directory: Some("/work/demo"),
        ..row("s1", &assistant(None, Some("m")))
    }]);
    assert_eq!(found[0].workspace_key, None);
    assert_eq!(found[0].workspace_label.as_deref(), Some("demo"));
}

#[test]
fn a_trailing_separator_does_not_produce_an_empty_label() {
    let found = read(vec![DevinRow {
        working_directory: Some("/work/demo/"),
        ..row("s1", &assistant(None, Some("m")))
    }]);
    assert_eq!(found[0].workspace_label.as_deref(), Some("demo"));
}

#[test]
fn malformed_json_is_a_visible_rejection() {
    let mut reader = DevinReader::new();
    assert_eq!(
        reader
            .ingest(
                DevinRow {
                    chat_message: b"not json",
                    ..row("s1", "x")
                },
                1_000
            )
            .expect_err("malformed"),
        ReaderError::InvalidRecord
    );
}

#[test]
fn a_negative_counter_is_a_visible_rejection() {
    let message = serde_json::json!({
        "role": "assistant",
        "metadata": {
            "generation_model": "m",
            "metrics": {"input_tokens": -5}
        }
    })
    .to_string();
    let mut reader = DevinReader::new();
    assert_eq!(
        reader
            .ingest(row("s1", &message), 1_000)
            .expect_err("negative counter"),
        ReaderError::InvalidRecord
    );
}

#[test]
fn an_empty_session_id_is_a_visible_rejection() {
    let message = assistant(None, Some("m"));
    let mut reader = DevinReader::new();
    assert_eq!(
        reader
            .ingest(row("", &message), 1_000)
            .expect_err("empty session"),
        ReaderError::InvalidRecord
    );
}

#[test]
fn a_reader_remembers_whether_it_has_supported_usage() {
    let mut reader = DevinReader::new();
    assert!(!reader.has_supported_usage());
    reader
        .ingest(row("s1", &assistant(None, Some("m"))), 1_000)
        .expect("accepted");
    assert!(reader.has_supported_usage());
}
