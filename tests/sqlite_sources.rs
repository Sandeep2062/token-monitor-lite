use chrono::DateTime;
use rusqlite::{Connection, params};
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::SystemTime;
use token_monitor_lite::source_inputs::{
    ArchivePeriod, CodexLocalUsageAdapter, MAX_ARCHIVE_ENTRY_BYTES, SessionArchiveAdapter,
    SqliteSourceError, SqliteSourceRole, classify_sqlite_source,
};
use token_monitor_lite::sources::{SourceFormat, SourceProgress, SourceReader};

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Root(PathBuf);

impl Root {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "token-monitor-sqlite-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Debug, PartialEq, Eq)]
struct FileState {
    exists: bool,
    bytes: u64,
    modified: Option<SystemTime>,
    mode: u32,
    hash: u128,
}

fn fingerprint(path: &Path) -> BTreeMap<&'static str, FileState> {
    let mut states = BTreeMap::new();
    for suffix in ["", "-wal", "-shm"] {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        let path = PathBuf::from(name);
        let state = match fs::symlink_metadata(&path) {
            Ok(metadata) => FileState {
                exists: true,
                bytes: metadata.len(),
                modified: metadata.modified().ok(),
                mode: {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt as _;
                        metadata.permissions().mode()
                    }
                    #[cfg(not(unix))]
                    {
                        0
                    }
                },
                hash: fs::read(&path)
                    .map(|bytes| {
                        let mut hash = 0xcbf29ce484222325_u128;
                        for byte in bytes {
                            hash ^= u128::from(byte);
                            hash = hash.wrapping_mul(0x100000001b3);
                        }
                        hash
                    })
                    .unwrap_or(0),
            },
            Err(_) => FileState {
                exists: false,
                bytes: 0,
                modified: None,
                mode: 0,
                hash: 0,
            },
        };
        states.insert(suffix, state);
    }
    states
}

fn archive_entry(client: &str, session: &str, captured_at: &str) -> Vec<u8> {
    let period = |total: u64, input: u64, cached: u64, output: u64, reasoning: u64| {
        json!({
            "totalTokens": total,
            "inputTokens": input,
            "cacheReadTokens": cached,
            "cacheWriteTokens": 0,
            "outputTokens": output,
            "reasoningTokens": reasoning,
            "models": { "gpt-5": total },
            "modelTokenComponents": {
                "gpt-5": {
                    "totalTokens": total,
                    "inputTokens": input,
                    "cacheReadTokens": cached,
                    "cacheWriteTokens": 0,
                    "outputTokens": output
                }
            }
        })
    };
    serde_json::to_vec(&json!({
        "client": client,
        "sessionId": session,
        "capturedAt": captured_at,
        "day": "2026-11-12",
        "month": "2026-11",
        "periodWindows": {
            "today": { "capturedAt": captured_at, "day": "2026-11-12" },
            "month": { "capturedAt": captured_at, "month": "2026-11" },
            "allTime": { "capturedAt": captured_at }
        },
        "periods": {
            "today": period(10, 6, 2, 2, 1),
            "month": period(30, 20, 5, 5, 2),
            "allTime": period(100, 70, 15, 15, 5)
        }
    }))
    .unwrap()
}

fn create_archive(root: &Root, name: &str) -> (PathBuf, Connection) {
    let path = root.path(name);
    let connection = Connection::open(&path).unwrap();
    connection
        .pragma_update(None, "journal_mode", "WAL")
        .unwrap();
    connection
        .pragma_update(None, "wal_autocheckpoint", 0)
        .unwrap();
    connection
        .execute_batch(
            "CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE sessions (
                session_key TEXT PRIMARY KEY,
                entry_json TEXT NOT NULL,
                revision INTEGER NOT NULL
             );
             CREATE INDEX sessions_revision_idx ON sessions(revision, session_key);",
        )
        .unwrap();
    for (key, value) in [
        ("schema-version", "1"),
        ("legacy-migrated", "1"),
        ("revision", "2"),
        ("pruned-day", "2026-11-01"),
        ("pruned-month", "2026-11"),
        ("future-optional", "ignored"),
    ] {
        connection
            .execute("INSERT INTO metadata VALUES (?1, ?2)", [key, value])
            .unwrap();
    }
    connection
        .execute(
            "INSERT INTO sessions VALUES (?1, ?2, 2)",
            params![
                "claude:session-a",
                archive_entry("claude", "session-a", "2026-11-12T10:00:00Z")
            ],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO sessions VALUES (?1, ?2, 1)",
            params![
                "codex:thread-b",
                archive_entry("codex", "thread-b", "2026-11-12T11:00:00Z")
            ],
        )
        .unwrap();
    (path, connection)
}

fn create_local_usage(root: &Root, name: &str) -> (PathBuf, Connection) {
    let path = root.path(name);
    let connection = Connection::open(&path).unwrap();
    connection
        .pragma_update(None, "journal_mode", "WAL")
        .unwrap();
    connection
        .pragma_update(None, "wal_autocheckpoint", 0)
        .unwrap();
    connection
        .execute_batch(
            "CREATE TABLE threads (
                account_key TEXT NOT NULL, thread_id TEXT NOT NULL, metadata TEXT NOT NULL,
                checkpoint TEXT, PRIMARY KEY (account_key, thread_id)
             );
             CREATE TABLE requests (
                account_key TEXT NOT NULL, thread_id TEXT NOT NULL, checkpoint_total INTEGER NOT NULL,
                observed_at TEXT NOT NULL, turn_id TEXT NOT NULL, model TEXT NOT NULL,
                cwd TEXT NOT NULL, usage TEXT NOT NULL,
                PRIMARY KEY (account_key, thread_id, checkpoint_total)
             );
             CREATE INDEX requests_thread ON requests(thread_id, observed_at);
             CREATE TABLE observer (
                singleton INTEGER PRIMARY KEY CHECK(singleton = 1), owner TEXT NOT NULL,
                pid INTEGER NOT NULL, expires_at INTEGER NOT NULL
             );
             CREATE TABLE agent_observer_intent (
                singleton INTEGER PRIMARY KEY CHECK(singleton = 1), pid INTEGER NOT NULL,
                expires_at INTEGER NOT NULL
             );",
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO threads VALUES (?1, ?2, ?3, NULL)",
            params!["account-sentinel", "thread-a", r#"{"title":"ignored"}"#],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO requests VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                "account-sentinel",
                "thread-a",
                100_i64,
                "2026-11-12T10:00:00Z",
                "turn-1",
                "gpt-5",
                "/private/project",
                r#"{"input":6,"cacheRead":2,"cacheWrite":0,"output":2,"reasoning":1,"total":10}"#
            ],
        )
        .unwrap();
    (path, connection)
}

#[test]
fn source_reader_publishes_archive_retained_rows_and_local_usage_requests() {
    let root = Root::new();
    let (archive_path, _archive_writer) = create_archive(&root, "reader-archive.sqlite");
    let mut archive = SourceReader::new(
        archive_path.clone(),
        SourceFormat::SessionArchiveSqlite,
        "archive",
    );
    let archive_step = archive.step().unwrap();
    assert_eq!(
        archive_step.progress,
        SourceProgress::Published {
            delta: Default::default()
        }
    );
    let archive_snapshot = archive.last_good().unwrap();
    assert!(archive_snapshot.rows.is_empty());
    assert_eq!(archive_snapshot.retained.len(), 6);
    assert_eq!(archive_snapshot.retained[0].client, "claude");

    let (local_path, _local_writer) = create_local_usage(&root, "reader-local.sqlite");
    let mut local = SourceReader::new(
        local_path,
        SourceFormat::CodexLocalUsageSqlite,
        "codex-local",
    );
    let local_step = local.step().unwrap();
    assert!(matches!(
        local_step.progress,
        SourceProgress::Published { .. }
    ));
    let local_snapshot = local.last_good().unwrap();
    assert_eq!(local_snapshot.rows.len(), 1);
    assert!(local_snapshot.retained.is_empty());
}

#[test]
fn sqlite_roles_are_classified_only_by_exact_main_names() {
    assert_eq!(
        classify_sqlite_source(Path::new("/data/session-usage-archive.sqlite")),
        Some(SqliteSourceRole::SessionArchive)
    );
    assert_eq!(
        classify_sqlite_source(Path::new("/data/codex-local-usage.sqlite")),
        Some(SqliteSourceRole::CodexLocalUsage)
    );
    assert_eq!(
        classify_sqlite_source(Path::new("/data/session-usage-archive.sqlite-wal")),
        None
    );
    assert_eq!(
        classify_sqlite_source(Path::new("/data/session-usage-archive.sqlite-shm")),
        None
    );
}

#[test]
fn session_archive_reads_wal_only_multi_session_data_without_mutating_files() {
    let root = Root::new();
    let (path, _writer) = create_archive(&root, "session-usage-archive.sqlite");
    let before = fingerprint(&path);
    assert!(before["-wal"].exists);
    assert!(before["-shm"].exists);

    let snapshot = SessionArchiveAdapter::new(&path).read().unwrap();
    assert_eq!(snapshot.revision, 2);
    assert_eq!(snapshot.pruned_day.as_deref(), Some("2026-11-01"));
    assert_eq!(snapshot.pruned_month.as_deref(), Some("2026-11"));
    assert_eq!(snapshot.sessions.len(), 2);
    assert_eq!(snapshot.sessions[0].client, "claude");
    assert_eq!(snapshot.sessions[0].session, "session-a");
    assert_eq!(snapshot.sessions[0].periods.len(), 3);
    let today = &snapshot.sessions[0].periods[0];
    assert_eq!(today.period, ArchivePeriod::Today);
    assert_eq!(today.total, 10);
    assert_eq!(today.input, 6);
    assert_eq!(today.cache_read, 2);
    assert_eq!(today.output, 2);
    assert_eq!(today.reasoning, 1);
    assert_eq!(today.unclassified, 0);
    assert!(today.components_complete);
    assert!(today.models_complete);
    assert_eq!(today.models.len(), 1);
    assert_eq!(today.models[0].model, "gpt-5");
    assert_eq!(today.models[0].total, 10);
    assert_eq!(fingerprint(&path), before);
}

#[test]
fn session_archive_rejects_bad_schema_revision_and_conflicting_identity() {
    let root = Root::new();
    let (path, writer) = create_archive(&root, "bad-archive.sqlite");
    writer
        .execute(
            "UPDATE metadata SET value = '2' WHERE key = 'schema-version'",
            [],
        )
        .unwrap();
    assert_eq!(
        SessionArchiveAdapter::new(&path).read().unwrap_err(),
        SqliteSourceError::Schema
    );

    writer
        .execute(
            "UPDATE metadata SET value = '1' WHERE key = 'schema-version'",
            [],
        )
        .unwrap();
    writer
        .execute(
            "INSERT INTO sessions VALUES ('codex:alias', ?1, 2)",
            [archive_entry("codex", "thread-b", "2026-11-12T12:00:00Z")],
        )
        .unwrap();
    assert_eq!(
        SessionArchiveAdapter::new(&path).read().unwrap_err(),
        SqliteSourceError::Conflict
    );

    writer
        .execute("DELETE FROM sessions WHERE session_key = 'codex:alias'", [])
        .unwrap();
    writer
        .execute(
            "INSERT INTO sessions VALUES ('codex:future', ?1, 3)",
            [archive_entry(
                "codex",
                "thread-future",
                "2026-11-12T12:00:00Z",
            )],
        )
        .unwrap();
    assert_eq!(
        SessionArchiveAdapter::new(&path).read().unwrap_err(),
        SqliteSourceError::Changed
    );
}

#[test]
fn session_archive_rejects_malformed_entries_and_missing_metadata_without_changes() {
    let root = Root::new();
    let (path, writer) = create_archive(&root, "malformed-archive.sqlite");
    writer
        .execute(
            "UPDATE sessions SET entry_json = ?1 WHERE session_key = 'claude:session-a'",
            [b"not json"],
        )
        .unwrap();
    let before = fingerprint(&path);
    assert_eq!(
        SessionArchiveAdapter::new(&path).read().unwrap_err(),
        SqliteSourceError::Invalid
    );
    assert_eq!(fingerprint(&path), before);

    writer
        .execute("DELETE FROM metadata WHERE key = 'legacy-migrated'", [])
        .unwrap();
    assert_eq!(
        SessionArchiveAdapter::new(&path).read().unwrap_err(),
        SqliteSourceError::Schema
    );
}

#[test]
fn codex_local_usage_normalizes_requests_into_path_and_account_free_rows() {
    let root = Root::new();
    let (path, _writer) = create_local_usage(&root, "codex-local-usage.sqlite");
    let before = fingerprint(&path);
    assert!(before["-wal"].exists);
    assert!(before["-shm"].exists);

    let rows = CodexLocalUsageAdapter::new(&path).read().unwrap();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.key.client, "codex");
    assert_eq!(row.key.session, "thread-a");
    assert_eq!(row.model, "gpt-5");
    assert_eq!(
        row.tokens,
        token_monitor_lite::accounting::TokenCounts {
            input: 6,
            output: 2,
            cache_read: 2,
            cache_write: 0,
            reasoning: 1,
        }
    );
    assert_eq!(
        row.timestamp_ms,
        DateTime::parse_from_rfc3339("2026-11-12T10:00:00Z")
            .unwrap()
            .timestamp_millis()
    );
    assert!(
        row.dedup_key
            .as_deref()
            .unwrap()
            .starts_with("codex-local:")
    );
    assert!(!format!("{row:?}").contains("account-sentinel"));
    assert!(!format!("{row:?}").contains("/private/project"));
    assert_eq!(fingerprint(&path), before);
}

#[test]
fn codex_local_usage_partitions_accounts_and_skips_native_backed_rows() {
    let root = Root::new();
    let (path, writer) = create_local_usage(&root, "codex-local-usage-partition.sqlite");
    writer
        .execute(
            "INSERT INTO threads VALUES (?1, ?2, ?3, NULL)",
            params!["second-account", "thread-a", r#"{"title":"ignored"}"#],
        )
        .unwrap();
    writer
        .execute(
            "INSERT INTO requests VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                "second-account",
                "thread-a",
                100_i64,
                "2026-11-12T10:00:00Z",
                "turn-1",
                "gpt-5",
                "/private/second",
                r#"{"input":6,"cacheRead":2,"cacheWrite":0,"output":2,"reasoning":1,"total":10}"#
            ],
        )
        .unwrap();
    let rows = CodexLocalUsageAdapter::new(&path).read().unwrap();
    assert_eq!(rows.len(), 2);
    assert_ne!(rows[0].dedup_key, rows[1].dedup_key);
    assert!(!format!("{rows:?}").contains("second-account"));
    assert!(!format!("{rows:?}").contains("account-sentinel"));

    writer
        .execute(
            "UPDATE threads SET metadata = ?1 WHERE account_key = 'second-account'",
            [r#"{"nativeBacked":true}"#],
        )
        .unwrap();
    let rows = CodexLocalUsageAdapter::new(&path).read().unwrap();
    assert_eq!(rows.len(), 1);
}

#[test]
fn codex_local_usage_rejects_malformed_usage_and_missing_schema() {
    let root = Root::new();
    let (path, writer) = create_local_usage(&root, "bad-local-usage.sqlite");
    writer
        .execute(
            "UPDATE requests SET usage = ?1 WHERE thread_id = 'thread-a'",
            [r#"{"input":6,"cacheRead":2,"cacheWrite":0,"output":2,"reasoning":9,"total":10}"#],
        )
        .unwrap();
    assert_eq!(
        CodexLocalUsageAdapter::new(&path).read().unwrap_err(),
        SqliteSourceError::Invalid
    );

    writer.execute("DROP INDEX requests_thread", []).unwrap();
    let before = fingerprint(&path);
    assert_eq!(
        CodexLocalUsageAdapter::new(&path).read().unwrap_err(),
        SqliteSourceError::Schema
    );
    assert_eq!(fingerprint(&path), before);
}

#[test]
fn sqlite_snapshot_does_not_create_missing_sidecars() {
    let root = Root::new();
    let (path, writer) = create_local_usage(&root, "plain-local-usage.sqlite");
    drop(writer);
    for suffix in ["-wal", "-shm"] {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        let _ = fs::remove_file(PathBuf::from(name));
    }
    let before = fingerprint(&path);
    assert!(!before["-wal"].exists);
    assert!(!before["-shm"].exists);
    let rows = CodexLocalUsageAdapter::new(&path).read().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(fingerprint(&path), before);
}

#[test]
fn codex_local_usage_keeps_distinct_checkpoint_corrections() {
    let root = Root::new();
    let (path, writer) = create_local_usage(&root, "corrections.sqlite");
    writer
        .execute(
            "INSERT INTO requests VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                "account-sentinel",
                "thread-a",
                120_i64,
                "2026-11-12T10:01:00Z",
                "turn-2",
                "gpt-5",
                "/private/correction",
                r#"{"input":2,"cacheRead":0,"cacheWrite":0,"output":3,"reasoning":0,"total":5}"#
            ],
        )
        .unwrap();
    let rows = CodexLocalUsageAdapter::new(&path).read().unwrap();
    assert_eq!(rows.len(), 2);
    assert_ne!(rows[0].dedup_key, rows[1].dedup_key);
    assert_eq!(rows[0].tokens.input + rows[1].tokens.input, 8);
    assert_eq!(rows[1].tokens.output, 3);
}

#[test]
fn codex_local_usage_rejects_excess_rows_before_paging() {
    let root = Root::new();
    let (path, writer) = create_local_usage(&root, "large-local-usage.sqlite");
    {
        let transaction = writer.unchecked_transaction().unwrap();
        {
            let mut statement = transaction
                .prepare(
                    "INSERT INTO requests(account_key, thread_id, checkpoint_total, observed_at,
                     turn_id, model, cwd, usage) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                )
                .unwrap();
            for index in 0..16_385_u64 {
                statement
                    .execute(params![
                        "account-sentinel",
                        "thread-a",
                        1_000_i64 + index as i64,
                        "2026-11-12T10:00:00Z",
                        format!("turn-{index}"),
                        "gpt-5",
                        "/private/large",
                        r#"{"input":1,"cacheRead":0,"cacheWrite":0,"output":1,"reasoning":0,"total":2}"#
                    ])
                    .unwrap();
            }
        }
        transaction.commit().unwrap();
    }
    assert_eq!(
        CodexLocalUsageAdapter::new(&path).read().unwrap_err(),
        SqliteSourceError::Capacity
    );
}

#[test]
fn sqlite_wal_without_shared_memory_is_reported_unavailable() {
    let root = Root::new();
    let (path, _writer) = create_local_usage(&root, "missing-shm.sqlite");
    let suffixed = |path: &std::path::Path, suffix: &str| {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        PathBuf::from(name)
    };
    let shm = suffixed(&path, "-shm");
    assert!(shm.exists());
    #[cfg(unix)]
    fs::remove_file(&shm).unwrap();
    #[cfg(not(unix))]
    let path = {
        // Windows keeps the live SHM locked, so copy the main/WAL bundle
        // without the SHM to reproduce the missing-shared-memory state.
        let copy = root.path("missing-shm-copy.sqlite");
        fs::copy(&path, &copy).unwrap();
        fs::copy(suffixed(&path, "-wal"), suffixed(&copy, "-wal")).unwrap();
        copy
    };
    assert_eq!(
        CodexLocalUsageAdapter::new(&path).read().unwrap_err(),
        SqliteSourceError::Unavailable
    );
}

#[test]
fn session_archive_rejects_excess_entry_bytes() {
    let root = Root::new();
    let (path, writer) = create_archive(&root, "huge-entry.sqlite");
    writer
        .execute(
            "INSERT INTO sessions VALUES ('codex:huge', ?1, 1)",
            [vec![b'x'; MAX_ARCHIVE_ENTRY_BYTES + 1]],
        )
        .unwrap();
    assert_eq!(
        SessionArchiveAdapter::new(&path).read().unwrap_err(),
        SqliteSourceError::Capacity
    );
}

#[test]
fn session_archive_rejects_excess_rows_before_parsing() {
    let root = Root::new();
    let (path, writer) = create_archive(&root, "large-archive.sqlite");
    {
        let transaction = writer.unchecked_transaction().unwrap();
        {
            let mut statement = transaction
                .prepare("INSERT INTO sessions VALUES (?1, ?2, 1)")
                .unwrap();
            for index in 0..10_001_u64 {
                statement
                    .execute(params![
                        format!("codex:extra-{index}"),
                        archive_entry("codex", &format!("extra-{index}"), "2026-11-12T12:00:00Z")
                    ])
                    .unwrap();
            }
        }
        transaction.commit().unwrap();
    }
    assert_eq!(
        SessionArchiveAdapter::new(&path).read().unwrap_err(),
        SqliteSourceError::Capacity
    );
}

#[test]
fn codex_local_usage_tolerates_future_columns_and_pages_past_one_batch() {
    let root = Root::new();
    let (path, writer) = create_local_usage(&root, "paged-local-usage.sqlite");
    writer
        .execute("ALTER TABLE requests ADD COLUMN future_column TEXT", [])
        .unwrap();
    {
        let transaction = writer.unchecked_transaction().unwrap();
        {
            let mut statement = transaction
                .prepare(
                    "INSERT INTO requests(account_key, thread_id, checkpoint_total, observed_at,
                     turn_id, model, cwd, usage) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                )
                .unwrap();
            for index in 0..1_100_u64 {
                statement
                    .execute(params![
                        "account-sentinel",
                        "thread-a",
                        1_000_i64 + index as i64,
                        format!("2026-11-12T10:{:02}:00Z", index % 60),
                        format!("turn-{index}"),
                        "gpt-5",
                        "/private/paged",
                        r#"{"input":1,"cacheRead":0,"cacheWrite":0,"output":1,"reasoning":0,"total":2}"#
                    ])
                    .unwrap();
            }
        }
        transaction.commit().unwrap();
    }
    let rows = CodexLocalUsageAdapter::new(&path).read().unwrap();
    assert_eq!(rows.len(), 1_101);
    let mut identities = rows
        .iter()
        .map(|row| row.dedup_key.clone().unwrap())
        .collect::<Vec<_>>();
    identities.sort();
    identities.dedup();
    assert_eq!(identities.len(), rows.len());
}
