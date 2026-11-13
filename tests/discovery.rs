use rusqlite::Connection;
use ruzstd::encoding::{CompressionLevel, compress_to_vec};
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use token_monitor_lite::aggregation::MAX_SOURCES;
use token_monitor_lite::discovery::{
    ArtifactKind, DiscoveryError, DiscoveryNotice, HostPaths, HostPlatform, MAX_ARTIFACT_DEPTH,
    MAX_ARTIFACT_FILE_BYTES, MAX_ARTIFACT_TOTAL_BYTES, MAX_ARTIFACTS, discover, resolve_paths,
};
use token_monitor_lite::sources::{SourceFormat, SourceId};

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Tree(PathBuf);

impl Tree {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "token-monitor-discovery-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn directory(&self, relative: &str) -> PathBuf {
        let path = self.0.join(relative);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn file(&self, relative: &str, bytes: &[u8]) -> PathBuf {
        let path = self.0.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
        path
    }

    fn paths(&self, clients: &[&str]) -> HostPaths {
        let mut paths = host(HostPlatform::Linux, self.0.to_str().unwrap());
        paths.native_data_override = Some(self.0.join("native-private"));
        paths.legacy_shared_override = Some(self.0.join("legacy-shared"));
        paths.enabled_clients = clients.iter().map(|client| (*client).to_owned()).collect();
        paths
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn host(platform: HostPlatform, home: &str) -> HostPaths {
    HostPaths {
        platform,
        home: PathBuf::from(home),
        app_data: None,
        local_app_data: None,
        xdg_config_home: None,
        xdg_state_home: None,
        legacy_shared_override: None,
        native_data_override: None,
        enabled_clients: BTreeSet::new(),
    }
}

fn zstd(bytes: &[u8]) -> Vec<u8> {
    compress_to_vec(bytes, CompressionLevel::Fastest)
}

fn codex_rollout(session: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    for value in [
        serde_json::json!({
            "type": "session_meta",
            "payload": {
                "id": session,
                "model_provider": "openai",
                "source": "cli"
            }
        }),
        serde_json::json!({
            "type": "turn_context",
            "timestamp": "2026-11-09T10:00:00Z",
            "payload": { "model": "gpt-5" }
        }),
        serde_json::json!({
            "type": "event_msg",
            "timestamp": "2026-11-09T10:00:01Z",
            "payload": {
                "type": "token_count",
                "info": {
                    "last_token_usage": { "input_tokens": 2 },
                    "total_token_usage": { "input_tokens": 2 }
                }
            }
        }),
    ] {
        bytes.extend(serde_json::to_vec(&value).unwrap());
        bytes.push(b'\n');
    }
    bytes
}

#[test]
fn platform_roots_are_deterministic_and_product_specific() {
    #[cfg(windows)]
    {
        let mut windows = host(HostPlatform::Windows, r"C:\Users\tester");
        windows.app_data = Some(PathBuf::from(r"C:\Users\tester\AppData\Roaming"));
        windows.local_app_data = Some(PathBuf::from(r"C:\Users\tester\AppData\Local"));
        let resolved = resolve_paths(&windows).unwrap();
        assert_eq!(
            resolved.native.root(),
            Path::new(r"C:\Users\tester\AppData\Local\Token Monitor Lite")
        );
        assert_eq!(
            resolved.legacy_user_data,
            PathBuf::from(r"C:\Users\tester\AppData\Roaming\Token Monitor")
        );
        assert_eq!(resolved.legacy_shared_data, resolved.legacy_user_data);
    }

    #[cfg(unix)]
    {
        let macos = host(HostPlatform::MacOs, "/Users/tester");
        let resolved = resolve_paths(&macos).unwrap();
        assert_eq!(
            resolved.native.root(),
            Path::new("/Users/tester/Library/Application Support/Token Monitor Lite")
        );
        assert_eq!(
            resolved.legacy_user_data,
            PathBuf::from("/Users/tester/Library/Application Support/Token Monitor")
        );
        assert_eq!(resolved.legacy_shared_data, resolved.legacy_user_data);

        let linux = host(HostPlatform::Linux, "/home/tester");
        let resolved = resolve_paths(&linux).unwrap();
        assert_eq!(
            resolved.native.root(),
            Path::new("/home/tester/.local/state/token-monitor-lite")
        );
        assert_eq!(
            resolved.legacy_user_data,
            PathBuf::from("/home/tester/.config/Token Monitor")
        );
        assert_eq!(resolved.legacy_shared_data, resolved.legacy_user_data);
    }
}

#[test]
#[cfg(unix)]
fn overrides_are_separate_and_xdg_paths_keep_their_roles() {
    let mut linux = host(HostPlatform::Linux, "/home/tester");
    linux.xdg_config_home = Some(PathBuf::from("/configuration"));
    linux.xdg_state_home = Some(PathBuf::from("/state"));
    linux.legacy_shared_override = Some(PathBuf::from("/legacy-shared"));
    linux.native_data_override = Some(PathBuf::from("/native-private"));

    let resolved = resolve_paths(&linux).unwrap();
    assert_eq!(resolved.native.root(), Path::new("/native-private"));
    assert_eq!(
        resolved.legacy_user_data,
        PathBuf::from("/configuration/Token Monitor")
    );
    assert_eq!(resolved.legacy_shared_data, PathBuf::from("/legacy-shared"));
}

#[test]
#[cfg(unix)]
fn invalid_or_overlapping_roots_are_rejected_without_current_dir_fallback() {
    let missing_home = host(HostPlatform::Linux, "");
    assert_eq!(
        resolve_paths(&missing_home).unwrap_err(),
        DiscoveryError::RequiredRootUnavailable
    );

    let mut relative = host(HostPlatform::Linux, "/home/tester");
    relative.native_data_override = Some(PathBuf::from("relative/native"));
    assert_eq!(
        resolve_paths(&relative).unwrap_err(),
        DiscoveryError::InvalidRoot
    );

    let mut parent = host(HostPlatform::Linux, "/home/tester");
    parent.legacy_shared_override = Some(PathBuf::from("/legacy/../shared"));
    assert_eq!(
        resolve_paths(&parent).unwrap_err(),
        DiscoveryError::InvalidRoot
    );

    let mut nested = host(HostPlatform::Linux, "/home/tester");
    nested.native_data_override = Some(PathBuf::from("/legacy/native"));
    nested.legacy_shared_override = Some(PathBuf::from("/legacy"));
    assert_eq!(
        resolve_paths(&nested).unwrap_err(),
        DiscoveryError::OverlappingRoots
    );

    let mut empty = host(HostPlatform::Linux, "/home/tester");
    empty.native_data_override = Some(PathBuf::new());
    assert_eq!(
        resolve_paths(&empty).unwrap_err(),
        DiscoveryError::InvalidRoot
    );
}

#[test]
#[cfg(unix)]
fn backend_path_types_redact_debug_output() {
    let linux = host(HostPlatform::Linux, "/private/user-name");
    let resolved = resolve_paths(&linux).unwrap();
    let rendered = format!("{:?}", resolved.native);
    assert_eq!(rendered, "NativePaths { paths: [redacted] }");
    assert!(!rendered.contains("private"));
}

#[test]
#[cfg(windows)]
fn windows_overrides_are_absolute_and_nonoverlapping() {
    let mut windows = host(HostPlatform::Windows, r"C:\Users\tester");
    windows.app_data = Some(PathBuf::from(r"C:\Users\tester\AppData\Roaming"));
    windows.local_app_data = Some(PathBuf::from(r"C:\Users\tester\AppData\Local"));
    windows.native_data_override = Some(PathBuf::from(r"C:\native-private"));
    windows.legacy_shared_override = Some(PathBuf::from(r"C:\legacy-shared"));
    let resolved = resolve_paths(&windows).unwrap();
    assert_eq!(resolved.native.root(), Path::new(r"C:\native-private"));
    assert_eq!(
        resolved.legacy_shared_data,
        PathBuf::from(r"C:\legacy-shared")
    );

    let mut relative = host(HostPlatform::Windows, r"C:\Users\tester");
    relative.app_data = Some(PathBuf::from(r"C:\Users\tester\AppData\Roaming"));
    relative.local_app_data = Some(PathBuf::from(r"C:\Users\tester\AppData\Local"));
    relative.native_data_override = Some(PathBuf::from(r"relative\native"));
    assert_eq!(
        resolve_paths(&relative).unwrap_err(),
        DiscoveryError::InvalidRoot
    );

    let mut nested = host(HostPlatform::Windows, r"C:\Users\tester");
    nested.app_data = Some(PathBuf::from(r"C:\Users\tester\AppData\Roaming"));
    nested.local_app_data = Some(PathBuf::from(r"C:\Users\tester\AppData\Local"));
    nested.native_data_override = Some(PathBuf::from(r"C:\legacy\native"));
    nested.legacy_shared_override = Some(PathBuf::from(r"C:\legacy"));
    assert_eq!(
        resolve_paths(&nested).unwrap_err(),
        DiscoveryError::OverlappingRoots
    );

    let rendered = format!("{:?}", resolved.native);
    assert_eq!(rendered, "NativePaths { paths: [redacted] }");
    assert!(!rendered.contains("native-private"));
}

#[test]
fn legacy_manifest_classifies_only_recognized_files_and_inert_plugin_directories() {
    let tree = Tree::new();
    for name in [
        "settings.json",
        "credentials.json",
        "limits-history.sqlite",
        "hub-devices.json",
        "icloud-revisions.json",
    ] {
        tree.file(&format!(".config/Token Monitor/{name}"), b"not opened");
    }
    for name in [
        "session-usage-archive.sqlite",
        "session-usage-archive.sqlite-wal",
        "session-usage-archive.sqlite-shm",
        "session-usage-archive.json",
        "daily-history-archive.json",
        "codex-local-usage.sqlite",
        "codex-local-usage.sqlite-wal",
        "codex-local-usage.sqlite-shm",
    ] {
        tree.file(&format!("legacy-shared/{name}"), b"not opened");
    }
    for name in [
        "plugin-manager",
        "plugin-settings",
        "plugin-data",
        "plugin-credentials",
    ] {
        tree.directory(&format!(".config/Token Monitor/{name}"));
    }
    tree.directory(".config/Token Monitor/plugins/example-package");
    tree.file("unrelated-home-file.json", b"ignored");

    let report = discover(&tree.paths(&[])).unwrap();
    let legacy = report.legacy.unwrap();
    let kinds = legacy
        .artifacts
        .iter()
        .map(|artifact| artifact.kind)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        kinds,
        BTreeSet::from([
            ArtifactKind::Settings,
            ArtifactKind::Credentials,
            ArtifactKind::LimitsHistory,
            ArtifactKind::HubDevices,
            ArtifactKind::IcloudRevisions,
            ArtifactKind::SessionUsageArchive,
            ArtifactKind::SessionUsageArchiveWal,
            ArtifactKind::SessionUsageArchiveShm,
            ArtifactKind::SessionUsageArchiveJson,
            ArtifactKind::DailyHistoryArchive,
            ArtifactKind::CodexLocalUsage,
            ArtifactKind::CodexLocalUsageWal,
            ArtifactKind::CodexLocalUsageShm,
            ArtifactKind::PluginManager,
            ArtifactKind::PluginPackage,
            ArtifactKind::PluginSettings,
            ArtifactKind::PluginData,
            ArtifactKind::PluginCredentials,
        ])
    );
    assert_eq!(legacy.artifacts.len(), 18);
    assert!(
        legacy
            .artifacts
            .iter()
            .all(|artifact| artifact.eligible_for_staging)
    );
    assert!(report.notices.is_empty());
    assert!(!format!("{legacy:?}").contains(tree.0.to_str().unwrap()));
}

#[cfg(unix)]
#[test]
fn non_regular_recognized_entries_are_not_staging_candidates_and_notices_are_redacted() {
    use std::os::unix::fs::symlink;

    let tree = Tree::new();
    let target = tree.file("outside.json", b"{}");
    let legacy = tree.directory(".config/Token Monitor");
    symlink(target, legacy.join("settings.json")).unwrap();

    let report = discover(&tree.paths(&[])).unwrap();
    let artifact = &report.legacy.as_ref().unwrap().artifacts[0];
    assert_eq!(artifact.kind, ArtifactKind::Settings);
    assert!(!artifact.eligible_for_staging);
    assert_eq!(
        report.notices,
        vec![DiscoveryNotice::ArtifactNotRegular {
            kind: ArtifactKind::Settings,
            ordinal: 1,
        }]
    );
    let rendered = format!("{:?}", report.notices);
    assert!(!rendered.contains(tree.0.to_str().unwrap()));
    assert!(!rendered.contains("outside.json"));
}

#[test]
fn artifact_manifest_enforces_file_depth_count_and_total_byte_bounds() {
    let oversized = Tree::new();
    let file = oversized.file(".config/Token Monitor/settings.json", b"");
    File::options()
        .write(true)
        .open(file)
        .unwrap()
        .set_len(MAX_ARTIFACT_FILE_BYTES + 1)
        .unwrap();
    let report = discover(&oversized.paths(&[])).unwrap();
    assert!(!report.legacy.unwrap().artifacts[0].eligible_for_staging);
    assert!(matches!(
        report.notices.as_slice(),
        [DiscoveryNotice::ArtifactFileTooLarge {
            kind: ArtifactKind::Settings,
            ordinal: 1
        }]
    ));

    let too_large_in_total = Tree::new();
    let file_count = MAX_ARTIFACT_TOTAL_BYTES / MAX_ARTIFACT_FILE_BYTES + 1;
    for index in 0..file_count {
        let file = too_large_in_total.file(
            &format!(".config/Token Monitor/plugin-data/value-{index}.bin"),
            b"",
        );
        File::options()
            .write(true)
            .open(file)
            .unwrap()
            .set_len(MAX_ARTIFACT_FILE_BYTES)
            .unwrap();
    }
    let report = discover(&too_large_in_total.paths(&[])).unwrap();
    assert!(!report.legacy.unwrap().artifacts[0].eligible_for_staging);
    assert!(matches!(
        report.notices.as_slice(),
        [DiscoveryNotice::ArtifactTotalBytesExceeded {
            kind: ArtifactKind::PluginData,
            ordinal: 1
        }]
    ));

    let too_deep = Tree::new();
    let mut relative = String::from(".config/Token Monitor/plugin-data");
    for index in 0..=MAX_ARTIFACT_DEPTH {
        relative.push_str(&format!("/level-{index}"));
    }
    too_deep.file(&format!("{relative}/value.json"), b"{}");
    let report = discover(&too_deep.paths(&[])).unwrap();
    assert!(!report.legacy.unwrap().artifacts[0].eligible_for_staging);
    assert!(matches!(
        report.notices.as_slice(),
        [DiscoveryNotice::ArtifactDepthExceeded {
            kind: ArtifactKind::PluginData,
            ordinal: 1
        }]
    ));

    let too_many = Tree::new();
    for index in 0..=MAX_ARTIFACTS {
        too_many.directory(&format!(".config/Token Monitor/plugins/package-{index:03}"));
    }
    let report = discover(&too_many.paths(&[])).unwrap();
    assert_eq!(report.legacy.unwrap().artifacts.len(), MAX_ARTIFACTS);
    assert!(
        report
            .notices
            .contains(&DiscoveryNotice::ArtifactLimitReached)
    );
}

#[test]
fn source_discovery_admits_only_selected_regular_files_validated_by_current_readers() {
    let tree = Tree::new();
    tree.file(
        ".claude/projects/project/claude-a.jsonl",
        br#"{"type":"assistant","sessionId":"claude-a","timestamp":"2026-11-09T10:00:00Z","message":{"id":"message-a","model":"claude","usage":{"input_tokens":3}}}
"#,
    );
    tree.file(
        ".codex/sessions/2026/codex-a.jsonl",
        br#"{"type":"session_meta","payload":{"id":"codex-a","model_provider":"openai","source":"cli"}}
{"type":"turn_context","timestamp":"2026-11-09T10:00:00Z","payload":{"model":"gpt-5"}}
{"type":"event_msg","timestamp":"2026-11-09T10:00:01Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":2},"total_token_usage":{"input_tokens":2}}}}
"#,
    );
    tree.file(".claude/projects/project/zz-broken.jsonl", b"not json\n");
    tree.file("random.jsonl", b"not a conventional source\n");

    let report = discover(&tree.paths(&["codex", "claude", "gemini"])).unwrap();
    assert_eq!(report.sources.len(), 2);
    assert_eq!(report.sources[0].format, SourceFormat::ClaudeCli);
    assert_eq!(report.sources[0].session, "claude-a");
    assert_eq!(report.sources[1].format, SourceFormat::CodexCli);
    assert_eq!(report.sources[1].session, "codex-a");
    assert_eq!(report.sources[0].source_id.as_storage_key().len(), 42);
    assert_ne!(report.sources[0].source_id, report.sources[1].source_id);
    assert_eq!(
        report.sources[0].source_id,
        SourceId::for_path(report.sources[0].format, report.sources[0].path())
    );
    assert_eq!(
        report.sources[0].monitored_source().source_id,
        report.sources[0].source_id
    );
    assert!(
        report
            .notices
            .contains(&DiscoveryNotice::UnsupportedClient {
                client: "gemini".to_owned(),
            })
    );
    assert!(report.notices.contains(&DiscoveryNotice::SourceInvalid {
        client: "claude",
        ordinal: 2,
    }));
    assert!(!format!("{:?}", report.sources).contains(tree.0.to_str().unwrap()));

    let claude_only = discover(&tree.paths(&["claude"])).unwrap();
    assert_eq!(claude_only.sources.len(), 1);
    assert_eq!(claude_only.sources[0].format, SourceFormat::ClaudeCli);
    assert_eq!(
        claude_only.sources[0].source_id,
        report.sources[0].source_id
    );
}

#[test]
fn sqlite_archive_and_local_usage_are_discovered_as_main_file_sources() {
    let tree = Tree::new();
    let shared = tree.0.join("legacy-shared");
    fs::create_dir_all(&shared).unwrap();
    let archive = Connection::open(shared.join("session-usage-archive.sqlite")).unwrap();
    archive
        .execute_batch(
            "CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE sessions (
                session_key TEXT PRIMARY KEY,
                entry_json TEXT NOT NULL,
                revision INTEGER NOT NULL
             );
             CREATE INDEX sessions_revision_idx ON sessions(revision, session_key);
             INSERT INTO metadata VALUES ('schema-version', '1');
             INSERT INTO metadata VALUES ('legacy-migrated', '1');
             INSERT INTO metadata VALUES ('revision', '0');",
        )
        .unwrap();
    drop(archive);
    let local = Connection::open(shared.join("codex-local-usage.sqlite")).unwrap();
    local
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
    drop(local);

    let report = discover(&tree.paths(&[])).unwrap();
    assert_eq!(report.sources.len(), 2);
    assert!(report.sources.iter().any(|source| {
        source.format == SourceFormat::SessionArchiveSqlite && source.session == "archive"
    }));
    assert!(report.sources.iter().any(|source| {
        source.format == SourceFormat::CodexLocalUsageSqlite && source.session == "codex-local"
    }));
    assert!(!format!("{:?}", report.sources).contains(tree.0.to_str().unwrap()));
}

#[test]
fn compressed_codex_rollouts_are_admitted_only_at_codex_rollout_roles() {
    let tree = Tree::new();
    let compressed = zstd(&codex_rollout("codex-compressed"));
    let admitted = tree.file(".codex/sessions/2026/rollout.jsonl.zst", &compressed);
    tree.file(".codex/sessions/2026/not-jsonl.zst", &compressed);
    tree.file(
        ".claude/projects/project/claude.jsonl.zst",
        &zstd(b"{\"type\":\"assistant\",\"sessionId\":\"claude\"}\n"),
    );
    tree.file(
        ".codex/sessions/2026/corrupt.jsonl.zst",
        b"not a zstd frame",
    );

    let report = discover(&tree.paths(&["codex", "claude"])).unwrap();
    assert_eq!(report.sources.len(), 1);
    assert_eq!(report.sources[0].format, SourceFormat::CodexCli);
    assert_eq!(report.sources[0].session, "codex-compressed");
    assert_eq!(
        report.sources[0].source_id,
        SourceId::for_path(SourceFormat::CodexCli, &admitted)
    );
    assert!(
        report
            .notices
            .contains(&DiscoveryNotice::SourceUnsupportedCompression {
                client: "codex",
                ordinal: 2,
            })
    );
    assert!(
        report
            .notices
            .contains(&DiscoveryNotice::SourceUnsupportedCompression {
                client: "claude",
                ordinal: 1,
            })
    );
    assert!(report.notices.contains(&DiscoveryNotice::SourceInvalid {
        client: "codex",
        ordinal: 1,
    }));
    assert!(!format!("{:?}", report.sources).contains(tree.0.to_str().unwrap()));
}

#[test]
fn opaque_source_identity_is_stable_across_discovery_and_format_qualified() {
    let tree = Tree::new();
    let path = tree.file(
        ".claude/projects/project/stable.jsonl",
        br#"{"type":"assistant","sessionId":"stable","timestamp":"2026-11-09T10:00:00Z","message":{"id":"message-stable","model":"claude","usage":{"input_tokens":1}}}
"#,
    );

    let first = discover(&tree.paths(&["claude"])).unwrap();
    let second = discover(&tree.paths(&["claude"])).unwrap();
    assert_eq!(first.sources[0].source_id, second.sources[0].source_id);
    assert_eq!(
        first.sources[0].source_id,
        SourceId::for_path(SourceFormat::ClaudeCli, &path)
    );
    assert_ne!(
        first.sources[0].source_id,
        SourceId::for_path(SourceFormat::CodexCli, &path)
    );
    let rendered = format!("{:?}", first.sources[0]);
    assert!(!rendered.contains(path.to_str().unwrap()));
    assert!(!rendered.contains("stable"));
}

#[test]
fn source_count_is_capped_and_excess_is_reported() {
    let tree = Tree::new();
    for index in 0..=MAX_SOURCES {
        let session = format!("session-{index:04}");
        tree.file(
            &format!(".claude/projects/project/{session}.jsonl"),
            format!(r#"{{"type":"assistant","sessionId":"{session}","timestamp":"2026-11-09T10:00:00Z","message":{{"id":"message-{index}","model":"claude","usage":{{"input_tokens":1}}}}}}
"#).as_bytes(),
        );
    }

    let report = discover(&tree.paths(&["claude"])).unwrap();
    assert_eq!(report.sources.len(), MAX_SOURCES);
    assert!(
        report
            .notices
            .contains(&DiscoveryNotice::SourceLimitReached)
    );
}
