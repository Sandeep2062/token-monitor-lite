use std::cell::Cell;
use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};
use token_monitor_lite::aggregation::BucketTimezone;
use token_monitor_lite::discovery::{HostPaths, HostPlatform, resolve_paths};
use token_monitor_lite::parsers::ReaderError;
use token_monitor_lite::runtime::{
    Monitor, MonitorSettings, MonitorSnapshot, MonitoredSource, RuntimePersistence,
    SourceHealthStatus, WatchStatus,
};
use token_monitor_lite::source_inputs::SqliteSourceError;
use token_monitor_lite::sources::{SourceError, SourceFormat, SourceId};
use token_monitor_lite::storage::{
    CanonicalCommit, NativeSettings, PersistedRuntimeSnapshot, StoreError, StoreHandle,
    StoreSnapshot,
};

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct FixtureSource {
    directory: PathBuf,
    path: PathBuf,
}

impl FixtureSource {
    fn new(bytes: &[u8]) -> Self {
        let directory = std::env::temp_dir().join(format!(
            "token-monitor-runtime-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("session.jsonl");
        fs::write(&path, bytes).unwrap();
        Self { directory, path }
    }

    fn append(&self, bytes: &[u8]) {
        OpenOptions::new()
            .append(true)
            .open(&self.path)
            .unwrap()
            .write_all(bytes)
            .unwrap();
    }

    fn write(&self, bytes: &[u8]) {
        fs::write(&self.path, bytes).unwrap();
    }
}

impl Drop for FixtureSource {
    fn drop(&mut self) {
        if self.directory.exists() {
            fs::remove_dir_all(&self.directory).unwrap();
        }
    }
}

fn claude(id: &str, input: u64) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(&serde_json::json!({
        "type":"assistant", "timestamp":"2026-11-09T10:00:00Z",
        "message":{"id":id,"model":"claude","usage":{"input_tokens":input}}
    }))
    .unwrap();
    bytes.push(b'\n');
    bytes
}

fn settings(generation: u64, sources: Vec<MonitoredSource>) -> MonitorSettings {
    MonitorSettings {
        generation,
        day_generation: 0,
        pricing_generation: 0,
        timezone: "UTC".parse::<BucketTimezone>().unwrap(),
        sources,
    }
}

fn open_store(root: &std::path::Path) -> StoreHandle {
    let paths = resolve_paths(&HostPaths {
        platform: HostPlatform::Linux,
        home: root.to_path_buf(),
        app_data: None,
        local_app_data: None,
        xdg_config_home: None,
        xdg_state_home: None,
        legacy_shared_override: Some(root.join("legacy")),
        native_data_override: Some(root.join("native")),
        enabled_clients: BTreeSet::new(),
    })
    .unwrap();
    StoreHandle::open(paths.native).unwrap().0
}

fn save_settings(store: &StoreHandle, expected_revision: u64) -> Arc<StoreSnapshot> {
    store
        .save_settings(
            expected_revision,
            NativeSettings::from_json_bytes(
                format!(r#"{{"revision":{}}}"#, expected_revision + 1).as_bytes(),
            )
            .unwrap(),
        )
        .unwrap()
}

fn source(path: PathBuf, session: &str) -> MonitoredSource {
    MonitoredSource {
        source_id: SourceId::for_path(SourceFormat::ClaudeCli, &path),
        path,
        format: SourceFormat::ClaudeCli,
        session: session.to_owned(),
    }
}

struct GatedStore {
    inner: StoreHandle,
    gate: Arc<(Mutex<bool>, Condvar)>,
    block_next: AtomicBool,
}

impl GatedStore {
    fn new(inner: StoreHandle) -> Arc<Self> {
        Arc::new(Self {
            inner,
            gate: Arc::new((Mutex::new(false), Condvar::new())),
            block_next: AtomicBool::new(false),
        })
    }

    fn block_next_commit(&self) {
        *self.gate.0.lock().unwrap() = false;
        self.block_next.store(true, Ordering::SeqCst);
    }

    fn release(&self) {
        *self.gate.0.lock().unwrap() = true;
        self.gate.1.notify_all();
    }
}

impl RuntimePersistence for GatedStore {
    fn runtime_snapshot(&self) -> Result<Arc<PersistedRuntimeSnapshot>, StoreError> {
        self.inner.runtime_snapshot()
    }

    fn commit_runtime(
        &self,
        commit: CanonicalCommit,
    ) -> Result<Arc<PersistedRuntimeSnapshot>, StoreError> {
        self.inner.commit_runtime(commit)
    }

    fn commit_runtime_observed(
        &self,
        commit: CanonicalCommit,
        _wait: Duration,
        on_wait: &mut dyn FnMut(),
    ) -> Result<Arc<PersistedRuntimeSnapshot>, StoreError> {
        if self.block_next.swap(false, Ordering::SeqCst) {
            on_wait();
            let (lock, condition) = &*self.gate;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = condition.wait(released).unwrap();
            }
        }
        self.inner.commit_runtime(commit)
    }
}

fn receive_until(
    receiver: &mpsc::Receiver<Arc<MonitorSnapshot>>,
    timeout: Duration,
    predicate: impl Fn(&MonitorSnapshot) -> bool,
) -> Arc<MonitorSnapshot> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let snapshot = receiver
            .recv_timeout(remaining)
            .expect("monitor did not publish the expected snapshot in time");
        if predicate(&snapshot) {
            return snapshot;
        }
    }
}

#[test]
fn native_append_publishes_exact_usage_within_five_seconds() {
    let fixture = FixtureSource::new(b"\n");
    let store = open_store(&fixture.directory);
    let stored = save_settings(&store, 0);
    let (sender, receiver) = mpsc::channel();
    let monitor = Monitor::start(
        settings(
            stored.settings_generation,
            vec![source(fixture.path.clone(), "s")],
        ),
        store.clone(),
        move |s| {
            sender.send(s).unwrap();
        },
    )
    .unwrap();
    receive_until(&receiver, Duration::from_secs(5), |snapshot| {
        snapshot.canonical.totals.input == 0
    });

    let started = Instant::now();
    fixture.append(&claude("response-a", 7));
    let snapshot = receive_until(&receiver, Duration::from_secs(5), |snapshot| {
        snapshot.canonical.totals.input == 7
    });

    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(
        snapshot.source_health[0].status,
        SourceHealthStatus::Healthy
    );
    assert!(snapshot.metrics.scanned_bytes >= 1);
    assert_eq!(snapshot.metrics.owned_worker_threads, 1);
    drop(monitor);
    store.shutdown().unwrap();
}

#[test]
fn dirty_signals_during_blocked_publication_are_coalesced_and_retried() {
    let fixture = FixtureSource::new(&claude("response-a", 7));
    let store = open_store(&fixture.directory);
    let stored = save_settings(&store, 0);
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let block_first = Arc::new(AtomicBool::new(true));
    let (sender, receiver) = mpsc::channel();
    let publish_gate = gate.clone();
    let publish_block_first = block_first.clone();
    let monitor = Monitor::start(
        settings(
            stored.settings_generation,
            vec![source(fixture.path.clone(), "s")],
        ),
        store.clone(),
        move |s| {
            sender.send(s).unwrap();
            if publish_block_first.swap(false, Ordering::SeqCst) {
                let (lock, condition) = &*publish_gate;
                let mut released = lock.lock().unwrap();
                while !*released {
                    released = condition.wait(released).unwrap();
                }
            }
        },
    )
    .unwrap();
    let first = receive_until(&receiver, Duration::from_secs(5), |snapshot| {
        snapshot.canonical.totals.input == 7
    });
    assert_eq!(first.metrics.owned_worker_threads, 1);

    fixture.append(&claude("response-b", 3));
    for _ in 0..10_000 {
        assert!(monitor.request_refresh());
    }
    let (lock, condition) = &*gate;
    *lock.lock().unwrap() = true;
    condition.notify_all();

    let second = receive_until(&receiver, Duration::from_secs(5), |snapshot| {
        snapshot.canonical.totals.input == 10
    });
    assert!(second.metrics.coalesced_signals > 0);
    drop(monitor);
    store.shutdown().unwrap();
}

#[test]
fn read_failure_and_unconfiguration_retain_last_good_source() {
    let fixture = FixtureSource::new(&claude("response-a", 9));
    let store = open_store(&fixture.directory);
    let stored = save_settings(&store, 0);
    let (sender, receiver) = mpsc::channel();
    let monitor = Monitor::start(
        settings(
            stored.settings_generation,
            vec![source(fixture.path.clone(), "s")],
        ),
        store.clone(),
        move |s| {
            sender.send(s).unwrap();
        },
    )
    .unwrap();
    receive_until(&receiver, Duration::from_secs(5), |snapshot| {
        snapshot.canonical.totals.input == 9
    });

    fs::remove_file(&fixture.path).unwrap();
    assert!(monitor.request_refresh());
    let failed = receive_until(&receiver, Duration::from_secs(5), |snapshot| {
        snapshot.source_health[0].status == SourceHealthStatus::ReadFailed
    });
    assert_eq!(failed.canonical.totals.input, 9);
    assert!(failed.source_health[0].source_generation.is_some());

    let stored = save_settings(&store, 1);
    assert!(
        monitor
            .replace_settings(settings(stored.settings_generation, Vec::new()))
            .unwrap()
    );
    let retained = receive_until(&receiver, Duration::from_secs(5), |snapshot| {
        snapshot.source_health[0].status == SourceHealthStatus::Retained
    });
    assert_eq!(retained.canonical.totals.input, 9);
    assert_eq!(
        retained.runtime_generation,
        failed.runtime_generation + 1,
        "a settings-only change commits one new runtime generation"
    );
    assert_eq!(retained.settings_generation, stored.settings_generation);
    drop(monitor);
    store.shutdown().unwrap();
}

#[test]
fn unavailable_storage_publishes_delayed_health_without_newer_generation() {
    let fixture = FixtureSource::new(&claude("response-a", 7));
    let store = open_store(&fixture.directory);
    let stored = save_settings(&store, 0);
    let (sender, receiver) = mpsc::channel();
    let monitor = Monitor::start(
        settings(
            stored.settings_generation,
            vec![source(fixture.path.clone(), "s")],
        ),
        store.clone(),
        move |snapshot| sender.send(snapshot).unwrap(),
    )
    .unwrap();
    let committed = receive_until(&receiver, Duration::from_secs(5), |snapshot| {
        snapshot.canonical.totals.input == 7
    });

    store.shutdown().unwrap();
    fixture.append(&claude("response-b", 3));
    assert!(monitor.request_refresh());
    let delayed = receive_until(&receiver, Duration::from_secs(5), |snapshot| {
        snapshot.source_health[0].status == SourceHealthStatus::PersistenceDelayed
    });
    assert_eq!(delayed.canonical.totals.input, 7);
    assert_eq!(delayed.runtime_generation, committed.runtime_generation);
    assert_eq!(delayed.source_health[0].source_generation, Some(1));
    assert!(receiver.recv_timeout(Duration::from_millis(300)).is_err());
    drop(monitor);
}

#[test]
fn restart_publishes_retained_commit_before_unavailable_source_scan() {
    let fixture = FixtureSource::new(&claude("response-a", 9));
    let store = open_store(&fixture.directory);
    let stored = save_settings(&store, 0);
    let monitored = source(fixture.path.clone(), "s");
    let (sender, receiver) = mpsc::channel();
    let monitor = Monitor::start(
        settings(stored.settings_generation, vec![monitored.clone()]),
        store.clone(),
        move |snapshot| sender.send(snapshot).unwrap(),
    )
    .unwrap();
    receive_until(&receiver, Duration::from_secs(5), |snapshot| {
        snapshot.canonical.totals.input == 9
    });
    drop(monitor);
    store.shutdown().unwrap();
    fs::remove_file(&fixture.path).unwrap();

    let store = open_store(&fixture.directory);
    let stored = store.snapshot().unwrap();
    let (sender, receiver) = mpsc::channel();
    let monitor = Monitor::start(
        settings(stored.settings_generation, vec![monitored]),
        store.clone(),
        move |snapshot| sender.send(snapshot).unwrap(),
    )
    .unwrap();
    let retained = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(retained.canonical.totals.input, 9);
    assert_eq!(retained.runtime_generation, 1);
    assert_eq!(
        retained.source_health[0].status,
        SourceHealthStatus::Retained
    );

    let unavailable = receive_until(&receiver, Duration::from_secs(5), |snapshot| {
        snapshot.source_health[0].status == SourceHealthStatus::ReadFailed
    });
    assert_eq!(unavailable.canonical.totals.input, 9);
    assert_eq!(unavailable.runtime_generation, retained.runtime_generation);
    drop(monitor);
    store.shutdown().unwrap();
}

#[test]
fn latest_settings_replace_roots_and_stale_generations_are_discarded() {
    let first = FixtureSource::new(&claude("response-a", 1));
    let stale = FixtureSource::new(&claude("response-b", 2));
    let latest = FixtureSource::new(&claude("response-c", 3));
    let store = open_store(&first.directory);
    let stored = save_settings(&store, 0);
    let (sender, receiver) = mpsc::channel();
    let monitor = Monitor::start(
        settings(
            stored.settings_generation,
            vec![source(first.path.clone(), "first")],
        ),
        store.clone(),
        move |s| {
            sender.send(s).unwrap();
        },
    )
    .unwrap();
    receive_until(&receiver, Duration::from_secs(5), |snapshot| {
        snapshot.canonical.totals.input == 1
    });

    let second = save_settings(&store, 1);
    assert!(
        monitor
            .replace_settings(settings(
                second.settings_generation,
                vec![source(stale.path.clone(), "stale")],
            ))
            .unwrap()
    );
    let third = save_settings(&store, 2);
    assert!(
        monitor
            .replace_settings(settings(
                third.settings_generation,
                vec![source(latest.path.clone(), "latest")],
            ))
            .unwrap()
    );
    assert!(
        !monitor
            .replace_settings(settings(2, vec![source(stale.path.clone(), "stale")]))
            .unwrap()
    );

    let snapshot = receive_until(&receiver, Duration::from_secs(5), |snapshot| {
        snapshot.settings_generation == third.settings_generation
            && snapshot.canonical.totals.input == 4
    });
    assert_eq!(snapshot.source_health.len(), 2);
    assert!(snapshot.source_health.iter().any(|health| {
        health.source_id == SourceId::for_path(SourceFormat::ClaudeCli, &latest.path)
            && health.status == SourceHealthStatus::Healthy
    }));
    assert!(snapshot.source_health.iter().any(|health| {
        health.source_id == SourceId::for_path(SourceFormat::ClaudeCli, &first.path)
            && health.status == SourceHealthStatus::Retained
    }));
    drop(monitor);
    store.shutdown().unwrap();
}

#[test]
fn watcher_setup_failure_is_explicit_and_manual_refresh_remains_available() {
    let missing = std::env::temp_dir()
        .join(format!(
            "token-monitor-missing-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
        .join("session.jsonl");
    let store_fixture = FixtureSource::new(b"\n");
    let store = open_store(&store_fixture.directory);
    let stored = save_settings(&store, 0);
    let (sender, receiver) = mpsc::channel();
    let monitor = Monitor::start(
        settings(stored.settings_generation, vec![source(missing, "missing")]),
        store.clone(),
        move |s| {
            sender.send(s).unwrap();
        },
    )
    .unwrap();

    let snapshot = receive_until(&receiver, Duration::from_secs(5), |snapshot| {
        snapshot.watch_status == WatchStatus::Unavailable
            && snapshot.source_health[0].status == SourceHealthStatus::ReadFailed
    });
    assert_eq!(snapshot.canonical.totals.input, 0);
    assert!(!snapshot.runtime_initialized);
    assert!(monitor.request_refresh());
    drop(monitor);
    store.shutdown().unwrap();
}

#[test]
fn shutdown_joins_the_owned_worker_without_waiting_for_debounce() {
    let fixture = FixtureSource::new(b"\n");
    let store = open_store(&fixture.directory);
    let stored = save_settings(&store, 0);
    let (sender, receiver) = mpsc::channel();
    let monitor = Monitor::start(
        settings(
            stored.settings_generation,
            vec![source(fixture.path.clone(), "s")],
        ),
        store.clone(),
        move |s| {
            sender.send(s).unwrap();
        },
    )
    .unwrap();
    receive_until(&receiver, Duration::from_secs(5), |_| true);

    let started = Instant::now();
    drop(monitor);
    assert!(started.elapsed() < Duration::from_secs(2));
    store.shutdown().unwrap();
}

#[test]
fn delayed_commit_reports_retained_health_then_publishes_exactly_one_commit() {
    let fixture = FixtureSource::new(&claude("response-a", 7));
    let store = open_store(&fixture.directory);
    let stored = save_settings(&store, 0);
    let gate = GatedStore::new(store.clone());
    let (sender, receiver) = mpsc::channel();
    let monitor = Monitor::start_with_persistence(
        settings(
            stored.settings_generation,
            vec![source(fixture.path.clone(), "s")],
        ),
        gate.clone(),
        move |snapshot| sender.send(snapshot).unwrap(),
    )
    .unwrap();
    let initial = receive_until(&receiver, Duration::from_secs(5), |snapshot| {
        snapshot.canonical.totals.input == 7
            && snapshot.source_health[0].status == SourceHealthStatus::Healthy
    });
    assert_eq!(initial.runtime_generation, 1);

    gate.block_next_commit();
    fixture.append(&claude("response-b", 3));
    assert!(monitor.request_refresh());
    let delayed = receive_until(&receiver, Duration::from_secs(5), |snapshot| {
        snapshot.source_health[0].status == SourceHealthStatus::PersistenceDelayed
    });
    assert_eq!(delayed.canonical.totals.input, 7);
    assert_eq!(delayed.runtime_generation, initial.runtime_generation);
    gate.release();

    let committed = receive_until(&receiver, Duration::from_secs(5), |snapshot| {
        snapshot.canonical.totals.input == 10
            && snapshot.source_health[0].status == SourceHealthStatus::Healthy
    });
    assert_eq!(committed.runtime_generation, 2);
    assert!(receiver.recv_timeout(Duration::from_millis(300)).is_err());
    drop(monitor);
    store.shutdown().unwrap();
}

#[test]
fn filesystem_event_admitted_during_commit_rearms_reconciliation() {
    let fixture = FixtureSource::new(&claude("response-a", 7));
    let store = open_store(&fixture.directory);
    let stored = save_settings(&store, 0);
    let gate = GatedStore::new(store.clone());
    let (sender, receiver) = mpsc::channel();
    let monitor = Monitor::start_with_persistence(
        settings(
            stored.settings_generation,
            vec![source(fixture.path.clone(), "s")],
        ),
        gate.clone(),
        move |snapshot| sender.send(snapshot).unwrap(),
    )
    .unwrap();
    receive_until(&receiver, Duration::from_secs(5), |snapshot| {
        snapshot.canonical.totals.input == 7
    });

    gate.block_next_commit();
    fixture.append(&claude("response-b", 3));
    assert!(monitor.request_refresh());
    receive_until(&receiver, Duration::from_secs(5), |snapshot| {
        snapshot.source_health[0].status == SourceHealthStatus::PersistenceDelayed
    });

    fixture.append(&claude("response-c", 4));
    assert!(monitor.request_refresh());
    gate.release();

    let complete = receive_until(&receiver, Duration::from_secs(5), |snapshot| {
        snapshot.canonical.totals.input == 14
            && snapshot.source_health[0].status == SourceHealthStatus::Healthy
    });
    assert_eq!(complete.runtime_generation, 3);
    drop(monitor);
    store.shutdown().unwrap();
}

#[test]
fn stale_commit_after_settings_replace_publishes_only_the_new_generation() {
    let fixture = FixtureSource::new(&claude("response-a", 7));
    let store = open_store(&fixture.directory);
    let stored = save_settings(&store, 0);
    let gate = GatedStore::new(store.clone());
    let (sender, receiver) = mpsc::channel();
    let monitor = Monitor::start_with_persistence(
        settings(
            stored.settings_generation,
            vec![source(fixture.path.clone(), "s")],
        ),
        gate.clone(),
        move |snapshot| sender.send(snapshot).unwrap(),
    )
    .unwrap();
    receive_until(&receiver, Duration::from_secs(5), |snapshot| {
        snapshot.canonical.totals.input == 7
    });

    gate.block_next_commit();
    fixture.append(&claude("response-b", 3));
    assert!(monitor.request_refresh());
    receive_until(&receiver, Duration::from_secs(5), |snapshot| {
        snapshot.source_health[0].status == SourceHealthStatus::PersistenceDelayed
    });

    let replaced = save_settings(&store, 1);
    assert!(
        monitor
            .replace_settings(settings(
                replaced.settings_generation,
                vec![source(fixture.path.clone(), "s")],
            ))
            .unwrap()
    );
    gate.release();

    let stale_published = Cell::new(false);
    let refreshed = receive_until(&receiver, Duration::from_secs(5), |snapshot| {
        if snapshot.settings_generation == stored.settings_generation
            && snapshot.canonical.totals.input == 10
        {
            stale_published.set(true);
        }
        snapshot.settings_generation == replaced.settings_generation
            && snapshot.canonical.totals.input == 10
            && snapshot.source_health[0].status == SourceHealthStatus::Healthy
    });
    assert!(!stale_published.get());
    assert_eq!(refreshed.runtime_generation, 2);
    drop(monitor);
    store.shutdown().unwrap();
}

#[test]
fn recovered_source_returns_to_healthy_without_duplicate_observations() {
    let bytes = claude("response-a", 9);
    let fixture = FixtureSource::new(&bytes);
    let store = open_store(&fixture.directory);
    let stored = save_settings(&store, 0);
    let (sender, receiver) = mpsc::channel();
    let monitor = Monitor::start(
        settings(
            stored.settings_generation,
            vec![source(fixture.path.clone(), "s")],
        ),
        store.clone(),
        move |snapshot| sender.send(snapshot).unwrap(),
    )
    .unwrap();
    receive_until(&receiver, Duration::from_secs(5), |snapshot| {
        snapshot.canonical.totals.input == 9
    });

    fs::remove_file(&fixture.path).unwrap();
    assert!(monitor.request_refresh());
    receive_until(&receiver, Duration::from_secs(5), |snapshot| {
        snapshot.source_health[0].status == SourceHealthStatus::ReadFailed
    });

    fixture.write(&bytes);
    assert!(monitor.request_refresh());
    let recovered = receive_until(&receiver, Duration::from_secs(5), |snapshot| {
        snapshot.source_health[0].status == SourceHealthStatus::Healthy
    });
    assert_eq!(recovered.canonical.totals.input, 9);
    assert_eq!(recovered.canonical.session_models.len(), 1);
    assert_eq!(
        recovered.source_health[0].source_generation,
        Some(1),
        "restored identical bytes reuse the committed source generation"
    );
    drop(monitor);
    store.shutdown().unwrap();
}

#[test]
fn source_failures_map_to_fixed_health_statuses() {
    assert_eq!(
        SourceHealthStatus::from_source_error(&SourceError::Reader(ReaderError::InvalidRecord)),
        SourceHealthStatus::DecodingFailed
    );
    assert_eq!(
        SourceHealthStatus::from_source_error(&SourceError::Reader(ReaderError::Unsupported(
            "shape"
        ))),
        SourceHealthStatus::Unsupported
    );
    assert_eq!(
        SourceHealthStatus::from_source_error(&SourceError::Reader(ReaderError::Capacity("rows"))),
        SourceHealthStatus::CapacityExceeded
    );
    assert_eq!(
        SourceHealthStatus::from_source_error(&SourceError::Sqlite(SqliteSourceError::Busy)),
        SourceHealthStatus::SourceBusy
    );
    assert_eq!(
        SourceHealthStatus::from_source_error(&SourceError::Sqlite(SqliteSourceError::Schema)),
        SourceHealthStatus::Unsupported
    );
    assert_eq!(
        SourceHealthStatus::from_source_error(&SourceError::Io(
            std::io::ErrorKind::PermissionDenied
        )),
        SourceHealthStatus::SourceBusy
    );
    assert_eq!(
        SourceHealthStatus::from_source_error(&SourceError::Io(std::io::ErrorKind::NotFound)),
        SourceHealthStatus::ReadFailed
    );
    assert_eq!(
        SourceHealthStatus::from_source_error(&SourceError::Overflow),
        SourceHealthStatus::CanonicalizationFailed
    );
}
