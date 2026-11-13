//! Differential fixtures copied byte-for-byte from Tokscale pin
//! d3df620b2cdd9502c70aececdbc6ece61fe26fc9. Hashes and MIT provenance are
//! recorded in docs/migration/parser-provenance.md.

use serde_json::Value;
use token_monitor_lite::accounting::TokenCounts;
use token_monitor_lite::parsers::{claude::ClaudeReader, codex::CodexReader};

#[test]
fn pinned_claude_fixture_matches_supported_tokscale_rows() {
    let mut reader = ClaudeReader::new("claude-v12");
    for record in include_str!("fixtures/pinned/claude-parser2-complete.jsonl").lines() {
        reader.ingest(record.as_bytes()).unwrap();
    }
    let expected: Value =
        serde_json::from_str(include_str!("fixtures/pinned/claude-parser2-expected.json")).unwrap();
    let expected = expected.as_array().unwrap();
    let rows = reader.snapshot();

    assert_eq!(rows.len(), expected.len());
    for (row, expected) in rows.iter().zip(expected) {
        assert_eq!(row.key.client, expected["client"].as_str().unwrap());
        assert_eq!(row.key.session, expected["session_id"].as_str().unwrap());
        assert_eq!(row.model, expected["model_id"].as_str().unwrap());
        assert_eq!(
            row.billing_provider_hint.as_deref(),
            expected["provider_id"].as_str()
        );
        assert_eq!(row.timestamp_ms, expected["timestamp"].as_i64().unwrap());
        assert_eq!(row.dedup_key.as_deref(), expected["dedup_key"].as_str());
        assert_eq!(
            row.tokens,
            TokenCounts {
                input: expected["tokens"]["input"].as_u64().unwrap(),
                output: expected["tokens"]["output"].as_u64().unwrap(),
                cache_read: expected["tokens"]["cache_read"].as_u64().unwrap(),
                cache_write: expected["tokens"]["cache_write"].as_u64().unwrap(),
                reasoning: expected["tokens"]["reasoning"].as_u64().unwrap(),
            }
        );
        assert_eq!(
            row.cache_write_1h,
            expected["tokens"]["cache_write_1h"].as_u64().unwrap()
        );
    }
}

#[test]
fn pinned_codex_fixture_matches_record_and_compaction_settlement() {
    let mut reader = CodexReader::new("synthetic-codex-fixture18");
    for record in include_str!("fixtures/pinned/codex-parser18-prefix.jsonl").lines() {
        reader.ingest(record.as_bytes()).unwrap();
    }
    reader.finish().unwrap();

    let rows = reader.snapshot();
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows[0].tokens,
        TokenCounts {
            input: 80,
            output: 10,
            cache_read: 20,
            cache_write: 0,
            reasoning: 0,
        }
    );
    assert_eq!(rows[0].timestamp_ms, 1_790_812_860_000);
    assert_eq!(rows[0].billing_provider_hint.as_deref(), Some("gateway"));
    assert_eq!(
        rows[0].dedup_key.as_deref(),
        Some("codex:token_count-total:synthetic-codex-fixture18:gateway:gpt-5.5:100:10:20:0")
    );

    assert_eq!(
        rows[1].tokens,
        TokenCounts {
            input: 150,
            output: 20,
            cache_read: 50,
            cache_write: 0,
            reasoning: 0,
        }
    );
    assert_eq!(rows[1].timestamp_ms, 1_790_812_862_000);
    assert_eq!(rows[1].service_tier, None);
    assert_eq!(
        rows[1].dedup_key.as_deref(),
        Some("codex:usage-record:synthetic-codex-fixture18:response-settled")
    );

    assert_eq!(
        rows[2].tokens,
        TokenCounts {
            input: 240,
            output: 30,
            cache_read: 60,
            cache_write: 0,
            reasoning: 0,
        }
    );
    assert_eq!(rows[2].timestamp_ms, 1_790_812_921_000);
    assert_eq!(rows[2].service_tier.as_deref(), Some("priority"));
    assert_eq!(
        rows[2].dedup_key.as_deref(),
        Some("codex:usage-record:synthetic-codex-fixture18:response-pending")
    );
}
