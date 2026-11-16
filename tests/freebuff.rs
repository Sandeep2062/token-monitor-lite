//! Freebuff contract: the Codebuff shape under its own client id, partitioned
//! by the `free` lane marker.

use serde_json::json;
use std::path::Path;
use token_monitor_lite::parsers::freebuff::{CLIENT, is_freebuff_path, reader, session_from_path};

fn read(
    session: &str,
    messages: serde_json::Value,
) -> Vec<token_monitor_lite::parsers::ResponseUsage> {
    let mut r = reader(session).with_fallback_timestamp(Some(1_782_718_000_000));
    r.ingest(messages.to_string().as_bytes()).expect("accepted");
    r.snapshot()
}

fn ai(usage: serde_json::Value) -> serde_json::Value {
    json!([{
        "id": "m1",
        "variant": "ai",
        "timestamp": 1_782_700_000_000i64,
        "metadata": { "model": "claude-sonnet-4-5", "usage": usage }
    }])
}

#[test]
fn observations_carry_the_freebuff_client_id() {
    let found = read("s", ai(json!({"inputTokens": 100, "outputTokens": 50})));
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].key.client, CLIENT);
    assert_eq!(found[0].tokens.input, 100);
    assert_eq!(found[0].tokens.output, 50);
}

#[test]
fn the_free_lane_marker_is_recognised() {
    assert!(is_freebuff_path(Path::new(
        "/home/u/.config/manicode/projects/p/chats/free/chat-messages.json"
    )));
    assert!(is_freebuff_path(Path::new(
        "/home/u/.config/manicode/projects/free/p/chats/c/chat-messages.json"
    )));
    assert!(is_freebuff_path(Path::new(
        "/home/u/.config/manicode/projects/p/chats/FREE/chat-messages.json"
    )));
}

#[test]
fn a_codebuff_path_is_not_the_free_lane() {
    // `base2-free` used to be the marker prefix; the segment is what survives
    // the rename to `base3-free*`, and a plain Codebuff path has neither.
    assert!(!is_freebuff_path(Path::new(
        "/home/u/.config/manicode/projects/p/chats/c/chat-messages.json"
    )));
    assert!(!is_freebuff_path(Path::new(
        "/home/u/.config/manicode/projects/freedom/chats/c/chat-messages.json"
    )));
}

#[test]
fn a_free_substring_that_is_not_a_segment_does_not_match() {
    // `freebuff` and `freedom` are not the `free` lane.
    assert!(!is_freebuff_path(Path::new(
        "/x/freebuff/chat-messages.json"
    )));
    assert!(!is_freebuff_path(Path::new(
        "/x/freeware/chat-messages.json"
    )));
}

#[test]
fn the_session_identity_matches_the_codebuff_shape() {
    let path = Path::new(
        "/home/u/.config/manicode/projects/free/chats/2025-12-14T10-00-00.000Z/chat-messages.json",
    );
    assert_eq!(
        session_from_path(path),
        "manicode/free/2025-12-14T10-00-00.000Z"
    );
}

#[test]
fn the_three_usage_locations_remain_fallbacks() {
    let messages = json!([{
        "id": "m1",
        "role": "assistant",
        "metadata": {
            "model": "gpt-5",
            "usage": { "inputTokens": 10, "outputTokens": 5 },
            "codebuff": { "usage": { "inputTokens": 999 } }
        }
    }]);
    let found = read("s", messages);
    assert_eq!(found[0].tokens.input, 10);
    assert_eq!(found[0].tokens.output, 5);
}

#[test]
fn a_freebuff_observation_keeps_its_upstream_identity() {
    let found = read("s", ai(json!({"inputTokens": 1})));
    assert_eq!(found[0].dedup_key.as_deref(), Some("m1"));
}

#[test]
fn malformed_json_is_a_visible_rejection() {
    let mut r = reader("s").with_fallback_timestamp(Some(1_000));
    assert!(r.ingest(b"not json").is_err());
}
