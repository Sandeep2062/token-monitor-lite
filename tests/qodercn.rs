//! Synthetic Qoder CN cases adapted from the pinned Token Monitor fork reader
//! `crates/tokscale-core/src/token_monitor/qodercn.rs`.
//!
//! Both lanes are covered: the legacy `chat_message` SQLite store and the
//! Claude-compatible JSONL transcripts. Both report a cache-inclusive prompt
//! count, so the same two arithmetic rules are asserted in each.

use rusqlite::{Connection, params};
use serde_json::json;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use token_monitor_lite::accounting::TokenCounts;
use token_monitor_lite::parsers::ReaderError;
use token_monitor_lite::parsers::qodercn::{
    CLIENT, DEFAULT_MODEL, Lane, QodercnReader, QodercnRow, database_counters, display_model,
    jsonl_counters, transcript_model,
};
use token_monitor_lite::source_inputs::QodercnAdapter;

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Root(PathBuf);

impl Root {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "token-monitor-qodercn-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn row<'a>(lane: Lane, prompt: u64, cached: u64, output: u64) -> QodercnRow<'a> {
    QodercnRow {
        lane,
        source: "local",
        session_id: Some("sess_1"),
        message_id: Some("msg_1"),
        prompt,
        cached,
        output,
        model: Some("qmodel"),
        workspace: None,
        created_at_ms: 1_782_718_000_000,
    }
}

#[test]
fn a_database_row_splits_the_cache_inclusive_prompt() {
    let mut reader = QodercnReader::new("local");
    // 120 is prompt+cached, so input keeps the uncached part and cache_read the
    // overlap: the two buckets must sum to the reported 120, never 220.
    reader.ingest(row(Lane::Database, 120, 20, 50)).unwrap();
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key.client, CLIENT);
    assert_eq!(
        rows[0].tokens,
        TokenCounts {
            input: 100,
            output: 50,
            cache_read: 20,
            cache_write: 0,
            reasoning: 0,
        }
    );
    assert_eq!(rows[0].tokens.input + rows[0].tokens.cache_read, 120);
}

#[test]
fn a_cached_count_above_the_prompt_is_clamped_rather_than_inverted() {
    let mut reader = QodercnReader::new("local");
    reader.ingest(row(Lane::Database, 100, 400, 10)).unwrap();
    let rows = reader.snapshot();
    // input saturates at zero and cache_read takes the prompt: the row stays
    // internally consistent instead of reporting a negative input.
    assert_eq!(rows[0].tokens.input, 0);
    assert_eq!(rows[0].tokens.cache_read, 100);
}

#[test]
fn an_all_zero_row_is_not_measured_usage() {
    let mut reader = QodercnReader::new("local");
    reader.ingest(row(Lane::Database, 0, 0, 0)).unwrap();
    // Plan-billed first-party rows record credits and a context ratio but no
    // tokens; they are not measured zero usage.
    assert!(reader.snapshot().is_empty());
    assert!(!reader.has_supported_usage());
}

#[test]
fn an_output_only_row_is_kept() {
    let mut reader = QodercnReader::new("local");
    reader.ingest(row(Lane::Database, 0, 0, 25)).unwrap();
    assert_eq!(reader.snapshot().len(), 1);
}

#[test]
fn the_model_code_becomes_the_app_display_name() {
    let mut reader = QodercnReader::new("local");
    reader.ingest(row(Lane::Database, 10, 0, 5)).unwrap();
    assert_eq!(reader.snapshot()[0].model, "Qwen3.7-Plus");
    assert_eq!(display_model("gm51model"), "GLM-5.2");
    assert_eq!(display_model("mmodel"), "MiniMax-M3");
}

#[test]
fn an_unmapped_model_code_passes_through_unchanged() {
    // A custom model is not in the app's table, so it keeps its own code.
    assert_eq!(display_model("my-custom-model"), "my-custom-model");
}

#[test]
fn a_missing_model_falls_back_to_the_documented_default() {
    let mut reader = QodercnReader::new("local");
    let mut input = row(Lane::Database, 10, 0, 5);
    input.model = None;
    reader.ingest(input).unwrap();
    assert_eq!(reader.snapshot()[0].model, DEFAULT_MODEL);
}

#[test]
fn a_routing_tier_carries_no_provider_hint() {
    // A tier names a tier, not a model; attaching the client as a provider hint
    // would let catalog pricing resolve it to some other vendor's model.
    for tier in ["auto", "ultimate", "performance", "efficient", "lite"] {
        let mut reader = QodercnReader::new("local");
        let mut input = row(Lane::Database, 10, 0, 5);
        input.model = Some(tier);
        reader.ingest(input).unwrap();
        let rows = reader.snapshot();
        assert_eq!(rows[0].billing_provider_hint, None, "tier {tier}");
        assert_ne!(rows[0].model, DEFAULT_MODEL);
    }
}

#[test]
fn a_real_model_carries_the_client_as_provider_hint() {
    let mut reader = QodercnReader::new("local");
    reader.ingest(row(Lane::Database, 10, 0, 5)).unwrap();
    assert_eq!(
        reader.snapshot()[0].billing_provider_hint.as_deref(),
        Some(CLIENT)
    );
}

#[test]
fn the_two_lanes_do_not_collide_on_identity() {
    let mut reader = QodercnReader::new("local");
    reader.ingest(row(Lane::Database, 10, 0, 5)).unwrap();
    reader.ingest(row(Lane::Jsonl, 10, 0, 5)).unwrap();
    let keys: Vec<_> = reader
        .snapshot()
        .iter()
        .map(|row| row.dedup_key.clone().unwrap())
        .collect();
    assert_eq!(keys.len(), 2);
    assert_ne!(keys[0], keys[1]);
    assert!(keys[0].contains(":db:"));
    assert!(keys[1].contains(":jsonl:"));
}

#[test]
fn an_unknown_session_is_named_rather_than_dropped() {
    let mut reader = QodercnReader::new("local");
    let mut input = row(Lane::Database, 10, 0, 5);
    input.session_id = None;
    reader.ingest(input).unwrap();
    assert_eq!(reader.snapshot()[0].key.session, "qodercn:db:local:unknown");
}

#[test]
fn an_absolute_workspace_path_never_becomes_the_workspace_key() {
    let mut reader = QodercnReader::new("local");
    let mut input = row(Lane::Database, 10, 0, 5);
    input.workspace = Some("/home/someone/projects/demo");
    reader.ingest(input).unwrap();
    let rows = reader.snapshot();
    // The store rejects absolute-path text in identity columns, so only the
    // final component survives, as a presentation label.
    assert_eq!(rows[0].workspace_key, None);
    assert_eq!(rows[0].workspace_label.as_deref(), Some("demo"));
}

#[test]
fn a_bare_project_name_is_kept_as_the_label() {
    let mut reader = QodercnReader::new("local");
    let mut input = row(Lane::Database, 10, 0, 5);
    input.workspace = Some("demo-project");
    reader.ingest(input).unwrap();
    let rows = reader.snapshot();
    assert_eq!(rows[0].workspace_key, None);
    assert_eq!(rows[0].workspace_label.as_deref(), Some("demo-project"));
}

#[test]
fn the_no_project_sentinel_is_not_a_label() {
    let mut reader = QodercnReader::new("local");
    let mut input = row(Lane::Database, 10, 0, 5);
    input.workspace = Some(".");
    reader.ingest(input).unwrap();
    let rows = reader.snapshot();
    assert_eq!(rows[0].workspace_key, None);
    assert_eq!(rows[0].workspace_label, None);
}

#[test]
fn cost_is_never_claimed() {
    let mut reader = QodercnReader::new("local");
    reader.ingest(row(Lane::Database, 10, 0, 5)).unwrap();
    assert_eq!(
        reader.snapshot()[0].cost,
        token_monitor_lite::parsers::CostStatus::Unavailable
    );
}

#[test]
fn a_transcript_line_uses_the_same_split_as_the_database() {
    let mut reader = QodercnReader::new("sess-file");
    reader
        .ingest_record(
            br#"{"type":"assistant","sessionId":"s9","timestamp":"2026-06-20T10:00:05Z","message":{"id":"m1","model":"qmodel","usage":{"input_tokens":120,"cache_read_input_tokens":20,"output_tokens":50}}}"#,
        )
        .unwrap();
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key.session, "qodercn:jsonl:sess-file:s9");
    assert_eq!(
        rows[0].tokens,
        TokenCounts {
            input: 100,
            output: 50,
            cache_read: 20,
            cache_write: 0,
            reasoning: 0,
        }
    );
    assert_eq!(rows[0].timestamp_ms, 1_781_949_605_000);
}

#[test]
fn a_non_assistant_transcript_line_contributes_nothing() {
    let mut reader = QodercnReader::new("sess-file");
    reader
        .ingest_record(br#"{"type":"user","sessionId":"s9","message":{"role":"user"}}"#)
        .unwrap();
    assert!(reader.snapshot().is_empty());
}

#[test]
fn a_transcript_line_without_usage_contributes_nothing() {
    let mut reader = QodercnReader::new("sess-file");
    reader
        .ingest_record(
            br#"{"type":"assistant","sessionId":"s9","message":{"id":"m1","content":"no usage"}}"#,
        )
        .unwrap();
    assert!(reader.snapshot().is_empty());
}

#[test]
fn a_transcript_custom_profile_prefix_is_stripped_but_its_model_kept() {
    assert_eq!(
        transcript_model("qoder-custom-profile/gm51model"),
        "GLM-5.2"
    );
    // Only the profile prefix is stripped: a provider-qualified real model
    // survives intact.
    assert_eq!(
        transcript_model("qoder-custom-profile/vendor/real-model"),
        "vendor/real-model"
    );
}

#[test]
fn a_fractional_database_counter_truncates_like_the_pinned_adapter() {
    // The pinned adapter runs `Number(value)` and truncates, so a fractional
    // counter is a real reading rounded down, not a malformed one.
    let usage = json!({"prompt_tokens": 10.9, "completion_tokens": 5.4});
    assert_eq!(database_counters(&usage), Ok((10, 0, 5)));
}

#[test]
fn a_negative_database_counter_is_a_visible_rejection() {
    let negative = json!({"prompt_tokens": 10, "completion_tokens": -0.5});
    assert_eq!(
        database_counters(&negative),
        Err(ReaderError::InvalidRecord)
    );
}

#[test]
fn a_non_object_token_info_is_a_visible_rejection() {
    assert_eq!(
        database_counters(&json!("not an object")),
        Err(ReaderError::InvalidRecord)
    );
    assert_eq!(
        jsonl_counters(&json!([1, 2, 3])),
        Err(ReaderError::InvalidRecord)
    );
}

#[test]
fn a_missing_required_counter_is_a_visible_rejection() {
    // prompt_tokens is required; cached_tokens is optional and defaults to 0.
    assert_eq!(
        database_counters(&json!({"completion_tokens": 5})),
        Err(ReaderError::InvalidRecord)
    );
    assert_eq!(
        database_counters(&json!({"prompt_tokens": 10})),
        Err(ReaderError::InvalidRecord)
    );
    assert_eq!(
        database_counters(&json!({"prompt_tokens": 10, "completion_tokens": 5})),
        Ok((10, 0, 5))
    );
}

fn create_database(path: &PathBuf, with_project: bool, rows: &[(i64, &str, &str, &str)]) {
    let connection = Connection::open(path).unwrap();
    if with_project {
        connection
            .execute_batch(
                "CREATE TABLE chat_session (session_id TEXT, project_name TEXT);
                 CREATE TABLE chat_message (
                     id TEXT, session_id TEXT, request_id TEXT, role TEXT,
                     token_info TEXT, model_info TEXT, gmt_create INTEGER
                 );",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO chat_session (session_id, project_name) VALUES ('s1', 'demo-project')",
                [],
            )
            .unwrap();
    } else {
        connection
            .execute_batch(
                "CREATE TABLE chat_message (
                     id TEXT, session_id TEXT, request_id TEXT, role TEXT,
                     token_info TEXT, model_info TEXT, gmt_create INTEGER
                 );",
            )
            .unwrap();
    }
    for (created, id, role, token_info) in rows {
        connection
            .execute(
                "INSERT INTO chat_message
                     (id, session_id, request_id, role, token_info, model_info, gmt_create)
                 VALUES (?1, 's1', 'req_1', ?2, ?3, '{\"modelKey\":\"gm51model\"}', ?4)",
                params![id, role, token_info, created],
            )
            .unwrap();
    }
}

#[test]
fn the_database_lane_reads_assistant_rows_with_usage() {
    let root = Root::new();
    let path = root.path("local.db");
    create_database(
        &path,
        true,
        &[
            (
                1_782_718_000_000,
                "m1",
                "assistant",
                r#"{"prompt_tokens":120,"cached_tokens":20,"completion_tokens":50}"#,
            ),
            (
                1_782_718_001_000,
                "m2",
                "user",
                r#"{"prompt_tokens":999,"cached_tokens":0,"completion_tokens":0}"#,
            ),
            (1_782_718_002_000, "m3", "assistant", "{}"),
        ],
    );
    let rows = QodercnAdapter::new(&path).read().unwrap();
    // The user row and the empty token_info row are both excluded by the query.
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].model, "GLM-5.2");
    assert_eq!(
        rows[0].tokens,
        TokenCounts {
            input: 100,
            output: 50,
            cache_read: 20,
            cache_write: 0,
            reasoning: 0,
        }
    );
    assert_eq!(rows[0].workspace_label.as_deref(), Some("demo-project"));
    assert_eq!(rows[0].timestamp_ms, 1_782_718_000_000);
}

#[test]
fn the_database_lane_works_without_the_optional_project_table() {
    let root = Root::new();
    let path = root.path("local.db");
    create_database(
        &path,
        false,
        &[(
            1_782_718_000_000,
            "m1",
            "assistant",
            r#"{"prompt_tokens":10,"cached_tokens":0,"completion_tokens":5}"#,
        )],
    );
    let rows = QodercnAdapter::new(&path).read().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].workspace_label, None);
}

#[test]
fn the_database_lane_rejects_a_missing_chat_message_table() {
    let root = Root::new();
    let path = root.path("empty.db");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch("CREATE TABLE unrelated (id TEXT);")
        .unwrap();
    drop(connection);
    assert!(QodercnAdapter::new(&path).read().is_err());
}

#[test]
fn a_control_character_in_an_identity_is_rejected() {
    let mut reader = QodercnReader::new("local");
    let mut input = row(Lane::Database, 10, 0, 5);
    input.model = Some("bad\u{0}model");
    // A model carrying a control character cannot be a stable identity.
    assert!(reader.ingest(input).is_err());
}
