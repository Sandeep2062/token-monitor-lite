//! Synthetic Grok Build `updates.jsonl` cases adapted from the pinned Tokscale
//! `grok.rs` reader. Two usage lanes exist: authoritative `turn_completed`
//! breakdowns, and legacy cumulative `totalTokens` counter deltas.

use token_monitor_lite::accounting::TokenCounts;
use token_monitor_lite::parsers::ReaderError;
use token_monitor_lite::parsers::grok::{CLIENT, GrokReader, PROVIDER, UNKNOWN_MODEL};

fn new_reader() -> GrokReader {
    GrokReader::new("session-a").with_fallback_timestamp(Some(1_700_000_000_000))
}

#[test]
fn turn_completed_becomes_one_observation() {
    let mut reader = new_reader();
    reader
        .ingest(
            br#"{"params":{"_meta":{"agentTimestampMs":1770983426420},"update":{"sessionUpdate":"turn_completed","promptId":"prompt-1","_meta":{"modelId":"grok-4"},"usage":{"inputTokens":1562,"outputTokens":2463,"cachedReadTokens":200,"reasoningTokens":63,"totalTokens":4025}}}}"#,
        )
        .unwrap();
    let rows = reader.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key.client, CLIENT);
    assert_eq!(rows[0].key.session, "session-a");
    assert_eq!(rows[0].model, "grok-4");
    assert_eq!(rows[0].billing_provider_hint.as_deref(), Some(PROVIDER));
    assert_eq!(rows[0].timestamp_ms, 1_770_983_426_420);
    assert_eq!(
        rows[0].dedup_key.as_deref(),
        Some("grok:session-a:prompt:prompt-1")
    );
    // `cachedReadTokens` is a subset of input and `reasoningTokens` a subset of
    // output, so both are lifted out to keep the buckets additive.
    assert_eq!(
        rows[0].tokens,
        TokenCounts {
            input: 1362,
            output: 2400,
            cache_read: 200,
            cache_write: 0,
            reasoning: 63,
        }
    );
}

#[test]
fn reasoning_is_a_subset_of_output_not_added_to_it() {
    let mut reader = new_reader();
    reader
        .ingest(
            br#"{"params":{"timestamp":1770983426,"update":{"sessionUpdate":"turn_completed","usage":{"inputTokens":10,"outputTokens":100,"reasoningTokens":100}}}}"#,
        )
        .unwrap();
    let tokens = reader.snapshot()[0].tokens;
    assert_eq!(tokens.output, 0);
    assert_eq!(tokens.reasoning, 100);
}

#[test]
fn a_retransmitted_prompt_keeps_the_larger_total() {
    let mut reader = new_reader();
    reader
        .ingest(
            br#"{"params":{"timestamp":1770983426,"update":{"sessionUpdate":"turn_completed","promptId":"p1","usage":{"inputTokens":100,"outputTokens":50}}}}"#,
        )
        .unwrap();
    // The same prompt reported again with more complete counts replaces it.
    reader
        .ingest(
            br#"{"params":{"timestamp":1770983427,"update":{"sessionUpdate":"turn_completed","promptId":"p1","usage":{"inputTokens":150,"outputTokens":60}}}}"#,
        )
        .unwrap();
    assert_eq!(reader.snapshot().len(), 1);
    assert_eq!(reader.snapshot()[0].tokens.input, 150);

    // A smaller retransmission never lowers a count.
    reader
        .ingest(
            br#"{"params":{"timestamp":1770983428,"update":{"sessionUpdate":"turn_completed","promptId":"p1","usage":{"inputTokens":1,"outputTokens":1}}}}"#,
        )
        .unwrap();
    assert_eq!(reader.snapshot()[0].tokens.input, 150);
}

#[test]
fn distinct_prompts_stay_separate() {
    let mut reader = new_reader();
    reader
        .ingest(
            br#"{"params":{"timestamp":1770983426,"update":{"sessionUpdate":"turn_completed","promptId":"p1","usage":{"inputTokens":100,"outputTokens":50}}}}"#,
        )
        .unwrap();
    reader
        .ingest(
            br#"{"params":{"timestamp":1770983427,"update":{"sessionUpdate":"turn_completed","promptId":"p2","usage":{"inputTokens":200,"outputTokens":70}}}}"#,
        )
        .unwrap();
    assert_eq!(reader.snapshot().len(), 2);
}

#[test]
fn non_usage_updates_are_ignored() {
    let mut reader = new_reader();
    reader
        .ingest(
            br#"{"params":{"update":{"sessionUpdate":"agent_thought_chunk","content":"thinking"}}}"#,
        )
        .unwrap();
    reader
        .ingest(br#"{"params":{"update":{"sessionUpdate":"tool_call","title":"run"}}}"#)
        .unwrap();
    assert!(reader.snapshot().is_empty());
    assert!(!reader.has_supported_usage());
}

#[test]
fn all_zero_usage_is_skipped() {
    let mut reader = new_reader();
    reader
        .ingest(
            br#"{"params":{"timestamp":1770983426,"update":{"sessionUpdate":"turn_completed","usage":{"inputTokens":0,"outputTokens":0}}}}"#,
        )
        .unwrap();
    assert!(reader.snapshot().is_empty());
}

#[test]
fn negative_and_non_integer_counters_are_rejected() {
    let mut reader = new_reader();
    assert!(matches!(
        reader.ingest(
            br#"{"params":{"timestamp":1770983426,"update":{"sessionUpdate":"turn_completed","usage":{"inputTokens":-1,"outputTokens":5}}}}"#
        ),
        Err(ReaderError::InvalidRecord)
    ));
    assert!(matches!(
        reader.ingest(
            br#"{"params":{"timestamp":1770983426,"update":{"sessionUpdate":"turn_completed","usage":{"inputTokens":1.5,"outputTokens":5}}}}"#
        ),
        Err(ReaderError::InvalidRecord)
    ));
    assert!(reader.snapshot().is_empty());
}

#[test]
fn an_incoherent_reported_total_is_rejected() {
    let mut reader = new_reader();
    // `totalTokens` disagrees with the inclusive input+output sum, so the
    // breakdown cannot be trusted.
    assert!(matches!(
        reader.ingest(
            br#"{"params":{"timestamp":1770983426,"update":{"sessionUpdate":"turn_completed","usage":{"inputTokens":10,"outputTokens":5,"totalTokens":999}}}}"#
        ),
        Err(ReaderError::InvalidRecord)
    ));
}

#[test]
fn legacy_cumulative_counters_become_turn_deltas() {
    let mut reader = new_reader();
    // A user message opens a turn; the cumulative counter then advances.
    reader
        .ingest(
            br#"{"params":{"timestamp":1770983410,"update":{"sessionUpdate":"user_message_chunk"}}}"#,
        )
        .unwrap();
    reader
        .ingest(br#"{"params":{"timestamp":1770983411,"_meta":{"totalTokens":100}}}"#)
        .unwrap();
    reader
        .ingest(br#"{"params":{"timestamp":1770983412,"_meta":{"totalTokens":300}}}"#)
        .unwrap();
    assert_eq!(reader.snapshot().len(), 1);
    // The counter proves 300 tokens of work; no input/output split is invented.
    assert_eq!(
        reader.snapshot()[0].tokens,
        TokenCounts {
            input: 300,
            ..TokenCounts::default()
        }
    );
    assert_eq!(reader.snapshot()[0].model, UNKNOWN_MODEL);
}

#[test]
fn a_rewound_cumulative_counter_is_ignored() {
    let mut reader = new_reader();
    reader
        .ingest(
            br#"{"params":{"timestamp":1770983410,"update":{"sessionUpdate":"user_message_chunk"}}}"#,
        )
        .unwrap();
    reader
        .ingest(br#"{"params":{"timestamp":1770983411,"_meta":{"totalTokens":500}}}"#)
        .unwrap();
    // Intermediate rewinds while streaming must not create a second turn.
    reader
        .ingest(br#"{"params":{"timestamp":1770983412,"_meta":{"totalTokens":200}}}"#)
        .unwrap();
    assert_eq!(reader.snapshot().len(), 1);
    assert_eq!(reader.snapshot()[0].tokens.input, 500);
}

#[test]
fn a_single_model_usage_key_names_the_model_when_none_is_declared() {
    let mut reader = new_reader();
    reader
        .ingest(
            br#"{"params":{"timestamp":1770983426,"update":{"sessionUpdate":"turn_completed","usage":{"inputTokens":10,"outputTokens":5,"modelUsage":{"grok-3":{"inputTokens":10}}}}}}"#,
        )
        .unwrap();
    assert_eq!(reader.snapshot()[0].model, "grok-3");
}

#[test]
fn malformed_lines_are_rejected() {
    let mut reader = new_reader();
    assert!(matches!(
        reader.ingest(b"not json"),
        Err(ReaderError::InvalidRecord)
    ));
}

#[test]
fn unusable_timestamp_falls_back_to_the_file_mtime() {
    let mut reader = new_reader();
    reader
        .ingest(
            br#"{"params":{"update":{"sessionUpdate":"turn_completed","usage":{"inputTokens":10,"outputTokens":5}}}}"#,
        )
        .unwrap();
    assert_eq!(reader.snapshot()[0].timestamp_ms, 1_700_000_000_000);
}

#[test]
fn no_usable_timestamp_anywhere_is_visible_unsupported() {
    let mut reader = GrokReader::new("session-b");
    assert!(matches!(
        reader.ingest(
            br#"{"params":{"update":{"sessionUpdate":"turn_completed","usage":{"inputTokens":10,"outputTokens":5}}}}"#
        ),
        Err(ReaderError::Unsupported(_) | ReaderError::Declined(_))
    ));
}
