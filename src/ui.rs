//! Pure bounded presentation over immutable monitor snapshots. This module
//! performs no I/O and never exposes source paths or parser diagnostics.

use crate::accounting::TokenCounts;
use crate::aggregation::BucketTimezone;
use crate::aggregation::SessionModelRollup;
use crate::analytics::{UsagePeriod, daily_series, model_breakdown, period_totals};
use crate::application::{
    ApplicationNotice, ApplicationSnapshot, CredentialAvailability, SettingsDraft, SettingsView,
    SourceChoice,
};
use crate::runtime::{MonitorSnapshot, SourceHealthStatus, WatchStatus};
use crate::sources::{SourceFormat, SourceId};
use crate::storage::{ActivationState, ImportState, PreserveState};
use chrono::DateTime;
use std::cmp::Reverse;
use std::collections::BTreeSet;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub const SESSION_PAGE_SIZE: usize = 50;
pub const HOME_RECENT_LIMIT: usize = 5;
pub const TREND_DAYS: usize = 30;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SessionIdentity {
    pub client: String,
    pub session: String,
    pub model: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionRow {
    pub identity: SessionIdentity,
    pub selection_key: String,
    pub last_activity: String,
    pub input: String,
    pub output: String,
    pub cache: String,
    pub reasoning: String,
    pub tracked_tokens: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceSeverity {
    Healthy,
    Waiting,
    Retained,
    WatcherUnavailable,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceHealthRow {
    pub name: String,
    pub format: String,
    pub status: String,
    pub detail: String,
    pub severity: SourceSeverity,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HomeSummary {
    pub tracked_tokens: String,
    pub input: String,
    pub output: String,
    pub cache: String,
    pub reasoning: String,
    pub cost: String,
    pub has_usage: bool,
    pub empty_message: String,
    pub source_headline: String,
    pub period_heading: String,
    pub period_index: i32,
    pub input_permille: i32,
    pub output_permille: i32,
    pub cache_permille: i32,
    pub reasoning_permille: i32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingsClientRow {
    pub id: String,
    pub label: String,
    pub enabled: bool,
    pub committed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingsSourceRow {
    pub id: String,
    pub name: String,
    pub client: String,
    pub selected: bool,
    pub committed: bool,
    pub status: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportReportRow {
    pub artifact: String,
    pub preservation: String,
    pub semantic_import: String,
    pub activation: String,
    pub records: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingsPresentation {
    pub revision: u64,
    pub committed_timezone: String,
    pub draft_timezone: String,
    pub clients: Vec<SettingsClientRow>,
    pub sources: Vec<SettingsSourceRow>,
    pub import_rows: Vec<ImportReportRow>,
    pub credential_status: String,
    pub save_enabled: bool,
    pub cancel_enabled: bool,
    pub validation_notice: String,
    pub notice: String,
}

impl Default for SettingsPresentation {
    fn default() -> Self {
        Self {
            revision: 0,
            committed_timezone: "UTC".to_owned(),
            draft_timezone: "UTC".to_owned(),
            clients: client_rows(
                &SettingsView {
                    revision: 0,
                    enabled_clients: Vec::new(),
                    timezone: "UTC".to_owned(),
                    selected_source_ids: Vec::new(),
                    sources: Vec::new(),
                },
                None,
            ),
            sources: Vec::new(),
            import_rows: Vec::new(),
            credential_status: "Unavailable".to_owned(),
            save_enabled: false,
            cancel_enabled: false,
            validation_notice: String::new(),
            notice: "Settings are unavailable until startup completes.".to_owned(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrendRow {
    pub label: String,
    pub total: String,
    pub permille: i32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrendsPresentation {
    pub headline: String,
    pub rows: Vec<TrendRow>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelRow {
    pub label: String,
    pub total: String,
    pub permille: i32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelsPresentation {
    pub headline: String,
    pub rows: Vec<ModelRow>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionDetail {
    pub heading: String,
    pub selected_model: String,
    pub tracked: String,
    pub input: String,
    pub output: String,
    pub cache: String,
    pub reasoning: String,
    pub first_seen: String,
    pub last_seen: String,
    pub models: Vec<ModelRow>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdatePresentation {
    pub status: String,
    pub available: bool,
    pub busy: bool,
}

impl Default for UpdatePresentation {
    fn default() -> Self {
        Self {
            status: "Check GitHub Releases for a newer version.".to_owned(),
            available: false,
            busy: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiPresentation {
    pub generation: u64,
    pub home: HomeSummary,
    pub trends: TrendsPresentation,
    pub models: ModelsPresentation,
    pub detail: Option<SessionDetail>,
    pub update: UpdatePresentation,
    pub recent_sessions: Vec<SessionRow>,
    pub sessions: Vec<SessionRow>,
    pub sources: Vec<SourceHealthRow>,
    pub page_label: String,
    pub can_previous: bool,
    pub can_next: bool,
    pub search_query: String,
    pub selected_session: Option<SessionIdentity>,
    pub selection_notice: Option<String>,
    pub refresh_available: bool,
    pub watch_status: String,
    pub settings: SettingsPresentation,
}

#[derive(Default)]
pub struct UiController {
    snapshot: Option<Arc<MonitorSnapshot>>,
    application_snapshot: Option<Arc<ApplicationSnapshot>>,
    settings_draft: Option<SettingsDraft>,
    startup_notice: Option<String>,
    command_notice: Option<String>,
    search_query: String,
    page: usize,
    selected_session: Option<SessionIdentity>,
    selection_notice: Option<String>,
    period: UsagePeriod,
    clock_ms: Option<i64>,
    update: UpdatePresentation,
}

impl UiController {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn accept_snapshot(&mut self, snapshot: Arc<MonitorSnapshot>) -> bool {
        if self
            .snapshot
            .as_ref()
            .is_some_and(|current| snapshot.generation <= current.generation)
        {
            return false;
        }

        self.selection_notice = None;
        if let Some(selected) = self.selected_session.as_ref()
            && !snapshot
                .canonical
                .session_models
                .iter()
                .any(|row| identity(row) == *selected)
        {
            self.selected_session = None;
            self.selection_notice =
                Some("The selected session is no longer in the captured snapshot.".to_owned());
        }
        self.snapshot = Some(snapshot);
        self.clamp_page();
        true
    }

    pub fn accept_application_snapshot(&mut self, snapshot: Arc<ApplicationSnapshot>) -> bool {
        if self
            .application_snapshot
            .as_ref()
            .is_some_and(|current| snapshot.revision <= current.revision)
        {
            return false;
        }

        let settings_changed = self
            .application_snapshot
            .as_ref()
            .is_none_or(|current| current.settings.revision != snapshot.settings.revision);
        if let Some(draft) = snapshot.draft.clone() {
            self.settings_draft = Some(draft);
        } else if settings_changed || self.settings_draft.is_none() {
            self.settings_draft = Some(snapshot.settings.open_draft());
        }
        if let Some(monitor) = snapshot.monitor.clone() {
            self.accept_snapshot(monitor);
        }
        self.application_snapshot = Some(snapshot);
        self.startup_notice = None;
        self.command_notice = None;
        true
    }

    pub fn set_startup_notice(&mut self, notice: &str) {
        self.startup_notice = Some(notice.to_owned());
    }

    pub fn set_command_notice(&mut self, notice: &str) {
        self.command_notice = Some(notice.to_owned());
    }

    pub fn set_settings_timezone(&mut self, timezone: &str) {
        if let Some(draft) = self.settings_draft.as_mut() {
            draft.timezone.clear();
            draft.timezone.push_str(timezone);
        }
    }

    pub fn set_settings_client_enabled(&mut self, client: &str, enabled: bool) {
        if !is_supported_client(client) {
            return;
        }
        let Some(draft) = self.settings_draft.as_mut() else {
            return;
        };
        draft.enabled_clients.retain(|value| value != client);
        if enabled {
            draft.enabled_clients.push(client.to_owned());
            draft.enabled_clients.sort();
        } else {
            let available: BTreeSet<&str> = self
                .application_snapshot
                .as_ref()
                .into_iter()
                .flat_map(|snapshot| snapshot.settings.sources.iter())
                .filter(|source| source.client == client)
                .map(|source| source.id.as_str())
                .collect();
            draft
                .selected_source_ids
                .retain(|id| !available.contains(id.as_str()));
        }
    }

    pub fn select_all_settings_sources(&mut self) {
        let Some(snapshot) = self.application_snapshot.as_ref() else {
            return;
        };
        let Some(draft) = self.settings_draft.as_mut() else {
            return;
        };
        let enabled = draft.enabled_clients.clone();
        draft.selected_source_ids = snapshot
            .settings
            .sources
            .iter()
            .filter(|source| {
                safe_source(source)
                    && (source.client == "mixed" || enabled.contains(&source.client))
            })
            .map(|source| source.id.clone())
            .collect();
    }

    pub fn set_settings_source_selected(&mut self, source_id: &str, selected: bool) {
        let Some(snapshot) = self.application_snapshot.as_ref() else {
            return;
        };
        let Some(source) = snapshot
            .settings
            .sources
            .iter()
            .find(|source| source.id == source_id && safe_source(source))
        else {
            return;
        };
        let Some(draft) = self.settings_draft.as_mut() else {
            return;
        };
        draft.selected_source_ids.retain(|id| id != source_id);
        if selected && draft.enabled_clients.contains(&source.client) {
            draft.selected_source_ids.push(source_id.to_owned());
            draft.selected_source_ids.sort();
        }
    }

    pub fn settings_save_draft(&self) -> Option<SettingsDraft> {
        let snapshot = self.application_snapshot.as_ref()?;
        let draft = self.settings_draft.as_ref()?;
        (draft != &snapshot.settings.open_draft()
            && settings_validation_notice(&snapshot.settings, draft).is_none())
        .then(|| draft.clone())
    }

    pub fn settings_draft_base_revision(&self) -> Option<u64> {
        self.settings_draft
            .as_ref()
            .map(|draft| draft.base_revision)
    }

    pub fn cancel_settings_draft(&mut self) {
        if let Some(snapshot) = self.application_snapshot.as_ref() {
            self.settings_draft = Some(snapshot.settings.open_draft());
        }
    }

    pub fn set_search(&mut self, query: &str) {
        self.search_query.clear();
        self.search_query.push_str(query);
        self.page = 0;
    }

    pub fn set_period(&mut self, period: UsagePeriod) {
        self.period = period;
    }

    pub fn set_period_index(&mut self, index: usize) {
        if let Some(period) = UsagePeriod::from_index(index) {
            self.period = period;
        }
    }

    pub fn set_clock_ms(&mut self, clock_ms: i64) {
        self.clock_ms = Some(clock_ms);
    }

    pub fn set_update_checking(&mut self) {
        self.update = UpdatePresentation {
            status: "Checking for updates…".to_owned(),
            available: false,
            busy: true,
        };
    }

    pub fn set_update_available(&mut self, version: &str) {
        self.update = UpdatePresentation {
            status: format!("Update v{version} is available."),
            available: true,
            busy: false,
        };
    }

    pub fn set_update_current(&mut self, version: &str) {
        self.update = UpdatePresentation {
            status: format!("You're up to date (v{version})."),
            available: false,
            busy: false,
        };
    }

    pub fn set_update_unavailable(&mut self) {
        self.update = UpdatePresentation {
            status: "Update check unavailable. Try again later.".to_owned(),
            available: false,
            busy: false,
        };
    }

    pub fn set_update_downloading(&mut self) {
        self.update = UpdatePresentation {
            status: "Downloading and verifying the update…".to_owned(),
            available: true,
            busy: true,
        };
    }

    pub fn set_update_installing(&mut self) {
        self.update = UpdatePresentation {
            status: "Installing update. The application will close.".to_owned(),
            available: false,
            busy: true,
        };
    }

    pub fn set_update_manual(&mut self, path: &str) {
        self.update = UpdatePresentation {
            status: format!("Update downloaded to {path}. Install it manually."),
            available: false,
            busy: false,
        };
    }

    pub fn set_update_install_failed(&mut self) {
        self.update = UpdatePresentation {
            status: "Update could not be installed. Try again later.".to_owned(),
            available: false,
            busy: false,
        };
    }

    pub fn next_page(&mut self) {
        let page_count = self.page_count();
        if self.page + 1 < page_count {
            self.page += 1;
        }
    }

    pub fn previous_page(&mut self) {
        self.page = self.page.saturating_sub(1);
    }

    pub fn select_session(&mut self, selection_key: &str) -> bool {
        let Some(snapshot) = self.snapshot.as_ref() else {
            return false;
        };
        let selected = sorted_sessions(snapshot)
            .into_iter()
            .find(|row| selection_key_for(row) == selection_key)
            .map(identity);
        if selected.is_none() {
            return false;
        }
        self.selected_session = selected;
        self.selection_notice = None;
        true
    }

    pub fn clear_session_selection(&mut self) {
        self.selected_session = None;
        self.selection_notice = None;
    }

    pub fn presentation(&self) -> UiPresentation {
        let Some(snapshot) = self.snapshot.as_ref() else {
            let mut presentation = empty_presentation(&self.search_query);
            presentation.settings = self.settings_presentation();
            presentation.update = self.update.clone();
            return presentation;
        };
        let sorted = sorted_sessions(snapshot);
        let recent_sessions = sorted
            .iter()
            .take(HOME_RECENT_LIMIT)
            .map(|row| session_row(row))
            .collect();
        let filtered = sorted
            .into_iter()
            .filter(|row| matches_query(row, &self.search_query))
            .collect::<Vec<_>>();
        let page_count = page_count(filtered.len());
        let page = self.page.min(page_count.saturating_sub(1));
        let start = page.saturating_mul(SESSION_PAGE_SIZE).min(filtered.len());
        let end = start.saturating_add(SESSION_PAGE_SIZE).min(filtered.len());
        let sessions = filtered[start..end]
            .iter()
            .map(|row| session_row(row))
            .collect();
        let sources = source_rows(snapshot);

        UiPresentation {
            generation: snapshot.generation,
            home: home_summary(snapshot, &sources, self.period, self.clock_ms),
            trends: trends_presentation(snapshot, self.clock_ms),
            models: models_presentation(snapshot, self.period, self.clock_ms),
            detail: session_detail(snapshot, self.selected_session.as_ref()),
            update: self.update.clone(),
            recent_sessions,
            sessions,
            sources,
            page_label: format!("{} / {page_count}", page + 1),
            can_previous: page > 0,
            can_next: page + 1 < page_count,
            search_query: self.search_query.clone(),
            selected_session: self.selected_session.clone(),
            selection_notice: self.selection_notice.clone(),
            refresh_available: !snapshot.source_health.is_empty(),
            watch_status: match snapshot.watch_status {
                WatchStatus::Available => "Watching configured sources".to_owned(),
                WatchStatus::Unavailable => "Watcher unavailable · refresh manually".to_owned(),
            },
            settings: self.settings_presentation(),
        }
    }

    fn settings_presentation(&self) -> SettingsPresentation {
        let Some(snapshot) = self.application_snapshot.as_ref() else {
            let mut empty = SettingsPresentation::default();
            if let Some(notice) = &self.startup_notice {
                empty.notice = notice.clone();
            }
            return empty;
        };
        let draft = self
            .settings_draft
            .as_ref()
            .cloned()
            .unwrap_or_else(|| snapshot.settings.open_draft());
        let validation = settings_validation_notice(&snapshot.settings, &draft);
        let dirty = draft != snapshot.settings.open_draft();
        let sources = snapshot
            .settings
            .sources
            .iter()
            .filter(|source| safe_source(source))
            .enumerate()
            .map(|(index, source)| SettingsSourceRow {
                id: source.id.clone(),
                name: format!("Source {}", index + 1),
                client: client_label(&source.client).to_owned(),
                selected: draft.selected_source_ids.contains(&source.id),
                committed: snapshot.settings.selected_source_ids.contains(&source.id),
                status: if source.client == "mixed"
                    || draft.enabled_clients.contains(&source.client)
                {
                    "Available"
                } else {
                    "Unavailable · client disabled"
                }
                .to_owned(),
            })
            .collect();
        let import_rows = snapshot
            .import_report
            .items
            .iter()
            .map(|item| ImportReportRow {
                artifact: artifact_label(&item.artifact_kind).to_owned(),
                preservation: preservation_label(item.preservation).to_owned(),
                semantic_import: import_label(item.import).to_owned(),
                activation: activation_label(item.activation).to_owned(),
                records: format_number(u128::from(item.records)),
            })
            .collect();

        SettingsPresentation {
            revision: snapshot.settings.revision,
            committed_timezone: safe_committed_timezone(&snapshot.settings.timezone),
            draft_timezone: safe_editable_text(&draft.timezone),
            clients: client_rows(&snapshot.settings, Some(&draft)),
            sources,
            import_rows,
            credential_status: credential_status(snapshot),
            save_enabled: dirty && validation.is_none(),
            cancel_enabled: dirty,
            validation_notice: validation.unwrap_or_default(),
            notice: self.command_notice.clone().unwrap_or_else(|| {
                snapshot
                    .notice
                    .as_ref()
                    .map(application_notice)
                    .unwrap_or_default()
                    .to_owned()
            }),
        }
    }

    fn page_count(&self) -> usize {
        let count = self.snapshot.as_ref().map_or(0, |snapshot| {
            snapshot
                .canonical
                .session_models
                .iter()
                .filter(|row| matches_query(row, &self.search_query))
                .count()
        });
        page_count(count)
    }

    fn clamp_page(&mut self) {
        self.page = self.page.min(self.page_count().saturating_sub(1));
    }
}

fn credential_status(snapshot: &ApplicationSnapshot) -> String {
    match (
        snapshot.credential_status.pending,
        snapshot.credential_status.availability,
    ) {
        (true, CredentialAvailability::Available) => "Pending verification".to_owned(),
        (true, CredentialAvailability::Unavailable) => {
            "Pending · platform credential storage unavailable".to_owned()
        }
        (false, CredentialAvailability::Available) => "Available".to_owned(),
        (false, CredentialAvailability::Unavailable) => "Unavailable".to_owned(),
    }
}

pub struct SnapshotMailbox {
    pending: Mutex<Option<Arc<MonitorSnapshot>>>,
    scheduled: AtomicBool,
}

impl Default for SnapshotMailbox {
    fn default() -> Self {
        Self {
            pending: Mutex::new(None),
            scheduled: AtomicBool::new(false),
        }
    }
}

impl SnapshotMailbox {
    /// Retains only `snapshot`. `true` means the caller owns scheduling the
    /// next event-loop delivery.
    pub fn submit(&self, snapshot: Arc<MonitorSnapshot>) -> bool {
        let Ok(mut pending) = self.pending.lock() else {
            return false;
        };
        *pending = Some(snapshot);
        drop(pending);
        self.scheduled
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub fn take(&self) -> Option<Arc<MonitorSnapshot>> {
        self.pending.lock().ok()?.take()
    }

    /// Completes one delivery. `true` means a submission raced with delivery
    /// while scheduling was already owned, so the caller must schedule once.
    pub fn finish_delivery(&self) -> bool {
        self.scheduled.store(false, Ordering::Release);
        let has_pending = self.pending.lock().is_ok_and(|pending| pending.is_some());
        has_pending
            && self
                .scheduled
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
    }
}

fn empty_presentation(search_query: &str) -> UiPresentation {
    UiPresentation {
        generation: 0,
        home: HomeSummary {
            tracked_tokens: "0".to_owned(),
            input: "0".to_owned(),
            output: "0".to_owned(),
            cache: "0".to_owned(),
            reasoning: "0".to_owned(),
            cost: "Unavailable".to_owned(),
            has_usage: false,
            empty_message: "No sources configured".to_owned(),
            source_headline: "No configured sources".to_owned(),
            period_heading: "TRACKED TOKENS · TODAY".to_owned(),
            period_index: 0,
            input_permille: 0,
            output_permille: 0,
            cache_permille: 0,
            reasoning_permille: 0,
        },
        trends: TrendsPresentation {
            headline: "Daily tracked tokens · last 30 days".to_owned(),
            rows: Vec::new(),
        },
        models: ModelsPresentation {
            headline: "Model share".to_owned(),
            rows: Vec::new(),
        },
        detail: None,
        update: UpdatePresentation::default(),
        recent_sessions: Vec::new(),
        sessions: Vec::new(),
        sources: Vec::new(),
        page_label: "1 / 1".to_owned(),
        can_previous: false,
        can_next: false,
        search_query: search_query.to_owned(),
        selected_session: None,
        selection_notice: None,
        refresh_available: false,
        watch_status: "No sources configured".to_owned(),
        settings: SettingsPresentation::default(),
    }
}

fn client_rows(settings: &SettingsView, draft: Option<&SettingsDraft>) -> Vec<SettingsClientRow> {
    ["claude", "codex"]
        .into_iter()
        .map(|client| SettingsClientRow {
            id: client.to_owned(),
            label: client_label(client).to_owned(),
            enabled: draft.map_or_else(
                || settings.enabled_clients.contains(&client.to_owned()),
                |draft| draft.enabled_clients.contains(&client.to_owned()),
            ),
            committed: settings.enabled_clients.contains(&client.to_owned()),
        })
        .collect()
}

fn settings_validation_notice(settings: &SettingsView, draft: &SettingsDraft) -> Option<String> {
    if draft.base_revision != settings.revision {
        return Some("Reload or cancel before saving these edits.".to_owned());
    }
    if BucketTimezone::from_str(&draft.timezone).is_err() {
        return Some("Enter a valid IANA timezone.".to_owned());
    }
    let clients = draft.enabled_clients.iter().collect::<BTreeSet<_>>();
    if clients.len() != draft.enabled_clients.len()
        || draft
            .enabled_clients
            .iter()
            .any(|client| !is_supported_client(client))
    {
        return Some("Choose only supported clients.".to_owned());
    }
    let selected = draft.selected_source_ids.iter().collect::<BTreeSet<_>>();
    if selected.len() != draft.selected_source_ids.len()
        || draft.selected_source_ids.iter().any(|id| {
            !settings.sources.iter().any(|source| {
                source.id == *id
                    && safe_source(source)
                    && (source.client == "mixed" || draft.enabled_clients.contains(&source.client))
            })
        })
    {
        return Some("Choose only available sources.".to_owned());
    }
    None
}

fn safe_source(source: &SourceChoice) -> bool {
    is_selectable_source_client(&source.client) && SourceId::parse_storage_key(&source.id).is_some()
}

fn is_selectable_source_client(client: &str) -> bool {
    matches!(client, "claude" | "codex" | "mixed")
}

fn is_supported_client(client: &str) -> bool {
    matches!(client, "claude" | "codex")
}

fn client_label(client: &str) -> &'static str {
    match client {
        "claude" => "Claude CLI",
        "codex" => "Codex CLI",
        "mixed" => "Session archive",
        _ => "Unavailable client",
    }
}

fn safe_committed_timezone(timezone: &str) -> String {
    if BucketTimezone::from_str(timezone).is_ok() {
        timezone.to_owned()
    } else {
        "Unavailable".to_owned()
    }
}

fn safe_editable_text(value: &str) -> String {
    let path_like = value.starts_with('/')
        || value.starts_with("\\\\")
        || (value.len() >= 3
            && value.as_bytes()[0].is_ascii_alphabetic()
            && value.as_bytes()[1] == b':'
            && matches!(value.as_bytes()[2], b'/' | b'\\'));
    if value.len() > 128 || value.chars().any(char::is_control) || path_like {
        "Unavailable".to_owned()
    } else {
        value.to_owned()
    }
}

fn application_notice(notice: &ApplicationNotice) -> &'static str {
    match notice {
        ApplicationNotice::InvalidSettings(_) => "Review the settings before saving.",
        ApplicationNotice::Conflict => "Settings changed elsewhere. Your edits are retained.",
        ApplicationNotice::StorageUnavailable => {
            "Settings storage unavailable. Last saved values are still shown."
        }
        ApplicationNotice::MonitorUnavailable => {
            "Monitoring is unavailable. Last captured usage is still shown."
        }
        ApplicationNotice::DiscoveryUnavailable => {
            "Source discovery is unavailable. Last known sources are still shown."
        }
    }
}

fn preservation_label(state: PreserveState) -> &'static str {
    match state {
        PreserveState::Preserved => "Preserved",
        PreserveState::NotPresent => "Not present",
        PreserveState::Failed => "Preservation failed",
    }
}

fn import_label(state: ImportState) -> &'static str {
    match state {
        ImportState::Imported => "Imported",
        ImportState::Unsupported => "Unsupported",
        ImportState::Excluded => "Excluded",
        ImportState::Failed => "Import failed",
        ImportState::NotAttempted => "Not attempted",
    }
}

fn activation_label(state: ActivationState) -> &'static str {
    match state {
        ActivationState::Activated => "Active",
        ActivationState::Inactive => "Inactive",
        ActivationState::Failed => "Activation failed",
        ActivationState::NotApplicable => "Not applicable",
    }
}

fn artifact_label(kind: &str) -> &'static str {
    match kind {
        "settings" => "Settings",
        "credentials" => "Credentials",
        "limits-history" => "Limits history",
        "hub-devices" => "Hub devices",
        "icloud-revisions" => "iCloud revisions",
        "session-usage-archive" => "Session usage archive",
        "session-usage-archive-wal" => "Session archive journal",
        "session-usage-archive-shm" => "Session archive shared state",
        "session-usage-archive-json" => "Session archive JSON",
        "daily-history-archive" => "Daily history archive",
        "codex-local-usage" => "Codex local usage",
        "codex-local-usage-wal" => "Codex usage journal",
        "codex-local-usage-shm" => "Codex usage shared state",
        "plugin-manager" => "Plugin manager",
        "plugin-package" => "Plugin package",
        "plugin-settings" => "Plugin settings",
        "plugin-data" => "Plugin data",
        "plugin-credentials" => "Plugin credentials",
        _ => "Retained legacy item",
    }
}

fn page_count(row_count: usize) -> usize {
    row_count.div_ceil(SESSION_PAGE_SIZE).max(1)
}

fn sorted_sessions(snapshot: &MonitorSnapshot) -> Vec<&SessionModelRollup> {
    let mut rows = snapshot.canonical.session_models.iter().collect::<Vec<_>>();
    rows.sort_by_key(|row| (Reverse(row.last_seen_ms), identity(row)));
    rows
}

fn identity(row: &SessionModelRollup) -> SessionIdentity {
    SessionIdentity {
        client: row.key.client.clone(),
        session: row.key.session.clone(),
        model: row.model.clone(),
    }
}

fn selection_key_for(row: &SessionModelRollup) -> String {
    serde_json::to_string(&(&row.key.client, &row.key.session, &row.model))
        .expect("session identity strings always serialize")
}

fn matches_query(row: &SessionModelRollup, query: &str) -> bool {
    let haystack = format!("{} {} {}", row.key.client, row.key.session, row.model).to_lowercase();
    query
        .split_whitespace()
        .all(|term| haystack.contains(&term.to_lowercase()))
}

fn session_row(row: &SessionModelRollup) -> SessionRow {
    let cache = u128::from(row.tokens.cache_read) + u128::from(row.tokens.cache_write);
    SessionRow {
        identity: identity(row),
        selection_key: selection_key_for(row),
        last_activity: format_timestamp(row.last_seen_ms),
        input: format_number(u128::from(row.tokens.input)),
        output: format_number(u128::from(row.tokens.output)),
        cache: format_number(cache),
        reasoning: format_number(u128::from(row.tokens.reasoning)),
        tracked_tokens: format_number(tracked_total(row.tokens)),
    }
}

fn source_rows(snapshot: &MonitorSnapshot) -> Vec<SourceHealthRow> {
    snapshot
        .source_health
        .iter()
        .enumerate()
        .map(|(index, source)| {
            let severity = match source.status {
                SourceHealthStatus::Retained => SourceSeverity::Retained,
                SourceHealthStatus::Healthy | SourceHealthStatus::Pending
                    if snapshot.watch_status == WatchStatus::Unavailable =>
                {
                    SourceSeverity::WatcherUnavailable
                }
                SourceHealthStatus::Healthy => SourceSeverity::Healthy,
                SourceHealthStatus::Pending => SourceSeverity::Waiting,
                _ if source.source_generation.is_some() => SourceSeverity::Retained,
                _ => SourceSeverity::Failed,
            };
            SourceHealthRow {
                name: format!("Source {}", index + 1),
                format: source_format(source.format).to_owned(),
                status: status_label(source.status).to_owned(),
                detail: match (severity, source.source_generation) {
                    (SourceSeverity::Retained, Some(generation)) => {
                        format!("Last-good generation {generation} is still shown")
                    }
                    (SourceSeverity::WatcherUnavailable, Some(generation)) => {
                        format!("Generation {generation} · manual refresh available")
                    }
                    (SourceSeverity::WatcherUnavailable, None) => {
                        "No successful observation · refresh manually".to_owned()
                    }
                    (SourceSeverity::Healthy, Some(generation)) => {
                        format!("Generation {generation} · current snapshot")
                    }
                    _ => "No successful observation".to_owned(),
                },
                severity,
            }
        })
        .collect()
}

fn current_time_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

fn home_summary(
    snapshot: &MonitorSnapshot,
    sources: &[SourceHealthRow],
    period: UsagePeriod,
    clock_ms: Option<i64>,
) -> HomeSummary {
    let now_ms = clock_ms.unwrap_or_else(current_time_ms);
    let periods = period_totals(&snapshot.canonical, now_ms).unwrap_or_default();
    let usage = periods.get(period);
    let totals = usage.tokens;
    let input = u128::from(totals.input);
    let output = u128::from(totals.output);
    let cache = u128::from(totals.cache_read) + u128::from(totals.cache_write);
    let reasoning = u128::from(totals.reasoning);
    let tracked = usage.total;
    let mut healthy = 0;
    let mut retained = 0;
    let mut failed = 0;
    let mut waiting = 0;
    for source in sources {
        match source.severity {
            SourceSeverity::Healthy => healthy += 1,
            SourceSeverity::Retained => retained += 1,
            SourceSeverity::Failed => failed += 1,
            SourceSeverity::Waiting | SourceSeverity::WatcherUnavailable => waiting += 1,
        }
    }
    let has_usage = !snapshot.canonical.session_models.is_empty()
        || !snapshot.canonical.retained_periods.is_empty();
    HomeSummary {
        tracked_tokens: format_number(tracked),
        input: format_number(input),
        output: format_number(output),
        cache: format_number(cache),
        reasoning: format_number(reasoning),
        cost: "Unavailable".to_owned(),
        has_usage,
        empty_message: if has_usage {
            String::new()
        } else {
            "No supported usage yet".to_owned()
        },
        source_headline: format!(
            "Healthy {healthy} · Retained {retained} · Waiting {waiting} · Failed {failed}"
        ),
        period_heading: format!(
            "TRACKED TOKENS · {}",
            match period {
                UsagePeriod::Today => "TODAY",
                UsagePeriod::Month => "MONTH",
                UsagePeriod::AllTime => "ALL CAPTURED",
            }
        ),
        period_index: i32::try_from(period.index()).unwrap_or(0),
        input_permille: fraction_permille(input, tracked),
        output_permille: fraction_permille(output, tracked),
        cache_permille: fraction_permille(cache, tracked),
        reasoning_permille: fraction_permille(reasoning, tracked),
    }
}

fn trends_presentation(snapshot: &MonitorSnapshot, clock_ms: Option<i64>) -> TrendsPresentation {
    let now_ms = clock_ms.unwrap_or_else(current_time_ms);
    let Ok(series) = daily_series(&snapshot.canonical, TREND_DAYS, now_ms) else {
        return TrendsPresentation {
            headline: "Trend history unavailable".to_owned(),
            rows: Vec::new(),
        };
    };
    let busiest = series.iter().map(|day| day.total).max().unwrap_or(0);
    let sum = series
        .iter()
        .fold(0_u128, |sum, day| sum.saturating_add(day.total));
    TrendsPresentation {
        headline: format!(
            "Daily tracked tokens · last {TREND_DAYS} days · {} total",
            format_number(sum)
        ),
        rows: series
            .iter()
            .map(|day| TrendRow {
                label: day.day.format("%Y-%m-%d").to_string(),
                total: format_number(day.total),
                permille: fraction_permille(day.total, busiest),
            })
            .collect(),
    }
}

fn models_presentation(
    snapshot: &MonitorSnapshot,
    period: UsagePeriod,
    clock_ms: Option<i64>,
) -> ModelsPresentation {
    let now_ms = clock_ms.unwrap_or_else(current_time_ms);
    let Ok(rows) = model_breakdown(&snapshot.canonical, period, now_ms) else {
        return ModelsPresentation {
            headline: "Model breakdown unavailable".to_owned(),
            rows: Vec::new(),
        };
    };
    let total = rows
        .iter()
        .fold(0_u128, |sum, row| sum.saturating_add(row.total));
    ModelsPresentation {
        headline: format!(
            "Model share · {} · {} tracked tokens",
            period.label(),
            format_number(total)
        ),
        rows: rows
            .iter()
            .map(|row| ModelRow {
                label: format!("{} · {}", row.client, row.model),
                total: format_number(row.total),
                permille: fraction_permille(row.total, total),
            })
            .collect(),
    }
}

fn session_detail(
    snapshot: &MonitorSnapshot,
    selected: Option<&SessionIdentity>,
) -> Option<SessionDetail> {
    let selected = selected?;
    let rows = snapshot
        .canonical
        .session_models
        .iter()
        .filter(|row| row.key.client == selected.client && row.key.session == selected.session)
        .collect::<Vec<_>>();
    if rows.is_empty() {
        return None;
    }
    let mut rows = rows;
    rows.sort_by(|left, right| {
        tracked_total(right.tokens)
            .cmp(&tracked_total(left.tokens))
            .then_with(|| left.model.cmp(&right.model))
    });
    let mut tokens = TokenCounts::default();
    let mut first_seen = i64::MAX;
    let mut last_seen = i64::MIN;
    for row in &rows {
        tokens = tokens.checked_add(row.tokens).ok()?;
        first_seen = first_seen.min(row.first_seen_ms);
        last_seen = last_seen.max(row.last_seen_ms);
    }
    let tracked = tracked_total(tokens);
    Some(SessionDetail {
        heading: format!("{} · {}", selected.client, selected.session),
        selected_model: selected.model.clone(),
        tracked: format_number(tracked),
        input: format_number(u128::from(tokens.input)),
        output: format_number(u128::from(tokens.output)),
        cache: format_number(u128::from(tokens.cache_read) + u128::from(tokens.cache_write)),
        reasoning: format_number(u128::from(tokens.reasoning)),
        first_seen: format_timestamp(first_seen),
        last_seen: format_timestamp(last_seen),
        models: rows
            .iter()
            .map(|row| {
                let total = tracked_total(row.tokens);
                ModelRow {
                    label: row.model.clone(),
                    total: format_number(total),
                    permille: fraction_permille(total, tracked),
                }
            })
            .collect(),
    })
}

fn tracked_total(tokens: TokenCounts) -> u128 {
    u128::from(tokens.input)
        + u128::from(tokens.output)
        + u128::from(tokens.cache_read)
        + u128::from(tokens.cache_write)
        + u128::from(tokens.reasoning)
}

fn fraction_permille(value: u128, total: u128) -> i32 {
    if total == 0 {
        return 0;
    }
    i32::try_from(value.saturating_mul(1_000) / total).unwrap_or(1_000)
}

fn format_number(value: u128) -> String {
    let digits = value.to_string();
    let mut formatted = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            formatted.push(',');
        }
        formatted.push(character);
    }
    formatted
}

fn format_timestamp(timestamp_ms: i64) -> String {
    DateTime::from_timestamp_millis(timestamp_ms).map_or_else(
        || "Unavailable".to_owned(),
        |timestamp| timestamp.format("%Y-%m-%d %H:%M UTC").to_string(),
    )
}

fn source_format(format: SourceFormat) -> &'static str {
    match format {
        SourceFormat::ClaudeCli => "Claude CLI",
        SourceFormat::CodexCli => "Codex CLI",
        SourceFormat::SessionArchiveSqlite => "Session archive",
        SourceFormat::CodexLocalUsageSqlite => "Codex local usage",
    }
}

fn status_label(status: SourceHealthStatus) -> &'static str {
    match status {
        SourceHealthStatus::Pending => "Waiting",
        SourceHealthStatus::Healthy => "Healthy",
        SourceHealthStatus::Retained => "Retained after error",
        SourceHealthStatus::ReadFailed => "Read failed",
        SourceHealthStatus::DecodingFailed => "Decoding failed",
        SourceHealthStatus::SourceBusy => "Source busy",
        SourceHealthStatus::Unsupported => "Unsupported",
        SourceHealthStatus::CapacityExceeded => "Capacity exceeded",
        SourceHealthStatus::CanonicalizationFailed => "Canonicalization failed",
        SourceHealthStatus::PersistenceDelayed => "Persistence delayed",
    }
}
