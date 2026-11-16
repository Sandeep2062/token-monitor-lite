//! Synthetic MiMo Code cases adapted from the pinned Tokscale `micode.rs`
//! reader. MiMo stores assistant turns in OpenCode's `message` schema, so the
//! cases here focus on the four declared departures from OpenCode plus the
//! shared token semantics.

use token_monitor_lite::accounting::TokenCounts;
use token_monitor_lite::parsers::CostStatus;
use token_monitor_lite::parsers::ReaderError;
use token_monitor_lite::parsers::mimo::{CLIENT, MimoReader, MimoRecord};

fn ingest(reader: &mut MimoReader, data: &[u8]) -> Result<(), ReaderError> {
    reader.ingest(MimoRecord {
        message_id: "msg-1",
        session_id: "session-a",
        data,
        fallback_timestamp_ms: Some(1_700_000_000_000),
        database: "mimocode.db",
    })
}

fn new_reader() -> MimoReader {
    MimoReader::new()
}

#[test]
fn an_assistant_message_becomes_one_observation() {
    let mut reader = new_reader();
    ingest(
        &mut reader,
        br#"{"role":"assistant","modelID":"mimo-v2","providerID":"xiaomi","tokens":{"input":120,"output":80,"reasoning":30,"cache":{"read":40,"write":5}},"time":{"created":1770983426420}}"#,
    )
    .unwrap();
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key.client, CLIENT);
    assert_eq!(rows[0].key.session, "session-a");
    assert_eq!(rows[0].model, "mimo-v2");
    assert_eq!(rows[0].billing_provider_hint.as_deref(), Some("xiaomi"));
    assert_eq!(rows[0].timestamp_ms, 1_770_983_426_420);
    // Reasoning is a subset of the inclusive `output`, so it is split out once.
    assert_eq!(
        rows[0].tokens,
        TokenCounts {
            input: 120,
            output: 50,
            cache_read: 40,
            cache_write: 5,
            reasoning: 30,
        }
    );
}

#[test]
fn a_missing_cache_block_is_zero_not_a_rejection() {
    let mut reader = new_reader();
    // OpenCode rejects this shape; MiMo explicitly does not, because its
    // assistant messages may omit `cache` entirely.
    ingest(
        &mut reader,
        br#"{"role":"assistant","modelID":"mimo-v2","tokens":{"input":10,"output":5},"time":{"created":1770983426420}}"#,
    )
    .unwrap();
    let tokens = reader.snapshot()[0].tokens;
    assert_eq!(tokens.cache_read, 0);
    assert_eq!(tokens.cache_write, 0);
}

#[test]
fn a_partial_cache_block_zeroes_only_the_missing_member() {
    let mut reader = new_reader();
    ingest(
        &mut reader,
        br#"{"role":"assistant","modelID":"mimo-v2","tokens":{"input":10,"output":5,"cache":{"read":7}},"time":{"created":1770983426420}}"#,
    )
    .unwrap();
    let tokens = reader.snapshot()[0].tokens;
    assert_eq!(tokens.cache_read, 7);
    assert_eq!(tokens.cache_write, 0);
}

#[test]
fn epoch_seconds_are_scaled_to_milliseconds() {
    let mut reader = new_reader();
    // Some MiMo builds write seconds where OpenCode writes milliseconds, which
    // would otherwise land the record ~1000x in the past.
    ingest(
        &mut reader,
        br#"{"role":"assistant","modelID":"mimo-v2","tokens":{"input":10,"output":5},"time":{"created":1770983426}}"#,
    )
    .unwrap();
    assert_eq!(reader.snapshot()[0].timestamp_ms, 1_770_983_426_000);
}

#[test]
fn an_explicit_zero_cost_is_reported_and_survives() {
    let mut reader = new_reader();
    ingest(
        &mut reader,
        br#"{"role":"assistant","modelID":"mimo-v2","tokens":{"input":10,"output":5},"cost":0.0,"time":{"created":1770983426420}}"#,
    )
    .unwrap();
    // A free-promo turn genuinely bills nothing; it must not be repriced.
    assert_eq!(reader.snapshot()[0].cost, CostStatus::Reported(0));
}

#[test]
fn a_reported_cost_becomes_nano_usd() {
    let mut reader = new_reader();
    ingest(
        &mut reader,
        br#"{"role":"assistant","modelID":"mimo-v2","tokens":{"input":10,"output":5},"cost":0.0042,"time":{"created":1770983426420}}"#,
    )
    .unwrap();
    assert_eq!(reader.snapshot()[0].cost, CostStatus::Reported(4_200_000));
}

#[test]
fn a_negative_or_absurd_cost_is_rejected_not_clamped() {
    for cost in ["-0.01", "1000001.0"] {
        let mut reader = new_reader();
        let data = format!(
            r#"{{"role":"assistant","modelID":"mimo-v2","tokens":{{"input":10,"output":5}},"cost":{cost},"time":{{"created":1770983426420}}}}"#
        );
        let error = ingest(&mut reader, data.as_bytes()).unwrap_err();
        assert_eq!(error, ReaderError::InvalidRecord);
    }
}

#[test]
fn a_message_without_tokens_contributes_nothing() {
    let mut reader = new_reader();
    ingest(
        &mut reader,
        br#"{"role":"assistant","modelID":"mimo-v2","time":{"created":1770983426420}}"#,
    )
    .unwrap();
    assert!(reader.snapshot().is_empty());
    assert!(!reader.has_supported_usage());
}

#[test]
fn a_non_assistant_role_is_rejected() {
    let mut reader = new_reader();
    let error = ingest(
        &mut reader,
        br#"{"role":"user","modelID":"mimo-v2","tokens":{"input":10,"output":5},"time":{"created":1770983426420}}"#,
    )
    .unwrap_err();
    assert_eq!(error, ReaderError::Declined("MiMo assistant role"));
}

#[test]
fn a_negative_counter_is_rejected_not_clamped() {
    let mut reader = new_reader();
    let error = ingest(
        &mut reader,
        br#"{"role":"assistant","modelID":"mimo-v2","tokens":{"input":-1,"output":5},"time":{"created":1770983426420}}"#,
    )
    .unwrap_err();
    assert_eq!(error, ReaderError::InvalidRecord);
}

#[test]
fn reasoning_above_output_is_rejected_rather_than_underflowing() {
    let mut reader = new_reader();
    let error = ingest(
        &mut reader,
        br#"{"role":"assistant","modelID":"mimo-v2","tokens":{"input":10,"output":5,"reasoning":9},"time":{"created":1770983426420}}"#,
    )
    .unwrap_err();
    assert_eq!(error, ReaderError::Declined("MiMo reasoning/output"));
}

#[test]
fn a_missing_model_is_visible_unsupported() {
    let mut reader = new_reader();
    let error = ingest(
        &mut reader,
        br#"{"role":"assistant","tokens":{"input":10,"output":5},"time":{"created":1770983426420}}"#,
    )
    .unwrap_err();
    assert_eq!(error, ReaderError::Declined("MiMo message model"));
}

#[test]
fn the_nested_model_object_is_accepted() {
    let mut reader = new_reader();
    ingest(
        &mut reader,
        br#"{"role":"assistant","model":{"id":"mimo-v2","providerID":"xiaomi"},"tokens":{"input":10,"output":5},"time":{"created":1770983426420}}"#,
    )
    .unwrap();
    let row = &reader.snapshot()[0];
    assert_eq!(row.model, "mimo-v2");
    assert_eq!(row.billing_provider_hint.as_deref(), Some("xiaomi"));
}

#[test]
fn a_missing_timestamp_uses_the_row_fallback() {
    let mut reader = new_reader();
    ingest(
        &mut reader,
        br#"{"role":"assistant","modelID":"mimo-v2","tokens":{"input":10,"output":5}}"#,
    )
    .unwrap();
    assert_eq!(reader.snapshot()[0].timestamp_ms, 1_700_000_000_000);
}

#[test]
fn identities_are_namespaced_by_database_name() {
    let data = br#"{"role":"assistant","modelID":"mimo-v2","tokens":{"input":10,"output":5},"time":{"created":1770983426420}}"#;
    let mut alpha = MimoReader::new();
    alpha
        .ingest(MimoRecord {
            message_id: "row-1",
            session_id: "session-a",
            data,
            fallback_timestamp_ms: None,
            database: "mimocode-alpha.db",
        })
        .unwrap();
    let mut beta = MimoReader::new();
    beta.ingest(MimoRecord {
        message_id: "row-1",
        session_id: "session-a",
        data,
        fallback_timestamp_ms: None,
        database: "mimocode-beta.db",
    })
    .unwrap();
    // The same row id in two channel-suffixed databases is two distinct
    // observations, not one collapsed duplicate.
    assert_ne!(alpha.snapshot()[0].dedup_key, beta.snapshot()[0].dedup_key);
}

#[test]
fn a_malformed_payload_is_a_visible_candidate_failure() {
    let mut reader = new_reader();
    assert_eq!(
        ingest(&mut reader, b"{not json").unwrap_err(),
        ReaderError::InvalidRecord
    );
}

#[test]
fn a_control_character_in_the_model_is_rejected() {
    let mut reader = new_reader();
    let error = ingest(
        &mut reader,
        b"{\"role\":\"assistant\",\"modelID\":\"mimo\\u0007v2\",\"tokens\":{\"input\":10,\"output\":5},\"time\":{\"created\":1770983426420}}",
    )
    .unwrap_err();
    assert_eq!(error, ReaderError::InvalidRecord);
}
