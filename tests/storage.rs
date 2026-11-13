use rusqlite::Connection;
use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;
use token_monitor_lite::discovery::{HostPaths, HostPlatform, NativePaths, resolve_paths};
use token_monitor_lite::storage::{
    ActivationState, BACKUP_DIRECTORY_NAME, CredentialOperation, CredentialOperationKind,
    CredentialOperationState, DATABASE_FILE_NAME, ImportReportItem, ImportState, ImportedSession,
    NativeSettings, PreserveState, RECOVERY_DIRECTORY_NAME, StagedArtifact, StoreError,
    StoreHandle, ValidatedImport,
};

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct NativeTree(PathBuf);

impl NativeTree {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "token-monitor-storage-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn paths(&self) -> NativePaths {
        let inputs = HostPaths {
            platform: HostPlatform::Linux,
            home: self.0.clone(),
            app_data: None,
            local_app_data: None,
            xdg_config_home: None,
            xdg_state_home: None,
            legacy_shared_override: Some(self.0.join("legacy")),
            native_data_override: Some(self.0.join("native")),
            enabled_clients: BTreeSet::new(),
        };
        resolve_paths(&inputs).unwrap().native
    }

    fn native_root(&self) -> PathBuf {
        self.0.join("native")
    }

    fn database(&self) -> PathBuf {
        self.native_root().join(DATABASE_FILE_NAME)
    }
}

impl Drop for NativeTree {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn settings(bytes: &[u8]) -> NativeSettings {
    NativeSettings::from_json_bytes(bytes).unwrap()
}

fn session(client: &str, id: &str, revision: u64, json: &[u8]) -> ImportedSession {
    ImportedSession {
        client: client.to_owned(),
        session: id.to_owned(),
        original_entry_json: json.to_vec(),
        legacy_key: format!("legacy-{client}-{id}"),
        source_revision: revision,
        archive_metadata_json: Some(br#"{"archiveOnly":true}"#.to_vec()),
    }
}

fn import(run_id: &str) -> ValidatedImport {
    ValidatedImport {
        run_id: run_id.to_owned(),
        expected_settings_revision: 0,
        settings: None,
        sessions: Vec::new(),
        report_items: Vec::new(),
        staged_artifacts: Vec::new(),
        credential_references: Vec::new(),
        credential_operations: Vec::new(),
    }
}

#[test]
fn schema_v3_enables_foreign_keys_and_has_no_secret_or_source_path_columns() {
    let tree = NativeTree::new();
    let (store, snapshot) = StoreHandle::open(tree.paths()).unwrap();
    assert_eq!(snapshot.schema_version, 4);
    assert!(snapshot.foreign_keys_enabled);
    store.shutdown().unwrap();

    let connection = Connection::open(tree.database()).unwrap();
    assert_eq!(
        connection
            .query_row("PRAGMA foreign_keys", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        1
    );
    let mut tables = connection
        .prepare("SELECT name FROM sqlite_schema WHERE type='table' ORDER BY name")
        .unwrap();
    let table_names = tables
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    for table in table_names {
        if table.starts_with("sqlite_") {
            continue;
        }
        let mut columns = connection
            .prepare(&format!("PRAGMA table_info('{table}')"))
            .unwrap();
        for column in columns
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
        {
            let column = column.unwrap().to_ascii_lowercase();
            assert!(!column.contains("secret"));
            assert!(!column.contains("password"));
            assert!(!column.contains("raw_path"));
            assert!(!column.contains("source_path"));
        }
    }
    assert!(
        connection
            .execute(
                "INSERT INTO import_report_items
                 (run_id, ordinal, artifact_kind, preservation_state, import_state,
                  activation_state, records)
                 VALUES ('missing', 0, 'settings', 'preserved', 'imported', 'active', 1)",
                [],
            )
            .is_err()
    );
}

#[test]
fn schema_v1_upgrades_directly_to_v3_without_losing_committed_operations() {
    let tree = NativeTree::new();
    let (store, _) = StoreHandle::open(tree.paths()).unwrap();
    store.shutdown().unwrap();

    let connection = Connection::open(tree.database()).unwrap();
    connection
        .execute(
            "INSERT INTO import_runs(run_id, committed_at) VALUES ('run-v1', 1)",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO credential_operations(
                 operation_id, run_id, credential_id, operation, operation_state
             ) VALUES (
                 'operation-0000000000000001', 'run-v1',
                 'credential-0000000000000001', 'set', 'metadata-committed'
             )",
            [],
        )
        .unwrap();
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
             DROP TABLE pending_credential_imports;
             UPDATE schema_meta SET version = 1 WHERE id = 1;
             PRAGMA user_version = 1;",
        )
        .unwrap();
    drop(connection);

    let (store, snapshot) = StoreHandle::open(tree.paths()).unwrap();
    assert_eq!(snapshot.schema_version, 4);
    assert!(snapshot.pending_credential_imports.is_empty());
    assert_eq!(snapshot.credential_operations.len(), 1);
    assert_eq!(
        snapshot.credential_operations[0].operation.state,
        CredentialOperationState::MetadataCommitted
    );
    assert!(tree.native_root().join(BACKUP_DIRECTORY_NAME).exists());
    store.shutdown().unwrap();
}

#[test]
fn settings_and_original_session_json_survive_byte_for_byte_across_reopen() {
    let tree = NativeTree::new();
    let settings_bytes = br#"{ "clients" : null, "unknown" : {"kept":true} }"#;
    let original = br#"{ "client":"claude", "session":"one", "unknown":null }"#;
    let (store, _) = StoreHandle::open(tree.paths()).unwrap();
    let saved = store.save_settings(0, settings(settings_bytes)).unwrap();
    assert_eq!(
        saved.settings.as_ref().unwrap().json_bytes(),
        settings_bytes
    );

    let mut next = import("run-exact-json");
    next.expected_settings_revision = saved.settings_revision;
    next.sessions.push(session("claude", "one", 7, original));
    let adopted = store.adopt_import(next).unwrap();
    assert_eq!(adopted.sessions[0].original_entry_json, original);
    store.shutdown().unwrap();

    let (store, reopened) = StoreHandle::open(tree.paths()).unwrap();
    assert_eq!(
        reopened.settings.as_ref().unwrap().json_bytes(),
        settings_bytes
    );
    assert_eq!(reopened.sessions[0].original_entry_json, original);
    assert_eq!(reopened.sessions[0].source_revision, 7);
    store.shutdown().unwrap();
}

#[test]
fn canonical_sessions_upsert_by_client_session_and_highest_revision() {
    let tree = NativeTree::new();
    let (store, _) = StoreHandle::open(tree.paths()).unwrap();
    let mut first = import("run-upsert-1");
    first.sessions = vec![
        session("claude", "same", 2, br#"{"value":"new"}"#),
        session("claude", "same", 1, br#"{"value":"old"}"#),
        session("codex", "same", 1, br#"{"value":"other-client"}"#),
    ];
    let snapshot = store.adopt_import(first).unwrap();
    assert_eq!(snapshot.sessions.len(), 2);
    assert_eq!(snapshot.sessions[0].client, "claude");
    assert_eq!(snapshot.sessions[0].source_revision, 2);
    assert_eq!(snapshot.sessions[1].client, "codex");

    let mut second = import("run-upsert-2");
    second.sessions = vec![session("claude", "same", 3, br#"{"value":"newest"}"#)];
    let snapshot = store.adopt_import(second).unwrap();
    assert_eq!(snapshot.sessions.len(), 2);
    assert_eq!(snapshot.sessions[0].source_revision, 3);
    assert_eq!(
        snapshot.sessions[0].original_entry_json,
        br#"{"value":"newest"}"#
    );
    store.shutdown().unwrap();
}

#[test]
fn a_failing_import_row_rolls_back_settings_sessions_and_report() {
    let tree = NativeTree::new();
    let (store, before) = StoreHandle::open(tree.paths()).unwrap();
    let mut next = import("run-rollback");
    next.settings = Some(settings(br#"{"clients":"claude"}"#));
    next.sessions = vec![
        session("a-client", "valid", 1, br#"{"ok":true}"#),
        session("z/invalid", "invalid", 1, br#"{"ok":false}"#),
    ];
    next.report_items.push(ImportReportItem {
        artifact_kind: "settings".to_owned(),
        preservation: PreserveState::Preserved,
        import: ImportState::Imported,
        activation: ActivationState::Activated,
        records: 1,
    });

    assert!(matches!(
        store.adopt_import(next),
        Err(StoreError::InvalidImport)
    ));
    let after = store.snapshot().unwrap();
    assert_eq!(after.settings_revision, before.settings_revision);
    assert!(after.settings.is_none());
    assert!(after.sessions.is_empty());
    assert!(after.import_runs.is_empty());
    assert!(after.report_items.is_empty());
    store.shutdown().unwrap();
}

#[test]
fn raw_secrets_and_absolute_legacy_paths_are_rejected_and_never_persisted() {
    let tree = NativeTree::new();
    let secret = "STORAGE_SECRET_SENTINEL_7c1f";
    assert!(matches!(
        NativeSettings::from_json_bytes(format!(r#"{{"apiKey":"{secret}"}}"#).as_bytes()),
        Err(StoreError::SecretRejected)
    ));

    let (store, _) = StoreHandle::open(tree.paths()).unwrap();
    let mut next = import("run-secret-reject");
    next.sessions.push(session(
        "claude",
        "secret-row",
        1,
        format!(r#"{{"nested":{{"refresh_token":"{secret}"}}}}"#).as_bytes(),
    ));
    assert!(matches!(
        store.adopt_import(next),
        Err(StoreError::SecretRejected)
    ));

    let mut path_row = import("run-path-row-reject");
    path_row.sessions.push(session(
        "claude",
        "path-row",
        1,
        br#"{"cwd":"/legacy/private/project"}"#,
    ));
    assert!(matches!(
        store.adopt_import(path_row),
        Err(StoreError::InvalidImport)
    ));

    let mut path_metadata = import("run-path-metadata-reject");
    path_metadata.staged_artifacts.push(StagedArtifact {
        staging_id: "/legacy/private/settings.json".to_owned(),
        artifact_kind: "settings".to_owned(),
        bytes: 10,
    });
    assert!(matches!(
        store.adopt_import(path_metadata),
        Err(StoreError::InvalidImport)
    ));
    store.shutdown().unwrap();

    for entry in fs::read_dir(tree.native_root()).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            let bytes = fs::read(path).unwrap();
            assert!(
                !bytes
                    .windows(secret.len())
                    .any(|part| part == secret.as_bytes())
            );
            assert!(!bytes.windows(15).any(|part| part == b"/legacy/private"));
        }
    }
}

#[test]
fn settings_save_is_compare_and_swap_and_generations_survive_restart() {
    let tree = NativeTree::new();
    let (store, _) = StoreHandle::open(tree.paths()).unwrap();
    let first = store
        .save_settings(0, settings(br#"{"clients":"claude"}"#))
        .unwrap();
    assert_eq!(first.settings_revision, 1);
    assert_eq!(first.settings_generation, 1);
    assert!(matches!(
        store.save_settings(0, settings(br#"{"clients":"codex"}"#)),
        Err(StoreError::Conflict {
            current_revision: 1
        })
    ));
    assert_eq!(
        store
            .snapshot()
            .unwrap()
            .settings
            .as_ref()
            .unwrap()
            .json_bytes(),
        br#"{"clients":"claude"}"#
    );
    store.shutdown().unwrap();

    let (store, reopened) = StoreHandle::open(tree.paths()).unwrap();
    assert_eq!(reopened.settings_revision, 1);
    assert_eq!(reopened.settings_generation, 1);
    store.shutdown().unwrap();
}

#[test]
fn bounded_admission_returns_busy_and_snapshots_never_show_partial_imports() {
    let tree = NativeTree::new();
    let (store, _) = StoreHandle::open(tree.paths()).unwrap();
    let lock = Connection::open(tree.database()).unwrap();
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();

    let workers = 24;
    let barrier = Arc::new(Barrier::new(workers + 1));
    let mut joins = Vec::new();
    for ordinal in 0..workers {
        let store = store.clone();
        let barrier = barrier.clone();
        joins.push(thread::spawn(move || {
            barrier.wait();
            store.save_settings(0, settings(format!(r#"{{"worker":{ordinal}}}"#).as_bytes()))
        }));
    }
    barrier.wait();
    thread::sleep(Duration::from_millis(50));
    lock.execute_batch("COMMIT").unwrap();
    let results = joins
        .into_iter()
        .map(|join| join.join().unwrap())
        .collect::<Vec<_>>();
    assert!(
        results
            .iter()
            .any(|result| matches!(result, Err(StoreError::Busy)))
    );
    assert_eq!(store.snapshot().unwrap().settings_revision, 1);

    let mut bulk = import("run-atomic-snapshot");
    bulk.expected_settings_revision = 1;
    bulk.sessions = (0..500)
        .map(|index| {
            session(
                "claude",
                &format!("session-{index:04}"),
                1,
                br#"{"tokens":1}"#,
            )
        })
        .collect();
    let writer = {
        let store = store.clone();
        thread::spawn(move || store.adopt_import(bulk))
    };
    while !writer.is_finished() {
        let count = store.snapshot().unwrap().sessions.len();
        assert!(count == 0 || count == 500);
    }
    assert_eq!(writer.join().unwrap().unwrap().sessions.len(), 500);
    store.shutdown().unwrap();
}

#[test]
fn shutdown_drains_an_accepted_command_before_joining_the_writer() {
    let tree = NativeTree::new();
    let (store, _) = StoreHandle::open(tree.paths()).unwrap();
    let lock = Connection::open(tree.database()).unwrap();
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();

    let save = {
        let store = store.clone();
        thread::spawn(move || store.save_settings(0, settings(br#"{"shutdown":"drained"}"#)))
    };
    thread::sleep(Duration::from_millis(25));
    let shutdown = thread::spawn(move || store.shutdown());
    thread::sleep(Duration::from_millis(25));
    lock.execute_batch("COMMIT").unwrap();

    assert_eq!(save.join().unwrap().unwrap().settings_revision, 1);
    shutdown.join().unwrap().unwrap();
}

#[test]
fn version_zero_migrates_after_backup_and_future_schema_is_untouched() {
    let version_zero = NativeTree::new();
    fs::create_dir_all(version_zero.native_root()).unwrap();
    Connection::open(version_zero.database())
        .unwrap()
        .close()
        .unwrap();
    let (store, snapshot) = StoreHandle::open(version_zero.paths()).unwrap();
    assert_eq!(snapshot.schema_version, 4);
    store.shutdown().unwrap();
    assert!(
        fs::read_dir(version_zero.native_root().join(BACKUP_DIRECTORY_NAME))
            .unwrap()
            .any(|entry| entry.unwrap().path().extension() == Some("sqlite3".as_ref()))
    );

    let future = NativeTree::new();
    fs::create_dir_all(future.native_root()).unwrap();
    let connection = Connection::open(future.database()).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE schema_meta(version INTEGER NOT NULL, applied_at INTEGER NOT NULL);
             INSERT INTO schema_meta(version, applied_at) VALUES (99, 0);",
        )
        .unwrap();
    connection.close().unwrap();
    let before = fs::read(future.database()).unwrap();
    assert!(matches!(
        StoreHandle::open(future.paths()),
        Err(StoreError::FutureSchema(99))
    ));
    assert_eq!(fs::read(future.database()).unwrap(), before);
}

#[test]
fn backup_is_verified_and_a_pre_rename_failure_keeps_active_data_usable() {
    let tree = NativeTree::new();
    let (store, _) = StoreHandle::open(tree.paths()).unwrap();
    store
        .save_settings(0, settings(br#"{"unknown":"retained"}"#))
        .unwrap();
    let backup = store.create_backup().unwrap();
    assert_eq!(backup.schema_version, 4);
    assert_eq!(backup.settings_revision, 1);
    let backup_path = tree
        .native_root()
        .join(BACKUP_DIRECTORY_NAME)
        .join(format!("{}.sqlite3", backup.id));
    let connection =
        Connection::open_with_flags(backup_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    assert_eq!(
        connection
            .query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    store.shutdown().unwrap();

    let failed = NativeTree::new();
    let (store, _) = StoreHandle::open(failed.paths()).unwrap();
    store
        .save_settings(0, settings(br#"{"value":"before"}"#))
        .unwrap();
    fs::write(
        failed.native_root().join(BACKUP_DIRECTORY_NAME),
        b"not a directory",
    )
    .unwrap();
    assert!(matches!(store.create_backup(), Err(StoreError::Io)));
    store.shutdown().unwrap();
    let (store, reopened) = StoreHandle::open(failed.paths()).unwrap();
    assert_eq!(
        reopened.settings.as_ref().unwrap().json_bytes(),
        br#"{"value":"before"}"#
    );
    store.shutdown().unwrap();
}

#[test]
fn corrupt_active_database_recovers_only_from_a_verified_compatible_backup() {
    let tree = NativeTree::new();
    let (store, _) = StoreHandle::open(tree.paths()).unwrap();
    store
        .save_settings(0, settings(br#"{"recover":"this"}"#))
        .unwrap();
    store.create_backup().unwrap();
    store.shutdown().unwrap();
    fs::write(tree.database(), b"corrupt active database").unwrap();

    let (store, recovered) = StoreHandle::open(tree.paths()).unwrap();
    assert_eq!(
        recovered.settings.as_ref().unwrap().json_bytes(),
        br#"{"recover":"this"}"#
    );
    assert!(
        fs::read_dir(tree.native_root().join(RECOVERY_DIRECTORY_NAME))
            .unwrap()
            .next()
            .is_some()
    );
    store.shutdown().unwrap();

    let no_backup = NativeTree::new();
    let (store, _) = StoreHandle::open(no_backup.paths()).unwrap();
    store.shutdown().unwrap();
    let corrupt = b"corrupt and irreplaceable";
    fs::write(no_backup.database(), corrupt).unwrap();
    assert!(matches!(
        StoreHandle::open(no_backup.paths()),
        Err(StoreError::RecoveryRequired)
    ));
    assert_eq!(fs::read(no_backup.database()).unwrap(), corrupt);
}

#[test]
fn credential_journal_transitions_are_compare_and_set_and_survive_restart() {
    let tree = NativeTree::new();
    let paths = tree.paths();
    let operation = CredentialOperation {
        operation_id: "operation-0123456789abcdef".to_owned(),
        credential_id: "credential-fedcba9876543210".to_owned(),
        operation: CredentialOperationKind::Set,
        state: CredentialOperationState::Prepared,
    };
    let (store, _) = StoreHandle::open(paths.clone()).unwrap();
    let prepared = store
        .prepare_credential_import("credential-run", 0, vec![operation.clone()])
        .unwrap();
    assert_eq!(prepared.pending_credential_imports.len(), 1);
    assert_eq!(
        prepared.credential_operations[0].operation.state,
        CredentialOperationState::Prepared
    );
    store.shutdown().unwrap();

    let (store, restarted) = StoreHandle::open(paths.clone()).unwrap();
    assert_eq!(restarted.pending_credential_imports.len(), 1);
    assert_eq!(
        restarted.credential_operations[0].operation.state,
        CredentialOperationState::Prepared
    );
    let written = store
        .advance_credential_operation(
            &operation.operation_id,
            CredentialOperationState::Prepared,
            CredentialOperationState::SecretWritten,
        )
        .unwrap();
    assert_eq!(
        written.credential_operations[0].operation.state,
        CredentialOperationState::SecretWritten
    );
    assert_eq!(
        store.advance_credential_operation(
            &operation.operation_id,
            CredentialOperationState::Prepared,
            CredentialOperationState::SecretWritten,
        ),
        Err(StoreError::CredentialTransitionConflict)
    );
    store.shutdown().unwrap();

    let (store, restarted) = StoreHandle::open(paths).unwrap();
    assert_eq!(
        restarted.credential_operations[0].operation.state,
        CredentialOperationState::SecretWritten
    );
    let verified = store
        .advance_credential_operation(
            &operation.operation_id,
            CredentialOperationState::SecretWritten,
            CredentialOperationState::Verified,
        )
        .unwrap();
    assert_eq!(
        verified.credential_operations[0].operation.state,
        CredentialOperationState::Verified
    );
    store.shutdown().unwrap();
}

#[test]
fn credential_import_finalization_requires_every_operation_to_be_verified() {
    let tree = NativeTree::new();
    let (store, _) = StoreHandle::open(tree.paths()).unwrap();
    let mut pending = import("credential-finalize");
    pending
        .credential_references
        .push(token_monitor_lite::storage::CredentialReference {
            credential_id: "credential-0123456789abcdef".to_owned(),
            credential_kind: "fixed".to_owned(),
        });
    let operation = CredentialOperation {
        operation_id: "operation-fedcba9876543210".to_owned(),
        credential_id: pending.credential_references[0].credential_id.clone(),
        operation: CredentialOperationKind::Set,
        state: CredentialOperationState::Prepared,
    };
    store
        .prepare_credential_import(&pending.run_id, 0, vec![operation.clone()])
        .unwrap();
    assert_eq!(
        store.adopt_import(pending.clone()),
        Err(StoreError::CredentialsNotVerified)
    );
    store
        .advance_credential_operation(
            &operation.operation_id,
            CredentialOperationState::Prepared,
            CredentialOperationState::SecretWritten,
        )
        .unwrap();
    store
        .advance_credential_operation(
            &operation.operation_id,
            CredentialOperationState::SecretWritten,
            CredentialOperationState::Verified,
        )
        .unwrap();
    let adopted = store.adopt_import(pending).unwrap();
    assert!(adopted.pending_credential_imports.is_empty());
    assert_eq!(adopted.credential_references.len(), 1);
    assert_eq!(
        adopted.credential_operations[0].operation.state,
        CredentialOperationState::MetadataCommitted
    );
    store.shutdown().unwrap();
}
