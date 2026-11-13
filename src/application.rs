//! Validated, secret-free application settings boundary. Filesystem paths stay
//! in discovery-owned candidates and never enter settings views or diagnostics.

use crate::aggregation::BucketTimezone;
use crate::config::Preferences;
use crate::credentials::{CredentialService, stable_operation_id};
use crate::discovery::{DiscoveryReport, HostPaths, discover};
use crate::migration::{
    StagedCredentialImport, prepare_staged_credentials, recover_staged_credentials,
    stage_and_validate,
};
use crate::runtime::{Monitor, MonitorSettings, MonitorSnapshot};
use crate::sources::SourceFormat;
use crate::storage::{
    ActivationState, CredentialOperation, CredentialOperationKind, CredentialOperationState,
    ImportState, NativeSettings, PreserveState, StoreError, StoreHandle, StoreSnapshot,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::str::FromStr;
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use thiserror::Error;

const SUPPORTED_CLIENTS: &[&str] = &["claude", "codex"];
const COMMAND_CAPACITY: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsField {
    Revision,
    EnabledClients,
    Timezone,
    Sources,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsReason {
    Stale,
    Duplicate,
    Unsupported,
    InvalidTimezone,
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SettingsIssue {
    pub field: SettingsField,
    pub reason: SettingsReason,
}

#[derive(Clone, PartialEq, Eq)]
pub struct SettingsValidationError {
    issues: Vec<SettingsIssue>,
}

impl SettingsValidationError {
    pub fn issues(&self) -> &[SettingsIssue] {
        &self.issues
    }

    pub fn has(&self, field: SettingsField, reason: SettingsReason) -> bool {
        self.issues.contains(&SettingsIssue { field, reason })
    }
}

impl fmt::Debug for SettingsValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SettingsValidationError")
            .field("issues", &self.issues)
            .finish()
    }
}

impl fmt::Display for SettingsValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "settings validation failed ({} issues)",
            self.issues.len()
        )
    }
}

impl std::error::Error for SettingsValidationError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceChoice {
    pub id: String,
    pub client: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingsView {
    pub revision: u64,
    pub enabled_clients: Vec<String>,
    pub timezone: String,
    pub selected_source_ids: Vec<String>,
    pub sources: Vec<SourceChoice>,
}

impl SettingsView {
    pub fn open_draft(&self) -> SettingsDraft {
        SettingsDraft {
            base_revision: self.revision,
            enabled_clients: self.enabled_clients.clone(),
            timezone: self.timezone.clone(),
            selected_source_ids: self.selected_source_ids.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingsDraft {
    pub base_revision: u64,
    pub enabled_clients: Vec<String>,
    pub timezone: String,
    pub selected_source_ids: Vec<String>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SettingsEditorError {
    #[error("native settings document is invalid")]
    InvalidDocument,
}

pub struct SettingsEditor {
    preferences: Preferences,
    view: SettingsView,
    candidate_ids: BTreeSet<String>,
    candidate_clients: BTreeMap<String, String>,
}

impl SettingsEditor {
    pub fn new(
        discovery: &DiscoveryReport,
        snapshot: &StoreSnapshot,
    ) -> Result<Self, SettingsEditorError> {
        let preferences: Preferences = match &snapshot.settings {
            Some(settings) => serde_json::from_slice(settings.json_bytes())
                .map_err(|_| SettingsEditorError::InvalidDocument)?,
            None => {
                serde_json::from_slice(b"{}").map_err(|_| SettingsEditorError::InvalidDocument)?
            }
        };
        let sources = discovery
            .sources
            .iter()
            .map(|source| SourceChoice {
                id: source.source_id.as_storage_key(),
                client: source_client(source.format).to_owned(),
            })
            .collect::<Vec<_>>();
        let candidate_ids: BTreeSet<String> =
            sources.iter().map(|source| source.id.clone()).collect();
        if candidate_ids.len() != sources.len() {
            return Err(SettingsEditorError::InvalidDocument);
        }
        let candidate_clients = sources
            .iter()
            .map(|source| (source.id.clone(), source.client.clone()))
            .collect();
        let enabled_clients = preferences.clients().map(parse_clients).unwrap_or_default();
        let timezone = preferences.timezone().unwrap_or("UTC").to_owned();
        let selected_source_ids = preferences
            .native_source_ids()
            .unwrap_or_default()
            .into_iter()
            .map(str::to_owned)
            .collect();
        let view = SettingsView {
            revision: snapshot.settings_revision,
            enabled_clients,
            timezone,
            selected_source_ids,
            sources,
        };
        Ok(Self {
            preferences,
            view,
            candidate_ids,
            candidate_clients,
        })
    }

    pub fn view(&self) -> &SettingsView {
        &self.view
    }

    pub fn open_draft(&self) -> SettingsDraft {
        self.view.open_draft()
    }

    pub fn validate(
        &self,
        draft: &SettingsDraft,
    ) -> Result<NativeSettings, SettingsValidationError> {
        if draft.base_revision != self.view.revision {
            return Err(SettingsValidationError {
                issues: vec![SettingsIssue {
                    field: SettingsField::Revision,
                    reason: SettingsReason::Stale,
                }],
            });
        }

        let mut issues = Vec::new();
        let mut clients = BTreeSet::new();
        for client in &draft.enabled_clients {
            if !clients.insert(client.as_str()) {
                push_issue(
                    &mut issues,
                    SettingsField::EnabledClients,
                    SettingsReason::Duplicate,
                );
            }
            if !SUPPORTED_CLIENTS.contains(&client.as_str()) {
                push_issue(
                    &mut issues,
                    SettingsField::EnabledClients,
                    SettingsReason::Unsupported,
                );
            }
        }
        if BucketTimezone::from_str(&draft.timezone).is_err() {
            push_issue(
                &mut issues,
                SettingsField::Timezone,
                SettingsReason::InvalidTimezone,
            );
        }
        let mut selected = BTreeSet::new();
        for source in &draft.selected_source_ids {
            if !selected.insert(source.as_str()) {
                push_issue(
                    &mut issues,
                    SettingsField::Sources,
                    SettingsReason::Duplicate,
                );
            }
            let unavailable = !self.candidate_ids.contains(source)
                || self
                    .candidate_clients
                    .get(source)
                    .is_some_and(|client| client != "mixed" && !clients.contains(client.as_str()));
            if unavailable {
                push_issue(
                    &mut issues,
                    SettingsField::Sources,
                    SettingsReason::Unavailable,
                );
            }
        }
        if !issues.is_empty() {
            return Err(SettingsValidationError { issues });
        }

        let bytes = self
            .preferences
            .with_native_edits(
                &draft.enabled_clients,
                &draft.timezone,
                &draft.selected_source_ids,
            )
            .map_err(|_| SettingsValidationError {
                issues: vec![SettingsIssue {
                    field: SettingsField::Revision,
                    reason: SettingsReason::Stale,
                }],
            })?;
        NativeSettings::from_json_bytes(&bytes).map_err(|_| SettingsValidationError {
            issues: vec![SettingsIssue {
                field: SettingsField::EnabledClients,
                reason: SettingsReason::Unsupported,
            }],
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportReportItemView {
    pub artifact_kind: String,
    pub preservation: PreserveState,
    pub import: ImportState,
    pub activation: ActivationState,
    pub records: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImportReportView {
    pub items: Vec<ImportReportItemView>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceAvailability {
    Monitoring,
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialAvailability {
    Available,
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CredentialStatus {
    pub availability: CredentialAvailability,
    pub pending: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApplicationNotice {
    InvalidSettings(SettingsValidationError),
    Conflict,
    StorageUnavailable,
    MonitorUnavailable,
    DiscoveryUnavailable,
}

#[derive(Clone)]
pub struct ApplicationSnapshot {
    pub revision: u64,
    pub settings: SettingsView,
    pub draft: Option<SettingsDraft>,
    pub import_report: ImportReportView,
    pub monitor: Option<Arc<MonitorSnapshot>>,
    pub source_availability: SourceAvailability,
    pub credential_status: CredentialStatus,
    pub notice: Option<ApplicationNotice>,
}

impl fmt::Debug for ApplicationSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ApplicationSnapshot")
            .field("revision", &self.revision)
            .field("settings", &self.settings)
            .field("draft", &self.draft)
            .field("import_report", &self.import_report)
            .field("monitor", &self.monitor.as_ref().map(|_| "[redacted]"))
            .field("source_availability", &self.source_availability)
            .field("credential_status", &self.credential_status)
            .field("notice", &self.notice)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApplicationCommand {
    SaveSettings(SettingsDraft),
    CancelSettings { base_revision: u64 },
    Refresh,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ApplicationError {
    #[error("application discovery failed")]
    Discovery,
    #[error("application migration failed")]
    Migration,
    #[error("application storage could not open")]
    StorageOpen,
    #[error("application settings are unavailable")]
    InvalidSettings,
    #[error("application worker could not start")]
    WorkerStart,
    #[error("application credential worker could not start")]
    CredentialWorkerStart,
    #[error("application worker is unavailable")]
    WorkerPanicked,
    #[error("application storage shutdown failed")]
    StorageShutdown,
    #[error("application credential shutdown failed")]
    CredentialShutdown,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ApplicationSubmitError {
    #[error("application command queue is busy")]
    Busy,
    #[error("application service is closed")]
    Closed,
}

enum WorkerCommand {
    User(ApplicationCommand),
    MonitorPublished,
    Shutdown(SyncSender<Result<(), ApplicationError>>),
}

struct ApplicationInner {
    sender: SyncSender<WorkerCommand>,
    latest: Arc<RwLock<Arc<ApplicationSnapshot>>>,
    stopped: Mutex<bool>,
    join: Mutex<Option<JoinHandle<()>>>,
}

impl ApplicationInner {
    fn stop_and_join(&self) -> Result<(), ApplicationError> {
        let response = {
            let mut stopped = self
                .stopped
                .lock()
                .map_err(|_| ApplicationError::WorkerPanicked)?;
            if *stopped {
                None
            } else {
                *stopped = true;
                let (sender, receiver) = mpsc::sync_channel(1);
                self.sender
                    .send(WorkerCommand::Shutdown(sender))
                    .map_err(|_| ApplicationError::WorkerPanicked)?;
                Some(receiver)
            }
        };
        let worker_result = response
            .map(|receiver| {
                receiver
                    .recv()
                    .map_err(|_| ApplicationError::WorkerPanicked)
            })
            .transpose()?
            .unwrap_or(Ok(()));
        if let Some(join) = self
            .join
            .lock()
            .map_err(|_| ApplicationError::WorkerPanicked)?
            .take()
        {
            join.join().map_err(|_| ApplicationError::WorkerPanicked)?;
        }
        worker_result
    }
}

impl Drop for ApplicationInner {
    fn drop(&mut self) {
        if let Ok(stopped) = self.stopped.get_mut()
            && !*stopped
        {
            *stopped = true;
            let (sender, _) = mpsc::sync_channel(1);
            let _ = self.sender.send(WorkerCommand::Shutdown(sender));
        }
        if let Ok(join) = self.join.get_mut()
            && let Some(join) = join.take()
        {
            let _ = join.join();
        }
    }
}

#[derive(Clone)]
pub struct ApplicationService {
    inner: Arc<ApplicationInner>,
}

impl fmt::Debug for ApplicationService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ApplicationService { state: [redacted] }")
    }
}

impl ApplicationService {
    /// Resolve current inputs, stage/adopt a first-run legacy installation once,
    /// then start the bounded application lane.
    pub fn bootstrap(inputs: HostPaths) -> Result<Self, ApplicationError> {
        let discovery = discover(&inputs).map_err(|_| ApplicationError::Discovery)?;
        let (store, stored) = StoreHandle::open(discovery.native.clone())
            .map_err(|_| ApplicationError::StorageOpen)?;
        let credentials =
            CredentialService::start().map_err(|_| ApplicationError::CredentialWorkerStart)?;
        let mut credential_generation = 0;
        let credential_status = bootstrap_credentials(
            &discovery,
            &store,
            &credentials,
            &mut credential_generation,
            &stored,
        )?;
        Self::start_with_inputs(
            discovery,
            store,
            credentials,
            credential_status,
            credential_generation,
            Some(inputs),
        )
    }

    pub fn start(discovery: DiscoveryReport, store: StoreHandle) -> Result<Self, ApplicationError> {
        let credentials =
            CredentialService::start().map_err(|_| ApplicationError::CredentialWorkerStart)?;
        let stored = store
            .snapshot()
            .map_err(|_| ApplicationError::InvalidSettings)?;
        let credential_status = credential_status(&stored, CredentialAvailability::Available);
        Self::start_with_inputs(discovery, store, credentials, credential_status, 0, None)
    }

    fn start_with_inputs(
        discovery: DiscoveryReport,
        store: StoreHandle,
        credentials: CredentialService,
        credential_status: CredentialStatus,
        credential_generation: u64,
        inputs: Option<HostPaths>,
    ) -> Result<Self, ApplicationError> {
        let stored = store
            .snapshot()
            .map_err(|_| ApplicationError::InvalidSettings)?;
        let editor = SettingsEditor::new(&discovery, &stored)
            .map_err(|_| ApplicationError::InvalidSettings)?;
        let initial = Arc::new(ApplicationSnapshot {
            revision: 1,
            settings: editor.view().clone(),
            draft: None,
            import_report: import_report(&stored),
            monitor: None,
            source_availability: SourceAvailability::Unavailable,
            credential_status,
            notice: None,
        });
        let latest = Arc::new(RwLock::new(initial));
        let (sender, receiver) = mpsc::sync_channel(COMMAND_CAPACITY);
        let pending_monitor = Arc::new(Mutex::new(None));
        let state = ApplicationState {
            inputs,
            discovery,
            store,
            credentials: Some(credentials),
            credential_status,
            credential_generation,
            editor,
            monitor: None,
            pending_monitor: pending_monitor.clone(),
            latest: latest.clone(),
        };
        let worker_sender = sender.clone();
        let join = thread::Builder::new()
            .name("token-monitor-application".to_owned())
            .spawn(move || worker_main(receiver, worker_sender, state))
            .map_err(|_| ApplicationError::WorkerStart)?;
        Ok(Self {
            inner: Arc::new(ApplicationInner {
                sender,
                latest,
                stopped: Mutex::new(false),
                join: Mutex::new(Some(join)),
            }),
        })
    }

    pub fn snapshot(&self) -> Result<Arc<ApplicationSnapshot>, ApplicationSubmitError> {
        self.inner
            .latest
            .read()
            .map(|snapshot| snapshot.clone())
            .map_err(|_| ApplicationSubmitError::Closed)
    }

    /// Enqueues without waiting for discovery, SQLite, monitor, or filesystem work.
    pub fn submit(&self, command: ApplicationCommand) -> Result<(), ApplicationSubmitError> {
        let stopped = self
            .inner
            .stopped
            .lock()
            .map_err(|_| ApplicationSubmitError::Closed)?;
        if *stopped {
            return Err(ApplicationSubmitError::Closed);
        }
        self.inner
            .sender
            .try_send(WorkerCommand::User(command))
            .map_err(|error| match error {
                TrySendError::Full(_) => ApplicationSubmitError::Busy,
                TrySendError::Disconnected(_) => ApplicationSubmitError::Closed,
            })
    }

    pub fn shutdown(self) -> Result<(), ApplicationError> {
        self.inner.stop_and_join()
    }
}

struct ApplicationState {
    inputs: Option<HostPaths>,
    discovery: DiscoveryReport,
    store: StoreHandle,
    credentials: Option<CredentialService>,
    credential_status: CredentialStatus,
    credential_generation: u64,
    editor: SettingsEditor,
    monitor: Option<Monitor>,
    pending_monitor: Arc<Mutex<Option<Arc<MonitorSnapshot>>>>,
    latest: Arc<RwLock<Arc<ApplicationSnapshot>>>,
}

fn worker_main(
    receiver: mpsc::Receiver<WorkerCommand>,
    sender: SyncSender<WorkerCommand>,
    mut state: ApplicationState,
) {
    state.apply_monitor_settings(&sender);
    state.publish(None, None, state.current_notice());
    while let Ok(command) = receiver.recv() {
        match command {
            WorkerCommand::User(command) => state.handle(command, &sender),
            WorkerCommand::MonitorPublished => state.publish_pending_monitor(),
            WorkerCommand::Shutdown(response) => {
                drop(state.monitor.take());
                let credential_result = state
                    .credentials
                    .take()
                    .map(CredentialService::shutdown)
                    .transpose()
                    .map_err(|_| ApplicationError::CredentialShutdown);
                let storage_result = state
                    .store
                    .shutdown()
                    .map_err(|_| ApplicationError::StorageShutdown);
                let result = credential_result.and(storage_result);
                let _ = response.send(result);
                return;
            }
        }
        state.publish_pending_monitor();
    }
}

impl ApplicationState {
    fn handle(&mut self, command: ApplicationCommand, sender: &SyncSender<WorkerCommand>) {
        match command {
            ApplicationCommand::SaveSettings(draft) => self.save(draft, sender),
            ApplicationCommand::CancelSettings { .. } => self.publish(None, None, None),
            ApplicationCommand::Refresh => self.refresh(sender),
        }
    }

    fn refresh(&mut self, sender: &SyncSender<WorkerCommand>) {
        if let Some(credentials) = &self.credentials {
            match resume_pending_credentials(
                &self.discovery,
                &self.store,
                credentials,
                &mut self.credential_generation,
            ) {
                Ok(status) => self.credential_status = status,
                Err(_) => {
                    self.publish(None, None, Some(ApplicationNotice::StorageUnavailable));
                    return;
                }
            }
        }
        if let Some(inputs) = &self.inputs {
            let Ok(discovery) = discover(inputs) else {
                self.publish(None, None, Some(ApplicationNotice::DiscoveryUnavailable));
                return;
            };
            let Ok(stored) = self.store.snapshot() else {
                self.publish(None, None, Some(ApplicationNotice::StorageUnavailable));
                return;
            };
            let Ok(editor) = SettingsEditor::new(&discovery, &stored) else {
                self.publish(None, None, Some(ApplicationNotice::StorageUnavailable));
                return;
            };
            self.discovery = discovery;
            self.editor = editor;
            if let Some(notice) = self.apply_monitor_settings(sender) {
                self.publish(None, None, Some(notice));
                return;
            }
        }
        if let Some(monitor) = &self.monitor {
            monitor.request_refresh();
        }
        self.publish(None, None, None);
    }

    fn save(&mut self, draft: SettingsDraft, sender: &SyncSender<WorkerCommand>) {
        let next = match self.editor.validate(&draft) {
            Ok(next) => next,
            Err(error) => {
                let notice = if error.has(SettingsField::Revision, SettingsReason::Stale) {
                    ApplicationNotice::Conflict
                } else {
                    ApplicationNotice::InvalidSettings(error)
                };
                self.publish(Some(draft), None, Some(notice));
                return;
            }
        };
        match self.store.save_settings(draft.base_revision, next) {
            Ok(stored) => {
                let Ok(editor) = SettingsEditor::new(&self.discovery, &stored) else {
                    self.publish(
                        Some(draft),
                        None,
                        Some(ApplicationNotice::StorageUnavailable),
                    );
                    return;
                };
                self.editor = editor;
                let monitor_notice = self.apply_monitor_settings(sender);
                self.publish(None, None, monitor_notice);
            }
            Err(StoreError::Conflict { .. }) => {
                if let Ok(stored) = self.store.snapshot()
                    && let Ok(editor) = SettingsEditor::new(&self.discovery, &stored)
                {
                    self.editor = editor;
                }
                self.publish(Some(draft), None, Some(ApplicationNotice::Conflict));
            }
            Err(_) => self.publish(
                Some(draft),
                None,
                Some(ApplicationNotice::StorageUnavailable),
            ),
        }
    }

    fn apply_monitor_settings(
        &mut self,
        sender: &SyncSender<WorkerCommand>,
    ) -> Option<ApplicationNotice> {
        let Some(settings) = self.monitor_settings() else {
            drop(self.monitor.take());
            if let Ok(mut pending) = self.pending_monitor.lock() {
                *pending = None;
            }
            return None;
        };
        if let Some(monitor) = &self.monitor {
            return match monitor.replace_settings(settings) {
                Ok(_) => None,
                Err(_) => Some(ApplicationNotice::MonitorUnavailable),
            };
        }
        let pending = self.pending_monitor.clone();
        let sender = sender.clone();
        match Monitor::start(settings, self.store.clone(), move |snapshot| {
            if let Ok(mut latest) = pending.lock() {
                *latest = Some(snapshot);
            }
            let _ = sender.try_send(WorkerCommand::MonitorPublished);
        }) {
            Ok(monitor) => {
                self.monitor = Some(monitor);
                None
            }
            Err(_) => Some(ApplicationNotice::MonitorUnavailable),
        }
    }

    fn monitor_settings(&self) -> Option<MonitorSettings> {
        if self.editor.view.selected_source_ids.is_empty() {
            return None;
        }
        let timezone = BucketTimezone::from_str(&self.editor.view.timezone).ok()?;
        let selected = self
            .editor
            .view
            .selected_source_ids
            .iter()
            .filter_map(|id| {
                self.editor
                    .view
                    .sources
                    .iter()
                    .position(|source| &source.id == id)
            })
            .collect::<Vec<_>>();
        let stored = self.store.snapshot().ok()?;
        Some(MonitorSettings {
            generation: stored.settings_generation,
            day_generation: stored.day_generation,
            pricing_generation: stored.pricing_generation,
            timezone,
            sources: selected
                .into_iter()
                .map(|index| self.discovery.sources[index].monitored_source())
                .collect(),
        })
    }

    fn current_notice(&self) -> Option<ApplicationNotice> {
        if self.editor.validate(&self.editor.open_draft()).is_ok() {
            None
        } else {
            Some(ApplicationNotice::InvalidSettings(
                self.editor
                    .validate(&self.editor.open_draft())
                    .expect_err("invalid settings were checked"),
            ))
        }
    }

    fn publish_pending_monitor(&mut self) {
        let pending = self
            .pending_monitor
            .lock()
            .ok()
            .and_then(|mut pending| pending.take());
        let Some(monitor) = pending else {
            return;
        };
        let Ok(stored) = self.store.snapshot() else {
            return;
        };
        if monitor.settings_generation == stored.settings_generation {
            self.publish(None, Some(Some(monitor)), None);
        }
    }

    fn publish(
        &self,
        draft: Option<SettingsDraft>,
        monitor: Option<Option<Arc<MonitorSnapshot>>>,
        notice: Option<ApplicationNotice>,
    ) {
        let previous = match self.latest.read() {
            Ok(previous) => previous.clone(),
            Err(_) => return,
        };
        let stored = self.store.snapshot().ok();
        let next = Arc::new(ApplicationSnapshot {
            revision: previous.revision.saturating_add(1),
            settings: self.editor.view().clone(),
            draft,
            import_report: stored
                .as_deref()
                .map(import_report)
                .unwrap_or_else(|| previous.import_report.clone()),
            monitor: if self.monitor.is_some() {
                monitor.unwrap_or_else(|| previous.monitor.clone())
            } else {
                None
            },
            source_availability: if self.monitor.is_some() {
                SourceAvailability::Monitoring
            } else {
                SourceAvailability::Unavailable
            },
            credential_status: self.credential_status,
            notice,
        });
        if let Ok(mut latest) = self.latest.write() {
            *latest = next;
        }
    }
}

fn bootstrap_credentials(
    discovery: &DiscoveryReport,
    store: &StoreHandle,
    credentials: &CredentialService,
    generation: &mut u64,
    initial: &StoreSnapshot,
) -> Result<CredentialStatus, ApplicationError> {
    let mut availability = CredentialAvailability::Available;
    if !initial.pending_credential_imports.is_empty() {
        let status = resume_pending_credentials(discovery, store, credentials, generation)?;
        availability = status.availability;
        if status.pending {
            return Ok(status);
        }
    }

    let current = store
        .snapshot()
        .map_err(|_| ApplicationError::StorageOpen)?;
    if current.settings_revision == 0
        && current.import_runs.is_empty()
        && current.pending_credential_imports.is_empty()
        && let Some(legacy) = &discovery.legacy
    {
        let import = stage_and_validate(legacy, &discovery.native)
            .map_err(|_| ApplicationError::Migration)?;
        let needs_credential_planning = !import.credential_references.is_empty()
            || import
                .staged_artifacts
                .iter()
                .any(|artifact| artifact.artifact_kind == "credentials");
        if !needs_credential_planning {
            store
                .adopt_import(import)
                .map_err(|_| ApplicationError::StorageOpen)?;
            let stored = store
                .snapshot()
                .map_err(|_| ApplicationError::StorageOpen)?;
            return Ok(credential_status(&stored, availability));
        }
        let import = prepare_staged_credentials(import, &discovery.native)
            .map_err(|_| ApplicationError::Migration)?;
        if import.credentials.transfers.is_empty() {
            store
                .adopt_import(import.import)
                .map_err(|_| ApplicationError::StorageOpen)?;
        } else {
            prepare_credential_journal(store, &import)?;
            if let Err(error) = run_staged_credentials(store, credentials, generation, import) {
                match error {
                    CredentialRunError::Credential => {
                        availability = CredentialAvailability::Unavailable;
                    }
                    CredentialRunError::Storage => return Err(ApplicationError::StorageOpen),
                    CredentialRunError::Invalid => return Err(ApplicationError::Migration),
                }
            }
        }
    }
    let stored = store
        .snapshot()
        .map_err(|_| ApplicationError::StorageOpen)?;
    Ok(credential_status(&stored, availability))
}

fn resume_pending_credentials(
    discovery: &DiscoveryReport,
    store: &StoreHandle,
    credentials: &CredentialService,
    generation: &mut u64,
) -> Result<CredentialStatus, ApplicationError> {
    let snapshot = store
        .snapshot()
        .map_err(|_| ApplicationError::StorageOpen)?;
    let run_ids = snapshot
        .pending_credential_imports
        .iter()
        .map(|pending| pending.run_id.clone())
        .collect::<Vec<_>>();
    let mut availability = CredentialAvailability::Available;
    for run_id in run_ids {
        let import = recover_staged_credentials(&discovery.native, &run_id)
            .map_err(|_| ApplicationError::Migration)?;
        if let Err(error) = run_staged_credentials(store, credentials, generation, import) {
            match error {
                CredentialRunError::Credential => {
                    availability = CredentialAvailability::Unavailable;
                    break;
                }
                CredentialRunError::Storage => return Err(ApplicationError::StorageOpen),
                CredentialRunError::Invalid => return Err(ApplicationError::Migration),
            }
        }
    }
    let stored = store
        .snapshot()
        .map_err(|_| ApplicationError::StorageOpen)?;
    Ok(credential_status(&stored, availability))
}

fn prepare_credential_journal(
    store: &StoreHandle,
    staged: &StagedCredentialImport,
) -> Result<(), ApplicationError> {
    let operations = staged
        .import
        .credential_references
        .iter()
        .map(|reference| CredentialOperation {
            operation_id: stable_operation_id(&staged.import.run_id, &reference.credential_id),
            credential_id: reference.credential_id.clone(),
            operation: CredentialOperationKind::Set,
            state: CredentialOperationState::Prepared,
        })
        .collect();
    store
        .prepare_credential_import(
            &staged.import.run_id,
            staged.import.expected_settings_revision,
            operations,
        )
        .map(|_| ())
        .map_err(|_| ApplicationError::StorageOpen)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CredentialRunError {
    Credential,
    Storage,
    Invalid,
}

fn run_staged_credentials(
    store: &StoreHandle,
    credentials: &CredentialService,
    generation: &mut u64,
    staged: StagedCredentialImport,
) -> Result<(), CredentialRunError> {
    let snapshot = store.snapshot().map_err(|_| CredentialRunError::Storage)?;
    let operations = snapshot
        .credential_operations
        .iter()
        .filter(|stored| stored.run_id == staged.import.run_id)
        .map(|stored| (stored.operation.credential_id.as_str(), &stored.operation))
        .collect::<BTreeMap<_, _>>();
    if operations.len() != staged.credentials.transfers.len() {
        return Err(CredentialRunError::Invalid);
    }

    for transfer in staged.credentials.transfers {
        let credential_id = transfer.credential_id.clone();
        let operation = operations
            .get(credential_id.as_str())
            .copied()
            .ok_or(CredentialRunError::Invalid)?;
        if operation.operation != CredentialOperationKind::Set
            || operation.operation_id != stable_operation_id(&staged.import.run_id, &credential_id)
        {
            return Err(CredentialRunError::Invalid);
        }
        let request_generation = next_credential_generation(generation)?;
        match operation.state {
            CredentialOperationState::Prepared => {
                let expected = credentials
                    .set(request_generation, &credential_id, transfer.into_material())
                    .map_err(|_| CredentialRunError::Credential)?;
                store
                    .advance_credential_operation(
                        &operation.operation_id,
                        CredentialOperationState::Prepared,
                        CredentialOperationState::SecretWritten,
                    )
                    .map_err(|_| CredentialRunError::Storage)?;
                let actual = credentials
                    .get(request_generation, &operation.credential_id)
                    .map_err(|_| CredentialRunError::Credential)?;
                if !actual.matches(&expected) {
                    return Err(CredentialRunError::Credential);
                }
                store
                    .advance_credential_operation(
                        &operation.operation_id,
                        CredentialOperationState::SecretWritten,
                        CredentialOperationState::Verified,
                    )
                    .map_err(|_| CredentialRunError::Storage)?;
            }
            CredentialOperationState::SecretWritten => {
                credentials
                    .verify(request_generation, &credential_id, transfer.into_material())
                    .map_err(|_| CredentialRunError::Credential)?;
                store
                    .advance_credential_operation(
                        &operation.operation_id,
                        CredentialOperationState::SecretWritten,
                        CredentialOperationState::Verified,
                    )
                    .map_err(|_| CredentialRunError::Storage)?;
            }
            CredentialOperationState::Verified => {}
            CredentialOperationState::MetadataCommitted => {
                return Err(CredentialRunError::Invalid);
            }
        }
    }
    store
        .adopt_import(staged.import)
        .map(|_| ())
        .map_err(|_| CredentialRunError::Storage)
}

fn next_credential_generation(generation: &mut u64) -> Result<u64, CredentialRunError> {
    *generation = generation
        .checked_add(1)
        .ok_or(CredentialRunError::Credential)?;
    Ok(*generation)
}

fn credential_status(
    snapshot: &StoreSnapshot,
    availability: CredentialAvailability,
) -> CredentialStatus {
    CredentialStatus {
        availability,
        pending: !snapshot.pending_credential_imports.is_empty(),
    }
}

fn import_report(snapshot: &StoreSnapshot) -> ImportReportView {
    let mut items = snapshot
        .report_items
        .iter()
        .map(|stored| ImportReportItemView {
            artifact_kind: stored.item.artifact_kind.clone(),
            preservation: stored.item.preservation,
            import: stored.item.import,
            activation: stored.item.activation,
            records: stored.item.records,
        })
        .collect::<Vec<_>>();
    if !snapshot.pending_credential_imports.is_empty() {
        items.push(ImportReportItemView {
            artifact_kind: "credentials".to_owned(),
            preservation: PreserveState::Preserved,
            import: ImportState::NotAttempted,
            activation: ActivationState::Inactive,
            records: snapshot
                .credential_operations
                .iter()
                .filter(|operation| {
                    operation.operation.state != CredentialOperationState::MetadataCommitted
                })
                .count() as u64,
        });
    }
    ImportReportView { items }
}

fn parse_clients(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|client| !client.is_empty())
        .map(str::to_owned)
        .collect()
}

fn push_issue(issues: &mut Vec<SettingsIssue>, field: SettingsField, reason: SettingsReason) {
    let issue = SettingsIssue { field, reason };
    if !issues.contains(&issue) {
        issues.push(issue);
    }
}

fn source_client(format: SourceFormat) -> &'static str {
    match format {
        SourceFormat::ClaudeCli => "claude",
        SourceFormat::CodexCli | SourceFormat::CodexLocalUsageSqlite => "codex",
        SourceFormat::SessionArchiveSqlite => "mixed",
    }
}

#[cfg(test)]
mod credential_tests {
    use super::*;
    use crate::credentials::TestCredentialStore;
    use crate::discovery::{HostPlatform, discover};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TREE: AtomicU64 = AtomicU64::new(0);

    struct TestTree(PathBuf);

    impl TestTree {
        fn credential_fixture(secret: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "token-monitor-credential-application-{}-{}",
                std::process::id(),
                NEXT_TREE.fetch_add(1, Ordering::Relaxed)
            ));
            let settings = root.join(".config/Token Monitor/settings.json");
            fs::create_dir_all(settings.parent().expect("settings parent"))
                .expect("create fixture");
            fs::write(
                settings,
                format!(r#"{{"clients":"claude","clineApiKey":"{secret}"}}"#),
            )
            .expect("write fixture");
            Self(root)
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
    }

    impl Drop for TestTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn unavailable_store_stays_pending_then_recovers_without_plaintext_metadata() {
        let secret = "PRIVATE_APPLICATION_CREDENTIAL_SENTINEL";
        let tree = TestTree::credential_fixture(secret);
        let discovery = discover(&tree.paths()).expect("discover fixture");
        let (store, initial) = StoreHandle::open(discovery.native.clone()).expect("open store");
        let backend = TestCredentialStore::default();
        backend.set_unavailable(true);
        let credentials =
            CredentialService::start_test(backend.clone()).expect("start credentials");
        let mut generation = 0;

        let status =
            bootstrap_credentials(&discovery, &store, &credentials, &mut generation, &initial)
                .expect("bootstrap pending credentials");
        assert_eq!(status.availability, CredentialAvailability::Unavailable);
        assert!(status.pending);
        let pending = store.snapshot().expect("pending snapshot");
        assert!(pending.settings.is_none());
        assert_eq!(pending.pending_credential_imports.len(), 1);
        assert!(
            pending.credential_operations.iter().all(|operation| {
                operation.operation.state == CredentialOperationState::Prepared
            })
        );
        assert!(!format!("{pending:?}").contains(secret));
        assert!(
            !fs::read(
                discovery
                    .native
                    .root()
                    .join(crate::storage::DATABASE_FILE_NAME)
            )
            .expect("read database")
            .windows(secret.len())
            .any(|window| window == secret.as_bytes())
        );

        backend.set_unavailable(false);
        let recovered =
            resume_pending_credentials(&discovery, &store, &credentials, &mut generation)
                .expect("recover credentials");
        assert_eq!(recovered.availability, CredentialAvailability::Available);
        assert!(!recovered.pending);
        let committed = store.snapshot().expect("committed snapshot");
        assert_eq!(committed.settings_revision, 1);
        assert_eq!(committed.credential_references.len(), 1);
        assert!(committed.credential_operations.iter().all(|operation| {
            operation.operation.state == CredentialOperationState::MetadataCommitted
        }));
        assert!(
            !committed
                .settings
                .as_ref()
                .expect("settings")
                .json_bytes()
                .windows(secret.len())
                .any(|window| window == secret.as_bytes())
        );

        let probe = credentials.clone();
        let application = ApplicationService::start_with_inputs(
            discovery,
            store,
            credentials,
            recovered,
            generation,
            None,
        )
        .expect("start application");
        application.shutdown().expect("joined shutdown");
        assert_eq!(
            probe.get(generation, "credential-0000000000000000"),
            Err(crate::credentials::CredentialError::Closed)
        );
    }

    #[test]
    fn restart_after_secret_written_verifies_existing_secret_before_adoption() {
        let tree = TestTree::credential_fixture("PRIVATE_RECOVERY_SENTINEL");
        let discovery = discover(&tree.paths()).expect("discover fixture");
        let (store, initial) = StoreHandle::open(discovery.native.clone()).expect("open store");
        let backend = TestCredentialStore::default();
        backend.fail_next_get();
        let credentials =
            CredentialService::start_test(backend.clone()).expect("start credentials");
        let mut generation = 0;

        let interrupted =
            bootstrap_credentials(&discovery, &store, &credentials, &mut generation, &initial)
                .expect("leave secret-written journal");
        assert_eq!(
            interrupted.availability,
            CredentialAvailability::Unavailable
        );
        assert!(
            store
                .snapshot()
                .expect("snapshot")
                .credential_operations
                .iter()
                .all(|operation| operation.operation.state
                    == CredentialOperationState::SecretWritten)
        );
        credentials.shutdown().expect("stop first worker");

        let credentials = CredentialService::start_test(backend).expect("restart credentials");
        let mut generation = 0;
        let recovered =
            resume_pending_credentials(&discovery, &store, &credentials, &mut generation)
                .expect("resume secret-written journal");
        assert_eq!(recovered.availability, CredentialAvailability::Available);
        assert!(!recovered.pending);
        assert!(
            store
                .snapshot()
                .expect("snapshot")
                .credential_operations
                .iter()
                .all(|operation| operation.operation.state
                    == CredentialOperationState::MetadataCommitted)
        );
        credentials.shutdown().expect("stop credentials");
        store.shutdown().expect("stop store");
    }
}
