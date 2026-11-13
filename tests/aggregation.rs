use chrono::{DateTime, NaiveDate};
use std::sync::Arc;
use token_monitor_lite::accounting::{SessionKey, TokenCounts};
use token_monitor_lite::aggregation::{
    AggregationError, BucketTimezone, CanonicalLedger, MAX_SOURCES,
};
use token_monitor_lite::parsers::{CostStatus, ResponseUsage};
use token_monitor_lite::sources::{ObservationSourceState, SourceFormat, SourceId, SourceSnapshot};

fn timestamp(value: &str) -> i64 {
    DateTime::parse_from_rfc3339(value)
        .unwrap()
        .timestamp_millis()
}

fn timezone(name: &str) -> BucketTimezone {
    name.parse().unwrap()
}

fn usage(
    client: &str,
    session: &str,
    model: &str,
    at: &str,
    input: u64,
    dedup_key: Option<&str>,
) -> ResponseUsage {
    ResponseUsage {
        key: SessionKey::new(client, session),
        model: model.into(),
        tokens: TokenCounts {
            input,
            ..TokenCounts::default()
        },
        cache_write_1h: 0,
        timestamp_ms: timestamp(at),
        dedup_key: dedup_key.map(str::to_owned),
        billing_provider_hint: None,
        service_tier: None,
        agent: None,
        cost: CostStatus::Unavailable,
    }
}

fn source_id(label: &str) -> SourceId {
    let mut bytes = [0_u8; 16];
    for (index, byte) in label.bytes().enumerate() {
        bytes[index % bytes.len()] = bytes[index % bytes.len()]
            .wrapping_mul(31)
            .wrapping_add(byte);
    }
    SourceId::from_bytes(bytes)
}

fn source(label: &str, generation: u64, rows: Vec<ResponseUsage>) -> Arc<SourceSnapshot> {
    let format = match rows.first().map(|row| row.key.client.as_str()) {
        Some("codex") => SourceFormat::CodexCli,
        _ => SourceFormat::ClaudeCli,
    };
    let mut totals = TokenCounts::default();
    let mut cache_write_1h = 0;
    for row in &rows {
        totals = totals.checked_add(row.tokens).unwrap();
        cache_write_1h += row.cache_write_1h;
    }
    Arc::new(SourceSnapshot {
        source_id: source_id(label),
        format,
        parser_pin: "fixture".to_owned(),
        generation,
        rows: rows.into(),
        retained: Arc::from([]),
        totals,
        cache_write_1h,
        incomplete_tail: false,
        has_supported_usage: true,
        source_state: ObservationSourceState::Live,
    })
}

#[test]
fn rolls_up_by_configured_local_day_model_and_client_scoped_session() {
    let mut ledger = CanonicalLedger::new(timezone("Europe/Helsinki"));
    ledger
        .replace_source(source(
            "a.jsonl",
            1,
            vec![
                usage(
                    "claude",
                    "shared",
                    "sonnet",
                    "2026-11-08T23:30:00Z",
                    10,
                    Some("message:a"),
                ),
                usage(
                    "claude",
                    "shared",
                    "opus",
                    "2026-11-09T23:30:00Z",
                    20,
                    Some("message:b"),
                ),
            ],
        ))
        .unwrap();
    ledger
        .replace_source(source(
            "b.jsonl",
            1,
            vec![usage(
                "codex",
                "shared",
                "gpt-5",
                "2026-11-09T10:00:00Z",
                30,
                Some("codex:c"),
            )],
        ))
        .unwrap();

    let snapshot = ledger.last_good();
    assert_eq!(snapshot.timezone_name, "Europe/Helsinki");
    assert_eq!(snapshot.totals.input, 60);
    assert_eq!(snapshot.day_models.len(), 3);
    assert_eq!(
        snapshot.day_models[0].day,
        NaiveDate::from_ymd_opt(2026, 11, 9).unwrap()
    );
    assert_eq!(snapshot.day_models[0].client, "claude");
    assert_eq!(snapshot.day_models[0].model, "sonnet");
    assert_eq!(
        snapshot.day_models[2].day,
        NaiveDate::from_ymd_opt(2026, 11, 10).unwrap()
    );
    assert_eq!(snapshot.session_models.len(), 3);
    assert_eq!(snapshot.session_models[0].key.client, "claude");
    assert_eq!(snapshot.session_models[0].key.session, "shared");
    assert_eq!(snapshot.session_models[2].key.client, "codex");
    assert_eq!(snapshot.session_models[2].key.session, "shared");
}

#[test]
fn stable_cross_file_identity_counts_once_and_records_all_provenance() {
    let mut ledger = CanonicalLedger::new(timezone("UTC"));
    let row = usage(
        "claude",
        "s",
        "sonnet",
        "2026-11-09T10:00:00Z",
        10,
        Some("message:copy"),
    );
    ledger
        .replace_source(source("a.jsonl", 1, vec![row.clone()]))
        .unwrap();
    let update = ledger
        .replace_source(source("b.jsonl", 1, vec![row]))
        .unwrap();

    assert_eq!(update.tokens.input, 0);
    let snapshot = ledger.last_good();
    assert_eq!(snapshot.totals.input, 10);
    assert_eq!(snapshot.day_models[0].sources.len(), 2);
    assert!(snapshot.sources.iter().all(|item| item.unique_rows == 0));
    assert!(snapshot.sources.iter().all(|item| item.overlap_rows == 1));
}

#[test]
fn unkeyed_rows_are_never_heuristically_deduplicated() {
    let mut ledger = CanonicalLedger::new(timezone("UTC"));
    let row = usage("claude", "s", "sonnet", "2026-11-09T10:00:00Z", 10, None);
    ledger
        .replace_source(source("a.jsonl", 1, vec![row.clone()]))
        .unwrap();
    ledger
        .replace_source(source("b.jsonl", 1, vec![row]))
        .unwrap();

    let snapshot = ledger.last_good();
    assert_eq!(snapshot.totals.input, 20);
    assert!(snapshot.sources.iter().all(|item| item.unique_rows == 1));
    assert!(snapshot.sources.iter().all(|item| item.overlap_rows == 0));
}

#[test]
fn conflicting_overlap_withholds_candidate_and_keeps_last_good() {
    let mut ledger = CanonicalLedger::new(timezone("UTC"));
    ledger
        .replace_source(source(
            "a.jsonl",
            1,
            vec![usage(
                "codex",
                "s",
                "gpt-5",
                "2026-11-09T10:00:00Z",
                10,
                Some("codex:copy"),
            )],
        ))
        .unwrap();
    let before = ledger.last_good();

    let error = ledger
        .replace_source(source(
            "b.jsonl",
            1,
            vec![usage(
                "codex",
                "s",
                "gpt-5",
                "2026-11-09T10:00:00Z",
                11,
                Some("codex:copy"),
            )],
        ))
        .unwrap_err();

    assert_eq!(error, AggregationError::ConflictingOverlap);
    assert!(Arc::ptr_eq(&before, &ledger.last_good()));
    assert_eq!(ledger.last_good().sources.len(), 1);
}

#[test]
fn source_replacement_publishes_signed_correction() {
    let mut ledger = CanonicalLedger::new(timezone("UTC"));
    ledger
        .replace_source(source(
            "a.jsonl",
            1,
            vec![usage(
                "claude",
                "s",
                "sonnet",
                "2026-11-09T10:00:00Z",
                100,
                Some("message:a"),
            )],
        ))
        .unwrap();
    let update = ledger
        .replace_source(source(
            "a.jsonl",
            2,
            vec![usage(
                "claude",
                "s",
                "sonnet",
                "2026-11-09T10:00:00Z",
                40,
                Some("message:a"),
            )],
        ))
        .unwrap();

    assert_eq!(update.tokens.input, -60);
    assert_eq!(ledger.last_good().generation, 2);
    assert_eq!(ledger.last_good().totals.input, 40);
}

#[test]
fn checked_rollup_overflow_does_not_publish() {
    let mut ledger = CanonicalLedger::new(timezone("UTC"));
    ledger
        .replace_source(source(
            "a.jsonl",
            1,
            vec![usage(
                "claude",
                "s",
                "sonnet",
                "2026-11-09T10:00:00Z",
                u64::MAX,
                Some("message:a"),
            )],
        ))
        .unwrap();
    let before = ledger.last_good();

    assert_eq!(
        ledger
            .replace_source(source(
                "b.jsonl",
                1,
                vec![usage(
                    "claude",
                    "t",
                    "sonnet",
                    "2026-11-09T11:00:00Z",
                    1,
                    Some("message:b"),
                )],
            ))
            .unwrap_err(),
        AggregationError::Overflow
    );
    assert!(Arc::ptr_eq(&before, &ledger.last_good()));
}

#[test]
fn named_timezone_follows_dst_at_local_midnight() {
    let mut ledger = CanonicalLedger::new(timezone("America/New_York"));
    ledger
        .replace_source(source(
            "dst.jsonl",
            1,
            vec![
                usage(
                    "claude",
                    "s",
                    "sonnet",
                    "2026-01-15T04:30:00Z",
                    10,
                    Some("message:winter"),
                ),
                usage(
                    "claude",
                    "s",
                    "sonnet",
                    "2026-07-15T04:30:00Z",
                    20,
                    Some("message:summer"),
                ),
            ],
        ))
        .unwrap();

    let snapshot = ledger.last_good();
    assert_eq!(snapshot.day_models.len(), 2);
    assert_eq!(
        snapshot.day_models[0].day,
        NaiveDate::from_ymd_opt(2026, 1, 14).unwrap()
    );
    assert_eq!(
        snapshot.day_models[1].day,
        NaiveDate::from_ymd_opt(2026, 7, 15).unwrap()
    );
}

#[test]
fn unknown_or_fixed_offset_timezone_is_rejected() {
    assert!("Mars/Olympus_Mons".parse::<BucketTimezone>().is_err());
    assert!("-05:00".parse::<BucketTimezone>().is_err());
}

#[test]
fn overlap_provenance_is_independent_of_source_arrival_order() {
    fn snapshot(labels: [&str; 2]) -> Vec<(SourceId, usize, usize)> {
        let row = usage(
            "claude",
            "s",
            "sonnet",
            "2026-11-09T10:00:00Z",
            10,
            Some("message:shared"),
        );
        let mut ledger = CanonicalLedger::new(timezone("UTC"));
        for label in labels {
            ledger
                .replace_source(source(label, 1, vec![row.clone()]))
                .unwrap();
        }
        ledger
            .last_good()
            .sources
            .iter()
            .map(|item| (item.source.source_id, item.unique_rows, item.overlap_rows))
            .collect()
    }

    assert_eq!(
        snapshot(["a.jsonl", "b.jsonl"]),
        snapshot(["b.jsonl", "a.jsonl"])
    );
}

#[test]
fn claude_fork_copy_merges_completeness_without_double_counting() {
    let mut bare = usage(
        "claude",
        "original",
        "sonnet",
        "2026-11-09T10:00:00Z",
        42_494,
        Some("message:response:request"),
    );
    bare.tokens.output = 10;
    let mut split = usage(
        "claude",
        "fork",
        "sonnet",
        "2026-11-09T10:00:00Z",
        265,
        Some("message:response:request"),
    );
    split.tokens.output = 12;
    split.tokens.cache_read = 42_000;
    split.tokens.cache_write = 120;

    let mut ledger = CanonicalLedger::new(timezone("UTC"));
    ledger
        .replace_source(source("original.jsonl", 1, vec![bare]))
        .unwrap();
    ledger
        .replace_source(source("fork.jsonl", 1, vec![split]))
        .unwrap();

    let snapshot = ledger.last_good();
    assert_eq!(snapshot.totals.input, 265);
    assert_eq!(snapshot.totals.output, 12);
    assert_eq!(snapshot.totals.cache_read, 42_000);
    assert_eq!(snapshot.totals.cache_write, 120);
    assert_eq!(snapshot.sources.len(), 2);
    assert_eq!(snapshot.session_models.len(), 1);
}

#[test]
fn conflict_can_recover_at_the_same_unpublished_source_generation() {
    let mut ledger = CanonicalLedger::new(timezone("UTC"));
    let canonical = usage(
        "codex",
        "s",
        "gpt-5",
        "2026-11-09T10:00:00Z",
        10,
        Some("codex:stable"),
    );
    ledger
        .replace_source(source("a.jsonl", 1, vec![canonical.clone()]))
        .unwrap();
    let mut conflicting = canonical.clone();
    conflicting.tokens.input = 11;
    assert_eq!(
        ledger
            .replace_source(source("b.jsonl", 1, vec![conflicting]))
            .unwrap_err(),
        AggregationError::ConflictingOverlap
    );

    ledger
        .replace_source(source("b.jsonl", 1, vec![canonical]))
        .unwrap();
    assert_eq!(ledger.last_good().totals.input, 10);
    assert_eq!(ledger.last_good().sources.len(), 2);
}

#[test]
fn stale_replacement_leaves_last_good_untouched() {
    let mut ledger = CanonicalLedger::new(timezone("UTC"));
    let row = usage(
        "claude",
        "s",
        "sonnet",
        "2026-11-09T10:00:00Z",
        10,
        Some("message:a"),
    );
    ledger
        .replace_source(source("a.jsonl", 2, vec![row.clone()]))
        .unwrap();
    let before = ledger.last_good();

    assert_eq!(
        ledger
            .replace_source(source("a.jsonl", 1, vec![row]))
            .unwrap_err(),
        AggregationError::StaleGeneration
    );
    assert!(Arc::ptr_eq(&before, &ledger.last_good()));
}

#[test]
fn retained_source_replacement_preserves_usage_without_purging() {
    let mut ledger = CanonicalLedger::new(timezone("UTC"));
    ledger
        .replace_source(source(
            "a.jsonl",
            7,
            vec![usage(
                "claude",
                "s",
                "sonnet",
                "2026-11-09T10:00:00Z",
                25,
                Some("message:a"),
            )],
        ))
        .unwrap();

    let mut retained = (*source(
        "a.jsonl",
        8,
        vec![usage(
            "claude",
            "s",
            "sonnet",
            "2026-11-09T10:00:00Z",
            25,
            Some("message:a"),
        )],
    ))
    .clone();
    retained.source_state = ObservationSourceState::Retained;
    let delta = ledger.replace_source(Arc::new(retained)).unwrap();
    assert_eq!(delta.tokens.input, 0);
    assert_eq!(ledger.last_good().totals.input, 25);
    assert_eq!(
        ledger.canonical().rows[0].source_state,
        ObservationSourceState::Retained
    );
}

#[test]
fn prepared_candidate_changes_nothing_until_activation() {
    let mut ledger = CanonicalLedger::new(timezone("UTC"));
    let before = ledger.last_good();
    let candidate = ledger
        .prepare_replace_source(source(
            "candidate",
            1,
            vec![usage(
                "claude",
                "s",
                "sonnet",
                "2026-11-09T10:00:00Z",
                9,
                Some("message:candidate"),
            )],
        ))
        .unwrap();
    assert!(Arc::ptr_eq(&before, &ledger.last_good()));
    assert_eq!(candidate.canonical().rollups.totals.input, 9);
    assert_eq!(ledger.activate(candidate).unwrap().tokens.input, 9);
    assert_eq!(ledger.last_good().totals.input, 9);
}

#[test]
fn invalid_timestamp_does_not_publish_candidate() {
    let mut ledger = CanonicalLedger::new(timezone("UTC"));
    ledger
        .replace_source(source(
            "a.jsonl",
            1,
            vec![usage(
                "claude",
                "s",
                "sonnet",
                "2026-11-09T10:00:00Z",
                10,
                Some("message:a"),
            )],
        ))
        .unwrap();
    let before = ledger.last_good();
    let mut invalid = usage(
        "claude",
        "s",
        "sonnet",
        "2026-11-09T10:00:00Z",
        20,
        Some("message:a"),
    );
    invalid.timestamp_ms = i64::MAX;

    assert_eq!(
        ledger
            .replace_source(source("a.jsonl", 2, vec![invalid]))
            .unwrap_err(),
        AggregationError::InvalidTimestamp
    );
    assert!(Arc::ptr_eq(&before, &ledger.last_good()));
}

#[test]
fn same_session_and_model_stay_partitioned_by_client() {
    let mut ledger = CanonicalLedger::new(timezone("UTC"));
    for client in ["claude", "codex"] {
        ledger
            .replace_source(source(
                &format!("{client}.jsonl"),
                1,
                vec![usage(
                    client,
                    "shared",
                    "same-model",
                    "2026-11-09T10:00:00Z",
                    10,
                    Some(&format!("{client}:response")),
                )],
            ))
            .unwrap();
    }

    let snapshot = ledger.last_good();
    assert_eq!(snapshot.session_models.len(), 2);
    assert_ne!(
        snapshot.session_models[0].key.client,
        snapshot.session_models[1].key.client
    );
}

#[test]
fn supplemental_ledger_rows_for_native_threads_are_ignored() {
    let mut ledger = CanonicalLedger::new(timezone("UTC"));
    ledger
        .replace_source(source(
            "native.jsonl",
            1,
            vec![usage(
                "codex",
                "thread-native",
                "gpt-5",
                "2026-11-09T10:00:00Z",
                10,
                Some("codex:token_count-total:thread-native:openai:gpt-5:10:0:0:0"),
            )],
        ))
        .unwrap();
    let base = source(
        "local.sqlite",
        1,
        vec![
            usage(
                "codex",
                "thread-native",
                "gpt-5",
                "2026-11-09T10:00:01Z",
                10,
                Some("codex-local-usage:account:thread-native:10"),
            ),
            usage(
                "codex",
                "thread-local-only",
                "gpt-5",
                "2026-11-09T10:00:02Z",
                7,
                Some("codex-local-usage:account:thread-local-only:7"),
            ),
        ],
    );
    let mut local = (*base).clone();
    local.format = SourceFormat::CodexLocalUsageSqlite;
    ledger.replace_source(Arc::new(local)).unwrap();

    let snapshot = ledger.last_good();
    assert_eq!(snapshot.totals.input, 17);
    assert_eq!(snapshot.session_models.len(), 2);
    let native = snapshot
        .session_models
        .iter()
        .find(|row| row.key.session == "thread-native")
        .expect("native session retained once");
    assert_eq!(native.tokens.input, 10);
}

#[test]
fn source_capacity_failure_keeps_last_good() {
    let mut ledger = CanonicalLedger::new(timezone("UTC"));
    for index in 0..MAX_SOURCES {
        ledger
            .replace_source(source(&format!("source-{index:04}.jsonl"), 1, Vec::new()))
            .unwrap();
    }
    let before = ledger.last_good();

    assert_eq!(
        ledger
            .replace_source(source("overflow.jsonl", 1, Vec::new()))
            .unwrap_err(),
        AggregationError::Capacity("source count")
    );
    assert!(Arc::ptr_eq(&before, &ledger.last_good()));
}
