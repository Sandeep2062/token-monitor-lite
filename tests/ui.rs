use std::path::PathBuf;
use std::sync::Arc;

use token_monitor_lite::accounting::{SessionKey, TokenCounts};
use token_monitor_lite::aggregation::{
    CanonicalSnapshot, DayModelRollup, SessionDayRollup, SessionModelRollup, SourceRef,
};
use token_monitor_lite::analytics::UsagePeriod;
use token_monitor_lite::application::{
    ApplicationNotice, ApplicationSnapshot, CredentialAvailability, CredentialStatus,
    ImportReportItemView, ImportReportView, SettingsDraft, SettingsView, SourceAvailability,
    SourceChoice,
};
use token_monitor_lite::runtime::{
    MonitorSnapshot, RuntimeMetrics, SourceHealth, SourceHealthStatus, WatchStatus,
};
use token_monitor_lite::sources::{SourceFormat, SourceId};
use token_monitor_lite::storage::{ActivationState, ImportState, PreserveState};
use token_monitor_lite::ui::{SESSION_PAGE_SIZE, SnapshotMailbox, SourceSeverity, UiController};

fn session(
    client: &str,
    session: &str,
    model: &str,
    last_seen_ms: i64,
    tokens: TokenCounts,
) -> SessionModelRollup {
    SessionModelRollup {
        key: SessionKey::new(client, session),
        model: model.to_owned(),
        tokens,
        cache_write_1h: 0,
        first_seen_ms: last_seen_ms,
        last_seen_ms,
        sources: Arc::<[SourceRef]>::from([]),
    }
}

fn health(
    path: &str,
    format: SourceFormat,
    status: SourceHealthStatus,
    source_generation: Option<u64>,
) -> SourceHealth {
    SourceHealth {
        source_id: SourceId::for_path(format, &PathBuf::from(path)),
        format,
        status,
        source_generation,
    }
}

fn snapshot(
    generation: u64,
    totals: TokenCounts,
    sessions: Vec<SessionModelRollup>,
    source_health: Vec<SourceHealth>,
    watch_status: WatchStatus,
) -> Arc<MonitorSnapshot> {
    let session_days = sessions
        .iter()
        .map(|row| SessionDayRollup {
            day: chrono::DateTime::from_timestamp_millis(row.last_seen_ms)
                .expect("test timestamp")
                .date_naive(),
            key: row.key.clone(),
            tokens: row.tokens,
            cache_write_1h: row.cache_write_1h,
        })
        .collect::<Vec<_>>();
    let day_models = sessions
        .iter()
        .map(|row| DayModelRollup {
            day: chrono::DateTime::from_timestamp_millis(row.last_seen_ms)
                .expect("test timestamp")
                .date_naive(),
            client: row.key.client.clone(),
            model: row.model.clone(),
            tokens: row.tokens,
            cache_write_1h: row.cache_write_1h,
            first_seen_ms: row.first_seen_ms,
            last_seen_ms: row.last_seen_ms,
            sources: row.sources.clone(),
        })
        .collect::<Vec<_>>();
    Arc::new(MonitorSnapshot {
        generation,
        runtime_generation: generation,
        runtime_initialized: true,
        settings_generation: 1,
        day_generation: 1,
        pricing_generation: 1,
        canonical: Arc::new(CanonicalSnapshot {
            generation,
            timezone_name: "UTC".to_owned(),
            totals,
            cache_write_1h: 0,
            day_models: day_models.into(),
            session_models: sessions.into(),
            session_days: session_days.into(),
            retained_periods: Arc::from([]),
            sources: Arc::from([]),
        }),
        source_health: source_health.into(),
        watch_status,
        metrics: RuntimeMetrics::default(),
    })
}

fn application_snapshot(
    revision: u64,
    settings_revision: u64,
    timezone: &str,
    draft: Option<SettingsDraft>,
    notice: Option<ApplicationNotice>,
    import_report: ImportReportView,
) -> Arc<ApplicationSnapshot> {
    Arc::new(ApplicationSnapshot {
        revision,
        settings: SettingsView {
            revision: settings_revision,
            enabled_clients: vec!["claude".to_owned()],
            timezone: timezone.to_owned(),
            selected_source_ids: vec![SourceId::from_bytes([1; 16]).as_storage_key()],
            sources: vec![
                SourceChoice {
                    id: SourceId::from_bytes([1; 16]).as_storage_key(),
                    client: "claude".to_owned(),
                },
                SourceChoice {
                    id: SourceId::from_bytes([2; 16]).as_storage_key(),
                    client: "codex".to_owned(),
                },
                SourceChoice {
                    id: SourceId::from_bytes([3; 16]).as_storage_key(),
                    client: "mixed".to_owned(),
                },
            ],
        },
        draft,
        import_report,
        monitor: None,
        source_availability: SourceAvailability::Unavailable,
        credential_status: CredentialStatus {
            availability: CredentialAvailability::Available,
            pending: false,
        },
        notice,
    })
}

#[test]
fn settings_keep_committed_and_draft_values_distinct_until_save_or_cancel() {
    let mut controller = UiController::new();
    assert!(controller.accept_application_snapshot(application_snapshot(
        1,
        7,
        "UTC",
        None,
        None,
        ImportReportView::default(),
    )));

    let initial = controller.presentation().settings;
    assert_eq!(initial.committed_timezone, "UTC");
    assert_eq!(initial.draft_timezone, "UTC");
    assert!(!initial.save_enabled);
    assert!(!initial.cancel_enabled);
    assert!(
        initial
            .sources
            .iter()
            .any(|row| { row.client == "Session archive" && row.status == "Available" })
    );

    controller.set_settings_timezone("Not/A_Timezone");
    let invalid = controller.presentation().settings;
    assert_eq!(invalid.committed_timezone, "UTC");
    assert_eq!(invalid.draft_timezone, "Not/A_Timezone");
    assert!(!invalid.save_enabled);
    assert!(invalid.cancel_enabled);
    assert_eq!(invalid.validation_notice, "Enter a valid IANA timezone.");

    controller.set_settings_timezone("America/New_York");
    controller.set_settings_client_enabled("codex", true);
    controller.select_all_settings_sources();
    assert_eq!(
        controller
            .settings_save_draft()
            .unwrap()
            .selected_source_ids
            .len(),
        3,
        "select-all includes both CLI sources and the mixed archive"
    );
    let edited = controller.presentation().settings;
    assert!(edited.save_enabled);
    assert!(edited.cancel_enabled);
    assert_eq!(
        controller.settings_save_draft().unwrap().timezone,
        "America/New_York"
    );

    controller.cancel_settings_draft();
    let cancelled = controller.presentation().settings;
    assert_eq!(cancelled.draft_timezone, "UTC");
    assert!(!cancelled.save_enabled);
    assert!(!cancelled.cancel_enabled);
    assert!(
        !cancelled
            .clients
            .iter()
            .find(|row| row.id == "codex")
            .unwrap()
            .enabled
    );
}

#[test]
fn settings_conflict_retains_user_input_with_a_short_redacted_notice() {
    let mut controller = UiController::new();
    controller.accept_application_snapshot(application_snapshot(
        1,
        7,
        "UTC",
        None,
        None,
        ImportReportView::default(),
    ));
    let retained = SettingsDraft {
        base_revision: 7,
        enabled_clients: vec!["claude".to_owned()],
        timezone: "America/Los_Angeles".to_owned(),
        selected_source_ids: vec![SourceId::from_bytes([1; 16]).as_storage_key()],
    };
    assert!(controller.accept_application_snapshot(application_snapshot(
        2,
        8,
        "America/Chicago",
        Some(retained),
        Some(ApplicationNotice::Conflict),
        ImportReportView::default(),
    )));

    let settings = controller.presentation().settings;
    assert_eq!(settings.committed_timezone, "America/Chicago");
    assert_eq!(settings.draft_timezone, "America/Los_Angeles");
    assert_eq!(
        settings.notice,
        "Settings changed elsewhere. Your edits are retained."
    );
    assert!(!settings.save_enabled);
    assert!(settings.cancel_enabled);
}

#[test]
fn migration_rows_label_preservation_import_and_activation_separately() {
    let mut controller = UiController::new();
    controller.accept_application_snapshot(application_snapshot(
        1,
        0,
        "UTC",
        None,
        None,
        ImportReportView {
            items: vec![
                ImportReportItemView {
                    artifact_kind: "settings".to_owned(),
                    preservation: PreserveState::Preserved,
                    import: ImportState::Imported,
                    activation: ActivationState::Activated,
                    records: 1,
                },
                ImportReportItemView {
                    artifact_kind: "plugin-package".to_owned(),
                    preservation: PreserveState::Preserved,
                    import: ImportState::Unsupported,
                    activation: ActivationState::Inactive,
                    records: 0,
                },
            ],
        },
    ));

    let rows = controller.presentation().settings.import_rows;
    assert_eq!(rows[0].preservation, "Preserved");
    assert_eq!(rows[0].semantic_import, "Imported");
    assert_eq!(rows[0].activation, "Active");
    assert_eq!(rows[1].preservation, "Preserved");
    assert_eq!(rows[1].semantic_import, "Unsupported");
    assert_eq!(rows[1].activation, "Inactive");
}

#[test]
fn application_projection_drops_private_backend_values_and_unknown_identifiers() {
    let private_path = "/home/alice/private/settings.json";
    let secret = "PRIVATE_UI_SECRET_SENTINEL";
    let credential_id = "credential-account-private";
    let mut input = application_snapshot(
        1,
        1,
        private_path,
        None,
        Some(ApplicationNotice::StorageUnavailable),
        ImportReportView {
            items: vec![ImportReportItemView {
                artifact_kind: format!("{secret}-{credential_id}"),
                preservation: PreserveState::Preserved,
                import: ImportState::Unsupported,
                activation: ActivationState::Inactive,
                records: 0,
            }],
        },
    );
    Arc::make_mut(&mut input)
        .settings
        .sources
        .push(SourceChoice {
            id: format!("{private_path}-{secret}"),
            client: "claude".to_owned(),
        });

    let mut controller = UiController::new();
    controller.accept_application_snapshot(input);
    let rendered = format!("{:?}", controller.presentation());
    assert!(!rendered.contains(private_path));
    assert!(!rendered.contains(secret));
    assert!(!rendered.contains(credential_id));
    assert!(rendered.contains("Settings storage unavailable"));
    assert!(rendered.contains("Retained legacy item"));
}

#[test]
fn exact_values_and_current_reader_components_remain_truthful() {
    let counts = TokenCounts {
        input: 9_007_199_254_740_993,
        output: 7,
        cache_read: 3,
        cache_write: 2,
        reasoning: 1,
    };
    let mut controller = UiController::new();
    controller.set_clock_ms(10);
    assert!(controller.accept_snapshot(snapshot(
        1,
        counts,
        vec![session("claude", "s", "sonnet", 10, counts)],
        Vec::new(),
        WatchStatus::Available,
    )));

    let presentation = controller.presentation();
    assert_eq!(presentation.home.input, "9,007,199,254,740,993");
    assert_eq!(presentation.home.tracked_tokens, "9,007,199,254,741,006");
    assert_eq!(presentation.home.cache, "5");
    assert_eq!(presentation.home.reasoning, "1");
    assert_eq!(presentation.home.cost, "Unavailable");
    assert!(presentation.home.has_usage);
}

#[test]
fn home_period_controls_switch_between_calendar_windows() {
    let day = |value: &str| {
        chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")
            .expect("test date")
            .and_hms_opt(12, 0, 0)
            .expect("test time")
            .and_utc()
            .timestamp_millis()
    };
    let mut controller = UiController::new();
    controller.set_clock_ms(day("2026-11-13"));
    controller.accept_snapshot(snapshot(
        1,
        TokenCounts::default(),
        vec![
            session(
                "claude",
                "today",
                "sonnet",
                day("2026-11-13"),
                TokenCounts {
                    input: 7,
                    ..TokenCounts::default()
                },
            ),
            session(
                "claude",
                "month",
                "sonnet",
                day("2026-11-01"),
                TokenCounts {
                    input: 11,
                    ..TokenCounts::default()
                },
            ),
            session(
                "claude",
                "previous",
                "sonnet",
                day("2026-10-31"),
                TokenCounts {
                    input: 13,
                    ..TokenCounts::default()
                },
            ),
        ],
        Vec::new(),
        WatchStatus::Available,
    ));

    let today = controller.presentation();
    assert_eq!(today.home.period_index, 0);
    assert_eq!(today.home.period_heading, "TRACKED TOKENS · TODAY");
    assert_eq!(today.home.input, "7");

    controller.set_period(UsagePeriod::Month);
    let month = controller.presentation();
    assert_eq!(month.home.period_index, 1);
    assert_eq!(month.home.input, "18");

    controller.set_period_index(2);
    let all = controller.presentation();
    assert_eq!(all.home.input, "31");
    assert_eq!(all.home.period_heading, "TRACKED TOKENS · ALL CAPTURED");

    controller.set_period_index(9);
    assert_eq!(controller.presentation().home.period_index, 2);
}

#[test]
fn trends_are_contiguous_and_scale_to_the_busiest_day() {
    let day = |value: &str| {
        chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")
            .expect("test date")
            .and_hms_opt(12, 0, 0)
            .expect("test time")
            .and_utc()
            .timestamp_millis()
    };
    let mut controller = UiController::new();
    controller.set_clock_ms(day("2026-11-13"));
    controller.accept_snapshot(snapshot(
        1,
        TokenCounts::default(),
        vec![
            session(
                "claude",
                "today",
                "sonnet",
                day("2026-11-13"),
                TokenCounts {
                    input: 10,
                    ..TokenCounts::default()
                },
            ),
            session(
                "claude",
                "yesterday",
                "sonnet",
                day("2026-11-12"),
                TokenCounts {
                    input: 20,
                    ..TokenCounts::default()
                },
            ),
        ],
        Vec::new(),
        WatchStatus::Available,
    ));

    let trends = controller.presentation().trends;
    assert_eq!(trends.rows.len(), token_monitor_lite::ui::TREND_DAYS);
    assert!(trends.headline.contains("last 30 days"));
    assert_eq!(trends.rows[28].label, "2026-11-12");
    assert_eq!(trends.rows[28].total, "20");
    assert_eq!(trends.rows[28].permille, 1_000);
    assert_eq!(trends.rows[29].label, "2026-11-13");
    assert_eq!(trends.rows[29].total, "10");
    assert_eq!(trends.rows[29].permille, 500);
    assert_eq!(trends.rows[0].total, "0");
}

#[test]
fn model_breakdown_tracks_the_selected_period_and_share() {
    let day = |value: &str| {
        chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")
            .expect("test date")
            .and_hms_opt(12, 0, 0)
            .expect("test time")
            .and_utc()
            .timestamp_millis()
    };
    let mut controller = UiController::new();
    controller.set_clock_ms(day("2026-11-13"));
    controller.accept_snapshot(snapshot(
        1,
        TokenCounts::default(),
        vec![
            session(
                "claude",
                "a",
                "sonnet",
                day("2026-11-13"),
                TokenCounts {
                    input: 10,
                    ..TokenCounts::default()
                },
            ),
            session(
                "claude",
                "b",
                "opus",
                day("2026-11-02"),
                TokenCounts {
                    input: 20,
                    ..TokenCounts::default()
                },
            ),
            session(
                "codex",
                "c",
                "haiku",
                day("2026-10-20"),
                TokenCounts {
                    input: 30,
                    ..TokenCounts::default()
                },
            ),
        ],
        Vec::new(),
        WatchStatus::Available,
    ));

    let today = controller.presentation().models;
    assert!(today.headline.contains("Today"));
    assert_eq!(today.rows.len(), 1);
    assert_eq!(today.rows[0].label, "claude · sonnet");

    controller.set_period(UsagePeriod::Month);
    let month = controller.presentation().models;
    assert!(month.headline.contains("Month"));
    assert_eq!(
        month
            .rows
            .iter()
            .map(|row| (row.label.as_str(), row.total.as_str()))
            .collect::<Vec<_>>(),
        vec![("claude · opus", "20"), ("claude · sonnet", "10")]
    );
    assert_eq!(month.rows[0].permille, 666);
    assert_eq!(month.rows[1].permille, 333);

    controller.set_period(UsagePeriod::AllTime);
    assert_eq!(
        controller.presentation().models.rows[0].label,
        "codex · haiku"
    );
}

#[test]
fn empty_data_is_distinct_from_a_captured_zero_row() {
    let mut controller = UiController::new();
    assert!(!controller.presentation().home.has_usage);
    assert_eq!(
        controller.presentation().home.empty_message,
        "No sources configured"
    );

    assert!(controller.accept_snapshot(snapshot(
        1,
        TokenCounts::default(),
        vec![session(
            "claude",
            "captured-zero",
            "sonnet",
            10,
            TokenCounts::default(),
        )],
        vec![health(
            "/secret/project/transcript.jsonl",
            SourceFormat::ClaudeCli,
            SourceHealthStatus::Healthy,
            Some(1),
        )],
        WatchStatus::Available,
    )));
    controller.set_clock_ms(10);
    let presentation = controller.presentation();
    assert!(presentation.home.has_usage);
    assert_eq!(presentation.home.tracked_tokens, "0");
    assert_eq!(presentation.home.empty_message, "");
}

#[test]
fn sessions_are_sorted_searched_and_paged_without_source_work() {
    let sessions = (0..55)
        .map(|index| {
            session(
                if index == 54 { "Codex" } else { "claude" },
                &format!("session-{index:02}"),
                if index == 54 { "GPT-5" } else { "sonnet" },
                index,
                TokenCounts {
                    input: index as u64,
                    ..TokenCounts::default()
                },
            )
        })
        .collect();
    let mut controller = UiController::new();
    controller.accept_snapshot(snapshot(
        1,
        TokenCounts::default(),
        sessions,
        Vec::new(),
        WatchStatus::Available,
    ));

    let first = controller.presentation();
    assert_eq!(first.sessions.len(), SESSION_PAGE_SIZE);
    assert_eq!(first.sessions[0].identity.session, "session-54");
    assert!(first.can_next);
    assert!(!first.can_previous);
    assert_eq!(first.page_label, "1 / 2");

    controller.next_page();
    controller.next_page();
    let second = controller.presentation();
    assert_eq!(second.sessions.len(), 5);
    assert!(second.can_previous);
    assert!(!second.can_next);

    controller.previous_page();
    controller.previous_page();
    let clamped_first = controller.presentation();
    assert_eq!(clamped_first.page_label, "1 / 2");
    assert!(!clamped_first.can_previous);

    controller.set_search("cOdEx gPt-5");
    let filtered = controller.presentation();
    assert_eq!(filtered.page_label, "1 / 1");
    assert_eq!(filtered.sessions.len(), 1);
    assert_eq!(filtered.sessions[0].identity.session, "session-54");
}

#[test]
fn selection_survives_only_while_stable_identity_exists() {
    let selected = session("claude", "selected", "sonnet", 10, TokenCounts::default());
    let mut controller = UiController::new();
    controller.accept_snapshot(snapshot(
        1,
        TokenCounts::default(),
        vec![selected.clone()],
        Vec::new(),
        WatchStatus::Available,
    ));
    let key = controller.presentation().sessions[0].selection_key.clone();
    assert!(!controller.select_session("unknown identity"));
    assert!(controller.select_session(&key));

    controller.accept_snapshot(snapshot(
        2,
        TokenCounts::default(),
        vec![selected],
        Vec::new(),
        WatchStatus::Available,
    ));
    assert_eq!(
        controller
            .presentation()
            .selected_session
            .as_ref()
            .map(|identity| identity.session.as_str()),
        Some("selected")
    );

    controller.accept_snapshot(snapshot(
        3,
        TokenCounts::default(),
        Vec::new(),
        Vec::new(),
        WatchStatus::Available,
    ));
    let cleared = controller.presentation();
    assert!(cleared.selected_session.is_none());
    assert_eq!(
        cleared.selection_notice.as_deref(),
        Some("The selected session is no longer in the captured snapshot.")
    );

    controller.accept_snapshot(snapshot(
        4,
        TokenCounts::default(),
        Vec::new(),
        Vec::new(),
        WatchStatus::Available,
    ));
    assert!(controller.presentation().selection_notice.is_none());
}

#[test]
fn selected_session_exposes_bounded_detail_and_can_be_cleared() {
    let mut controller = UiController::new();
    controller.set_clock_ms(10);
    controller.accept_snapshot(snapshot(
        1,
        TokenCounts::default(),
        vec![
            session(
                "claude",
                "shared",
                "sonnet",
                10,
                TokenCounts {
                    input: 10,
                    ..TokenCounts::default()
                },
            ),
            session(
                "claude",
                "shared",
                "opus",
                10,
                TokenCounts {
                    input: 30,
                    ..TokenCounts::default()
                },
            ),
        ],
        Vec::new(),
        WatchStatus::Available,
    ));
    let key = controller.presentation().sessions[0].selection_key.clone();
    assert!(controller.select_session(&key));
    let detail = controller.presentation().detail.expect("selected detail");
    assert!(detail.heading.contains("shared"));
    let selected = controller
        .presentation()
        .selected_session
        .expect("selected identity");
    assert_eq!(detail.selected_model, selected.model);
    assert_eq!(detail.tracked, "40");
    assert_eq!(detail.input, "40");
    assert_eq!(detail.models.len(), 2);
    assert_eq!(detail.models[0].label, "opus");
    assert_eq!(detail.models[0].permille, 750);

    controller.clear_session_selection();
    assert!(controller.presentation().detail.is_none());
    assert!(controller.presentation().selected_session.is_none());
}

#[test]
fn update_state_tracks_check_download_and_install_outcomes() {
    let mut controller = UiController::new();
    assert!(!controller.presentation().update.busy);

    controller.set_update_checking();
    assert!(controller.presentation().update.busy);
    assert!(!controller.presentation().update.available);

    controller.set_update_available("0.2.0");
    let available = controller.presentation().update;
    assert!(available.available);
    assert!(!available.busy);
    assert!(available.status.contains("0.2.0"));

    controller.set_update_downloading();
    assert!(controller.presentation().update.busy);

    controller.set_update_manual("C:\\updates\\setup.exe");
    let manual = controller.presentation().update;
    assert!(!manual.available);
    assert!(!manual.busy);
    assert!(manual.status.contains("setup.exe"));

    controller.set_update_unavailable();
    assert!(
        controller
            .presentation()
            .update
            .status
            .contains("unavailable")
    );
}

#[test]
fn stale_monitor_generation_cannot_replace_current_presentation() {
    let mut controller = UiController::new();
    controller.set_clock_ms(10);
    let counts = TokenCounts {
        input: 2,
        ..TokenCounts::default()
    };
    assert!(controller.accept_snapshot(snapshot(
        2,
        counts,
        vec![session("claude", "current", "sonnet", 10, counts)],
        Vec::new(),
        WatchStatus::Available,
    )));
    let stale = TokenCounts {
        input: 99,
        ..TokenCounts::default()
    };
    assert!(!controller.accept_snapshot(snapshot(
        1,
        stale,
        vec![session("claude", "stale", "sonnet", 10, stale)],
        Vec::new(),
        WatchStatus::Available,
    )));
    assert_eq!(controller.presentation().home.input, "2");
}

#[test]
fn source_health_is_redacted_and_distinguishes_recovery_states() {
    let sources = vec![
        health(
            "/Users/alice/secret/one.jsonl",
            SourceFormat::ClaudeCli,
            SourceHealthStatus::Pending,
            None,
        ),
        health(
            "C:\\Users\\alice\\private\\two.jsonl",
            SourceFormat::CodexCli,
            SourceHealthStatus::Healthy,
            Some(2),
        ),
        health(
            "/private/three.jsonl",
            SourceFormat::ClaudeCli,
            SourceHealthStatus::ReadFailed,
            Some(1),
        ),
        health(
            "/private/four.jsonl",
            SourceFormat::CodexCli,
            SourceHealthStatus::ReadFailed,
            None,
        ),
        health(
            "/private/five.jsonl",
            SourceFormat::ClaudeCli,
            SourceHealthStatus::PersistenceDelayed,
            Some(1),
        ),
        health(
            "/private/six.jsonl",
            SourceFormat::CodexCli,
            SourceHealthStatus::DecodingFailed,
            None,
        ),
    ];
    let mut controller = UiController::new();
    controller.accept_snapshot(snapshot(
        1,
        TokenCounts::default(),
        Vec::new(),
        sources,
        WatchStatus::Available,
    ));
    let rows = controller.presentation().sources;
    assert_eq!(rows[0].name, "Source 1");
    assert_eq!(rows[0].severity, SourceSeverity::Waiting);
    assert_eq!(rows[1].format, "Codex CLI");
    assert_eq!(rows[1].severity, SourceSeverity::Healthy);
    assert_eq!(rows[2].severity, SourceSeverity::Retained);
    assert_eq!(rows[3].severity, SourceSeverity::Failed);
    assert_eq!(rows[4].severity, SourceSeverity::Retained);
    assert_eq!(rows[4].status, "Persistence delayed");
    assert!(rows[4].detail.contains("Last-good generation 1"));
    assert_eq!(rows[5].severity, SourceSeverity::Failed);
    assert_eq!(rows[5].status, "Decoding failed");
    let rendered = format!("{rows:?}");
    assert!(!rendered.contains("alice"));
    assert!(!rendered.contains("private"));

    controller.accept_snapshot(snapshot(
        2,
        TokenCounts::default(),
        Vec::new(),
        vec![health(
            "/still/secret.jsonl",
            SourceFormat::ClaudeCli,
            SourceHealthStatus::Healthy,
            Some(3),
        )],
        WatchStatus::Unavailable,
    ));
    assert_eq!(
        controller.presentation().sources[0].severity,
        SourceSeverity::WatcherUnavailable
    );
}

#[test]
fn mailbox_coalesces_to_one_latest_scheduled_snapshot() {
    let mailbox = SnapshotMailbox::default();
    let mut schedule_requests = 0;
    for generation in 1..=10_000 {
        if mailbox.submit(snapshot(
            generation,
            TokenCounts::default(),
            Vec::new(),
            Vec::new(),
            WatchStatus::Available,
        )) {
            schedule_requests += 1;
        }
    }
    assert_eq!(schedule_requests, 1);
    assert_eq!(mailbox.take().unwrap().generation, 10_000);
    assert!(!mailbox.finish_delivery());

    assert!(mailbox.submit(snapshot(
        10_001,
        TokenCounts::default(),
        Vec::new(),
        Vec::new(),
        WatchStatus::Available,
    )));
    assert_eq!(mailbox.take().unwrap().generation, 10_001);
}
