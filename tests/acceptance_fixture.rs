use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use token_monitor_lite::accounting::TokenCounts;
use token_monitor_lite::aggregation::{BucketTimezone, CanonicalSnapshot};
use token_monitor_lite::discovery::{HostPaths, HostPlatform, discover};
use token_monitor_lite::runtime::{Monitor, MonitorSettings, MonitorSnapshot, SourceHealthStatus};
use token_monitor_lite::source_inputs::SqliteSourceError;
use token_monitor_lite::sources::{SourceError, SourceFormat, SourceProgress, SourceReader};
use token_monitor_lite::storage::{
    BACKUP_DIRECTORY_NAME, DATABASE_FILE_NAME, NativeSettings, StoreHandle,
};

static NEXT: AtomicUsize = AtomicUsize::new(0);

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    schema: String,
    revision: String,
    coverage: Coverage,
    release_target: ReleaseTarget,
    expected_aggregate: TokenCounts,
    expected_source_aggregate: TokenCounts,
    fixture_sha256: String,
    sources: Vec<SourceEntry>,
    bundles: Vec<BundleEntry>,
    scenarios: Vec<ScenarioEntry>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Coverage {
    canonical_observations: usize,
    canonical_sessions: usize,
    configured_sources: usize,
    decoded_bytes: u64,
    live_observations: usize,
    mixed_compressed_sqlite: bool,
    overlap_observations: usize,
    physical_bytes: u64,
    retained_history: String,
    retained_observations: usize,
    retained_periods: usize,
    retained_sessions: usize,
    source_formats: BTreeMap<String, usize>,
    usage_observations: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReleaseTarget {
    canonical_sessions: usize,
    configured_sources: usize,
    met: bool,
    missing: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SourceEntry {
    path: String,
    format: String,
    session: String,
    observations: usize,
    canonical_sessions: usize,
    suppressed_observations: usize,
    suppressed_totals: Option<TokenCounts>,
    decoded_bytes: u64,
    physical_bytes: u64,
    expected_totals: TokenCounts,
    retained_periods: usize,
}

#[derive(Deserialize)]
struct BundleEntry {
    main: String,
    sidecars: Vec<FileEntry>,
}

#[derive(Deserialize)]
struct FileEntry {
    path: String,
}

#[derive(Deserialize)]
struct ScenarioEntry {
    path: String,
    role: String,
}

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn generate() -> Self {
        let root = std::env::temp_dir().join(format!(
            "token-monitor-lite-acceptance-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let script = repository_root()
            .join("scripts")
            .join("generate-acceptance-fixture.py");
        let output = python_command()
            .arg(script)
            .arg("--output")
            .arg(&root)
            .output()
            .expect("run acceptance fixture generator");
        assert!(
            output.status.success(),
            "fixture generator failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("generator emits JSON report");
        assert_eq!(report["schema"], "token-monitor-lite-acceptance-v2");
        assert_eq!(report["sources"], 100);
        assert_eq!(report["canonicalSessions"], 1_000);
        assert_eq!(report["observations"], 2_786);
        assert_eq!(report["canonicalObservations"], 2_784);
        assert_eq!(report["retainedPeriods"], 120);
        assert_eq!(report["releaseTargetMet"], true);
        assert!(report["physicalBytes"].as_u64().unwrap_or(0) > 0);
        assert!(report["decodedBytes"].as_u64().unwrap_or(0) > 0);
        Self { root }
    }

    fn path(&self, relative: &str) -> PathBuf {
        assert!(!relative.starts_with(['/', '\\']));
        assert!(!relative.split(['/', '\\']).any(|part| part == ".."));
        relative
            .split('/')
            .fold(self.root.clone(), |path, part| path.join(part))
    }

    fn host_paths(&self) -> HostPaths {
        let home = self.path("home");
        HostPaths {
            platform: HostPlatform::Linux,
            home: home.clone(),
            app_data: None,
            local_app_data: None,
            xdg_config_home: Some(home.join(".config")),
            xdg_state_home: Some(home.join(".local/state")),
            legacy_shared_override: Some(home.join(".config/Token Monitor")),
            native_data_override: Some(self.root.join("native")),
            enabled_clients: BTreeSet::from(["claude".to_owned(), "codex".to_owned()]),
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).expect("remove generated acceptance fixture");
    }
}

fn repository_root() -> PathBuf {
    let current = std::env::current_dir().expect("test working directory");
    if current
        .join("scripts/generate-acceptance-fixture.py")
        .is_file()
    {
        current
    } else {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }
}

fn python_command() -> Command {
    if let Some(executable) = std::env::var_os("PYTHON") {
        return Command::new(executable);
    }
    for executable in ["python3", "python"] {
        if Command::new(executable)
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
        {
            return Command::new(executable);
        }
    }
    panic!("Python 3 is required for acceptance fixture tests");
}

fn source_format(format: &str) -> SourceFormat {
    match format {
        "claude-cli" => SourceFormat::ClaudeCli,
        "codex-cli" | "codex-cli-zstd" => SourceFormat::CodexCli,
        "session-archive-sqlite" => SourceFormat::SessionArchiveSqlite,
        "codex-local-usage-sqlite" => SourceFormat::CodexLocalUsageSqlite,
        other => panic!("unexpected generated source format: {other}"),
    }
}

fn publish(reader: &mut SourceReader) {
    loop {
        match reader.step().expect("read generated source").progress {
            SourceProgress::Pending => {}
            SourceProgress::Published { .. } => return,
            SourceProgress::Unchanged => panic!("new source must publish"),
        }
    }
}

fn read_manifest(fixture: &Fixture) -> Manifest {
    serde_json::from_slice(&fs::read(fixture.path("manifest.json")).expect("read manifest"))
        .expect("parse manifest")
}

fn distinct_sessions(canonical: &CanonicalSnapshot) -> usize {
    canonical
        .session_models
        .iter()
        .map(|row| (row.key.client.clone(), row.key.session.clone()))
        .collect::<BTreeSet<_>>()
        .len()
}

fn receive_until(
    receiver: &mpsc::Receiver<std::sync::Arc<MonitorSnapshot>>,
    timeout: Duration,
    predicate: impl Fn(&MonitorSnapshot) -> bool,
) -> std::sync::Arc<MonitorSnapshot> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let snapshot = receiver
            .recv_timeout(remaining)
            .expect("monitor did not publish the expected acceptance snapshot");
        if predicate(&snapshot) {
            return snapshot;
        }
    }
}

fn monitor_settings(
    store: &StoreHandle,
    discovery: &token_monitor_lite::discovery::DiscoveryReport,
) -> MonitorSettings {
    let stored = store.snapshot().expect("store snapshot");
    MonitorSettings {
        generation: stored.settings_generation,
        day_generation: stored.day_generation,
        pricing_generation: stored.pricing_generation,
        timezone: "UTC".parse::<BucketTimezone>().unwrap(),
        sources: discovery
            .sources
            .iter()
            .map(|source| source.monitored_source())
            .collect(),
    }
}

#[test]
fn generated_dataset_reparses_through_every_production_reader() {
    let fixture = Fixture::generate();
    let manifest = read_manifest(&fixture);
    assert_eq!(manifest.schema, "token-monitor-lite-acceptance-v2");
    assert_eq!(manifest.revision, "mixed-2026-11-13.1");
    assert_eq!(manifest.coverage.configured_sources, 100);
    assert_eq!(manifest.coverage.canonical_sessions, 1_000);
    assert_eq!(manifest.coverage.usage_observations, 2_786);
    assert_eq!(manifest.coverage.canonical_observations, 2_784);
    assert_eq!(manifest.coverage.overlap_observations, 2);
    assert_eq!(manifest.coverage.retained_periods, 120);
    assert_eq!(manifest.coverage.retained_sessions, 40);
    assert_eq!(manifest.coverage.retained_observations, 0);
    assert_eq!(
        manifest.coverage.live_observations,
        manifest.coverage.usage_observations
    );
    assert!(manifest.coverage.physical_bytes > 0);
    assert!(
        manifest.coverage.decoded_bytes >= manifest.coverage.physical_bytes,
        "compressed sources must declare decoded and physical bytes separately"
    );
    assert!(manifest.coverage.mixed_compressed_sqlite);
    assert_eq!(manifest.coverage.retained_history, "present");
    assert_eq!(manifest.release_target.canonical_sessions, 1_000);
    assert_eq!(manifest.release_target.configured_sources, 100);
    assert!(manifest.release_target.met);
    assert!(manifest.release_target.missing.is_empty());
    assert_eq!(
        manifest.coverage.source_formats,
        BTreeMap::from([
            ("claude-cli".to_owned(), 33),
            ("codex-cli".to_owned(), 33),
            ("codex-cli-zstd".to_owned(), 32),
            ("session-archive-sqlite".to_owned(), 1),
            ("codex-local-usage-sqlite".to_owned(), 1),
        ])
    );
    assert_eq!(manifest.bundles.len(), 2);
    assert!(
        manifest
            .bundles
            .iter()
            .all(|bundle| !bundle.sidecars.is_empty())
    );
    for bundle in &manifest.bundles {
        assert!(
            manifest
                .sources
                .iter()
                .any(|source| source.path == bundle.main)
        );
        for sidecar in &bundle.sidecars {
            assert!(fixture.path(&sidecar.path).is_file());
        }
    }
    for source in &manifest.sources {
        if source.suppressed_observations == 0 {
            assert!(source.suppressed_totals.is_none());
        } else {
            assert_eq!(source.suppressed_observations, 2);
            let suppressed = source.suppressed_totals.expect("suppressed totals");
            assert!(suppressed.input <= source.expected_totals.input);
            assert!(suppressed.output <= source.expected_totals.output);
        }
    }
    assert_eq!(manifest.fixture_sha256.len(), 64);

    let mut aggregate = TokenCounts::default();
    let mut observations = 0;
    let mut canonical_sessions = 0;
    let mut retained_periods = 0;
    for source in &manifest.sources {
        let format = source_format(&source.format);
        let mut reader =
            SourceReader::new(fixture.path(&source.path), format, source.session.clone());
        publish(&mut reader);
        let snapshot = reader.last_good().expect("published source snapshot");
        assert_eq!(snapshot.rows.len(), source.observations, "{}", source.path);
        assert_eq!(snapshot.totals, source.expected_totals, "{}", source.path);
        assert_eq!(
            snapshot.retained.len(),
            source.retained_periods,
            "{}",
            source.path
        );
        assert!(source.decoded_bytes >= source.physical_bytes);
        aggregate = aggregate
            .checked_add(snapshot.totals)
            .expect("generated aggregate stays in range");
        observations += snapshot.rows.len();
        canonical_sessions += source.canonical_sessions;
        retained_periods += snapshot.retained.len();
    }
    assert_eq!(observations, manifest.coverage.usage_observations);
    assert_eq!(aggregate, manifest.expected_source_aggregate);
    assert_eq!(retained_periods, manifest.coverage.retained_periods);
    assert_eq!(canonical_sessions, manifest.coverage.canonical_sessions);
    assert_eq!(
        manifest.coverage.usage_observations - manifest.coverage.overlap_observations,
        manifest.coverage.canonical_observations
    );
}

#[test]
fn discovered_tree_ingests_commits_reopens_and_survives_source_loss() {
    let fixture = Fixture::generate();
    let manifest = read_manifest(&fixture);
    let paths = fixture.host_paths();
    let discovery = discover(&paths).expect("discover generated tree");
    assert_eq!(discovery.sources.len(), 100);
    let discovered_paths = discovery
        .sources
        .iter()
        .map(|source| {
            source
                .path()
                .strip_prefix(&fixture.root)
                .expect("fixture path")
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect::<BTreeSet<_>>();
    let expected_paths = manifest
        .sources
        .iter()
        .map(|source| source.path.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(discovered_paths, expected_paths);

    let (store, _initial) = StoreHandle::open(discovery.native.clone()).expect("open store");
    let stored = store
        .save_settings(
            0,
            NativeSettings::from_json_bytes(br#"{"revision":1}"#).expect("native settings"),
        )
        .expect("save acceptance settings");
    assert!(stored.settings_generation > 0);
    let settings = monitor_settings(&store, &discovery);
    let (sender, receiver) = mpsc::channel();
    let monitor = Monitor::start(settings.clone(), store.clone(), move |snapshot| {
        let _ = sender.send(snapshot);
    })
    .expect("start mixed-format monitor");

    let committed = receive_until(&receiver, Duration::from_secs(300), |snapshot| {
        distinct_sessions(&snapshot.canonical) == 1_000
            && snapshot.canonical.totals == manifest.expected_aggregate
            && snapshot.source_health.len() == 100
            && snapshot
                .source_health
                .iter()
                .all(|source| source.status == SourceHealthStatus::Healthy)
    });
    assert_eq!(
        committed.canonical.retained_periods.len(),
        manifest.coverage.retained_periods
    );
    assert_eq!(
        committed.canonical.retained_periods[0].client,
        "claude".to_owned()
    );
    drop(monitor);

    let persisted = store.runtime_snapshot().expect("persisted runtime");
    assert!(persisted.initialized);
    assert_eq!(persisted.sources.len(), 100);
    let canonical = persisted.canonical.as_ref().expect("persisted canonical");
    assert_eq!(
        canonical.rows.len(),
        manifest.coverage.canonical_observations
    );
    assert_eq!(distinct_sessions(&canonical.rollups), 1_000);
    assert_eq!(canonical.rollups.totals, manifest.expected_aggregate);
    assert_eq!(
        canonical.rollups.retained_periods.len(),
        manifest.coverage.retained_periods
    );
    store.shutdown().expect("joined store shutdown");

    let home = fixture.path("home");
    let offline = fixture.root.join("home.offline");
    fs::rename(&home, &offline).expect("move sources offline");
    let (store, _) = StoreHandle::open(discovery.native.clone()).expect("reopen store offline");
    let offline_runtime = store.runtime_snapshot().expect("offline runtime");
    let offline_canonical = offline_runtime
        .canonical
        .as_ref()
        .expect("offline persisted canonical");
    assert_eq!(
        offline_canonical.rows.len(),
        manifest.coverage.canonical_observations
    );
    assert_eq!(distinct_sessions(&offline_canonical.rollups), 1_000);
    assert_eq!(
        offline_canonical.rollups.totals,
        manifest.expected_aggregate
    );
    assert_eq!(
        offline_canonical.rollups.retained_periods.len(),
        manifest.coverage.retained_periods
    );
    assert_eq!(offline_runtime.sources.len(), 100);

    let settings = monitor_settings(&store, &discovery);
    let (sender, receiver) = mpsc::channel();
    let monitor = Monitor::start(settings, store.clone(), move |snapshot| {
        let _ = sender.send(snapshot);
    })
    .expect("start unavailable-source monitor");
    let retained = receive_until(&receiver, Duration::from_secs(30), |snapshot| {
        distinct_sessions(&snapshot.canonical) == 1_000
            && snapshot.canonical.totals == manifest.expected_aggregate
    });
    assert!(retained.source_health.iter().all(|source| {
        matches!(
            source.status,
            SourceHealthStatus::Retained | SourceHealthStatus::ReadFailed
        )
    }));

    fs::rename(&offline, &home).expect("restore sources");
    assert!(monitor.request_refresh());
    let recovered = receive_until(&receiver, Duration::from_secs(300), |snapshot| {
        distinct_sessions(&snapshot.canonical) == 1_000
            && snapshot
                .source_health
                .iter()
                .all(|source| source.status == SourceHealthStatus::Healthy)
    });
    assert_eq!(recovered.canonical.totals, manifest.expected_aggregate);
    let persisted = store.runtime_snapshot().expect("post-recovery runtime");
    assert_eq!(
        persisted
            .canonical
            .as_ref()
            .expect("post-recovery canonical")
            .rows
            .len(),
        manifest.coverage.canonical_observations
    );

    let claude = manifest
        .sources
        .iter()
        .find(|source| source.format == "claude-cli")
        .expect("claude source");
    let claude_path = fixture.path(&claude.path);
    let original = fs::read(&claude_path).expect("read claude source");
    let mut appended = serde_json::to_vec(&serde_json::json!({
        "message": {
            "id": "acceptance-post-restart-message",
            "model": "claude-sonnet-4-5",
            "usage": {"input_tokens": 7}
        },
        "requestId": "acceptance-post-restart-request",
        "sessionId": claude.session.clone(),
        "timestamp": "2026-11-13T13:00:00Z",
        "type": "assistant"
    }))
    .expect("serialize appended record");
    appended.push(b'\n');
    fs::OpenOptions::new()
        .append(true)
        .open(&claude_path)
        .expect("open claude source")
        .write_all(&appended)
        .expect("append claude source");
    assert!(monitor.request_refresh());
    let mut expected = manifest.expected_aggregate;
    expected.input += 7;
    let updated = receive_until(&receiver, Duration::from_secs(300), |snapshot| {
        snapshot.canonical.totals == expected && distinct_sessions(&snapshot.canonical) == 1_000
    });
    assert_eq!(distinct_sessions(&updated.canonical), 1_000);
    assert_eq!(
        store
            .runtime_snapshot()
            .expect("updated runtime")
            .canonical
            .as_ref()
            .expect("updated canonical")
            .rows
            .len(),
        manifest.coverage.canonical_observations + 1
    );

    fs::write(&claude_path, &original).expect("restore claude bytes");
    assert!(monitor.request_refresh());
    let restored = receive_until(&receiver, Duration::from_secs(300), |snapshot| {
        snapshot.canonical.totals == manifest.expected_aggregate
            && distinct_sessions(&snapshot.canonical) == 1_000
    });
    assert_eq!(restored.canonical.totals, manifest.expected_aggregate);
    assert_eq!(
        store
            .runtime_snapshot()
            .expect("restored runtime")
            .canonical
            .as_ref()
            .expect("restored canonical")
            .rows
            .len(),
        manifest.coverage.canonical_observations,
        "restoring source bytes must not duplicate canonical rows"
    );
    let backup = store.create_backup().expect("verified native backup");
    assert!(backup.schema_version >= 4);
    let backup_generation = store.runtime_snapshot().expect("backup runtime").generation;

    let mut post_backup = serde_json::to_vec(&serde_json::json!({
        "message": {
            "id": "acceptance-post-backup-message",
            "model": "claude-sonnet-4-5",
            "usage": {"input_tokens": 5}
        },
        "requestId": "acceptance-post-backup-request",
        "sessionId": claude.session.clone(),
        "timestamp": "2026-11-13T14:00:00Z",
        "type": "assistant"
    }))
    .expect("serialize post-backup record");
    post_backup.push(b'\n');
    fs::OpenOptions::new()
        .append(true)
        .open(&claude_path)
        .expect("open claude source for post-backup append")
        .write_all(&post_backup)
        .expect("append post-backup record");
    assert!(monitor.request_refresh());
    let mut post_backup_totals = manifest.expected_aggregate;
    post_backup_totals.input += 5;
    receive_until(&receiver, Duration::from_secs(300), |snapshot| {
        snapshot.canonical.totals == post_backup_totals
    });
    let post_backup_generation = store
        .runtime_snapshot()
        .expect("post-backup runtime")
        .generation;
    assert!(post_backup_generation > backup_generation);
    drop(monitor);
    store.shutdown().expect("joined shutdown");

    let native_root = discovery.native.root().to_path_buf();
    let database = native_root.join(DATABASE_FILE_NAME);
    for suffix in ["-wal", "-shm"] {
        let _ = fs::remove_file(PathBuf::from(format!("{}{suffix}", database.display())));
    }
    fs::write(&database, b"corrupted native database").expect("corrupt native database");
    let (recovered, _) =
        StoreHandle::open(discovery.native.clone()).expect("recover from verified backup");
    let runtime = recovered.runtime_snapshot().expect("recovered runtime");
    assert_eq!(
        runtime.generation, backup_generation,
        "recovery restores the verified backup generation"
    );
    let canonical = runtime.canonical.as_ref().expect("recovered canonical");
    assert_eq!(
        canonical.rows.len(),
        manifest.coverage.canonical_observations
    );
    assert_eq!(distinct_sessions(&canonical.rollups), 1_000);
    assert_eq!(canonical.rollups.totals, manifest.expected_aggregate);
    recovered.shutdown().expect("recovered store shutdown");

    let mut native_files = vec![database.clone()];
    if let Ok(entries) = fs::read_dir(native_root.join(BACKUP_DIRECTORY_NAME)) {
        for entry in entries.flatten() {
            if entry.path().is_file() {
                native_files.push(entry.path());
            }
        }
    }
    for suffix in ["-wal", "-shm"] {
        let path = PathBuf::from(format!("{}{suffix}", database.display()));
        if path.is_file() {
            native_files.push(path);
        }
    }
    for path in native_files {
        let bytes = fs::read(&path).expect("read native acceptance file");
        for marker in [
            b"/home/".as_slice(),
            b"PRIVATE_".as_slice(),
            b"private-session".as_slice(),
            b"account-fixture".as_slice(),
        ] {
            assert!(
                !bytes.windows(marker.len()).any(|window| window == marker),
                "native acceptance file leaked private marker {}",
                String::from_utf8_lossy(marker)
            );
        }
    }
}

#[test]
fn generated_scenarios_cover_append_rewrite_rotation_compression_and_sqlite_failures() {
    let fixture = Fixture::generate();
    let manifest = read_manifest(&fixture);
    let scenario = |role: &str| {
        manifest
            .scenarios
            .iter()
            .find(|entry| entry.role == role)
            .map(|entry| fixture.path(&entry.path))
            .expect("scenario exists")
    };

    let working = fixture.path("working-scenario.jsonl");
    fs::copy(scenario("append-initial"), &working).expect("copy append base");
    let mut reader = SourceReader::new(working.clone(), SourceFormat::ClaudeCli, "scenario-claude");
    publish(&mut reader);
    assert_eq!(reader.last_good().expect("base snapshot").totals.input, 7);
    let appended = fs::read(scenario("append-record")).expect("read append record");
    fs::OpenOptions::new()
        .append(true)
        .open(&working)
        .expect("open append target")
        .write_all(&appended)
        .expect("append scenario record");
    publish(&mut reader);
    assert_eq!(
        reader.last_good().expect("append snapshot").totals.input,
        10
    );

    fs::copy(scenario("rewrite-after"), &working).expect("rewrite source");
    reader.request_full_reconciliation();
    publish(&mut reader);
    assert_eq!(
        reader.last_good().expect("rewrite snapshot").totals.input,
        5
    );

    fs::copy(scenario("rotation-after"), &working).expect("rotate source");
    publish(&mut reader);
    assert_eq!(
        reader.last_good().expect("rotation snapshot").totals.input,
        0
    );

    let growth_path = fixture.path("growth-scenario.jsonl");
    fs::copy(scenario("rewrite-grow-before"), &growth_path).expect("copy growth base");
    let mut growth = SourceReader::new(
        growth_path.clone(),
        SourceFormat::ClaudeCli,
        "scenario-claude",
    );
    publish(&mut growth);
    assert_eq!(growth.last_good().expect("growth base").totals.input, 7);
    fs::copy(scenario("rewrite-grow-after"), &growth_path).expect("grow source");
    growth.request_full_reconciliation();
    publish(&mut growth);
    assert_eq!(growth.last_good().expect("growth snapshot").totals.input, 8);

    let mut tail_reader = SourceReader::new(
        scenario("incomplete-tail"),
        SourceFormat::ClaudeCli,
        "scenario-claude",
    );
    publish(&mut tail_reader);
    let tail = tail_reader.last_good().expect("tail snapshot");
    assert!(tail.incomplete_tail);
    assert!(tail.rows.is_empty());

    let mut large_reader = SourceReader::new(
        scenario("large-record"),
        SourceFormat::ClaudeCli,
        "scenario-claude",
    );
    publish(&mut large_reader);
    assert_eq!(
        large_reader
            .last_good()
            .expect("large snapshot")
            .totals
            .input,
        1
    );

    let mut error_reader = SourceReader::new(
        scenario("source-error"),
        SourceFormat::ClaudeCli,
        "scenario-claude",
    );
    let error = error_reader
        .step()
        .expect_err("malformed framed record must fail");
    assert!(!format!("{error:?} {error}").contains("malformed"));
    assert!(error_reader.last_good().is_none());

    let valid_path = scenario("codex-valid");
    let valid_bytes = fs::read(&valid_path).expect("read valid zstd scenario");
    let mut valid_reader =
        SourceReader::new(valid_path.clone(), SourceFormat::CodexCli, "scenario-codex");
    publish(&mut valid_reader);
    let valid = valid_reader.last_good().expect("valid zstd snapshot");
    assert_eq!(valid.totals.input, 10);
    assert_eq!(valid.totals.cache_read, 2);
    assert_eq!(valid.totals.output, 4);
    assert_eq!(fs::read(&valid_path).unwrap(), valid_bytes);

    let truncated_path = scenario("codex-truncated");
    let truncated_bytes = fs::read(&truncated_path).expect("read truncated scenario");
    let mut truncated = SourceReader::new(
        truncated_path.clone(),
        SourceFormat::CodexCli,
        "scenario-codex",
    );
    assert!(truncated.step().is_err());
    assert!(truncated.last_good().is_none());
    assert_eq!(fs::read(&truncated_path).unwrap(), truncated_bytes);

    let corrupt_path = scenario("codex-corrupt");
    let corrupt_bytes = fs::read(&corrupt_path).expect("read corrupt scenario");
    let mut corrupt = SourceReader::new(
        corrupt_path.clone(),
        SourceFormat::CodexCli,
        "scenario-codex",
    );
    assert!(corrupt.step().is_err());
    assert!(corrupt.last_good().is_none());
    assert_eq!(fs::read(&corrupt_path).unwrap(), corrupt_bytes);

    let unsupported_path = scenario("archive-unsupported");
    let unsupported_bytes = fs::read(&unsupported_path).expect("read unsupported sqlite");
    let mut unsupported = SourceReader::new(
        unsupported_path.clone(),
        SourceFormat::SessionArchiveSqlite,
        "archive",
    );
    assert!(matches!(
        unsupported.step(),
        Err(SourceError::Sqlite(SqliteSourceError::Schema))
    ));
    assert!(unsupported.last_good().is_none());
    assert_eq!(fs::read(&unsupported_path).unwrap(), unsupported_bytes);

    let wrong_role_path = scenario("codex-local-usage-wrong-role");
    let wrong_role_bytes = fs::read(&wrong_role_path).expect("read wrong-role sqlite");
    let mut wrong_role = SourceReader::new(
        wrong_role_path.clone(),
        SourceFormat::CodexLocalUsageSqlite,
        "codex-local",
    );
    assert!(matches!(
        wrong_role.step(),
        Err(SourceError::Sqlite(SqliteSourceError::Schema))
    ));
    assert!(wrong_role.last_good().is_none());
    assert_eq!(fs::read(&wrong_role_path).unwrap(), wrong_role_bytes);

    let local_main = fixture.path("home/.config/Token Monitor/codex-local-usage.sqlite");
    let working_local = fixture.path("working-local-usage.sqlite");
    let working_wal = PathBuf::from(format!("{}-wal", working_local.display()));
    let working_shm = PathBuf::from(format!("{}-shm", working_local.display()));
    fs::copy(&local_main, &working_local).expect("copy local usage main");
    fs::copy(
        PathBuf::from(format!("{}-wal", local_main.display())),
        &working_wal,
    )
    .expect("copy local usage wal");
    fs::copy(
        PathBuf::from(format!("{}-shm", local_main.display())),
        &working_shm,
    )
    .expect("copy local usage shm");
    let mut replaced = SourceReader::new(
        working_local.clone(),
        SourceFormat::CodexLocalUsageSqlite,
        "codex-local",
    );
    publish(&mut replaced);
    let before = replaced.last_good().expect("local usage snapshot");
    assert!(before.rows.len() > 900);
    fs::remove_file(&working_wal).expect("remove replaced wal");
    fs::remove_file(&working_shm).expect("remove replaced shm");
    fs::write(&working_local, &wrong_role_bytes).expect("replace sqlite database");
    replaced.request_full_reconciliation();
    assert!(matches!(
        replaced.step(),
        Err(SourceError::Sqlite(SqliteSourceError::Schema))
    ));
    let after = replaced.last_good().expect("last-good retained");
    assert_eq!(after.totals, before.totals);
    assert_eq!(after.rows.len(), before.rows.len());
    assert_eq!(fs::read(&working_local).unwrap(), wrong_role_bytes);
}

#[test]
fn generator_refuses_a_nonempty_output_root() {
    let fixture = Fixture::generate();
    let script = repository_root()
        .join("scripts")
        .join("generate-acceptance-fixture.py");
    let output = python_command()
        .arg(script)
        .arg("--output")
        .arg(&fixture.root)
        .output()
        .expect("rerun acceptance fixture generator");
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).contains(&fixture.root.to_string_lossy()[..]));
}
