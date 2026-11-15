use rusqlite::Connection;
use ruzstd::encoding::{CompressionLevel, compress_to_vec};
use std::collections::{BTreeMap, BTreeSet};
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
        xdg_data_home: None,
        xdg_state_home: None,
        legacy_shared_override: None,
        native_data_override: None,
        enabled_clients: BTreeSet::new(),
        env_paths: BTreeMap::new(),
    }
}

fn zstd(bytes: &[u8]) -> Vec<u8> {
    compress_to_vec(bytes, CompressionLevel::Fastest)
}

fn opencode_database(path: &Path, session: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let connection = Connection::open(path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE session_v2 (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                directory TEXT NOT NULL,
                time_created INTEGER NOT NULL
             ) STRICT;
             CREATE TABLE session_message (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                type TEXT NOT NULL,
                seq INTEGER NOT NULL,
                time_created INTEGER NOT NULL,
                data TEXT NOT NULL
             ) STRICT;",
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO session_v2 (id, title, directory, time_created) VALUES (?1, '', '', 1763000000000)",
            [session],
        )
        .unwrap();
    let data = serde_json::json!({
        "role": "assistant",
        "model": {"id": "claude-sonnet-4-5", "providerID": "anthropic"},
        "time": {"created": 1_763_000_000_000_i64},
        "tokens": {
            "input": 10,
            "output": 5,
            "reasoning": 1,
            "cache": {"read": 2, "write": 1}
        }
    })
    .to_string();
    connection
        .execute(
            "INSERT INTO session_message (id, session_id, type, seq, time_created, data)
             VALUES (?1, ?2, 'assistant', 0, 1763000000000, ?3)",
            rusqlite::params![format!("{session}-message"), session, data],
        )
        .unwrap();
}

fn gemini_session_bytes(session: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "sessionId": session,
        "messages": [{
            "id": format!("{session}-message"),
            "timestamp": "2026-11-13T10:00:00Z",
            "model": "gemini-2.5-pro",
            "tokens": {"input": 10, "output": 4, "cached": 2, "thoughts": 1, "total": 15}
        }]
    }))
    .unwrap()
}

fn gemini_jsonl_bytes(session: &str) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(&serde_json::json!({
        "type": "init",
        "session_id": session,
        "model": "gemini-2.5-flash"
    }))
    .unwrap();
    bytes.push(b'\n');
    bytes.extend(
        serde_json::to_vec(&serde_json::json!({
            "type": "gemini",
            "id": "event-1",
            "timestamp": "2026-11-13T10:00:00Z",
            "tokens": {"input": 10, "output": 4, "cached": 2, "thoughts": 1, "total": 15}
        }))
        .unwrap(),
    );
    bytes.push(b'\n');
    bytes
}

fn cursor_cache_bytes(session: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "usageEventsDisplay": [{
            "usageEventId": format!("{session}-event"),
            "conversationId": session,
            "timestamp": 1_794_564_000_000_i64,
            "model": "claude-sonnet-4-5",
            "tokenUsage": {"inputTokens": 10, "outputTokens": 4}
        }]
    }))
    .unwrap()
}

fn cline_messages_bytes(session: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "sessionId": session,
        "agent": "main",
        "messages": [{
            "id": format!("{session}-message"),
            "role": "assistant",
            "ts": 1_794_564_000_000_i64,
            "modelInfo": {"id": "claude-sonnet-4-5", "provider": "anthropic"},
            "metrics": {
                "inputTokens": 100,
                "outputTokens": 20,
                "cacheReadTokens": 10,
                "cacheWriteTokens": 5
            }
        }]
    }))
    .unwrap()
}

fn amp_thread_bytes(session: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "id": session,
        "created": 1_794_564_000_000_i64,
        "usageLedger": {"events": [{
            "model": "claude-sonnet-4-5",
            "timestamp": "2026-11-13T10:00:00Z",
            "tokens": {"input": 10, "output": 4, "cacheReadInputTokens": 2}
        }]}
    }))
    .unwrap()
}

fn kilo_database(path: &Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let connection = Connection::open(path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE message (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                data TEXT NOT NULL
             );",
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO message (id, session_id, data) VALUES ('m1', 's1', ?1)",
            [serde_json::json!({
                "role": "assistant",
                "modelID": "minimax/m2.5",
                "providerID": "kilo",
                "tokens": {
                    "input": 10,
                    "output": 4,
                    "reasoning": 1,
                    "cache": {"read": 2, "write": 1}
                },
                "time": {"created": 1_794_564_000_000_i64}
            })
            .to_string()],
        )
        .unwrap();
}

fn goose_database(path: &Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let connection = Connection::open(path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE sessions (
                id TEXT PRIMARY KEY,
                model_config_json TEXT,
                provider_name TEXT,
                created_at TEXT NOT NULL,
                total_tokens INTEGER,
                input_tokens INTEGER,
                output_tokens INTEGER,
                accumulated_total_tokens INTEGER,
                accumulated_input_tokens INTEGER,
                accumulated_output_tokens INTEGER
             );",
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO sessions VALUES (
                'goose-session', '{\"model_name\":\"claude-sonnet-4-5\"}', 'anthropic',
                '2026-11-13T10:00:00Z', 150, 100, 50, 150, 100, 50
             )",
            [],
        )
        .unwrap();
}

fn openclaw_agent_store(path: &Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let connection = Connection::open(path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE transcript_events (
                session_id TEXT NOT NULL,
                seq INTEGER NOT NULL,
                event_json TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                PRIMARY KEY (session_id, seq)
             );",
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO transcript_events VALUES ('sess-store', 0, ?1, 1756548000000)",
            [r#"{"type":"message","id":"s1","message":{"role":"assistant","provider":"anthropic","model":"claude-opus-4-6","usage":{"input":7,"output":3},"timestamp":1756548000000}}"#],
        )
        .unwrap();
}

fn hermes_database(path: &Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let connection = Connection::open(path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE sessions (
                id TEXT PRIMARY KEY,
                model TEXT,
                billing_provider TEXT,
                started_at REAL NOT NULL,
                input_tokens INTEGER,
                output_tokens INTEGER,
                cache_read_tokens INTEGER,
                cache_write_tokens INTEGER,
                reasoning_tokens INTEGER
             );",
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO sessions VALUES (
                'hermes-session', 'claude-sonnet-4-5', 'anthropic',
                1794564000.0, 100, 50, 20, 10, 5
             )",
            [],
        )
        .unwrap();
}

fn copilot_otel_bytes(session: &str) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(&serde_json::json!({
        "type": "span",
        "traceId": "trace-1",
        "spanId": "span-1",
        "name": "chat gpt-5",
        "startTime": "2026-11-13T10:00:00Z",
        "attributes": {
            "gen_ai.operation.name": "chat",
            "gen_ai.response.model": "gpt-5",
            "gen_ai.conversation.id": session,
            "gen_ai.response.id": "resp-1",
            "gen_ai.usage.input_tokens": 10,
            "gen_ai.usage.output_tokens": 4
        }
    }))
    .unwrap();
    bytes.push(b'\n');
    bytes
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

    let report = discover(&tree.paths(&["codex", "claude", "kimi"])).unwrap();
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
                client: "kimi".to_owned(),
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
fn opencode_databases_are_discovered_only_at_the_xdg_data_role() {
    let tree = Tree::new();
    let root = tree.directory(".local/share/opencode");
    opencode_database(&root.join("opencode.db"), "session-a");
    opencode_database(&root.join("opencode-stable.db"), "session-b");
    tree.file(".local/share/opencode/opencode.db-journal", b"sidecar");
    tree.file(".local/share/opencode/unrelated.db", b"ignored");
    tree.file(".local/share/opencode/opencode-nightly.db", b"not sqlite");

    let mut paths = tree.paths(&["opencode"]);
    let report = discover(&paths).unwrap();
    assert_eq!(report.sources.len(), 2);
    assert!(report.sources.iter().all(|source| {
        source.format == SourceFormat::OpenCodeSqlite && source.session == "opencode"
    }));
    assert!(report.notices.iter().any(|notice| matches!(
        notice,
        DiscoveryNotice::SourceInvalid {
            client: "opencode",
            ..
        }
    )));

    paths.enabled_clients = BTreeSet::new();
    assert!(discover(&paths).unwrap().sources.is_empty());
}

#[test]
fn gemini_transcripts_are_discovered_only_at_tmp_roles() {
    let tree = Tree::new();
    tree.file(
        ".gemini/tmp/acme/chats/session-a.json",
        &gemini_session_bytes("session-a"),
    );
    tree.file(
        ".gemini/tmp/session-b.json",
        &gemini_session_bytes("session-b"),
    );
    tree.file(
        ".gemini/tmp/acme/chats/session-c.jsonl",
        &gemini_jsonl_bytes("session-c"),
    );
    tree.file(
        ".gemini/tmp/acme/not-a-transcript.json",
        &gemini_session_bytes("ignored"),
    );

    let mut paths = tree.paths(&["gemini"]);
    let report = discover(&paths).unwrap();
    assert_eq!(report.sources.len(), 3);
    assert!(report.sources.iter().all(|source| {
        source.format == SourceFormat::GeminiTranscript && !source.session.is_empty()
    }));

    paths.enabled_clients = BTreeSet::new();
    assert!(discover(&paths).unwrap().sources.is_empty());
}

#[test]
fn cursor_cache_is_discovered_only_at_exact_json_roles() {
    let tree = Tree::new();
    tree.file(
        ".config/tokscale/cursor-cache/usage.json",
        &cursor_cache_bytes("conv-active"),
    );
    tree.file(
        ".config/tokscale/cursor-cache/usage.account-a.json",
        &cursor_cache_bytes("conv-account"),
    );
    tree.file(".config/tokscale/cursor-cache/usage.csv", b"Date,Model\n");
    tree.file(
        ".config/tokscale/cursor-cache/archive/usage.json",
        &cursor_cache_bytes("archived"),
    );

    let mut paths = tree.paths(&["cursor"]);
    let report = discover(&paths).unwrap();
    assert_eq!(report.sources.len(), 2);
    assert!(report.sources.iter().all(|source| {
        source.format == SourceFormat::CursorCacheJson && source.session == "cursor"
    }));

    paths.enabled_clients = BTreeSet::new();
    assert!(discover(&paths).unwrap().sources.is_empty());
}

#[test]
fn cline_cli_messages_use_the_documented_root_precedence() {
    let tree = Tree::new();
    tree.file(
        ".cline/data/sessions/default-session/default-session.messages.json",
        &cline_messages_bytes("default-session"),
    );
    tree.file(
        ".config/Code/User/globalStorage/saoudrizwan.claude-dev/tasks/task-a/ui_messages.json",
        b"[]",
    );
    let mut paths = tree.paths(&["cline"]);
    let report = discover(&paths).unwrap();
    assert_eq!(report.sources.len(), 1);
    assert_eq!(report.sources[0].format, SourceFormat::ClineJson);
    assert_eq!(report.sources[0].session, "default-session");

    let override_root = tree.directory("session-override");
    tree.file(
        "session-override/override-session/override-session.messages.json",
        &cline_messages_bytes("override-session"),
    );
    paths
        .env_paths
        .insert("CLINE_SESSION_DATA_DIR".to_owned(), override_root.clone());
    paths
        .env_paths
        .insert("CLINE_DATA_DIR".to_owned(), tree.0.join("data-override"));
    paths
        .env_paths
        .insert("CLINE_DIR".to_owned(), tree.0.join("dir-override"));
    let report = discover(&paths).unwrap();
    assert_eq!(report.sources.len(), 1);
    assert_eq!(report.sources[0].session, "override-session");
}

#[test]
fn amp_threads_are_discovered_from_both_thread_roots() {
    let tree = Tree::new();
    tree.file(
        ".local/share/amp/threads/thread-a.json",
        &amp_thread_bytes("thread-a"),
    );
    tree.file(
        ".config/tokscale/amp-cache/threads/thread-b.json",
        &amp_thread_bytes("thread-b"),
    );
    tree.file(".local/share/amp/threads/ignored.txt", b"ignored");

    let mut paths = tree.paths(&["amp"]);
    let report = discover(&paths).unwrap();
    assert_eq!(report.sources.len(), 2);
    assert!(
        report
            .sources
            .iter()
            .all(|source| { source.format == SourceFormat::AmpJson && !source.session.is_empty() })
    );

    paths.enabled_clients = BTreeSet::new();
    assert!(discover(&paths).unwrap().sources.is_empty());
}

#[test]
fn kilo_database_is_discovered_at_the_xdg_data_role() {
    let tree = Tree::new();
    kilo_database(&tree.0.join(".local/share/kilo/kilo.db"));

    let mut paths = tree.paths(&["kilo"]);
    let report = discover(&paths).unwrap();
    assert_eq!(report.sources.len(), 1);
    assert_eq!(report.sources[0].format, SourceFormat::KiloSqlite);
    assert_eq!(report.sources[0].session, "kilo");

    paths.enabled_clients = BTreeSet::new();
    assert!(discover(&paths).unwrap().sources.is_empty());
}

#[test]
fn goose_database_uses_the_first_existing_or_authoritative_override() {
    let tree = Tree::new();
    goose_database(&tree.0.join(".local/share/goose/sessions/sessions.db"));

    let mut paths = tree.paths(&["goose"]);
    let report = discover(&paths).unwrap();
    assert_eq!(report.sources.len(), 1);
    assert_eq!(report.sources[0].format, SourceFormat::GooseSqlite);

    let override_root = tree.directory("goose-override");
    goose_database(&override_root.join("data/sessions/sessions.db"));
    paths
        .env_paths
        .insert("GOOSE_PATH_ROOT".to_owned(), override_root.clone());
    let report = discover(&paths).unwrap();
    assert_eq!(report.sources.len(), 1);
    assert_eq!(report.sources[0].format, SourceFormat::GooseSqlite);

    paths.enabled_clients = BTreeSet::new();
    assert!(discover(&paths).unwrap().sources.is_empty());
}

#[test]
fn openclaw_transcripts_and_agent_stores_are_discovered_under_agents() {
    let tree = Tree::new();
    tree.file(
        ".openclaw/agents/main/sessions/sess-a.jsonl",
        br#"{"type":"model_change","provider":"anthropic","modelId":"claude-sonnet-4-5"}
{"type":"message","id":"m1","message":{"role":"assistant","usage":{"input":100,"output":50},"timestamp":1700000000000}}
"#,
    );
    tree.file(
        ".openclaw/agents/main/sessions/archive-tier.sess-b.jsonl.reset.2026-08-30T10-00-00.000Z",
        br#"{"type":"message","id":"m2","message":{"role":"assistant","provider":"openai","model":"gpt-5.2","usage":{"input":10,"output":5},"timestamp":1700000000000}}
"#,
    );
    openclaw_agent_store(
        &tree
            .0
            .join(".openclaw/agents/main/agent/openclaw-agent.sqlite"),
    );
    // Not usage in either store.
    tree.file(".openclaw/agents/main/sessions/sessions.json", b"{}");
    tree.file(
        ".openclaw/agents/main/agent/incognito-openclaw-agent.sqlite",
        b"",
    );
    tree.file(".openclaw/agents/main/state/notes.txt", b"ignored");

    let mut paths = tree.paths(&["openclaw"]);
    let report = discover(&paths).unwrap();
    let mut formats: Vec<SourceFormat> =
        report.sources.iter().map(|source| source.format).collect();
    formats.sort();
    assert_eq!(
        formats,
        vec![
            SourceFormat::OpenClawJsonl,
            SourceFormat::OpenClawJsonl,
            SourceFormat::OpenClawSqlite,
        ],
        "the agent store and both of its transcripts are separate sources"
    );
    assert!(
        report
            .sources
            .iter()
            .all(|source| source.session == "openclaw")
    );

    paths.enabled_clients = BTreeSet::new();
    assert!(discover(&paths).unwrap().sources.is_empty());
}

#[test]
fn hermes_state_database_is_discovered_at_the_home_and_profile_roles() {
    let tree = Tree::new();
    hermes_database(&tree.0.join(".hermes/state.db"));

    let mut paths = tree.paths(&["hermes"]);
    let report = discover(&paths).unwrap();
    assert_eq!(report.sources.len(), 1);
    assert_eq!(report.sources[0].format, SourceFormat::HermesSqlite);

    // A profile with its own database is a second source.
    hermes_database(&tree.0.join(".hermes/profiles/work/state.db"));
    let report = discover(&paths).unwrap();
    assert_eq!(report.sources.len(), 2);
    assert!(
        report
            .sources
            .iter()
            .all(|source| source.format == SourceFormat::HermesSqlite)
    );

    // An absolute HERMES_HOME outranks the home directory.
    let override_root = tree.directory("hermes-override");
    hermes_database(&override_root.join("state.db"));
    paths
        .env_paths
        .insert("HERMES_HOME".to_owned(), override_root.clone());
    let report = discover(&paths).unwrap();
    assert_eq!(report.sources.len(), 1);

    paths.enabled_clients = BTreeSet::new();
    assert!(discover(&paths).unwrap().sources.is_empty());
}

#[test]
fn copilot_otel_jsonl_is_discovered_under_the_otel_role() {
    let tree = Tree::new();
    tree.file(
        ".copilot/otel/copilot-otel.jsonl",
        &copilot_otel_bytes("copilot-session"),
    );
    tree.file(".copilot/otel/ignored.json", b"{}");

    let mut paths = tree.paths(&["copilot"]);
    let report = discover(&paths).unwrap();
    assert_eq!(report.sources.len(), 1);
    assert_eq!(report.sources[0].format, SourceFormat::CopilotJsonl);
    assert_eq!(report.sources[0].session, "copilot-session");

    paths.enabled_clients = BTreeSet::new();
    assert!(discover(&paths).unwrap().sources.is_empty());
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

#[test]
fn pi_family_and_tencent_buddy_jsonl_sources_use_their_catalog_roots() {
    let tree = Tree::new();
    let pi = br#"{"type":"session","id":"pi-session","timestamp":"2026-11-13T10:00:00Z","cwd":"/tmp"}
{"type":"message","id":"b1","timestamp":"2026-11-13T10:00:01Z","message":{"role":"assistant","model":"claude-opus-5","usage":{"input":1,"output":2,"cacheRead":0,"cacheWrite":0}}}
"#;
    let omp = br#"{"type":"session","id":"omp-session","cwd":"/tmp"}
{"type":"message","id":"b1","timestamp":"2026-11-13T10:00:01Z","message":{"role":"assistant","model":"claude-opus-5","usage":{"input":1,"output":2}}}
"#;
    let senpi = br#"{"type":"session","id":"senpi-session","cwd":"/tmp"}
{"type":"message","id":"b1","timestamp":"2026-11-13T10:00:01Z","message":{"role":"assistant","model":"claude-opus-5","usage":{"input":1,"output":2}}}
"#;
    let buddy = br#"{"id":"a1","timestamp":1780000000000,"type":"message","role":"assistant","sessionId":"buddy-session","providerData":{"model":"glm-5.2","messageId":"m1"},"message":{"usage":{"input_tokens":4,"output_tokens":1}}}
"#;
    tree.file(".pi/agent/sessions/project/session.jsonl", pi);
    tree.file(".omp/agent/sessions/project/session.jsonl", omp);
    tree.file(".senpi/agent/sessions/project/session.jsonl", senpi);
    tree.file(".codebuddy/projects/project/session.jsonl", buddy);
    tree.file(".workbuddy/projects/project/session.jsonl", buddy);
    tree.file(".workbuddy-ai/projects/project/session.jsonl", buddy);

    let report = discover(&tree.paths(&["pi", "omp", "senpi", "codebuddy", "workbuddy"])).unwrap();
    let found = report
        .sources
        .iter()
        .map(|source| (source.format, source.session.as_str()))
        .collect::<BTreeSet<_>>();
    assert!(found.contains(&(SourceFormat::PiJsonl, "pi-session")));
    assert!(found.contains(&(SourceFormat::OmpJsonl, "omp-session")));
    assert!(found.contains(&(SourceFormat::SenpiJsonl, "senpi-session")));
    assert!(found.contains(&(SourceFormat::CodeBuddyJsonl, "buddy-session")));
    assert!(found.contains(&(SourceFormat::WorkBuddyJsonl, "buddy-session")));
}

#[test]
fn catalog_jsonl_roots_are_discovered_at_their_documented_locations() {
    let tree = Tree::new();
    tree.file(
        ".config/tokscale/antigravity-cache/cache.jsonl",
        br#"{"type":"usage","sessionId":"ag-session","timestamp":1780000000000,"input":2,"output":1}
"#,
    );
    tree.file(
        ".qwen/projects/project/chats/qwen.jsonl",
        br#"{"type":"assistant","sessionId":"qwen-session","timestamp":"2026-11-13T10:00:00Z","usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":1}}
"#,
    );
    tree.file(
        ".reasonix/stats/2026-11-13.jsonl",
        br#"{"ts":"2026-11-13T10:00:00Z","model":"deepseek/chat","prompt":5,"completion":1,"total":6}
"#,
    );
    tree.file(
        ".local/share/muse/sessions/2026/11/13/muse-session/session.jsonl",
        br#"{"recorded_at":1789790455896395,"sequence":1,"payload":{"event":{"kind":"model_completed","model":"muse-spark","usage":{"input_tokens":2,"output_tokens":1}}}}
"#,
    );
    tree.file(
        ".minimax/v2/sessions/2026/11/13/mcode.jsonl",
        br#"{"type":"exec.completed","turnId":"t1","sessionId":"mcode-session","timestampMs":1780000000000,"result":{"model":{"providerId":"minimax","modelId":"MiniMax-M2"},"usage":{"inputTokens":2,"outputTokens":1}}}
"#,
    );

    let report =
        discover(&tree.paths(&["antigravity", "qwen", "reasonix", "muse", "mcode"])).unwrap();
    let found = report
        .sources
        .iter()
        .map(|source| (source.format, source.session.as_str()))
        .collect::<BTreeSet<_>>();
    assert!(found.contains(&(SourceFormat::AntigravityJsonl, "ag-session")));
    assert!(found.contains(&(SourceFormat::QwenJsonl, "qwen-session")));
    assert!(found.contains(&(SourceFormat::MuseJsonl, "muse-session")));
    assert!(found.contains(&(SourceFormat::McodeJsonl, "mcode-session")));
    assert!(
        found
            .iter()
            .any(|(format, _)| *format == SourceFormat::ReasonixJsonl)
    );
}

#[test]
fn zed_threads_database_is_discovered_at_the_xdg_data_role() {
    let tree = Tree::new();
    let path = tree.0.join(".local/share/zed/threads/threads.db");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY,
                updated_at TEXT NOT NULL,
                created_at TEXT,
                data_type TEXT NOT NULL,
                data BLOB NOT NULL
            );",
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO threads VALUES ('thread-1', '2026-11-13T10:00:00Z', NULL, 'json', ?1)",
            [br#"{"model":{"provider":"zed.dev","model":"m"},"request_token_usage":{"0":{"input_tokens":2,"output_tokens":1}}}"#.as_slice()],
        )
        .unwrap();
    drop(connection);

    let report = discover(&tree.paths(&["zed"])).unwrap();
    assert_eq!(report.sources.len(), 1);
    assert_eq!(report.sources[0].format, SourceFormat::ZedSqlite);
    assert_eq!(report.sources[0].session, "zed");
}

#[test]
fn codebuddy_extension_logs_are_discovered_beside_projects() {
    let tree = Tree::new();
    tree.file(
        ".local/share/CodeBuddyExtension/Logs/main.log",
        concat!(
            "[2026/7/1 16:56:01.100] [Info] [CraftInvokableAgent] [agent-1]  Model prepared: Kimi-K2.7-Code (kimi-k2.7)\n",
            "[2026/7/1 16:56:02.200] [Info] [AgentReporter] [agent-1]  Agent execution successful with usage: {\"inputTokens\":10,\"outputTokens\":2}\n"
        )
        .as_bytes(),
    );

    let report = discover(&tree.paths(&["codebuddy"])).unwrap();
    assert_eq!(report.sources.len(), 1);
    assert_eq!(report.sources[0].format, SourceFormat::CodeBuddyJsonl);
    assert_eq!(report.sources[0].session, "main");
}

#[test]
fn workbuddy_database_is_only_a_fallback_when_no_jsonl_exists() {
    let tree = Tree::new();
    let db = tree.0.join(".workbuddy/workbuddy.db");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    let connection = Connection::open(&db).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, cwd TEXT, model TEXT);
             CREATE TABLE session_usage (
                 session_id TEXT PRIMARY KEY, used INTEGER, size INTEGER,
                 updated_at INTEGER, credit_json TEXT
             );
             INSERT INTO sessions VALUES ('db-session', NULL, 'deepseek-v4-pro');
             INSERT INTO session_usage VALUES ('db-session', 42, 1000, 1780000000000, '{}');",
        )
        .unwrap();
    drop(connection);

    let report = discover(&tree.paths(&["workbuddy"])).unwrap();
    assert_eq!(report.sources.len(), 1);
    assert_eq!(report.sources[0].format, SourceFormat::WorkBuddySqlite);

    tree.file(
        ".workbuddy/projects/project/session.jsonl",
        br#"{"id":"a1","timestamp":1780000000000,"type":"message","role":"assistant","sessionId":"jsonl-session","providerData":{"model":"m","messageId":"m1"},"message":{"usage":{"input_tokens":4,"output_tokens":1}}}
"#,
    );
    let report = discover(&tree.paths(&["workbuddy"])).unwrap();
    assert_eq!(report.sources.len(), 1);
    assert_eq!(report.sources[0].format, SourceFormat::WorkBuddyJsonl);
}
