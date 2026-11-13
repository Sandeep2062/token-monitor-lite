//! Bounded native watch admission and serial source reconciliation. Watcher
//! callbacks only coalesce signals; all file reads and canonical publication
//! happen on the owned worker thread. Published paths remain local-only.

use crate::aggregation::{BucketTimezone, CanonicalLedger, CanonicalSnapshot, MAX_SOURCES};
use crate::parsers::ReaderError;
use crate::source_inputs::{SourceInput, SqliteSourceError};
use crate::sources::{SourceError, SourceFormat, SourceId, SourceProgress, SourceReader};
use crate::storage::{CanonicalCommit, PersistedRuntimeSnapshot, StoreError, StoreHandle};
use anyhow::{Context, Result, anyhow, bail};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const DEBOUNCE: Duration = Duration::from_millis(250);
const CONTINUOUS_WRITE_CEILING: Duration = Duration::from_secs(2);
const PERSISTENCE_DELAY_REPORT: Duration = Duration::from_millis(100);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MonitoredSource {
    pub source_id: SourceId,
    pub path: PathBuf,
    pub format: SourceFormat,
    pub session: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MonitorSettings {
    pub generation: u64,
    pub day_generation: u64,
    pub pricing_generation: u64,
    pub timezone: BucketTimezone,
    pub sources: Vec<MonitoredSource>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceHealthStatus {
    Pending,
    Healthy,
    Retained,
    ReadFailed,
    DecodingFailed,
    SourceBusy,
    Unsupported,
    CapacityExceeded,
    CanonicalizationFailed,
    PersistenceDelayed,
}

impl SourceHealthStatus {
    pub fn from_source_error(error: &SourceError) -> Self {
        match error {
            SourceError::Io(io::ErrorKind::PermissionDenied | io::ErrorKind::WouldBlock) => {
                Self::SourceBusy
            }
            SourceError::Io(_) => Self::ReadFailed,
            SourceError::Reader(ReaderError::InvalidRecord | ReaderError::InvalidTimestamp) => {
                Self::DecodingFailed
            }
            SourceError::Reader(ReaderError::Unsupported(_)) => Self::Unsupported,
            SourceError::Reader(ReaderError::Capacity(_)) => Self::CapacityExceeded,
            SourceError::Reader(ReaderError::Overflow) => Self::CanonicalizationFailed,
            SourceError::Sqlite(SqliteSourceError::Busy | SqliteSourceError::Changed) => {
                Self::SourceBusy
            }
            SourceError::Sqlite(SqliteSourceError::Schema | SqliteSourceError::Conflict) => {
                Self::Unsupported
            }
            SourceError::Sqlite(SqliteSourceError::Invalid) => Self::DecodingFailed,
            SourceError::Sqlite(SqliteSourceError::Capacity) => Self::CapacityExceeded,
            SourceError::Sqlite(SqliteSourceError::Unavailable | SqliteSourceError::Io(_)) => {
                Self::ReadFailed
            }
            SourceError::Overflow | SourceError::GenerationOverflow => Self::CanonicalizationFailed,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceHealth {
    pub source_id: SourceId,
    pub format: SourceFormat,
    pub status: SourceHealthStatus,
    pub source_generation: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatchStatus {
    Available,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RuntimeMetrics {
    pub reconciliations: u64,
    pub scanned_bytes: u64,
    pub last_reconciliation_millis: u64,
    pub coalesced_signals: u64,
    pub owned_worker_threads: u8,
}

#[derive(Clone, Debug)]
pub struct MonitorSnapshot {
    /// Monotonic publication identity. Consumers should fence on this value,
    /// not on an individual source or canonical-ledger generation.
    pub generation: u64,
    pub runtime_generation: u64,
    pub runtime_initialized: bool,
    pub settings_generation: u64,
    pub day_generation: u64,
    pub pricing_generation: u64,
    pub canonical: Arc<CanonicalSnapshot>,
    pub source_health: Arc<[SourceHealth]>,
    pub watch_status: WatchStatus,
    pub metrics: RuntimeMetrics,
}

/// Narrow persistence boundary owned by the monitor worker. Implementations
/// must be safe to call from that worker only; the monitor never forwards
/// paths, raw source text, or SQLite handles.
pub trait RuntimePersistence: Send + Sync {
    fn runtime_snapshot(&self) -> Result<Arc<PersistedRuntimeSnapshot>, StoreError>;

    fn commit_runtime(
        &self,
        commit: CanonicalCommit,
    ) -> Result<Arc<PersistedRuntimeSnapshot>, StoreError>;

    fn commit_runtime_observed(
        &self,
        commit: CanonicalCommit,
        _wait: Duration,
        _on_wait: &mut dyn FnMut(),
    ) -> Result<Arc<PersistedRuntimeSnapshot>, StoreError> {
        self.commit_runtime(commit)
    }
}

impl RuntimePersistence for StoreHandle {
    fn runtime_snapshot(&self) -> Result<Arc<PersistedRuntimeSnapshot>, StoreError> {
        StoreHandle::runtime_snapshot(self)
    }

    fn commit_runtime(
        &self,
        commit: CanonicalCommit,
    ) -> Result<Arc<PersistedRuntimeSnapshot>, StoreError> {
        StoreHandle::commit_runtime(self, commit)
    }

    fn commit_runtime_observed(
        &self,
        commit: CanonicalCommit,
        wait: Duration,
        on_wait: &mut dyn FnMut(),
    ) -> Result<Arc<PersistedRuntimeSnapshot>, StoreError> {
        StoreHandle::commit_runtime_observed(self, commit, wait, on_wait)
    }
}

type Publish = Box<dyn Fn(Arc<MonitorSnapshot>) + Send>;

struct Signals {
    sender: SyncSender<()>,
    epoch: AtomicU64,
    coalesced: AtomicU64,
    shutdown: AtomicBool,
    watch_failed: AtomicBool,
    latest_settings: Mutex<MonitorSettings>,
}

impl Signals {
    fn signal(&self) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
        match self.sender.try_send(()) {
            Ok(()) => {}
            Err(TrySendError::Full(())) => {
                self.coalesced.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(())) => {}
        }
    }

    fn stop(&self) {
        self.shutdown.store(true, Ordering::Release);
        let _ = self.sender.try_send(());
    }

    fn settings(&self) -> Option<MonitorSettings> {
        self.latest_settings.lock().ok().map(|value| value.clone())
    }
}

/// Owns one native watcher and one serial reconciliation worker. Drop joins the
/// worker; callers must therefore drop it outside UI event processing.
pub struct Monitor {
    signals: Arc<Signals>,
    worker: Option<JoinHandle<()>>,
}

impl Monitor {
    pub fn start<F>(settings: MonitorSettings, store: StoreHandle, publish: F) -> Result<Self>
    where
        F: Fn(Arc<MonitorSnapshot>) + Send + 'static,
    {
        Self::start_with_persistence(settings, Arc::new(store), publish)
    }

    pub fn start_with_persistence<F>(
        settings: MonitorSettings,
        store: Arc<dyn RuntimePersistence>,
        publish: F,
    ) -> Result<Self>
    where
        F: Fn(Arc<MonitorSnapshot>) + Send + 'static,
    {
        validate_settings(&settings)?;
        let persisted = store
            .runtime_snapshot()
            .context("persisted runtime snapshot unavailable")?;
        let (sender, receiver) = mpsc::sync_channel(1);
        let signals = Arc::new(Signals {
            sender,
            epoch: AtomicU64::new(0),
            coalesced: AtomicU64::new(0),
            shutdown: AtomicBool::new(false),
            watch_failed: AtomicBool::new(false),
            latest_settings: Mutex::new(settings.clone()),
        });
        let callback_signals = signals.clone();
        let watcher =
            match notify::recommended_watcher(move |result: notify::Result<Event>| match result {
                Ok(event) => {
                    if !event.kind.is_access() && event_matches_sources(&event, &callback_signals) {
                        callback_signals.signal();
                    }
                }
                Err(_) => {
                    tracing::warn!("native source watch failed; manual refresh remains available");
                    callback_signals.watch_failed.store(true, Ordering::Release);
                    callback_signals.signal();
                }
            }) {
                Ok(watcher) => Some(watcher),
                Err(_) => {
                    tracing::warn!(
                        "native source watcher unavailable; manual refresh remains available"
                    );
                    signals.watch_failed.store(true, Ordering::Release);
                    None
                }
            };
        let mut watch_control = WatchControl::new(watcher);
        watch_control.configure(&settings);

        let worker_signals = signals.clone();
        let worker = thread::Builder::new()
            .name("token-monitor-worker".to_owned())
            .spawn(move || {
                worker_loop(
                    receiver,
                    worker_signals,
                    watch_control,
                    settings,
                    store,
                    persisted,
                    publish,
                );
            })
            .context("failed to start monitor worker")?;
        signals.signal();
        Ok(Self {
            signals,
            worker: Some(worker),
        })
    }

    /// Nonblocking and coalescing. `false` means shutdown has already begun.
    pub fn request_refresh(&self) -> bool {
        if self.signals.shutdown.load(Ordering::Acquire) {
            return false;
        }
        self.signals.signal();
        true
    }

    /// Retains only the newest monotonic settings/day/pricing generations.
    /// The worker observes the latest accepted value; intermediate replacements
    /// can be coalesced without reverting source selection.
    pub fn replace_settings(&self, settings: MonitorSettings) -> Result<bool> {
        validate_settings(&settings)?;
        if self.signals.shutdown.load(Ordering::Acquire) {
            return Ok(false);
        }
        let mut current = self
            .signals
            .latest_settings
            .lock()
            .map_err(|_| anyhow!("monitor settings state unavailable"))?;
        if settings.generation < current.generation
            || settings.day_generation < current.day_generation
            || settings.pricing_generation < current.pricing_generation
        {
            return Ok(false);
        }
        if settings.generation == current.generation
            && (settings.timezone != current.timezone || settings.sources != current.sources)
        {
            bail!("source settings changed without a new settings generation");
        }
        if *current == settings {
            return Ok(false);
        }
        *current = settings;
        drop(current);
        self.signals.signal();
        Ok(true)
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        self.signals.stop();
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            tracing::error!("monitor worker terminated unexpectedly");
        }
    }
}

fn same_observations(
    committed: &crate::sources::SourceSnapshot,
    candidate: &crate::sources::SourceSnapshot,
) -> bool {
    committed.source_id == candidate.source_id
        && committed.format == candidate.format
        && committed.parser_pin == candidate.parser_pin
        && committed.rows == candidate.rows
        && committed.retained == candidate.retained
        && committed.totals == candidate.totals
        && committed.cache_write_1h == candidate.cache_write_1h
        && committed.incomplete_tail == candidate.incomplete_tail
        && committed.has_supported_usage == candidate.has_supported_usage
}

fn validate_settings(settings: &MonitorSettings) -> Result<()> {
    if settings.sources.len() > MAX_SOURCES {
        bail!("monitor source capacity exceeded");
    }
    let mut paths = BTreeSet::new();
    let mut source_ids = BTreeSet::new();
    if settings
        .sources
        .iter()
        .any(|source| !paths.insert(source.path.clone()))
    {
        bail!("monitor source paths must be unique");
    }
    if settings
        .sources
        .iter()
        .any(|source| !source_ids.insert(source.source_id))
    {
        bail!("monitor source identities must be unique");
    }
    Ok(())
}

fn event_matches_sources(event: &Event, signals: &Signals) -> bool {
    if event.paths.is_empty() {
        return true;
    }
    let Ok(settings) = signals.latest_settings.lock() else {
        return true;
    };
    event.paths.iter().any(|event_path| {
        settings
            .sources
            .iter()
            .any(|source| event_path == &source.path)
    })
}

struct WatchControl {
    watcher: Option<RecommendedWatcher>,
    roots: BTreeSet<PathBuf>,
    available: bool,
}

impl WatchControl {
    fn new(watcher: Option<RecommendedWatcher>) -> Self {
        let available = watcher.is_some();
        Self {
            watcher,
            roots: BTreeSet::new(),
            available,
        }
    }

    fn configure(&mut self, settings: &MonitorSettings) {
        let desired = watch_roots(settings);
        let Some(watcher) = self.watcher.as_mut() else {
            self.roots.clear();
            self.available = desired.is_empty();
            return;
        };
        let mut active = BTreeSet::new();
        for root in &self.roots {
            if desired.contains(root) || watcher.unwatch(root).is_err() {
                active.insert(root.clone());
            }
        }
        for root in &desired {
            if !active.contains(root) {
                if watcher.watch(root, RecursiveMode::NonRecursive).is_ok() {
                    active.insert(root.clone());
                } else {
                    tracing::warn!(
                        "native source root unavailable; manual refresh remains available"
                    );
                }
            }
        }
        self.available = active == desired;
        self.roots = active;
    }

    fn status(&self, signals: &Signals) -> WatchStatus {
        if self.available && !signals.watch_failed.load(Ordering::Acquire) {
            WatchStatus::Available
        } else {
            WatchStatus::Unavailable
        }
    }
}

fn watch_roots(settings: &MonitorSettings) -> BTreeSet<PathBuf> {
    settings
        .sources
        .iter()
        .map(|source| {
            source
                .path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf()
        })
        .collect()
}

struct RuntimeSource {
    spec: MonitoredSource,
    reader: SourceReader,
    status: SourceHealthStatus,
    ledger_generation: Option<u64>,
}

impl RuntimeSource {
    fn new(spec: MonitoredSource, persisted: Option<Arc<crate::sources::SourceSnapshot>>) -> Self {
        let input = SourceInput::from_monitored_source(&spec);
        let mut reader = SourceReader::with_source_input(
            spec.source_id,
            spec.format,
            spec.session.clone(),
            input,
        );
        let ledger_generation = persisted.as_ref().map(|snapshot| snapshot.generation);
        if let Some(snapshot) = persisted {
            debug_assert!(reader.restore_last_good(snapshot));
        }
        Self {
            spec,
            reader,
            status: if ledger_generation.is_some() {
                SourceHealthStatus::Retained
            } else {
                SourceHealthStatus::Pending
            },
            ledger_generation,
        }
    }

    fn health(&self) -> SourceHealth {
        SourceHealth {
            source_id: self.spec.source_id,
            format: self.spec.format,
            status: self.status,
            source_generation: self.ledger_generation,
        }
    }
}

struct WorkerState {
    settings: MonitorSettings,
    sources: BTreeMap<SourceId, RuntimeSource>,
    committed_sources: BTreeMap<SourceId, Arc<crate::sources::SourceSnapshot>>,
    ledger: CanonicalLedger,
    store: Arc<dyn RuntimePersistence>,
    runtime_generation: u64,
    runtime_initialized: bool,
    committed_settings_generation: u64,
    committed_day_generation: u64,
    settings_dirty: bool,
    publication_generation: u64,
    watch_status: WatchStatus,
    coalesced: u64,
    publish: Publish,
    metrics: RuntimeMetrics,
}

impl WorkerState {
    fn new(
        settings: MonitorSettings,
        store: Arc<dyn RuntimePersistence>,
        persisted: Arc<PersistedRuntimeSnapshot>,
        publish: Publish,
    ) -> Result<Self> {
        let committed_sources = persisted
            .sources
            .iter()
            .map(|source| (source.source_id, source.clone()))
            .collect::<BTreeMap<_, _>>();
        let mut settings_dirty = false;
        let ledger = if persisted.initialized {
            let timezone = persisted
                .timezone_name
                .as_deref()
                .ok_or_else(|| anyhow!("persisted runtime timezone unavailable"))?
                .parse::<BucketTimezone>()
                .context("persisted runtime timezone invalid")?;
            settings_dirty = persisted.settings_generation != settings.generation
                || persisted.day_generation != settings.day_generation
                || timezone != settings.timezone;
            CanonicalLedger::rehydrate(
                timezone,
                committed_sources.values().cloned(),
                persisted.generation,
            )
            .context("persisted runtime canonical state invalid")?
        } else {
            CanonicalLedger::new(settings.timezone)
        };
        let sources = settings
            .sources
            .iter()
            .cloned()
            .map(|spec| {
                let persisted = committed_sources.get(&spec.source_id).cloned();
                (spec.source_id, RuntimeSource::new(spec, persisted))
            })
            .collect();
        Ok(Self {
            settings,
            sources,
            committed_sources,
            ledger,
            store,
            runtime_generation: persisted.generation,
            runtime_initialized: persisted.initialized,
            committed_settings_generation: persisted.settings_generation,
            committed_day_generation: persisted.day_generation,
            settings_dirty,
            publication_generation: 0,
            watch_status: WatchStatus::Available,
            coalesced: 0,
            publish,
            metrics: RuntimeMetrics {
                owned_worker_threads: 1,
                ..RuntimeMetrics::default()
            },
        })
    }

    fn apply_settings(&mut self, settings: MonitorSettings) {
        if settings.generation != self.settings.generation
            || settings.day_generation != self.settings.day_generation
            || settings.timezone != self.settings.timezone
        {
            self.settings_dirty = true;
        }
        let mut previous = std::mem::take(&mut self.sources);
        let mut sources = BTreeMap::new();
        for spec in settings.sources.iter().cloned() {
            let source = match previous.remove(&spec.source_id) {
                Some(source) if source.spec == spec => source,
                _ => RuntimeSource::new(
                    spec.clone(),
                    self.committed_sources.get(&spec.source_id).cloned(),
                ),
            };
            sources.insert(spec.source_id, source);
        }
        self.settings = settings;
        self.sources = sources;
    }

    fn reconcile(&mut self) -> ReconcileOutcome {
        let started = Instant::now();
        let mut scan_bytes = 0_u64;
        let mut outcome = ReconcileOutcome::default();
        let source_ids = self.sources.keys().copied().collect::<Vec<_>>();
        for source_id in source_ids {
            let candidate = {
                let source = self
                    .sources
                    .get_mut(&source_id)
                    .expect("source identity came from the runtime source map");
                loop {
                    match source.reader.step() {
                        Ok(step) => {
                            scan_bytes = scan_bytes
                                .saturating_add(u64::try_from(step.bytes_read).unwrap_or(u64::MAX));
                            match step.progress {
                                SourceProgress::Pending => continue,
                                SourceProgress::Unchanged | SourceProgress::Published { .. } => {
                                    break source.reader.last_good();
                                }
                            }
                        }
                        Err(error) => {
                            tracing::warn!(
                                "source reconciliation failed; last-good totals retained"
                            );
                            let status = SourceHealthStatus::from_source_error(&error);
                            if source.status != status {
                                source.status = status;
                                outcome.health_changed = true;
                            }
                            break None;
                        }
                    }
                }
            };
            let Some(candidate) = candidate else {
                continue;
            };
            let committed_generation = self
                .committed_sources
                .get(&source_id)
                .map(|source| source.generation);
            if let Some(committed) = self.committed_sources.get(&source_id)
                && (committed.generation == candidate.generation
                    || same_observations(committed, &candidate))
            {
                if let Some(source) = self.sources.get_mut(&source_id) {
                    if source.status != SourceHealthStatus::Healthy {
                        source.status = SourceHealthStatus::Healthy;
                        outcome.health_changed = true;
                    }
                    source.ledger_generation = committed_generation;
                }
                continue;
            }
            match self.commit_candidate(source_id, candidate) {
                Ok(()) => {
                    let source = self
                        .sources
                        .get_mut(&source_id)
                        .expect("committed source remains configured");
                    source.status = SourceHealthStatus::Healthy;
                    source.ledger_generation = self
                        .committed_sources
                        .get(&source_id)
                        .map(|source| source.generation);
                    outcome.committed = true;
                }
                Err(CommitCandidateError::Canonicalization) => {
                    tracing::warn!(
                        "canonical source replacement failed; last-good totals retained"
                    );
                    let source = self
                        .sources
                        .get_mut(&source_id)
                        .expect("failed source remains configured");
                    if source.status != SourceHealthStatus::CanonicalizationFailed {
                        source.status = SourceHealthStatus::CanonicalizationFailed;
                        outcome.health_changed = true;
                    }
                }
                Err(CommitCandidateError::Persistence { retry }) => {
                    tracing::warn!("runtime persistence delayed; last-good totals retained");
                    self.mark_persistence_delayed(source_id);
                    outcome.storage_delayed = true;
                    outcome.retry |= retry;
                }
            }
        }
        if self.settings_dirty && !outcome.storage_delayed {
            self.commit_settings_snapshot(&mut outcome);
        }
        self.metrics.reconciliations = self.metrics.reconciliations.saturating_add(1);
        self.metrics.scanned_bytes = self.metrics.scanned_bytes.saturating_add(scan_bytes);
        self.metrics.last_reconciliation_millis =
            u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        outcome
    }

    fn commit_candidate(
        &mut self,
        source_id: SourceId,
        source: Arc<crate::sources::SourceSnapshot>,
    ) -> std::result::Result<(), CommitCandidateError> {
        let mut prepared = CanonicalLedger::rehydrate(
            self.settings.timezone,
            self.committed_sources.values().cloned(),
            self.runtime_generation,
        )
        .map_err(|_| CommitCandidateError::Canonicalization)?;
        let candidate = prepared
            .prepare_replace_source(source)
            .map_err(|_| CommitCandidateError::Canonicalization)?;
        let commit = CanonicalCommit {
            expected_runtime_generation: self.runtime_generation,
            settings_generation: self.settings.generation,
            day_generation: self.settings.day_generation,
            timezone_name: self.settings.timezone.name().to_owned(),
            source: candidate.source().clone(),
            canonical: candidate.canonical(),
        };
        let persistence = self.store.clone();
        let mut reported = false;
        let result =
            persistence.commit_runtime_observed(commit, PERSISTENCE_DELAY_REPORT, &mut || {
                if !reported {
                    reported = true;
                    self.mark_persistence_delayed(source_id);
                }
            });
        let persisted = match result {
            Ok(persisted) => persisted,
            Err(error) => {
                if !reported {
                    self.mark_persistence_delayed(source_id);
                }
                let retry = matches!(error, StoreError::Busy | StoreError::RuntimeConflict { .. });
                return Err(CommitCandidateError::Persistence { retry });
            }
        };
        prepared
            .activate(candidate)
            .map_err(|_| CommitCandidateError::Canonicalization)?;
        self.committed_sources = persisted
            .sources
            .iter()
            .map(|source| (source.source_id, source.clone()))
            .collect();
        self.ledger = prepared;
        self.runtime_generation = persisted.generation;
        self.runtime_initialized = persisted.initialized;
        self.committed_settings_generation = persisted.settings_generation;
        self.committed_day_generation = persisted.day_generation;
        self.settings_dirty = false;
        Ok(())
    }

    /// Settings/day/timezone changes require one new runtime generation even
    /// when no source bytes changed. Bumping one committed source keeps the
    /// store's monotonic source fence while rebuilding every rollup under the
    /// new settings.
    fn commit_settings_snapshot(&mut self, outcome: &mut ReconcileOutcome) {
        let Some(base) = self.committed_sources.values().next().cloned() else {
            self.settings_dirty = false;
            return;
        };
        let Some(generation) = base.generation.checked_add(1) else {
            tracing::error!("settings snapshot generation overflow");
            return;
        };
        let bumped = Arc::new(crate::sources::SourceSnapshot {
            generation,
            ..(*base).clone()
        });
        let source_id = bumped.source_id;
        match self.commit_candidate(source_id, bumped.clone()) {
            Ok(()) => {
                if let Some(source) = self.sources.get_mut(&source_id) {
                    if !source.reader.restore_last_good(bumped) {
                        tracing::error!("settings snapshot source identity mismatch");
                    }
                    source.ledger_generation = Some(generation);
                    if source.status == SourceHealthStatus::PersistenceDelayed {
                        source.status = SourceHealthStatus::Healthy;
                        outcome.health_changed = true;
                    }
                }
                outcome.committed = true;
            }
            Err(CommitCandidateError::Canonicalization) => {
                tracing::warn!("settings snapshot canonicalization failed");
            }
            Err(CommitCandidateError::Persistence { retry }) => {
                outcome.storage_delayed = true;
                outcome.retry |= retry;
            }
        }
    }

    fn mark_persistence_delayed(&mut self, source_id: SourceId) {
        let changed = self.sources.get_mut(&source_id).is_some_and(|source| {
            if source.status == SourceHealthStatus::PersistenceDelayed {
                false
            } else {
                source.status = SourceHealthStatus::PersistenceDelayed;
                true
            }
        });
        if changed {
            self.publish_snapshot();
        }
    }

    fn publish_snapshot(&mut self) -> bool {
        let Some(snapshot) = self.snapshot() else {
            return false;
        };
        (self.publish)(Arc::new(snapshot));
        true
    }

    fn snapshot(&mut self) -> Option<MonitorSnapshot> {
        self.publication_generation = self.publication_generation.checked_add(1)?;
        self.metrics.coalesced_signals = self.coalesced;
        let mut source_health = self
            .committed_sources
            .values()
            .map(|source| {
                (
                    source.source_id,
                    SourceHealth {
                        source_id: source.source_id,
                        format: source.format,
                        status: SourceHealthStatus::Retained,
                        source_generation: Some(source.generation),
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        for source in self.sources.values() {
            source_health.insert(source.spec.source_id, source.health());
        }
        Some(MonitorSnapshot {
            generation: self.publication_generation,
            runtime_generation: self.runtime_generation,
            runtime_initialized: self.runtime_initialized,
            settings_generation: self.committed_settings_generation,
            day_generation: self.committed_day_generation,
            pricing_generation: self.settings.pricing_generation,
            canonical: self.ledger.last_good(),
            source_health: source_health.into_values().collect::<Vec<_>>().into(),
            watch_status: self.watch_status,
            metrics: self.metrics,
        })
    }
}

#[derive(Default)]
struct ReconcileOutcome {
    committed: bool,
    health_changed: bool,
    storage_delayed: bool,
    retry: bool,
}

enum CommitCandidateError {
    Canonicalization,
    Persistence { retry: bool },
}

fn worker_loop<F>(
    receiver: Receiver<()>,
    signals: Arc<Signals>,
    mut watch_control: WatchControl,
    settings: MonitorSettings,
    store: Arc<dyn RuntimePersistence>,
    persisted: Arc<PersistedRuntimeSnapshot>,
    publish: F,
) where
    F: Fn(Arc<MonitorSnapshot>) + Send + 'static,
{
    let mut state = match WorkerState::new(settings, store, persisted, Box::new(publish)) {
        Ok(state) => state,
        Err(error) => {
            tracing::error!(error = %error, "persisted monitor state invalid");
            return;
        }
    };
    state.watch_status = watch_control.status(&signals);
    state.coalesced = signals.coalesced.load(Ordering::Relaxed);
    if state.runtime_initialized && !state.publish_snapshot() {
        tracing::error!("first persisted monitor publication cannot overflow");
    }
    while wait_for_debounced_signal(&receiver, &signals) {
        let Some(latest) = signals.settings() else {
            tracing::error!("monitor settings state unavailable");
            break;
        };
        let settings_changed = latest != state.settings;
        if settings_changed {
            state.apply_settings(latest.clone());
            signals.watch_failed.store(false, Ordering::Release);
            watch_control.configure(&latest);
        }

        let scan_epoch = signals.epoch.load(Ordering::Acquire);
        state.watch_status = watch_control.status(&signals);
        state.coalesced = signals.coalesced.load(Ordering::Relaxed);
        let outcome = state.reconcile();
        let Some(current) = signals.settings() else {
            break;
        };
        if current != state.settings {
            signals.signal();
            continue;
        }
        if (outcome.committed
            || (settings_changed || outcome.health_changed) && !outcome.storage_delayed)
            && !state.publish_snapshot()
        {
            tracing::error!("monitor publication generation overflow");
            break;
        }

        if outcome.retry || signals.epoch.load(Ordering::Acquire) != scan_epoch {
            signals.signal();
        }
    }
}

fn wait_for_debounced_signal(receiver: &Receiver<()>, signals: &Signals) -> bool {
    if receiver.recv().is_err() || signals.shutdown.load(Ordering::Acquire) {
        return false;
    }
    let started = Instant::now();
    let mut observed_epoch = signals.epoch.load(Ordering::Acquire);
    loop {
        if signals.shutdown.load(Ordering::Acquire) {
            return false;
        }
        let elapsed = started.elapsed();
        if elapsed >= CONTINUOUS_WRITE_CEILING {
            break;
        }
        let wait = DEBOUNCE.min(CONTINUOUS_WRITE_CEILING - elapsed);
        match receiver.recv_timeout(wait) {
            Ok(()) => {
                observed_epoch = signals.epoch.load(Ordering::Acquire);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let current_epoch = signals.epoch.load(Ordering::Acquire);
                if current_epoch == observed_epoch {
                    break;
                }
                observed_epoch = current_epoch;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return false,
        }
    }
    while receiver.try_recv().is_ok() {}
    !signals.shutdown.load(Ordering::Acquire)
}
