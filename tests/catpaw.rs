//! Synthetic CatPaw cases adapted from the pinned Token Monitor fork reader
//! `crates/tokscale-core/src/token_monitor/catpaw.rs`. The two editions have
//! disjoint model-id namespaces, so both are covered explicitly.

use serde_json::json;
use token_monitor_lite::accounting::TokenCounts;
use token_monitor_lite::parsers::ReaderError;
use token_monitor_lite::parsers::catpaw::{CLIENT, CatpawReader, CatpawRow, Edition};

fn usage(input: i64, cached: i64, write: i64, output: i64) -> serde_json::Value {
    json!({
        "promptTokens": input,
        "cacheReadTokens": cached,
        "cacheWriteTokens": write,
        "completionTokens": output,
        "totalTokens": input + cached + write + output,
    })
}

/// A row whose JSON bytes are owned by the caller, so nothing is leaked or
/// outlived.
struct Fixture {
    usage_bytes: Vec<u8>,
    selection_bytes: Option<Vec<u8>>,
}

impl Fixture {
    fn new(usage: &serde_json::Value, selection: Option<&serde_json::Value>) -> Self {
        Self {
            usage_bytes: serde_json::to_vec(usage).unwrap(),
            selection_bytes: selection.map(|value| serde_json::to_vec(value).unwrap()),
        }
    }

    fn row(&self) -> CatpawRow<'_> {
        CatpawRow {
            conversation_id: "conv-1",
            message_id: "msg-1",
            created_at_ms: Some(1_780_000_000_000),
            updated_at_ms: Some(1_780_000_001_000),
            schema_version: 1,
            usage: &self.usage_bytes,
            recorded_total: None,
            selection: self.selection_bytes.as_deref(),
            namespace: "catpaw-memory-acct",
        }
    }
}

fn domestic(selection: Option<&serde_json::Value>) -> CatpawReader {
    let fixture = Fixture::new(&usage(10, 5, 2, 7), selection);
    let mut reader = CatpawReader::new(Edition::Domestic);
    reader.ingest(fixture.row()).unwrap();
    reader
}

#[test]
fn four_disjoint_buckets_become_one_observation() {
    let rows = domestic(None).snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key.client, CLIENT);
    // The buckets are disjoint and additive, so nothing is subtracted.
    assert_eq!(
        rows[0].tokens,
        TokenCounts {
            input: 10,
            output: 7,
            cache_read: 5,
            cache_write: 2,
            reasoning: 0,
        }
    );
    assert_eq!(rows[0].timestamp_ms, 1_780_000_000_000);
    assert_eq!(rows[0].key.session, "catpaw:catpaw-memory-acct:conv-1");
    assert_eq!(
        rows[0].dedup_key.as_deref(),
        Some("catpaw:catpaw-memory-acct:conv-1:msg-1")
    );
}

#[test]
fn reasoning_and_one_hour_cache_write_stay_zero() {
    let rows = domestic(None).snapshot();
    assert_eq!(rows[0].tokens.reasoning, 0);
    assert_eq!(rows[0].cache_write_1h, 0);
}

#[test]
fn a_bucket_sum_that_disagrees_with_the_recorded_total_is_rejected() {
    let mut reader = CatpawReader::new(Edition::Domestic);
    let broken = json!({
        "promptTokens": 10,
        "completionTokens": 5,
        "totalTokens": 999,
    });
    let fixture = Fixture::new(&broken, None);
    let error = reader.ingest(fixture.row()).unwrap_err();
    assert_eq!(error, ReaderError::Declined("CatPaw token sum"));
}

#[test]
fn the_sibling_total_anchors_a_row_whose_own_total_predates_cache_read() {
    let mut reader = CatpawReader::new(Edition::Domestic);
    // `totalTokens` omits the cache-read part, but the sibling full total
    // matches all four buckets, so the row is accepted.
    let legacy = json!({
        "promptTokens": 10,
        "completionTokens": 5,
        "cacheReadTokens": 4,
        "cacheWriteTokens": 1,
        "totalTokens": 15,
    });
    let fixture = Fixture::new(&legacy, None);
    let mut record = fixture.row();
    record.recorded_total = Some(20);
    reader.ingest(record).unwrap();
    assert_eq!(reader.snapshot()[0].tokens.cache_read, 4);
}

#[test]
fn a_negative_bucket_is_rejected() {
    let mut reader = CatpawReader::new(Edition::Domestic);
    let negative = json!({
        "promptTokens": -1,
        "completionTokens": 5,
        "totalTokens": 4,
    });
    assert_eq!(
        reader
            .ingest(Fixture::new(&negative, None).row())
            .unwrap_err(),
        ReaderError::InvalidRecord
    );
}

#[test]
fn a_non_integral_bucket_is_rejected() {
    let mut reader = CatpawReader::new(Edition::Domestic);
    let fractional = json!({
        "promptTokens": 1.5,
        "completionTokens": 5,
        "totalTokens": 6.5,
    });
    assert_eq!(
        reader
            .ingest(Fixture::new(&fractional, None).row())
            .unwrap_err(),
        ReaderError::InvalidRecord
    );
}

#[test]
fn an_unknown_message_schema_is_visible_unsupported() {
    let mut reader = CatpawReader::new(Edition::Domestic);
    let fixture = Fixture::new(&usage(1, 0, 0, 1), None);
    let mut record = fixture.row();
    record.schema_version = 2;
    assert_eq!(
        reader.ingest(record).unwrap_err(),
        ReaderError::Unsupported("CatPaw message schema")
    );
}

#[test]
fn the_current_selection_wins_over_the_legacy_initial_fields() {
    let selection = json!({
        "persistedModelId": 91,
        "initialSelectedModelId": 63,
    });
    let rows = domestic(Some(&selection)).snapshot();
    assert_eq!(rows[0].model, "glm-5.3-flash");
}

#[test]
fn an_auto_selection_keeps_the_auto_label_and_is_never_priced() {
    let selection = json!({
        "persistedModelSelection": {"isAuto": true, "modelId": 91},
    });
    let rows = domestic(Some(&selection)).snapshot();
    assert_eq!(rows[0].model, "auto");
    // Auto is a routing tier, so no provider hint may apply.
    assert_eq!(rows[0].billing_provider_hint, None);
}

#[test]
fn routing_tiers_keep_an_edition_qualified_label() {
    let selection = json!({ "persistedModelMode": "pro" });
    let rows = domestic(Some(&selection)).snapshot();
    // 10002 is a routing tier, not a catalog model.
    assert_eq!(rows[0].model, "catpaw-moon-model-10002");
    assert_eq!(rows[0].billing_provider_hint, None);
}

#[test]
fn a_conflicting_current_and_selected_id_is_labelled_not_priced() {
    let selection = json!({
        "persistedModelId": 63,
        "persistedModelSelection": {"isAuto": false, "modelId": 91},
    });
    let rows = domestic(Some(&selection)).snapshot();
    assert_eq!(rows[0].model, "catpaw-moon-model-conflict");
    assert_eq!(rows[0].billing_provider_hint, None);
}

#[test]
fn model_ids_do_not_transfer_between_editions() {
    // 91 is `glm-5.3-flash` domestically and unmapped overseas.
    let selection = json!({ "persistedModelId": 91 });
    let mut domestic_reader = CatpawReader::new(Edition::Domestic);
    domestic_reader
        .ingest(Fixture::new(&usage(1, 0, 0, 1), Some(&selection)).row())
        .unwrap();
    assert_eq!(domestic_reader.snapshot()[0].model, "glm-5.3-flash");

    let mut overseas_reader = CatpawReader::new(Edition::Overseas);
    overseas_reader
        .ingest(Fixture::new(&usage(1, 0, 0, 1), Some(&selection)).row())
        .unwrap();
    let overseas = &overseas_reader.snapshot()[0];
    assert_eq!(overseas.model, "catpaw-overseas-model-91");
    assert_eq!(overseas.billing_provider_hint, None);
}

#[test]
fn a_mapped_overseas_id_resolves_under_its_own_namespace() {
    let selection = json!({ "persistedModelId": 10000006 });
    let mut reader = CatpawReader::new(Edition::Overseas);
    reader
        .ingest(Fixture::new(&usage(1, 0, 0, 1), Some(&selection)).row())
        .unwrap();
    let rows = reader.snapshot();
    assert_eq!(rows[0].model, "glm-5.3-flash");
    assert_eq!(rows[0].billing_provider_hint.as_deref(), Some(CLIENT));
}

#[test]
fn a_missing_selection_is_labelled_unknown() {
    let rows = domestic(None).snapshot();
    assert_eq!(rows[0].model, "catpaw-moon-model-unknown");
    assert_eq!(rows[0].billing_provider_hint, None);
}

#[test]
fn a_row_with_no_timestamps_is_stored_at_the_epoch() {
    let mut reader = CatpawReader::new(Edition::Domestic);
    let fixture = Fixture::new(&usage(1, 0, 0, 1), None);
    let mut record = fixture.row();
    record.created_at_ms = None;
    record.updated_at_ms = None;
    reader.ingest(record).unwrap();
    // The epoch falls outside any window, matching the pinned reader.
    assert_eq!(reader.snapshot()[0].timestamp_ms, 0);
}

#[test]
fn updated_at_is_the_timestamp_fallback() {
    let mut reader = CatpawReader::new(Edition::Domestic);
    let fixture = Fixture::new(&usage(1, 0, 0, 1), None);
    let mut record = fixture.row();
    record.created_at_ms = None;
    record.updated_at_ms = Some(1_780_000_002_000);
    reader.ingest(record).unwrap();
    assert_eq!(reader.snapshot()[0].timestamp_ms, 1_780_000_002_000);
}

#[test]
fn an_out_of_range_timestamp_falls_back_to_the_epoch() {
    let mut reader = CatpawReader::new(Edition::Domestic);
    let fixture = Fixture::new(&usage(1, 0, 0, 1), None);
    let mut record = fixture.row();
    record.created_at_ms = Some(9_000_000_000_000_000);
    record.updated_at_ms = Some(9_000_000_000_000_000);
    reader.ingest(record).unwrap();
    assert_eq!(reader.snapshot()[0].timestamp_ms, 0);
}

#[test]
fn a_missing_identity_is_rejected() {
    let mut reader = CatpawReader::new(Edition::Domestic);
    let fixture = Fixture::new(&usage(1, 0, 0, 1), None);
    let mut record = fixture.row();
    record.message_id = "";
    assert_eq!(
        reader.ingest(record).unwrap_err(),
        ReaderError::InvalidRecord
    );
}

#[test]
fn the_database_name_namespaces_the_session() {
    let mut first = CatpawReader::new(Edition::Domestic);
    let fixture = Fixture::new(&usage(1, 0, 0, 1), None);
    let mut record = fixture.row();
    record.namespace = "catpaw-memory-alpha";
    first.ingest(record).unwrap();

    let mut second = CatpawReader::new(Edition::Domestic);
    let fixture = Fixture::new(&usage(1, 0, 0, 1), None);
    let mut record = fixture.row();
    record.namespace = "catpaw-memory-beta";
    second.ingest(record).unwrap();

    // Identical conversation and message ids in different account databases
    // stay distinct sources of truth.
    assert_ne!(
        first.snapshot()[0].key.session,
        second.snapshot()[0].key.session
    );
}

#[test]
fn edition_directories_map_to_their_documented_names() {
    assert_eq!(Edition::Domestic.directory(), "catpaw-moon");
    assert_eq!(Edition::Overseas.directory(), "catpaw-overseas");
    assert_eq!(
        Edition::from_database_name("catpaw-moon"),
        Some(Edition::Domestic)
    );
    assert_eq!(
        Edition::from_database_name("catpaw-overseas"),
        Some(Edition::Overseas)
    );
    assert_eq!(Edition::from_database_name("catpaw-memory-acct"), None);
}
