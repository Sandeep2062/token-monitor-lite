use rusqlite::Connection;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;
use token_monitor_lite::accounting::{SessionKey, TokenCounts};
use token_monitor_lite::aggregation::{BucketTimezone, CanonicalLedger};
use token_monitor_lite::discovery::{HostPaths, HostPlatform, NativePaths, resolve_paths};
use token_monitor_lite::parsers::{CostStatus, ResponseUsage};
use token_monitor_lite::sources::{
    ObservationSourceState, RetainedPeriod, RetainedPeriodUsage, SourceFormat, SourceId,
    SourceSnapshot,
};
use token_monitor_lite::storage::{
    BACKUP_DIRECTORY_NAME, CanonicalCommit, DATABASE_FILE_NAME, NativeSettings, StoreError,
    StoreHandle,
};

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct NativeTree(PathBuf);

impl NativeTree {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "token-monitor-persisted-runtime-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn paths(&self) -> NativePaths {
        resolve_paths(&HostPaths {
            platform: HostPlatform::Linux,
            home: self.0.clone(),
            app_data: None,
            local_app_data: None,
            xdg_config_home: None,
            xdg_state_home: None,
            legacy_shared_override: Some(self.0.join("legacy")),
            native_data_override: Some(self.0.join("native")),
            enabled_clients: Default::default(),
        })
        .unwrap()
        .native
    }

    fn database(&self) -> PathBuf {
        self.paths().root().join(DATABASE_FILE_NAME)
    }
}

impl Drop for NativeTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn source(seed: u8, generation: u64, input: u64) -> Arc<SourceSnapshot> {
    let usage = ResponseUsage {
        key: SessionKey::new("claude", format!("session-{seed}")),
        model: "sonnet".to_owned(),
        tokens: TokenCounts {
            input,
            ..TokenCounts::default()
        },
        cache_write_1h: 0,
        timestamp_ms: 1_799_000_000_000 + i64::from(seed),
        dedup_key: Some(format!("message-{seed}")),
        billing_provider_hint: None,
        service_tier: None,
        agent: None,
        cost: CostStatus::Unavailable,
    };
    Arc::new(SourceSnapshot {
        source_id: SourceId::from_bytes([seed; 16]),
        format: SourceFormat::ClaudeCli,
        parser_pin: "fixture-pin".to_owned(),
        generation,
        rows: Arc::from([usage]),
        retained: Arc::from([]),
        totals: TokenCounts {
            input,
            ..TokenCounts::default()
        },
        cache_write_1h: 0,
        incomplete_tail: false,
        has_supported_usage: true,
        source_state: ObservationSourceState::Live,
    })
}

#[test]
fn sqlite_local_usage_rows_commit_and_rehydrate_as_request_observations() {
    let tree = NativeTree::new();
    let (store, _) = StoreHandle::open(tree.paths()).unwrap();
    let usage = ResponseUsage {
        key: SessionKey::new("codex", "thread-a"),
        model: "gpt-5".to_owned(),
        tokens: TokenCounts {
            input: 6,
            output: 2,
            cache_read: 2,
            cache_write: 0,
            reasoning: 1,
        },
        cache_write_1h: 0,
        timestamp_ms: 1_794_218_400_000,
        dedup_key: Some("codex-local:fixture".to_owned()),
        billing_provider_hint: None,
        service_tier: None,
        agent: Some("agent-1".to_owned()),
        cost: CostStatus::Unavailable,
    };
    let source = Arc::new(SourceSnapshot {
        source_id: SourceId::from_bytes([10; 16]),
        format: SourceFormat::CodexLocalUsageSqlite,
        parser_pin: "fixture-pin".to_owned(),
        generation: 1,
        rows: Arc::from([usage]),
        retained: Arc::from([]),
        totals: TokenCounts {
            input: 6,
            output: 2,
            cache_read: 2,
            cache_write: 0,
            reasoning: 1,
        },
        cache_write_1h: 0,
        incomplete_tail: false,
        has_supported_usage: true,
        source_state: ObservationSourceState::Live,
    });
    let ledger = CanonicalLedger::new("UTC".parse().unwrap());
    let (_, commit) = commit_for(&ledger, source, 0);
    store.commit_runtime(commit).unwrap();
    store.shutdown().unwrap();

    let (store, snapshot) = StoreHandle::open(tree.paths()).unwrap();
    assert_eq!(snapshot.runtime.sources.len(), 1);
    assert_eq!(
        snapshot.runtime.sources[0].format,
        SourceFormat::CodexLocalUsageSqlite
    );
    assert_eq!(snapshot.runtime.sources[0].rows.len(), 1);
    assert_eq!(snapshot.runtime.sources[0].rows[0].key.session, "thread-a");
    assert_eq!(
        snapshot.runtime.sources[0].rows[0].agent.as_deref(),
        Some("agent-1")
    );
    store.shutdown().unwrap();
}

#[test]
fn sqlite_retained_periods_commit_and_rehydrate_without_request_rows() {
    let tree = NativeTree::new();
    let (store, _) = StoreHandle::open(tree.paths()).unwrap();
    let retained = Arc::from([RetainedPeriodUsage {
        client: "claude".to_owned(),
        session: "archive-session".to_owned(),
        period: RetainedPeriod::AllTime,
        captured_at_ms: 1_794_218_400_000,
        day: Some("2026-11-12".to_owned()),
        month: Some("2026-11".to_owned()),
        total: 100,
        input: 70,
        output: 15,
        cache_read: 10,
        cache_write: 5,
        reasoning: 2,
        unclassified: 0,
        components_complete: true,
        models_complete: true,
        models: Arc::from([]),
    }]);
    let source = Arc::new(SourceSnapshot {
        source_id: SourceId::from_bytes([9; 16]),
        format: SourceFormat::SessionArchiveSqlite,
        parser_pin: "fixture-pin".to_owned(),
        generation: 1,
        rows: Arc::from([]),
        retained,
        totals: TokenCounts::default(),
        cache_write_1h: 0,
        incomplete_tail: false,
        has_supported_usage: true,
        source_state: ObservationSourceState::Live,
    });
    let ledger = CanonicalLedger::new("UTC".parse().unwrap());
    let (candidate, commit) = commit_for(&ledger, source, 0);
    assert_eq!(candidate.canonical().rollups.retained_periods.len(), 1);
    store.commit_runtime(commit).unwrap();
    let stored = store.runtime_snapshot().unwrap();
    assert_eq!(stored.sources.len(), 1);
    assert_eq!(stored.sources[0].format, SourceFormat::SessionArchiveSqlite);
    assert_eq!(stored.sources[0].retained.len(), 1);
    assert!(stored.sources[0].rows.is_empty());
    assert_eq!(
        stored
            .canonical
            .as_ref()
            .unwrap()
            .rollups
            .retained_periods
            .len(),
        1
    );
    store.shutdown().unwrap();

    let (store, snapshot) = StoreHandle::open(tree.paths()).unwrap();
    assert_eq!(snapshot.runtime.sources.len(), 1);
    assert_eq!(
        snapshot.runtime.sources[0].retained[0].session,
        "archive-session"
    );
    assert_eq!(
        snapshot.runtime.sources[0].retained[0].period,
        RetainedPeriod::AllTime
    );
    store.shutdown().unwrap();
}

fn commit_for(
    ledger: &CanonicalLedger,
    source: Arc<SourceSnapshot>,
    expected_runtime_generation: u64,
) -> (
    token_monitor_lite::aggregation::CanonicalCandidate,
    CanonicalCommit,
) {
    let candidate = ledger.prepare_replace_source(source).unwrap();
    let commit = CanonicalCommit {
        expected_runtime_generation,
        settings_generation: 0,
        day_generation: 0,
        timezone_name: "UTC".to_owned(),
        source: candidate.source().clone(),
        canonical: candidate.canonical(),
    };
    (candidate, commit)
}

#[test]
fn fresh_schema_has_nine_strict_path_and_secret_free_runtime_roles() {
    let tree = NativeTree::new();
    let (store, snapshot) = StoreHandle::open(tree.paths()).unwrap();
    assert_eq!(snapshot.schema_version, 4);
    let runtime = store.runtime_snapshot().unwrap();
    assert_eq!(runtime.generation, 0);
    assert!(!runtime.initialized);
    assert!(runtime.sources.is_empty());
    assert!(runtime.canonical.is_none());
    store.shutdown().unwrap();

    let connection = Connection::open(tree.database()).unwrap();
    let roles = [
        "runtime_state",
        "runtime_sources",
        "source_observations",
        "canonical_observations",
        "canonical_provenance",
        "runtime_sqlite_sources",
        "source_sqlite_observations",
        "source_retained_periods",
        "source_retained_models",
    ];
    for role in roles {
        let sql: String = connection
            .query_row(
                "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = ?1",
                [role],
                |row| row.get(0),
            )
            .unwrap();
        assert!(sql.to_ascii_uppercase().contains(" STRICT"), "{role}");
        let mut columns = connection
            .prepare(&format!("PRAGMA table_info('{role}')"))
            .unwrap();
        for column in columns
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
        {
            let column = column.unwrap().to_ascii_lowercase();
            for forbidden in [
                "path",
                "raw",
                "secret",
                "password",
                "credential",
                "prompt",
                "title",
                "content",
            ] {
                assert!(!column.contains(forbidden), "{role}.{column}");
            }
        }
    }

    assert!(
        connection
            .execute(
                "INSERT INTO runtime_sources(
             source_id, format, client, parser_pin, source_generation, source_state,
             observation_count, session_count, last_observed_at
         ) VALUES ('bad', 'claude-jsonl', 'claude', 'pin', -1, 'live', 0, 0, NULL)",
                [],
            )
            .is_err()
    );
    assert!(
        connection
            .execute(
                "INSERT INTO canonical_provenance(
             canonical_id, source_id, source_generation, observation_ordinal
         ) VALUES ('missing', 'missing', 1, 0)",
                [],
            )
            .is_err()
    );
}

#[test]
fn v2_upgrade_preserves_existing_bytes_and_starts_uninitialized_runtime() {
    let tree = NativeTree::new();
    let settings = br#"{ "unknown" : {"preserved":true} }"#;
    let (store, _) = StoreHandle::open(tree.paths()).unwrap();
    store
        .save_settings(0, NativeSettings::from_json_bytes(settings).unwrap())
        .unwrap();
    store.shutdown().unwrap();

    let connection = Connection::open(tree.database()).unwrap();
    connection
        .execute_batch(
            "DROP TABLE canonical_provenance;
         DROP TABLE canonical_sqlite_provenance;
         DROP TABLE canonical_observations;
         DROP TABLE source_observations;
         DROP TABLE runtime_sources;
         DROP TABLE runtime_state;
         DROP TABLE source_retained_models;
         DROP TABLE source_retained_periods;
         DROP TABLE source_sqlite_observations;
         DROP TABLE runtime_sqlite_sources;
         UPDATE schema_meta SET version = 2 WHERE id = 1;
         PRAGMA user_version = 2;",
        )
        .unwrap();
    drop(connection);

    let (store, snapshot) = StoreHandle::open(tree.paths()).unwrap();
    assert_eq!(snapshot.schema_version, 4);
    assert_eq!(snapshot.settings.unwrap().json_bytes(), settings);
    assert!(!store.runtime_snapshot().unwrap().initialized);
    store.shutdown().unwrap();

    let backups = fs::read_dir(tree.paths().root().join(BACKUP_DIRECTORY_NAME))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    assert!(backups.iter().any(|path| {
        Connection::open(path).ok().is_some_and(|connection| {
            connection
                .query_row("SELECT version FROM schema_meta", [], |row| {
                    row.get::<_, i64>(0)
                })
                .ok()
                == Some(2)
        })
    }));
}

#[test]
fn runtime_commit_is_atomic_restart_safe_and_compare_and_set() {
    let tree = NativeTree::new();
    let (store, _) = StoreHandle::open(tree.paths()).unwrap();
    let timezone: BucketTimezone = "UTC".parse().unwrap();
    let mut ledger = CanonicalLedger::new(timezone);
    let (candidate, commit) = commit_for(&ledger, source(1, 1, 17), 0);
    let persisted = store.commit_runtime(commit.clone()).unwrap();
    assert_eq!(persisted.generation, 1);
    assert_eq!(persisted.sources.len(), 1);
    assert_eq!(
        persisted.canonical.as_ref().unwrap().rollups.totals.input,
        17
    );
    ledger.activate(candidate).unwrap();

    assert!(matches!(
        store.commit_runtime(commit),
        Err(StoreError::RuntimeConflict {
            current_generation: 1
        })
    ));
    assert_eq!(store.runtime_snapshot().unwrap().generation, 1);
    store.shutdown().unwrap();

    let (store, _) = StoreHandle::open(tree.paths()).unwrap();
    let restarted = store.runtime_snapshot().unwrap();
    assert_eq!(restarted.generation, 1);
    assert_eq!(restarted.sources[0].rows[0].tokens.input, 17);
    assert_eq!(
        restarted.canonical.as_ref().unwrap().rollups,
        ledger.last_good()
    );
    store.shutdown().unwrap();
}

#[test]
fn source_generations_are_monotonic_and_multi_source_replacement_is_atomic() {
    let tree = NativeTree::new();
    let (store, _) = StoreHandle::open(tree.paths()).unwrap();
    let timezone: BucketTimezone = "UTC".parse().unwrap();
    let mut ledger = CanonicalLedger::new(timezone);

    let (first, commit) = commit_for(&ledger, source(1, 1, 17), 0);
    store.commit_runtime(commit).unwrap();
    ledger.activate(first).unwrap();
    let (second, commit) = commit_for(&ledger, source(2, 1, 5), 1);
    store.commit_runtime(commit).unwrap();
    ledger.activate(second).unwrap();

    let (replacement, commit) = commit_for(&ledger, source(1, 2, 23), 2);
    let mut stale = commit.clone();
    stale.source = source(1, 1, 99);
    let before = store.runtime_snapshot().unwrap();
    assert_eq!(store.commit_runtime(stale), Err(StoreError::InvalidRuntime));
    assert!(Arc::ptr_eq(&before, &store.runtime_snapshot().unwrap()));

    let replaced = store.commit_runtime(commit).unwrap();
    ledger.activate(replacement).unwrap();
    assert_eq!(replaced.generation, 3);
    assert_eq!(replaced.sources.len(), 2);
    assert_eq!(
        replaced
            .sources
            .iter()
            .find(|persisted| persisted.source_id == SourceId::from_bytes([1; 16]))
            .unwrap()
            .generation,
        2
    );
    assert_eq!(
        replaced.canonical.as_ref().unwrap().rollups.totals.input,
        28
    );
    let canonical = ledger.canonical();
    assert_eq!(
        replaced.canonical.as_ref().unwrap().as_ref(),
        canonical.as_ref()
    );
    store.shutdown().unwrap();

    let (store, _) = StoreHandle::open(tree.paths()).unwrap();
    let restarted = store.runtime_snapshot().unwrap();
    assert_eq!(restarted.generation, 3);
    assert_eq!(restarted.sources.len(), 2);
    assert_eq!(
        restarted.canonical.as_ref().unwrap().rollups.totals.input,
        28
    );
    store.shutdown().unwrap();
}

#[test]
fn invalid_replacement_rolls_back_source_canonical_and_provenance() {
    let tree = NativeTree::new();
    let (store, _) = StoreHandle::open(tree.paths()).unwrap();
    let timezone: BucketTimezone = "UTC".parse().unwrap();
    let mut ledger = CanonicalLedger::new(timezone);
    let (first, commit) = commit_for(&ledger, source(1, 1, 17), 0);
    let before = store.commit_runtime(commit).unwrap();
    ledger.activate(first).unwrap();

    let (candidate, mut invalid) = commit_for(&ledger, source(1, 2, 23), 1);
    let mut malformed = (*invalid.canonical).clone();
    let mut rows = malformed.rows.to_vec();
    rows[0].provenance = Arc::from([]);
    malformed.rows = rows.into();
    invalid.canonical = Arc::new(malformed);
    assert!(matches!(
        store.commit_runtime(invalid),
        Err(StoreError::InvalidRuntime)
    ));
    let after = store.runtime_snapshot().unwrap();
    assert!(Arc::ptr_eq(&before, &after));
    assert_eq!(after.sources[0].generation, 1);
    assert_eq!(after.canonical.as_ref().unwrap().rollups.totals.input, 17);
    drop(candidate);
    store.shutdown().unwrap();
}

#[test]
fn unrelated_settings_write_reuses_the_same_immutable_runtime_snapshot() {
    let tree = NativeTree::new();
    let (store, _) = StoreHandle::open(tree.paths()).unwrap();
    let ledger = CanonicalLedger::new("UTC".parse().unwrap());
    let (_, commit) = commit_for(&ledger, source(1, 1, 17), 0);
    let runtime = store.commit_runtime(commit).unwrap();
    store
        .save_settings(
            0,
            NativeSettings::from_json_bytes(br#"{"clients":"claude"}"#).unwrap(),
        )
        .unwrap();
    let after = store.runtime_snapshot().unwrap();
    assert!(Arc::ptr_eq(&runtime, &after));
    store.shutdown().unwrap();
}

#[test]
fn runtime_queue_saturation_is_bounded_and_publishes_only_a_complete_commit() {
    let tree = NativeTree::new();
    let (store, _) = StoreHandle::open(tree.paths()).unwrap();
    let ledger = CanonicalLedger::new("UTC".parse().unwrap());
    let (_, commit) = commit_for(&ledger, source(1, 1, 17), 0);
    let lock = Connection::open(tree.database()).unwrap();
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();

    let barrier = Arc::new(Barrier::new(17));
    let joins = (0..16)
        .map(|_| {
            let store = store.clone();
            let commit = commit.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                store.commit_runtime(commit)
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    thread::sleep(Duration::from_millis(50));
    lock.execute_batch("COMMIT").unwrap();

    let results = joins
        .into_iter()
        .map(|join| join.join().unwrap())
        .collect::<Vec<_>>();
    assert!(results.iter().any(Result::is_ok));
    assert!(
        results
            .iter()
            .any(|result| matches!(result, Err(StoreError::Busy)))
    );
    assert_eq!(store.runtime_snapshot().unwrap().generation, 1);
    assert_eq!(
        store
            .runtime_snapshot()
            .unwrap()
            .canonical
            .as_ref()
            .unwrap()
            .rollups
            .totals
            .input,
        17
    );
    store.shutdown().unwrap();
}

#[test]
fn accepted_runtime_commit_drains_before_shutdown() {
    let tree = NativeTree::new();
    let (store, _) = StoreHandle::open(tree.paths()).unwrap();
    let ledger = CanonicalLedger::new("UTC".parse().unwrap());
    let (_, commit) = commit_for(&ledger, source(1, 1, 17), 0);
    let lock = Connection::open(tree.database()).unwrap();
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();

    let commit_thread = {
        let store = store.clone();
        thread::spawn(move || store.commit_runtime(commit))
    };
    thread::sleep(Duration::from_millis(25));
    let shutdown = thread::spawn(move || store.shutdown());
    thread::sleep(Duration::from_millis(25));
    lock.execute_batch("COMMIT").unwrap();

    assert_eq!(commit_thread.join().unwrap().unwrap().generation, 1);
    shutdown.join().unwrap().unwrap();
    let (store, _) = StoreHandle::open(tree.paths()).unwrap();
    assert_eq!(store.runtime_snapshot().unwrap().generation, 1);
    store.shutdown().unwrap();
}

#[test]
fn generation_conversion_overflow_rolls_back_without_republication() {
    let tree = NativeTree::new();
    let (store, _) = StoreHandle::open(tree.paths()).unwrap();
    let timezone: BucketTimezone = "UTC".parse().unwrap();
    let mut ledger = CanonicalLedger::new(timezone);
    let (candidate, commit) = commit_for(&ledger, source(1, 1, 17), 0);
    store.commit_runtime(commit).unwrap();
    ledger.activate(candidate).unwrap();
    store.shutdown().unwrap();

    let connection = Connection::open(tree.database()).unwrap();
    connection
        .execute(
            "UPDATE runtime_state SET runtime_generation = ?1 WHERE id = 1",
            [i64::MAX],
        )
        .unwrap();
    drop(connection);

    let (store, _) = StoreHandle::open(tree.paths()).unwrap();
    let before = store.runtime_snapshot().unwrap();
    assert_eq!(before.generation, i64::MAX as u64);
    let ledger =
        CanonicalLedger::rehydrate(timezone, before.sources.iter().cloned(), before.generation)
            .unwrap();
    let (candidate, commit) = commit_for(&ledger, source(1, 2, 23), before.generation);
    drop(candidate);
    assert_eq!(
        store.commit_runtime(commit),
        Err(StoreError::InvalidRuntime)
    );
    assert!(Arc::ptr_eq(&before, &store.runtime_snapshot().unwrap()));
    store.shutdown().unwrap();
}

#[test]
fn release_scale_rows_remain_shared_behind_immutable_arcs() {
    let tree = NativeTree::new();
    let (store, _) = StoreHandle::open(tree.paths()).unwrap();
    let rows = (0..1_000)
        .map(|index| ResponseUsage {
            key: SessionKey::new("claude", format!("session-{index:04}")),
            model: "sonnet".to_owned(),
            tokens: TokenCounts {
                input: 1,
                ..TokenCounts::default()
            },
            cache_write_1h: 0,
            timestamp_ms: 1_799_000_000_000 + i64::from(index),
            dedup_key: Some(format!("message-{index:04}")),
            billing_provider_hint: None,
            service_tier: None,
            agent: None,
            cost: CostStatus::Unavailable,
        })
        .collect::<Vec<_>>();
    let source = Arc::new(SourceSnapshot {
        source_id: SourceId::from_bytes([7; 16]),
        format: SourceFormat::ClaudeCli,
        parser_pin: "fixture-pin".to_owned(),
        generation: 1,
        rows: rows.into(),
        retained: Arc::from([]),
        totals: TokenCounts {
            input: 1_000,
            ..TokenCounts::default()
        },
        cache_write_1h: 0,
        incomplete_tail: false,
        has_supported_usage: true,
        source_state: ObservationSourceState::Live,
    });
    let ledger = CanonicalLedger::new("UTC".parse().unwrap());
    let (_, commit) = commit_for(&ledger, source.clone(), 0);
    let runtime = store.commit_runtime(commit).unwrap();
    let snapshot = store.snapshot().unwrap();
    assert!(Arc::ptr_eq(&runtime, &snapshot.runtime));
    assert!(Arc::ptr_eq(&source, &runtime.sources[0]));
    assert_eq!(runtime.sources[0].rows.len(), 1_000);
    assert_eq!(runtime.canonical.as_ref().unwrap().rows.len(), 1_000);
    assert!(Arc::ptr_eq(&runtime, &store.runtime_snapshot().unwrap()));
    store.shutdown().unwrap();
}

#[test]
fn logical_runtime_corruption_recovers_the_latest_verified_backup() {
    let tree = NativeTree::new();
    let (store, _) = StoreHandle::open(tree.paths()).unwrap();
    let ledger = CanonicalLedger::new("UTC".parse().unwrap());
    let (_, commit) = commit_for(&ledger, source(1, 1, 17), 0);
    store.commit_runtime(commit).unwrap();
    store.create_backup().unwrap();
    store.shutdown().unwrap();

    let connection = Connection::open(tree.database()).unwrap();
    connection
        .execute("UPDATE canonical_observations SET input_tokens = 99", [])
        .unwrap();
    drop(connection);

    let (store, snapshot) = StoreHandle::open(tree.paths()).unwrap();
    assert_eq!(snapshot.runtime.generation, 1);
    assert_eq!(
        snapshot
            .runtime
            .canonical
            .as_ref()
            .unwrap()
            .rollups
            .totals
            .input,
        17
    );
    assert!(
        fs::read_dir(
            tree.paths()
                .root()
                .join(token_monitor_lite::storage::RECOVERY_DIRECTORY_NAME)
        )
        .unwrap()
        .next()
        .is_some()
    );
    store.shutdown().unwrap();
}

#[test]
fn runtime_constraints_reject_direct_generation_and_referential_corruption() {
    let tree = NativeTree::new();
    let (store, _) = StoreHandle::open(tree.paths()).unwrap();
    store.shutdown().unwrap();
    let connection = Connection::open(tree.database()).unwrap();
    connection
        .pragma_update(None, "foreign_keys", true)
        .unwrap();
    assert!(
        connection
            .execute(
                "INSERT INTO runtime_state(
             id, runtime_generation, settings_generation, day_generation,
             timezone_name, parser_contract_revision, committed_at
         ) VALUES (2, 1, 0, 0, 'UTC', 'gate-2a-v1', 1)",
                [],
            )
            .is_err()
    );
    assert!(
        connection
            .execute(
                "INSERT INTO source_observations(
             source_id, source_generation, ordinal, client, session, model, observed_at,
             input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
             reasoning_tokens, cache_write_1h_tokens, dedup_identity, identity_class,
             provider_hint, service_tier
         ) VALUES ('missing', 1, 0, 'claude', 's', 'm', 1, 0, 0, 0, 0, 0, 0,
                   NULL, 'source-local', NULL, NULL)",
                [],
            )
            .is_err()
    );
    let count: i64 = connection
        .query_row("SELECT COUNT(*) FROM source_observations", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, 0);
}

#[test]
fn future_schema_is_rejected_without_mutation() {
    let tree = NativeTree::new();
    let (store, _) = StoreHandle::open(tree.paths()).unwrap();
    store.shutdown().unwrap();
    let connection = Connection::open(tree.database()).unwrap();
    connection
        .execute("UPDATE schema_meta SET version = 99", [])
        .unwrap();
    connection.pragma_update(None, "user_version", 99).unwrap();
    drop(connection);
    let before = fs::read(tree.database()).unwrap();
    assert!(matches!(
        StoreHandle::open(tree.paths()),
        Err(StoreError::FutureSchema(99))
    ));
    assert_eq!(fs::read(tree.database()).unwrap(), before);
}
