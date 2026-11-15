//! Prime Agent session usage, adapted from the pinned reader
//! `crates/tokscale-core/src/sessions/prime_agent.rs`. See
//! docs/migration/parser-provenance.md.
//!
//! Prime Agent stores root sessions in `~/.prime/agent/sessions/*.jsonl` and
//! RLM child sessions below the sibling `session-artifacts` tree. Both use the
//! Pi append-only JSONL record format, so token extraction is delegated to the
//! existing [`PiReader`] under this client's id rather than duplicated.
//!
//! # Why `child_usage_attributed` records are not emitted
//!
//! The pinned reader never emits them as messages: it scans each child's own
//! transcript directly. Their usage metadata is used only to **reverse**
//! aggregate parent usage that Prime may persist while serializing a fork, so
//! the copied parent can be deduplicated across files afterwards.
//!
//! That reversal is a cross-file operation — it needs the child transcript that
//! the parent row's aggregate covers — and this codebase's reader contract is
//! one byte slice per source with no sibling access. So it is documented as
//! unsupported rather than approximated, and a `child_usage_attributed` record
//! is skipped exactly as the pinned reader skips it. The child's own transcript
//! is still a source in its own right, so the usage is counted once, from the
//! file that actually recorded it.
//!
//! # Delegation
//!
//! [`PiReader`] owns the format: the `title` header tolerance, the `session`
//! header identity, `model_change` tracking, the usage-bearing entry types, and
//! the tool-summary model exclusion. Reusing it is what keeps this reader
//! consistent with the Pi and Proma clients, which share the same on-disk
//! shape.

use super::{ReaderError, ResponseUsage, pi::PiReader};

pub const MAX_RESPONSES: usize = 16_384;

pub const CLIENT: &str = "prime-agent";

/// The entry type whose usage must not become a message of its own.
const CHILD_USAGE_TYPE: &str = "child_usage_attributed";

/// One Prime Agent session transcript.
pub struct PrimeAgentReader {
    pi: PiReader,
}

impl PrimeAgentReader {
    pub fn new(session: &str) -> Self {
        Self {
            pi: PiReader::new(CLIENT, session),
        }
    }

    pub fn with_fallback_timestamp(mut self, timestamp_ms: Option<i64>) -> Self {
        self.pi = self.pi.with_fallback_timestamp(timestamp_ms);
        self
    }

    pub fn ingest(&mut self, record: &[u8]) -> Result<(), ReaderError> {
        // The child-attribution row is recognized here so it is skipped before
        // the Pi reader can interpret it, matching the pinned reader.
        if is_child_usage(record) {
            return Ok(());
        }
        self.pi.ingest(record)
    }

    pub fn snapshot(&self) -> Vec<ResponseUsage> {
        self.pi.snapshot()
    }

    pub fn has_supported_usage(&self) -> bool {
        !self.snapshot().is_empty()
    }

    /// Unsupported usage is reported as nothing rather than an error, so there
    /// is no end-of-stream work to do.
    pub fn finish(&mut self) -> Result<(), ReaderError> {
        Ok(())
    }
}

/// Whether the record is a `child_usage_attributed` entry.
///
/// The record is parsed into an owned value so the borrow does not outlive it.
fn is_child_usage(record: &[u8]) -> bool {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(record) else {
        return false;
    };
    value.get("type").and_then(serde_json::Value::as_str) == Some(CHILD_USAGE_TYPE)
}
