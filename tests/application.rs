use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use token_monitor_lite::application::{
    ApplicationCommand, ApplicationNotice, ApplicationService, CredentialAvailability,
    SettingsEditor, SettingsField, SettingsReason, SourceAvailability,
};
use token_monitor_lite::discovery::{HostPaths, HostPlatform, discover};
use token_monitor_lite::storage::{
    ActivationState, DATABASE_FILE_NAME, ImportState, PreserveState,
};
use token_monitor_lite::storage::{NativeSettings, StoreHandle};

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Tree(PathBuf);

impl Tree {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "token-monitor-application-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
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
            enabled_clients: BTreeSet::from(["claude".to_owned()]),
        }
    }

    fn claude_source(&self) -> PathBuf {
        let path = self
            .0
            .join(".claude/projects/private-project/private-session.jsonl");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            br#"{"type":"assistant","sessionId":"private-session","timestamp":"2026-11-09T10:00:00Z","message":{"id":"response-a","model":"claude","usage":{"input_tokens":7}}}
"#,
        )
        .unwrap();
        path
    }

    fn file(&self, relative: &str, bytes: &[u8]) -> PathBuf {
        let path = self.0.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
        path
    }

    fn database(&self) -> PathBuf {
        self.0.join("native-private").join(DATABASE_FILE_NAME)
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        if self.0.exists() {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }
}

#[test]
fn valid_draft_uses_only_current_opaque_candidates_and_preserves_unknown_settings() {
    let tree = Tree::new();
    let source_path = tree.claude_source();
    let discovery = discover(&tree.paths()).unwrap();
    let (store, _) = StoreHandle::open(discovery.native.clone()).unwrap();
    let stored = store
        .save_settings(
            0,
            NativeSettings::from_json_bytes(
                br#"{"clients":"claude","timezone":"UTC","future":{"value":null}}"#,
            )
            .unwrap(),
        )
        .unwrap();
    let editor = SettingsEditor::new(&discovery, &stored).unwrap();
    let view = editor.view();
    assert_eq!(view.revision, 1);
    assert_eq!(view.sources.len(), 1);
    assert!(!view.sources[0].id.contains("private"));
    assert!(!format!("{view:?}").contains(source_path.to_str().unwrap()));

    let mut draft = editor.open_draft();
    draft.timezone = "America/New_York".to_owned();
    draft.selected_source_ids = vec![view.sources[0].id.clone()];
    let next = editor.validate(&draft).unwrap();
    let value: serde_json::Value = serde_json::from_slice(next.json_bytes()).unwrap();
    assert_eq!(value["clients"], "claude");
    assert_eq!(value["timezone"], "America/New_York");
    assert_eq!(value["future"], serde_json::json!({"value":null}));
    assert_eq!(value["nativeSourceIds"].as_array().unwrap().len(), 1);
    assert!(!String::from_utf8_lossy(next.json_bytes()).contains(source_path.to_str().unwrap()));
    store.shutdown().unwrap();
}

#[test]
fn invalid_clients_timezone_and_sources_return_only_stable_field_reasons() {
    let tree = Tree::new();
    tree.claude_source();
    let discovery = discover(&tree.paths()).unwrap();
    let (store, snapshot) = StoreHandle::open(discovery.native.clone()).unwrap();
    let editor = SettingsEditor::new(&discovery, &snapshot).unwrap();
    let mut draft = editor.open_draft();
    draft.enabled_clients = vec![
        "claude".to_owned(),
        "claude".to_owned(),
        "future-reader".to_owned(),
    ];
    draft.timezone = "Not/A_Timezone".to_owned();
    draft.selected_source_ids = vec!["source-not-in-discovery".to_owned()];

    let error = editor.validate(&draft).unwrap_err();
    assert!(error.has(SettingsField::EnabledClients, SettingsReason::Duplicate));
    assert!(error.has(SettingsField::EnabledClients, SettingsReason::Unsupported));
    assert!(error.has(SettingsField::Timezone, SettingsReason::InvalidTimezone));
    assert!(error.has(SettingsField::Sources, SettingsReason::Unavailable));
    let rendered = format!("{error:?}");
    assert!(!rendered.contains("future-reader"));
    assert!(!rendered.contains("source-not-in-discovery"));
    assert!(!rendered.contains(tree.0.to_str().unwrap()));
    store.shutdown().unwrap();
}

#[test]
fn stale_draft_is_rejected_before_building_a_settings_document() {
    let tree = Tree::new();
    let discovery = discover(&tree.paths()).unwrap();
    let (store, snapshot) = StoreHandle::open(discovery.native.clone()).unwrap();
    let editor = SettingsEditor::new(&discovery, &snapshot).unwrap();
    let mut stale = editor.open_draft();
    stale.base_revision = 99;
    stale.timezone = "UTC".to_owned();

    let error = editor.validate(&stale).unwrap_err();
    assert_eq!(error.issues().len(), 1);
    assert!(error.has(SettingsField::Revision, SettingsReason::Stale));
    store.shutdown().unwrap();
}

#[test]
fn draft_cancel_and_save_keep_committed_state_separate() {
    let tree = Tree::new();
    let discovery = discover(&tree.paths()).unwrap();
    let (store, _) = StoreHandle::open(discovery.native.clone()).unwrap();
    let application = ApplicationService::start(discovery, store.clone()).unwrap();
    let initial = application.snapshot().unwrap();
    let mut draft = initial.settings.open_draft();
    draft.enabled_clients = vec!["claude".to_owned()];
    draft.timezone = "America/New_York".to_owned();

    assert_eq!(application.snapshot().unwrap().settings.revision, 0);
    assert_eq!(store.snapshot().unwrap().settings_revision, 0);
    application
        .submit(ApplicationCommand::CancelSettings { base_revision: 0 })
        .unwrap();
    wait_for(&application, |snapshot| {
        snapshot.revision > initial.revision
    });
    assert_eq!(store.snapshot().unwrap().settings_revision, 0);
    assert!(application.snapshot().unwrap().draft.is_none());

    application
        .submit(ApplicationCommand::SaveSettings(draft))
        .unwrap();
    let saved = wait_for(&application, |snapshot| snapshot.settings.revision == 1);
    assert_eq!(saved.settings.enabled_clients, ["claude"]);
    assert_eq!(saved.settings.timezone, "America/New_York");
    assert!(saved.draft.is_none());
    assert!(saved.notice.is_none());
    assert_eq!(store.snapshot().unwrap().settings_revision, 1);
    application.shutdown().unwrap();
}

#[test]
fn stale_save_cannot_overwrite_and_preserves_user_input_with_conflict_notice() {
    let tree = Tree::new();
    let discovery = discover(&tree.paths()).unwrap();
    let (store, _) = StoreHandle::open(discovery.native.clone()).unwrap();
    let application = ApplicationService::start(discovery, store.clone()).unwrap();
    let first = application.snapshot().unwrap().settings.open_draft();
    let mut stale = first.clone();
    stale.timezone = "America/Los_Angeles".to_owned();
    application
        .submit(ApplicationCommand::SaveSettings(first))
        .unwrap();
    wait_for(&application, |snapshot| snapshot.settings.revision == 1);
    application
        .submit(ApplicationCommand::SaveSettings(stale.clone()))
        .unwrap();
    let conflict = wait_for(&application, |snapshot| {
        snapshot.notice == Some(ApplicationNotice::Conflict)
    });

    assert_eq!(conflict.settings.revision, 1);
    assert_eq!(conflict.draft, Some(stale));
    assert_eq!(store.snapshot().unwrap().settings_revision, 1);
    application.shutdown().unwrap();
}

#[test]
fn invalid_save_retains_draft_without_writing_or_starting_a_monitor() {
    let tree = Tree::new();
    tree.claude_source();
    let discovery = discover(&tree.paths()).unwrap();
    let (store, _) = StoreHandle::open(discovery.native.clone()).unwrap();
    let application = ApplicationService::start(discovery, store.clone()).unwrap();
    let mut invalid = application.snapshot().unwrap().settings.open_draft();
    invalid.selected_source_ids = vec!["missing-source".to_owned()];
    application
        .submit(ApplicationCommand::SaveSettings(invalid.clone()))
        .unwrap();
    let rejected = wait_for(&application, |snapshot| {
        matches!(snapshot.notice, Some(ApplicationNotice::InvalidSettings(_)))
    });

    assert_eq!(rejected.draft, Some(invalid));
    assert_eq!(rejected.settings.revision, 0);
    assert!(rejected.monitor.is_none());
    assert_eq!(store.snapshot().unwrap().settings_revision, 0);
    application.shutdown().unwrap();
}

#[test]
fn accepted_source_selection_starts_monitor_and_deselection_stops_it() {
    let tree = Tree::new();
    let source_path = tree.claude_source();
    let discovery = discover(&tree.paths()).unwrap();
    let (store, _) = StoreHandle::open(discovery.native.clone()).unwrap();
    let application = ApplicationService::start(discovery, store.clone()).unwrap();
    let initial = application.snapshot().unwrap();
    let mut selected = initial.settings.open_draft();
    selected.enabled_clients = vec!["claude".to_owned()];
    selected.selected_source_ids = vec![initial.settings.sources[0].id.clone()];
    application
        .submit(ApplicationCommand::SaveSettings(selected))
        .unwrap();
    let monitoring = wait_for(&application, |snapshot| {
        snapshot
            .monitor
            .as_ref()
            .is_some_and(|monitor| monitor.settings_generation == 1)
    });
    assert_eq!(
        monitoring.source_availability,
        SourceAvailability::Monitoring
    );
    assert_eq!(
        monitoring.monitor.as_ref().unwrap().canonical.totals.input,
        7
    );
    let first_monitor = monitoring.monitor.as_ref().unwrap().clone();
    OpenOptions::new()
        .append(true)
        .open(source_path)
        .unwrap()
        .write_all(
            br#"{"type":"assistant","sessionId":"private-session","timestamp":"2026-11-09T10:01:00Z","message":{"id":"response-b","model":"claude","usage":{"input_tokens":3}}}
"#,
        )
        .unwrap();
    application.submit(ApplicationCommand::Refresh).unwrap();
    let corrected = wait_for(&application, |snapshot| {
        snapshot.monitor.as_ref().is_some_and(|monitor| {
            monitor.runtime_generation > first_monitor.runtime_generation
                && monitor.canonical.totals.input == 10
        })
    });
    assert_eq!(first_monitor.canonical.totals.input, 7);

    let mut deselected = corrected.settings.open_draft();
    deselected.selected_source_ids.clear();
    application
        .submit(ApplicationCommand::SaveSettings(deselected))
        .unwrap();
    let unavailable = wait_for(&application, |snapshot| {
        snapshot.settings.revision == 2
            && snapshot.source_availability == SourceAvailability::Unavailable
            && snapshot.monitor.is_none()
    });
    assert!(unavailable.notice.is_none());
    assert_eq!(store.snapshot().unwrap().settings_generation, 2);
    thread::sleep(Duration::from_millis(100));
    assert!(application.snapshot().unwrap().monitor.is_none());
    application.shutdown().unwrap();
}

#[test]
fn restart_publishes_retained_runtime_when_selected_source_is_unavailable() {
    let tree = Tree::new();
    let source_path = tree.claude_source();
    let application = ApplicationService::bootstrap(tree.paths()).unwrap();
    let initial = application.snapshot().unwrap();
    let mut selected = initial.settings.open_draft();
    selected.enabled_clients = vec!["claude".to_owned()];
    selected.selected_source_ids = vec![initial.settings.sources[0].id.clone()];
    application
        .submit(ApplicationCommand::SaveSettings(selected))
        .unwrap();
    let committed = wait_for(&application, |snapshot| {
        snapshot
            .monitor
            .as_ref()
            .is_some_and(|monitor| monitor.canonical.totals.input == 7)
    });
    assert!(committed.monitor.as_ref().unwrap().runtime_initialized);
    application.shutdown().unwrap();
    fs::remove_file(source_path).unwrap();

    let restarted = ApplicationService::bootstrap(tree.paths()).unwrap();
    let retained = wait_for(&restarted, |snapshot| {
        snapshot.monitor.as_ref().is_some_and(|monitor| {
            monitor.canonical.totals.input == 7
                && monitor.source_health.iter().any(|source| {
                    source.status == token_monitor_lite::runtime::SourceHealthStatus::Retained
                })
        })
    });
    assert_eq!(retained.source_availability, SourceAvailability::Monitoring);
    assert!(retained.notice.is_none());
    restarted.shutdown().unwrap();
}

#[test]
fn failed_runtime_storage_keeps_the_last_committed_application_monitor() {
    let tree = Tree::new();
    let source_path = tree.claude_source();
    let discovery = discover(&tree.paths()).unwrap();
    let (store, _) = StoreHandle::open(discovery.native.clone()).unwrap();
    let application = ApplicationService::start(discovery, store.clone()).unwrap();
    let initial = application.snapshot().unwrap();
    let mut selected = initial.settings.open_draft();
    selected.enabled_clients = vec!["claude".to_owned()];
    selected.selected_source_ids = vec![initial.settings.sources[0].id.clone()];
    application
        .submit(ApplicationCommand::SaveSettings(selected))
        .unwrap();
    let committed = wait_for(&application, |snapshot| {
        snapshot
            .monitor
            .as_ref()
            .is_some_and(|monitor| monitor.canonical.totals.input == 7)
    });
    let committed_monitor = committed.monitor.as_ref().unwrap().clone();

    store.shutdown().unwrap();
    OpenOptions::new()
        .append(true)
        .open(source_path)
        .unwrap()
        .write_all(
            br#"{"type":"assistant","sessionId":"private-session","timestamp":"2026-11-09T10:01:00Z","message":{"id":"response-b","model":"claude","usage":{"input_tokens":3}}}
"#,
        )
        .unwrap();
    let retained = wait_for(&application, |snapshot| {
        snapshot.monitor.as_ref().is_some_and(|monitor| {
            monitor.source_health.iter().any(|source| {
                source.status == token_monitor_lite::runtime::SourceHealthStatus::PersistenceDelayed
            })
        })
    });
    let retained_monitor = retained.monitor.as_ref().unwrap();
    assert_eq!(
        retained_monitor.runtime_generation,
        committed_monitor.runtime_generation
    );
    assert_eq!(retained_monitor.canonical.totals.input, 7);
    assert_eq!(committed_monitor.canonical.totals.input, 7);
    application.shutdown().unwrap();
}

#[test]
fn bootstrap_handles_first_run_restart_and_credential_free_migration_once() {
    let first_run = Tree::new();
    let application = ApplicationService::bootstrap(first_run.paths()).unwrap();
    let initial = application.snapshot().unwrap();
    assert_eq!(initial.settings.revision, 0);
    assert!(initial.import_report.items.is_empty());
    let mut draft = initial.settings.open_draft();
    draft.enabled_clients = vec!["claude".to_owned()];
    draft.timezone = "America/New_York".to_owned();
    application
        .submit(ApplicationCommand::SaveSettings(draft))
        .unwrap();
    wait_for(&application, |snapshot| snapshot.settings.revision == 1);
    application.shutdown().unwrap();

    let restarted = ApplicationService::bootstrap(first_run.paths()).unwrap();
    let restored = restarted.snapshot().unwrap();
    assert_eq!(restored.settings.revision, 1);
    assert_eq!(restored.settings.timezone, "America/New_York");
    restarted.shutdown().unwrap();

    let migrated = Tree::new();
    migrated.file(
        ".config/Token Monitor/settings.json",
        br#"{"clients":"claude","future":{"retained":true}}"#,
    );
    let application = ApplicationService::bootstrap(migrated.paths()).unwrap();
    let snapshot = application.snapshot().unwrap();
    assert_eq!(snapshot.settings.revision, 1);
    assert_eq!(snapshot.settings.enabled_clients, ["claude"]);
    assert!(snapshot.import_report.items.iter().any(|item| {
        item.preservation == PreserveState::Preserved
            && item.import == ImportState::Imported
            && item.activation == ActivationState::Activated
    }));
    application.shutdown().unwrap();

    let restarted = ApplicationService::bootstrap(migrated.paths()).unwrap();
    assert_eq!(restarted.snapshot().unwrap().import_report.items.len(), 1);
    restarted.shutdown().unwrap();
}

#[test]
fn bootstrap_reports_credential_pending_unsupported_and_invalid_legacy_data() {
    let credential = Tree::new();
    credential.file(
        ".config/Token Monitor/settings.json",
        br#"{"clients":"claude","clineApiKey":"PRIVATE_BOOTSTRAP_SENTINEL"}"#,
    );
    let application = ApplicationService::bootstrap(credential.paths()).unwrap();
    let snapshot = application.snapshot().unwrap();
    if snapshot.credential_status.availability == CredentialAvailability::Available {
        assert_eq!(snapshot.settings.revision, 1);
        assert!(snapshot.import_report.items.iter().any(|item| {
            item.preservation == PreserveState::Preserved
                && item.import == ImportState::Imported
                && item.activation == ActivationState::Activated
        }));
    } else {
        assert_eq!(snapshot.settings.revision, 0);
        assert!(snapshot.import_report.items.iter().any(|item| {
            item.preservation == PreserveState::Preserved
                && item.import == ImportState::NotAttempted
                && item.activation == ActivationState::Inactive
        }));
    }
    assert!(!format!("{snapshot:?}").contains("PRIVATE_BOOTSTRAP_SENTINEL"));
    application.shutdown().unwrap();

    let unsupported = Tree::new();
    unsupported.file(
        "legacy-shared/daily-history-archive.json",
        br#"{"future":"preserved"}"#,
    );
    let application = ApplicationService::bootstrap(unsupported.paths()).unwrap();
    let snapshot = application.snapshot().unwrap();
    assert!(snapshot.import_report.items.iter().any(|item| {
        item.preservation == PreserveState::Preserved
            && item.import == ImportState::Unsupported
            && item.activation == ActivationState::Inactive
    }));
    application.shutdown().unwrap();

    let invalid = Tree::new();
    invalid.file(
        ".config/Token Monitor/settings.json",
        br#"{"clients":["not-a-csv"]}"#,
    );
    let application = ApplicationService::bootstrap(invalid.paths()).unwrap();
    let snapshot = application.snapshot().unwrap();
    assert_eq!(snapshot.settings.revision, 0);
    assert!(snapshot.import_report.items.iter().any(|item| {
        item.preservation == PreserveState::Preserved
            && item.import == ImportState::Failed
            && item.activation == ActivationState::Inactive
    }));
    application.shutdown().unwrap();
}

#[test]
fn bootstrap_recovers_verified_backup_and_rejects_irreplaceable_database() {
    let recovered = Tree::new();
    let discovery = discover(&recovered.paths()).unwrap();
    let (store, _) = StoreHandle::open(discovery.native).unwrap();
    store
        .save_settings(
            0,
            NativeSettings::from_json_bytes(br#"{"clients":"claude","timezone":"UTC"}"#).unwrap(),
        )
        .unwrap();
    store.create_backup().unwrap();
    store.shutdown().unwrap();
    fs::write(recovered.database(), b"corrupt active database").unwrap();
    let application = ApplicationService::bootstrap(recovered.paths()).unwrap();
    assert_eq!(application.snapshot().unwrap().settings.revision, 1);
    application.shutdown().unwrap();

    let irreplaceable = Tree::new();
    let discovery = discover(&irreplaceable.paths()).unwrap();
    let (store, _) = StoreHandle::open(discovery.native).unwrap();
    store.shutdown().unwrap();
    fs::write(irreplaceable.database(), b"corrupt without backup").unwrap();
    assert!(matches!(
        ApplicationService::bootstrap(irreplaceable.paths()),
        Err(token_monitor_lite::application::ApplicationError::StorageOpen)
    ));
}

#[test]
fn cas_and_storage_failures_retain_the_last_good_application_snapshot() {
    let conflict_tree = Tree::new();
    let discovery = discover(&conflict_tree.paths()).unwrap();
    let (store, _) = StoreHandle::open(discovery.native.clone()).unwrap();
    let application = ApplicationService::start(discovery, store.clone()).unwrap();
    let mut stale = application.snapshot().unwrap().settings.open_draft();
    stale.timezone = "America/Los_Angeles".to_owned();
    store
        .save_settings(
            0,
            NativeSettings::from_json_bytes(br#"{"timezone":"UTC"}"#).unwrap(),
        )
        .unwrap();
    application
        .submit(ApplicationCommand::SaveSettings(stale.clone()))
        .unwrap();
    let conflict = wait_for(&application, |snapshot| {
        snapshot.notice == Some(ApplicationNotice::Conflict) && snapshot.settings.revision == 1
    });
    assert_eq!(conflict.draft, Some(stale));
    application.shutdown().unwrap();

    let failed_tree = Tree::new();
    let discovery = discover(&failed_tree.paths()).unwrap();
    let (store, _) = StoreHandle::open(discovery.native.clone()).unwrap();
    let application = ApplicationService::start(discovery, store.clone()).unwrap();
    let mut first = application.snapshot().unwrap().settings.open_draft();
    first.timezone = "America/New_York".to_owned();
    application
        .submit(ApplicationCommand::SaveSettings(first))
        .unwrap();
    let last_good = wait_for(&application, |snapshot| snapshot.settings.revision == 1);
    store.shutdown().unwrap();
    let mut failed = last_good.settings.open_draft();
    failed.timezone = "America/Chicago".to_owned();
    application
        .submit(ApplicationCommand::SaveSettings(failed.clone()))
        .unwrap();
    let retained = wait_for(&application, |snapshot| {
        snapshot.notice == Some(ApplicationNotice::StorageUnavailable)
    });
    assert_eq!(retained.settings, last_good.settings);
    assert_eq!(retained.draft, Some(failed));
    application.shutdown().unwrap();
}

fn wait_for(
    application: &ApplicationService,
    ready: impl Fn(&token_monitor_lite::application::ApplicationSnapshot) -> bool,
) -> std::sync::Arc<token_monitor_lite::application::ApplicationSnapshot> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let snapshot = application.snapshot().unwrap();
        if ready(&snapshot) {
            return snapshot;
        }
        assert!(Instant::now() < deadline, "application snapshot timed out");
        thread::sleep(Duration::from_millis(10));
    }
}
