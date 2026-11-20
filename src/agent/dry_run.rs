//! Persistence that keeps nothing, so `--dry-run` runs the real pipeline.
//!
//! The monitor is generic over `RuntimePersistence`, so a dry run can exercise the
//! actual discovery → parse → aggregate → account path instead of a lookalike,
//! while touching no file. That is the whole design: the only thing that changes
//! between a real run and a dry run is where the result goes.

use std::sync::{Arc, Mutex};

use crate::runtime::RuntimePersistence;
use crate::sources::{SourceId, SourceSnapshot};
use crate::storage::{CanonicalCommit, PersistedRuntimeSnapshot, StoreError};

/// Records what the monitor *tried* to persist, so a dry run can report it.
///
/// Keeping the count is deliberate: "dry run wrote nothing" is only meaningful if
/// we can also show it would have written *something*. A dry run that reports
/// zero commits either has nothing to collect or is broken, and those are
/// different problems.
#[derive(Default)]
pub struct NullPersistence {
    state: Mutex<DryState>,
}

#[derive(Default)]
struct DryState {
    commits: Vec<CommitFacts>,
    /// The per-source ledgers a real store would be holding.
    sources: std::collections::BTreeMap<SourceId, Arc<SourceSnapshot>>,
    generation: u64,
    settings_generation: u64,
    day_generation: u64,
    timezone: Option<String>,
}

/// The shape of a commit, without the payload.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CommitFacts {
    pub observation_count: usize,
}

impl NullPersistence {
    pub fn new() -> Self {
        Self::default()
    }

    /// How many commits would have been written.
    pub fn commit_count(&self) -> usize {
        self.state
            .lock()
            .map(|state| state.commits.len())
            .unwrap_or(0)
    }

    /// How many distinct sources a real store would now be holding.
    pub fn source_count(&self) -> usize {
        self.state
            .lock()
            .map(|state| state.sources.len())
            .unwrap_or(0)
    }

    fn snapshot(&self, state: &DryState) -> Arc<PersistedRuntimeSnapshot> {
        if state.sources.is_empty() {
            // A first real run on a fresh machine sees exactly this; dry-run must
            // handle it the same way rather than inventing an initialized store.
            return Arc::new(PersistedRuntimeSnapshot::empty());
        }
        // Only `sources` matters to the monitor on restore: it uses them to
        // reconstruct each reader's last good ledger. The rollups it publishes come
        // from its own ledger, rebuilt from those sources, so there is nothing to
        // fabricate here — and fabricating an empty rollup set would be a lie.
        Arc::new(PersistedRuntimeSnapshot {
            initialized: true,
            generation: state.generation,
            settings_generation: state.settings_generation,
            day_generation: state.day_generation,
            timezone_name: state.timezone.clone(),
            parser_contract_revision: PersistedRuntimeSnapshot::empty().parser_contract_revision,
            sources: state.sources.values().cloned().collect(),
            canonical: None,
        })
    }
}

impl RuntimePersistence for NullPersistence {
    fn runtime_snapshot(&self) -> Result<Arc<PersistedRuntimeSnapshot>, StoreError> {
        let Ok(state) = self.state.lock() else {
            return Ok(Arc::new(PersistedRuntimeSnapshot::empty()));
        };
        Ok(self.snapshot(&state))
    }

    fn commit_runtime(
        &self,
        commit: CanonicalCommit,
    ) -> Result<Arc<PersistedRuntimeSnapshot>, StoreError> {
        let Ok(mut state) = self.state.lock() else {
            return Ok(Arc::new(PersistedRuntimeSnapshot::empty()));
        };
        state.commits.push(CommitFacts {
            observation_count: commit.canonical.rows.len(),
        });
        // Accumulate exactly as a store would: this pass commits one source's
        // ledger and every other source stays where it was. Returning only the
        // latest commit is what made a dry run under-report its own totals.
        state
            .sources
            .insert(commit.source.source_id, commit.source.clone());
        state.generation = commit.expected_runtime_generation.saturating_add(1);
        state.settings_generation = commit.settings_generation;
        state.day_generation = commit.day_generation;
        state.timezone = Some(commit.timezone_name.clone());

        Ok(self.snapshot(&state))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_dry_run_sees_an_uninitialized_snapshot() {
        // Matches what a real first run sees, so the monitor takes the same path.
        let persistence = NullPersistence::new();
        let snapshot = persistence.runtime_snapshot().expect("snapshot");
        assert!(!snapshot.initialized);
        assert_eq!(snapshot.generation, 0);
        assert!(snapshot.canonical.is_none());
        assert!(snapshot.sources.is_empty());
    }

    #[test]
    fn nothing_is_recorded_until_a_commit_is_attempted() {
        let persistence = NullPersistence::new();
        assert_eq!(persistence.commit_count(), 0);
        assert_eq!(persistence.source_count(), 0);
    }

    #[test]
    fn the_snapshot_contract_revision_is_the_real_one() {
        // A dry run must not fabricate a different contract revision, or it would
        // exercise a path a real run never takes.
        let persistence = NullPersistence::new();
        let snapshot = persistence.runtime_snapshot().expect("snapshot");
        assert_eq!(
            snapshot.parser_contract_revision,
            crate::storage::PersistedRuntimeSnapshot::empty().parser_contract_revision
        );
    }
}
