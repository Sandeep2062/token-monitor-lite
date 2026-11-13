use rusqlite::Connection;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use token_monitor_lite::discovery::{HostPaths, HostPlatform, discover};
use token_monitor_lite::migration::{
    MigrationError, MigrationReason, SEMANTIC_ARCHIVE_FILE_NAME, STAGING_DIRECTORY_NAME,
    prepare_staged_credentials, recover_staged_credentials, stage_and_validate,
};
use token_monitor_lite::storage::{
    ActivationState, CredentialOperation, CredentialOperationKind, CredentialOperationState,
    DATABASE_FILE_NAME, ImportState, PreserveState, StoreError, StoreHandle,
};

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Tree(PathBuf);

impl Tree {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "token-monitor-migration-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn file(&self, relative: &str, bytes: &[u8]) -> PathBuf {
        let path = self.0.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
        path
    }

    fn paths(&self) -> HostPaths {
        HostPaths {
            platform: HostPlatform::Linux,
            home: self.0.clone(),
            app_data: None,
            local_app_data: None,
            xdg_config_home: None,
            xdg_state_home: None,
            legacy_shared_override: Some(self.0.join("legacy-shared")),
            native_data_override: Some(self.0.join("native-private")),
            enabled_clients: BTreeSet::new(),
        }
    }
}

#[test]
fn credential_plan_uses_stable_references_strips_settings_and_recovers_from_staging() {
    let tree = Tree::new();
    tree.file(
        ".config/Token Monitor/settings.json",
        br#"{"clients":"claude","clineApiKey":"SETTINGS_SENTINEL","future":{"kept":true}}"#,
    );
    tree.file(
        ".config/Token Monitor/credentials.json",
        br#"{"version":1,"credentials":{"hub":{"clientSecret":"STAGED_SENTINEL"},"future":{"token":"UNKNOWN_SENTINEL"}}}"#,
    );
    let discovery = discover(&tree.paths()).unwrap();
    let staged = stage_and_validate(discovery.legacy.as_ref().unwrap(), &discovery.native).unwrap();
    let run_id = staged.run_id.clone();
    let prepared = prepare_staged_credentials(staged, &discovery.native).unwrap();
    assert_eq!(prepared.credentials.transfers.len(), 2);
    assert_eq!(prepared.credentials.issues.len(), 1);
    assert_eq!(prepared.import.credential_references.len(), 2);
    assert!(
        prepared
            .import
            .credential_references
            .iter()
            .all(
                |reference| reference.credential_id.starts_with("credential-")
                    && !reference.credential_id.contains("cline")
            )
    );
    let settings: serde_json::Value =
        serde_json::from_slice(prepared.import.settings.as_ref().unwrap().json_bytes()).unwrap();
    assert!(settings.get("clineApiKey").is_none());
    assert_eq!(settings["future"]["kept"], true);
    assert!(!format!("{prepared:?}").contains("SETTINGS_SENTINEL"));
    assert!(!format!("{prepared:?}").contains("STAGED_SENTINEL"));
    let recovery_manifest = fs::read(
        discovery
            .native
            .root()
            .join(STAGING_DIRECTORY_NAME)
            .join(&run_id)
            .join("credential-recovery.json"),
    )
    .unwrap();
    for sentinel in [
        b"SETTINGS_SENTINEL".as_slice(),
        b"STAGED_SENTINEL".as_slice(),
        b"UNKNOWN_SENTINEL".as_slice(),
    ] {
        assert!(
            !recovery_manifest
                .windows(sentinel.len())
                .any(|window| window == sentinel)
        );
    }

    let recovered = recover_staged_credentials(&discovery.native, &run_id).unwrap();
    assert_eq!(recovered.import, prepared.import);
    assert_eq!(recovered.credentials.transfers.len(), 2);
}

impl Drop for Tree {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn staged_path(native: &Path, run_id: &str, staging_id: &str) -> PathBuf {
    native
        .join(STAGING_DIRECTORY_NAME)
        .join(run_id)
        .join(staging_id)
}

#[test]
fn recognized_files_and_plugin_trees_are_copied_privately_without_touching_sources() {
    let tree = Tree::new();
    let settings = tree.file(
        ".config/Token Monitor/settings.json",
        br#"{"clients":null,"future":{"enabled":true}}"#,
    );
    let daily = tree.file(
        "legacy-shared/daily-history-archive.json",
        br#"{"retained":true}"#,
    );
    let plugin = tree.file(
        ".config/Token Monitor/plugins/example/nested/state.json",
        br#"{"plugin":"inert"}"#,
    );
    let before = [
        (settings.clone(), fs::metadata(&settings).unwrap()),
        (daily.clone(), fs::metadata(&daily).unwrap()),
        (plugin.clone(), fs::metadata(&plugin).unwrap()),
    ];

    let discovery = discover(&tree.paths()).unwrap();
    let legacy = discovery.legacy.as_ref().unwrap();
    let import = stage_and_validate(legacy, &discovery.native).unwrap();
    assert_eq!(import.staged_artifacts.len(), 3);
    for artifact in &import.staged_artifacts {
        let staged = staged_path(
            discovery.native.root(),
            &import.run_id,
            &artifact.staging_id,
        );
        assert!(staged.exists());
    }

    let staged_plugin = import
        .staged_artifacts
        .iter()
        .find(|artifact| artifact.artifact_kind == "plugin-package")
        .unwrap();
    assert_eq!(
        fs::read(
            staged_path(
                discovery.native.root(),
                &import.run_id,
                &staged_plugin.staging_id,
            )
            .join("nested/state.json")
        )
        .unwrap(),
        br#"{"plugin":"inert"}"#
    );

    for (path, metadata) in before {
        let after = fs::metadata(&path).unwrap();
        assert_eq!(after.len(), metadata.len());
        assert_eq!(after.modified().unwrap(), metadata.modified().unwrap());
        assert!(!after.permissions().readonly());
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let run = discovery
            .native
            .root()
            .join(STAGING_DIRECTORY_NAME)
            .join(&import.run_id);
        assert_eq!(
            fs::metadata(run).unwrap().permissions().mode() & 0o777,
            0o700
        );
        for artifact in &import.staged_artifacts {
            let staged = staged_path(
                discovery.native.root(),
                &import.run_id,
                &artifact.staging_id,
            );
            if staged.is_file() {
                assert_eq!(
                    fs::metadata(staged).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
        }
    }
}

#[test]
fn sqlite_bundle_is_preserved_inert_and_semantic_backup_includes_wal_commit() {
    let tree = Tree::new();
    let database = tree.0.join("legacy-shared/session-usage-archive.sqlite");
    fs::create_dir_all(database.parent().unwrap()).unwrap();
    let connection = Connection::open(&database).unwrap();
    connection
        .pragma_update(None, "journal_mode", "WAL")
        .unwrap();
    connection
        .pragma_update(None, "wal_autocheckpoint", 0)
        .unwrap();
    connection
        .execute_batch(
            "CREATE TABLE metadata(key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE sessions(
                 session_key TEXT PRIMARY KEY,
                 entry_json TEXT NOT NULL,
                 revision INTEGER NOT NULL
             );
             INSERT INTO metadata(key, value) VALUES
                 ('schema-version', '1'), ('legacy-migrated', '1'), ('revision', '1');",
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO sessions(session_key, entry_json, revision) VALUES (?1, ?2, 1)",
            (
                "claude:wal-session",
                r#"{"client":"claude","sessionId":"wal-session","periods":{"today":{"totalTokens":1}}}"#,
            ),
        )
        .unwrap();
    assert!(database.with_extension("sqlite-wal").exists());

    let discovery = discover(&tree.paths()).unwrap();
    let import = stage_and_validate(discovery.legacy.as_ref().unwrap(), &discovery.native).unwrap();
    let main = import
        .staged_artifacts
        .iter()
        .find(|artifact| artifact.artifact_kind == "session-usage-archive")
        .unwrap();
    let wal = import
        .staged_artifacts
        .iter()
        .find(|artifact| artifact.artifact_kind == "session-usage-archive-wal")
        .unwrap();
    assert_eq!(
        Path::new(&main.staging_id).parent(),
        Path::new(&wal.staging_id).parent()
    );
    let semantic = staged_path(discovery.native.root(), &import.run_id, &main.staging_id)
        .parent()
        .unwrap()
        .join(SEMANTIC_ARCHIVE_FILE_NAME);
    let coherent =
        Connection::open_with_flags(semantic, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    assert_eq!(
        coherent
            .query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    assert_eq!(
        coherent
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    drop(connection);
}

#[cfg(unix)]
#[test]
fn plugin_links_are_never_followed_into_private_staging() {
    use std::os::unix::fs::symlink;

    let tree = Tree::new();
    let outside = tree.file("outside/secret.txt", b"must not be copied");
    let plugin = tree.0.join(".config/Token Monitor/plugins/example");
    fs::create_dir_all(&plugin).unwrap();
    symlink(outside, plugin.join("linked.txt")).unwrap();

    let discovery = discover(&tree.paths()).unwrap();
    let legacy = discovery.legacy.as_ref().unwrap();
    assert!(!legacy.artifacts[0].eligible_for_staging);
    let import = stage_and_validate(legacy, &discovery.native).unwrap();
    assert!(import.staged_artifacts.is_empty());
    assert!(import.sessions.is_empty());
}

#[test]
fn settings_import_is_byte_exact_and_invalid_or_credential_bearing_documents_stay_inactive() {
    let valid = Tree::new();
    let valid_bytes = br#"{ "clients" : null, "future" : {"enabled":true}, "sessionUsageArchive":{"unknown":null} }"#;
    valid.file(".config/Token Monitor/settings.json", valid_bytes);
    let discovery = discover(&valid.paths()).unwrap();
    let imported =
        stage_and_validate(discovery.legacy.as_ref().unwrap(), &discovery.native).unwrap();
    assert_eq!(imported.settings.unwrap().json_bytes(), valid_bytes);
    let item = &imported.report_items[0];
    assert_eq!(item.preservation, PreserveState::Preserved);
    assert_eq!(item.import, ImportState::Imported);
    assert_eq!(item.activation, ActivationState::Activated);

    let invalid = Tree::new();
    invalid.file(
        ".config/Token Monitor/settings.json",
        br#"{"clients":["claude"]}"#,
    );
    let discovery = discover(&invalid.paths()).unwrap();
    let rejected =
        stage_and_validate(discovery.legacy.as_ref().unwrap(), &discovery.native).unwrap();
    assert!(rejected.settings.is_none());
    assert_eq!(
        rejected.report_items[0].preservation,
        PreserveState::Preserved
    );
    assert_eq!(rejected.report_items[0].import, ImportState::Failed);
    assert_eq!(
        rejected.report_items[0].activation,
        ActivationState::Inactive
    );

    let pending = Tree::new();
    pending.file(
        ".config/Token Monitor/settings.json",
        br#"{"clients":"claude","claudeWebCookie":"MIGRATION_SECRET_SENTINEL"}"#,
    );
    let discovery = discover(&pending.paths()).unwrap();
    let validated =
        stage_and_validate(discovery.legacy.as_ref().unwrap(), &discovery.native).unwrap();
    assert!(validated.settings.is_none());
    assert_eq!(validated.report_items[0].import, ImportState::NotAttempted);
    assert_eq!(
        validated.report_items[0].activation,
        ActivationState::Inactive
    );
    assert_eq!(validated.credential_references.len(), 1);
    assert_eq!(
        validated.credential_references[0].credential_kind,
        "claudeWebCookie"
    );
    assert!(!format!("{validated:?}").contains("MIGRATION_SECRET_SENTINEL"));

    let unknown = Tree::new();
    unknown.file(
        ".config/Token Monitor/settings.json",
        br#"{"clients":"claude","futureProvider":{"privateAuthToken":"UNKNOWN_SECRET_VALUE"}}"#,
    );
    let discovery = discover(&unknown.paths()).unwrap();
    let retained =
        stage_and_validate(discovery.legacy.as_ref().unwrap(), &discovery.native).unwrap();
    assert!(retained.settings.is_none());
    assert!(retained.credential_references.is_empty());
    assert_eq!(
        retained.report_items[0].preservation,
        PreserveState::Preserved
    );
    assert_eq!(retained.report_items[0].import, ImportState::NotAttempted);
    assert_eq!(
        retained.report_items[0].activation,
        ActivationState::Inactive
    );
}

fn archive_fixture(tree: &Tree, metadata: &[(&str, &str)], rows: &[(&str, &[u8], i64)]) {
    let database = tree.0.join("legacy-shared/session-usage-archive.sqlite");
    fs::create_dir_all(database.parent().unwrap()).unwrap();
    let connection = Connection::open(database).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE metadata(key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE sessions(
                 session_key TEXT PRIMARY KEY,
                 entry_json TEXT NOT NULL,
                 revision INTEGER NOT NULL
             );",
        )
        .unwrap();
    for (key, value) in metadata {
        connection
            .execute(
                "INSERT INTO metadata(key, value) VALUES (?1, ?2)",
                (key, value),
            )
            .unwrap();
    }
    for (key, json, revision) in rows {
        connection
            .execute(
                "INSERT INTO sessions(session_key, entry_json, revision) VALUES (?1, ?2, ?3)",
                (key, json, revision),
            )
            .unwrap();
    }
}

#[test]
fn schema_v1_archive_imports_canonical_identity_and_retains_original_row_bytes() {
    let tree = Tree::new();
    let original = br#"{ "client" : "claude", "sessionId" : "session-one", "periods" : {"allTime":{"totalTokens":9}}, "future":null }"#;
    archive_fixture(
        &tree,
        &[
            ("schema-version", "1"),
            ("legacy-migrated", "1"),
            ("revision", "7"),
        ],
        &[("legacy-key-one", original, 7)],
    );
    let discovery = discover(&tree.paths()).unwrap();
    let validated =
        stage_and_validate(discovery.legacy.as_ref().unwrap(), &discovery.native).unwrap();
    assert_eq!(validated.sessions.len(), 1);
    assert_eq!(validated.sessions[0].client, "claude");
    assert_eq!(validated.sessions[0].session, "session-one");
    assert_eq!(validated.sessions[0].legacy_key, "legacy-key-one");
    assert_eq!(validated.sessions[0].source_revision, 7);
    assert_eq!(validated.sessions[0].original_entry_json, original);
    let report = validated
        .report_items
        .iter()
        .find(|item| item.artifact_kind == "session-usage-archive")
        .unwrap();
    assert_eq!(report.import, ImportState::Imported);
    assert_eq!(report.activation, ActivationState::Activated);
    assert_eq!(report.records, 1);
}

#[test]
fn archive_validation_rejects_unsupported_metadata_bad_rows_and_duplicate_identity() {
    type ArchiveMetadata<'a> = &'a [(&'a str, &'a str)];
    type ArchiveRows<'a> = Vec<(&'a str, &'a [u8], i64)>;
    type ArchiveCase<'a> = (ArchiveMetadata<'a>, ArchiveRows<'a>, MigrationReason);

    let cases: Vec<ArchiveCase<'_>> = vec![
        (
            &[
                ("schema-version", "2"),
                ("legacy-migrated", "1"),
                ("revision", "1"),
            ],
            vec![],
            MigrationReason::UnsupportedArchiveVersion,
        ),
        (
            &[
                ("schema-version", "1"),
                ("legacy-migrated", "1"),
                ("revision", "1"),
            ],
            vec![("bad-json", br#"{"client":"claude"# as &[u8], 1)],
            MigrationReason::MalformedArchiveRow,
        ),
        (
            &[
                ("schema-version", "1"),
                ("legacy-migrated", "1"),
                ("revision", "1"),
            ],
            vec![("no-session", br#"{"client":"claude"}"#, 1)],
            MigrationReason::UnresolvedArchiveIdentity,
        ),
        (
            &[
                ("schema-version", "1"),
                ("legacy-migrated", "1"),
                ("revision", "1"),
            ],
            vec![
                ("one", br#"{"client":"claude","sessionId":"same"}"#, 1),
                ("two", br#"{"client":"claude","sessionId":"same"}"#, 1),
            ],
            MigrationReason::ConflictingArchiveIdentity,
        ),
        (
            &[
                ("schema-version", "1"),
                ("legacy-migrated", "1"),
                ("revision", "1"),
            ],
            vec![(
                "future-row",
                br#"{"client":"claude","sessionId":"future"}"#,
                2,
            )],
            MigrationReason::ArchiveRevisionMismatch,
        ),
    ];
    for (metadata, rows, reason) in cases {
        let tree = Tree::new();
        archive_fixture(&tree, metadata, &rows);
        let discovery = discover(&tree.paths()).unwrap();
        assert_eq!(
            stage_and_validate(discovery.legacy.as_ref().unwrap(), &discovery.native),
            Err(MigrationError::Validation {
                kind: token_monitor_lite::discovery::ArtifactKind::SessionUsageArchive,
                reason,
            })
        );
    }
}

#[test]
fn unsupported_stores_are_preserved_and_reported_without_semantic_reads() {
    let tree = Tree::new();
    tree.file(
        "legacy-shared/codex-local-usage.sqlite",
        b"not a sqlite database",
    );
    tree.file(
        "legacy-shared/session-usage-archive.json",
        br#"{"sessions":{"future":true}}"#,
    );
    tree.file(
        ".config/Token Monitor/plugins/example/index.js",
        b"throw new Error('must not run')",
    );
    let discovery = discover(&tree.paths()).unwrap();
    let validated =
        stage_and_validate(discovery.legacy.as_ref().unwrap(), &discovery.native).unwrap();
    assert!(validated.sessions.is_empty());
    assert_eq!(validated.staged_artifacts.len(), 3);
    assert!(validated.report_items.iter().all(|item| {
        item.preservation == PreserveState::Preserved
            && item.import == ImportState::Unsupported
            && item.activation == ActivationState::Inactive
    }));
}

#[test]
fn mixed_adoption_is_atomic_and_a_failed_attempt_can_retry_without_losing_staging() {
    let tree = Tree::new();
    tree.file(
        ".config/Token Monitor/settings.json",
        br#"{"clients":"claude","future":null}"#,
    );
    tree.file(
        "legacy-shared/daily-history-archive.json",
        br#"{"unsupported":true}"#,
    );
    archive_fixture(
        &tree,
        &[
            ("schema-version", "1"),
            ("legacy-migrated", "1"),
            ("revision", "3"),
        ],
        &[(
            "legacy-session",
            br#"{"client":"claude","sessionId":"atomic-session","periods":{}}"#,
            3,
        )],
    );
    let discovery = discover(&tree.paths()).unwrap();
    let first = stage_and_validate(discovery.legacy.as_ref().unwrap(), &discovery.native).unwrap();
    let first_staging = discovery
        .native
        .root()
        .join(STAGING_DIRECTORY_NAME)
        .join(&first.run_id);
    assert!(first_staging.exists());

    let (store, before) = StoreHandle::open(discovery.native.clone()).unwrap();
    let mut injected_failure = first;
    injected_failure.sessions[0].client = "invalid/client".to_owned();
    assert_eq!(
        store.adopt_import(injected_failure),
        Err(StoreError::InvalidImport)
    );
    let after_failure = store.snapshot().unwrap();
    assert_eq!(after_failure.as_ref(), &before);
    assert!(first_staging.exists());

    let retry = stage_and_validate(discovery.legacy.as_ref().unwrap(), &discovery.native).unwrap();
    let retry_staging = discovery
        .native
        .root()
        .join(STAGING_DIRECTORY_NAME)
        .join(&retry.run_id);
    let adopted = store.adopt_import(retry).unwrap();
    assert_eq!(adopted.import_runs.len(), 1);
    assert_eq!(adopted.sessions.len(), 1);
    assert_eq!(adopted.sessions[0].session, "atomic-session");
    assert_eq!(adopted.settings_revision, 1);
    assert_eq!(
        adopted
            .report_items
            .iter()
            .filter(|row| row.item.artifact_kind == "daily-history-archive")
            .count(),
        1
    );
    assert!(adopted.report_items.iter().any(|row| {
        row.item.artifact_kind == "daily-history-archive"
            && row.item.preservation == PreserveState::Preserved
            && row.item.import == ImportState::Unsupported
            && row.item.activation == ActivationState::Inactive
    }));
    assert!(first_staging.exists());
    assert!(retry_staging.exists());
    store.shutdown().unwrap();
}

#[test]
fn pending_credentials_can_record_safe_metadata_without_writing_secret_settings() {
    let tree = Tree::new();
    let sentinel = "PENDING_CREDENTIAL_VALUE_94d1";
    tree.file(
        ".config/Token Monitor/settings.json",
        format!(r#"{{"clients":"claude","claudeWebCookie":"{sentinel}"}}"#).as_bytes(),
    );
    let discovery = discover(&tree.paths()).unwrap();
    let staged = stage_and_validate(discovery.legacy.as_ref().unwrap(), &discovery.native).unwrap();
    let pending = prepare_staged_credentials(staged, &discovery.native).unwrap();
    let operations = pending
        .import
        .credential_references
        .iter()
        .map(|reference| CredentialOperation {
            operation_id: token_monitor_lite::credentials::stable_operation_id(
                &pending.import.run_id,
                &reference.credential_id,
            ),
            credential_id: reference.credential_id.clone(),
            operation: CredentialOperationKind::Set,
            state: CredentialOperationState::Prepared,
        })
        .collect();
    let (store, _) = StoreHandle::open(discovery.native.clone()).unwrap();
    let prepared = store
        .prepare_credential_import(
            &pending.import.run_id,
            pending.import.expected_settings_revision,
            operations,
        )
        .unwrap();
    assert!(prepared.settings.is_none());
    assert!(prepared.credential_references.is_empty());
    assert_eq!(prepared.pending_credential_imports.len(), 1);
    assert_eq!(prepared.credential_operations.len(), 1);
    store.shutdown().unwrap();

    let database = fs::read(discovery.native.root().join(DATABASE_FILE_NAME)).unwrap();
    assert!(
        !database
            .windows(sentinel.len())
            .any(|bytes| bytes == sentinel.as_bytes())
    );
}
