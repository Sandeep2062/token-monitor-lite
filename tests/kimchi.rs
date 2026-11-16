//! Kimchi Coding reader contract.
//!
//! Kimchi writes the Pi record format, so the extraction is `PiReader`'s. These
//! tests pin what this client adds: the distinct client id and the dedup key
//! that names it.

use serde_json::json;
use token_monitor_lite::parsers::kimchi::{KimchiReader, reader};

fn header(id: &str) -> serde_json::Value {
    json!({
        "type": "session",
        "version": 3,
        "id": id,
        "timestamp": "2026-08-01T00:00:00.000Z",
        "cwd": "/tmp/kimchi-project"
    })
}

fn assistant(input: u64, output: u64) -> serde_json::Value {
    json!({
        "type": "message",
        "id": "msg_001",
        "timestamp": "2026-08-01T00:00:01.000Z",
        "message": {
            "role": "assistant",
            "model": "kimi-k2.6",
            "provider": "kimchi-dev",
            "usage": {"input": input, "output": output, "cacheRead": 50}
        }
    })
}

fn read(values: &[serde_json::Value]) -> Vec<token_monitor_lite::parsers::ResponseUsage> {
    let mut reader = reader("dir-session").with_fallback_timestamp(Some(1_785_542_400_000));
    for value in values {
        reader
            .ingest(&serde_json::to_vec(value).unwrap())
            .expect("record is accepted");
    }
    reader.snapshot()
}

#[test]
fn usage_is_reported_under_the_kimchi_client() {
    let found = read(&[header("kimchi_ses_001"), assistant(9441, 131)]);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].key.client, "kimchi");
    assert_eq!(found[0].key.session, "kimchi_ses_001");
}

#[test]
fn the_header_names_the_session() {
    let found = read(&[header("kimchi_ses_001"), assistant(1, 1)]);
    assert_eq!(found[0].key.session, "kimchi_ses_001");
}

#[test]
fn the_message_carries_its_model_and_provider() {
    let found = read(&[header("s"), assistant(1, 1)]);
    assert_eq!(found[0].model, "kimi-k2.6");
    assert_eq!(
        found[0].billing_provider_hint.as_deref(),
        Some("kimchi-dev")
    );
}

#[test]
fn the_counters_are_read_from_the_pi_usage_block() {
    let found = read(&[header("s"), assistant(9441, 131)]);
    assert_eq!(found[0].tokens.input, 9441);
    assert_eq!(found[0].tokens.output, 131);
    assert_eq!(found[0].tokens.cache_read, 50);
}

#[test]
fn the_dedup_key_names_the_client() {
    // The key must name kimchi, not pi: the two clients read the same format, so
    // a shared key would let one client's row suppress the other's. The Pi
    // reader prefixes the client and folds the counters in, which is what makes
    // a re-read of the same record idempotent.
    let found = read(&[header("kimchi_ses_001"), assistant(1, 1)]);
    let key = found[0].dedup_key.as_deref().expect("a key is set");
    assert!(key.starts_with("kimchi:"), "key names the client: {key}");
    assert!(key.contains("msg_001"), "key names the message: {key}");
}

#[test]
fn the_workspace_is_not_derived_from_the_header_cwd() {
    // The Pi reader does not turn a `cwd` into a workspace label, and this
    // client adds nothing on top of it.
    let found = read(&[header("s"), assistant(1, 1)]);
    assert_eq!(found[0].workspace_label, None);
    assert_eq!(found[0].workspace_key, None);
}

#[test]
fn an_empty_transcript_has_no_supported_usage() {
    let reader: KimchiReader = reader("s");
    assert!(!reader.has_supported_usage());
}

#[test]
fn a_title_line_before_the_header_is_tolerated() {
    let title = json!({"type": "title", "title": "Fixture"});
    let found = read(&[title, header("s"), assistant(1, 1)]);
    assert_eq!(found.len(), 1);
}
