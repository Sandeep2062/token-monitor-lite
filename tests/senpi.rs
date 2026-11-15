//! Synthetic OmO Native (Senpi) cases: Senpi uses the Pi record format and
//! keeps `usage.reasoning` informational.

use serde_json::json;
use token_monitor_lite::accounting::TokenCounts;
use token_monitor_lite::parsers::senpi::SenpiReader;

fn ingest(reader: &mut SenpiReader, value: serde_json::Value) {
    reader.ingest(&serde_json::to_vec(&value).unwrap()).unwrap();
}

#[test]
fn senpi_reasoning_is_not_added_to_output() {
    let mut reader = SenpiReader::new("senpi", "fallback");
    ingest(
        &mut reader,
        json!({
            "type": "session",
            "version": 3,
            "id": "senpi-session",
            "cwd": "/tmp/workspace"
        }),
    );
    ingest(
        &mut reader,
        json!({
            "type": "message",
            "id": "b2",
            "timestamp": "2026-07-29T15:20:01.000Z",
            "message": {
                "role": "assistant",
                "model": "claude-opus-5",
                "provider": "anthropic",
                "usage": {
                    "input": 2,
                    "output": 43,
                    "cacheRead": 0,
                    "cacheWrite": 38241,
                    "totalTokens": 38286,
                    "reasoning": 16
                }
            }
        }),
    );
    let rows = reader.snapshot();
    assert_eq!(rows[0].key.client, "senpi");
    assert_eq!(
        rows[0].tokens,
        TokenCounts {
            input: 2,
            output: 43,
            cache_read: 0,
            cache_write: 38241,
            reasoning: 0,
        }
    );
    assert_eq!(
        rows[0].tokens.input + rows[0].tokens.output + rows[0].tokens.cache_write,
        38286
    );
}

#[test]
fn session_title_records_do_not_become_agent_attribution() {
    let mut reader = SenpiReader::new("senpi", "fallback");
    ingest(
        &mut reader,
        json!({"type": "session", "version": 3, "id": "s", "cwd": "/tmp"}),
    );
    ingest(
        &mut reader,
        json!({
            "type": "session_info",
            "id": "c3",
            "timestamp": "2026-07-29T15:23:22.174Z",
            "name": "Investigate Senpi rendering"
        }),
    );
    ingest(
        &mut reader,
        json!({
            "type": "message",
            "id": "d4",
            "timestamp": "2026-07-29T15:23:30.000Z",
            "message": {
                "role": "assistant",
                "model": "claude-opus-5",
                "usage": {"input": 10, "output": 5}
            }
        }),
    );
    assert_eq!(reader.snapshot()[0].agent, None);
}
