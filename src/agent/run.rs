//! One agent run: discover, collect, report.
//!
//! The desktop's collection core is reused verbatim — `discover()` and `Monitor`
//! are the same code paths the window uses. The only choice this module makes is
//! *where the result goes*: a real store, or nowhere.

use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::agent::config::{AgentConfig, ClientSelection};
use crate::agent::dry_run::NullPersistence;
use crate::agent::summary::Summary;
use crate::aggregation::BucketTimezone;
use crate::discovery::{DiscoveryReport, HostPaths, discover};
use crate::runtime::{Monitor, MonitorSettings, MonitorSnapshot, RuntimePersistence};

use crate::storage::StoreHandle;

/// How long a single pass may take before it is reported as timed out.
///
/// Generous on purpose: a first run over 460 sources writes one ledger per source,
/// which took well over five minutes on the development host. A scheduler would
/// rather wait than see a false failure.
pub const PASS_TIMEOUT: Duration = Duration::from_secs(1800);

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("discovery failed: {0}")]
    Discovery(&'static str),
    #[error("store could not be opened")]
    Store,
    /// The pass was still running when the timeout elapsed.
    ///
    /// Reported only when no snapshot arrived. Note that the worker is joined on
    /// drop, so this error is raised *after* any in-flight reconciliation
    /// finishes — it cannot interrupt one. Saying so is better than implying a
    /// deadline this code does not actually enforce.
    #[error("collection did not finish within {}s", PASS_TIMEOUT.as_secs())]
    Timeout,
    #[error("collection failed")]
    Monitor,
}

/// Run once and produce a summary.
pub fn run_once(config: &AgentConfig, mut paths: HostPaths) -> Result<Summary, AgentError> {
    // `--store` / `TOKEN_MONITOR_LITE_DATA_DIR` redirects the application data root.
    // Without this the flag was accepted and ignored, which is worse than absent:
    // a caller who points a scheduled job at a scratch directory would silently
    // keep writing to the real one.
    if let Some(store) = &config.store {
        paths.native_data_override = Some(store.clone());
    }

    // Discovery reads an *empty* enabled set as "collect nothing" — the desktop
    // fills it from saved settings. A CLI run has no settings to read, so an
    // unconfigured selection means every client the catalog tracks.
    match &config.clients {
        ClientSelection::All => {
            paths.enabled_clients = crate::catalog::client_ids()
                .into_iter()
                .map(str::to_owned)
                .collect();
        }
        ClientSelection::Some(clients) => {
            paths.enabled_clients = clients.iter().cloned().collect();
        }
    }

    let report = discover(&paths).map_err(|_| AgentError::Discovery("discover"))?;
    let timezone = resolve_timezone(config)?;
    let settings = settings_from(&report, config, timezone);

    if config.dry_run {
        // No store: `StoreHandle::open` creates the database and spawns a writer,
        // which would break the zero-write promise before any real work began.
        let persistence = Arc::new(NullPersistence::new());
        let snapshot = collect(&settings, persistence.clone())?;
        return Ok(Summary::from_snapshot(
            &snapshot,
            true,
            persistence.commit_count(),
        ));
    }

    let (store, _) = StoreHandle::open(report.native.clone()).map_err(|_| AgentError::Store)?;
    let snapshot = match collect(&settings, Arc::new(store.clone())) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            let _ = store.shutdown();
            return Err(error);
        }
    };
    let commits = store
        .runtime_snapshot()
        .map(|snapshot| snapshot.generation as usize)
        .unwrap_or(0);
    store.shutdown().map_err(|_| AgentError::Store)?;
    Ok(Summary::from_snapshot(&snapshot, false, commits))
}

/// Run the monitor for one completed pass and capture the snapshot it publishes.
///
/// The worker now publishes after its first pass even when nothing changed (see
/// `runtime.rs`), which is what lets a one-shot run terminate on an idle machine.
fn collect(
    settings: &MonitorSettings,
    persistence: Arc<dyn RuntimePersistence>,
) -> Result<Arc<MonitorSnapshot>, AgentError> {
    let published: Arc<Mutex<Option<Arc<MonitorSnapshot>>>> = Arc::new(Mutex::new(None));
    let sink = published.clone();
    let monitor = Monitor::start_with_persistence(settings.clone(), persistence, move |snapshot| {
        if let Ok(mut slot) = sink.lock() {
            *slot = Some(snapshot);
        }
    })
    .map_err(|_| AgentError::Monitor)?;

    let deadline = Instant::now() + PASS_TIMEOUT;
    loop {
        if let Ok(slot) = published.lock()
            && let Some(snapshot) = slot.as_ref()
        {
            let snapshot = snapshot.clone();
            drop(slot);
            drop(monitor);
            return Ok(snapshot);
        }
        if Instant::now() >= deadline {
            drop(monitor);
            return Err(AgentError::Timeout);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn resolve_timezone(config: &AgentConfig) -> Result<BucketTimezone, AgentError> {
    // No zone given: UTC rather than the host's, so a scheduled run on a server in
    // another region buckets days the same way every time.
    let name = config.timezone.as_deref().unwrap_or("UTC");
    BucketTimezone::from_str(name).map_err(|_| AgentError::Discovery("timezone"))
}

fn settings_from(
    report: &DiscoveryReport,
    _config: &AgentConfig,
    timezone: BucketTimezone,
) -> MonitorSettings {
    // Selection already happened in discovery, through `paths.enabled_clients`.
    // Filtering again here would duplicate the rule and let the two drift.
    let sources = report
        .sources
        .iter()
        .map(|source| source.monitored_source())
        .collect::<Vec<_>>();
    MonitorSettings {
        generation: 0,
        day_generation: 0,
        pricing_generation: 0,
        timezone,
        sources,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_timezone_is_refused_not_defaulted() {
        let config = AgentConfig {
            once: true,
            dry_run: true,
            clients: ClientSelection::All,
            timezone: Some("Invalid/Zone".to_owned()),
            store: None,
            json: false,
        };
        assert!(matches!(
            resolve_timezone(&config),
            Err(AgentError::Discovery("timezone"))
        ));
    }

    #[test]
    fn an_absent_timezone_buckets_in_utc() {
        // Deterministic across servers in different regions.
        let config = AgentConfig {
            once: true,
            dry_run: true,
            clients: ClientSelection::All,
            timezone: None,
            store: None,
            json: false,
        };
        let zone = resolve_timezone(&config).expect("zone");
        assert_eq!(zone.name(), "UTC");
    }
}
