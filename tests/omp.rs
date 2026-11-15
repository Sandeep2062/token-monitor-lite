//! Synthetic Oh My Pi cases: OMP uses the shared Pi record format with its own
//! client id and root.

use serde_json::json;
use token_monitor_lite::parsers::omp::OmpReader;

fn ingest(reader: &mut OmpReader, value: serde_json::Value) {
    reader.ingest(&serde_json::to_vec(&value).unwrap()).unwrap();
}

#[test]
fn omp_session_and_subagent_files_are_independent_sessions() {
    for session in ["main-session", "main-session-TelegramBrokerMap"] {
        let mut reader = OmpReader::new("omp", session);
        ingest(
            &mut reader,
            json!({
                "type": "session",
                "version": 3,
                "id": session,
                "cwd": "/tmp/proj"
            }),
        );
        ingest(
            &mut reader,
            json!({
                "type": "message",
                "id": "b2",
                "timestamp": "2026-08-20T15:27:40.000Z",
                "message": {
                    "role": "assistant",
                    "model": "claude-opus-5",
                    "provider": "anthropic",
                    "usage": {
                        "input": 12,
                        "output": 34,
                        "cacheRead": 0,
                        "cacheWrite": 0,
                        "totalTokens": 46
                    }
                }
            }),
        );
        let rows = reader.snapshot();
        assert_eq!(rows[0].key.client, "omp");
        assert_eq!(rows[0].key.session, session);
        assert_eq!(rows[0].tokens.input + rows[0].tokens.output, 46);
    }
}

#[test]
fn omp_model_less_usage_falls_back_to_the_tools_bucket() {
    let mut reader = OmpReader::new("omp", "s");
    ingest(
        &mut reader,
        json!({"type": "session", "version": 3, "id": "s", "cwd": "/tmp"}),
    );
    ingest(
        &mut reader,
        json!({
            "type": "compaction",
            "id": "c1",
            "timestamp": "2026-08-20T15:28:00.000Z",
            "usage": {"input": 3, "output": 1}
        }),
    );
    assert_eq!(reader.snapshot()[0].model, "tools/summaries");
}
