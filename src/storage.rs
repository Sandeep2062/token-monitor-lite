//! Recoverable native SQLite persistence.
//!
//! One named worker owns the only read-write connection. Callers exchange
//! bounded commands and immutable, secret-free snapshots; SQLite handles never
//! cross this module boundary.

use crate::accounting::{SessionKey, TokenCounts};
use crate::aggregation::{
    BucketTimezone, CanonicalLedger, CanonicalObservation, CanonicalObservationSnapshot,
    CanonicalProvenanceLink, CanonicalSnapshot, MAX_CANONICAL_ROWS, MAX_SOURCES,
};
use crate::discovery::NativePaths;
use crate::parsers::{CostStatus, ResponseUsage};
use crate::sources::{
    ObservationSourceState, RetainedModelUsage, RetainedPeriod, RetainedPeriodUsage, SourceFormat,
    SourceId, SourceSnapshot,
};
use rusqlite::backup::Backup;
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex, RwLock, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;

pub const DATABASE_FILE_NAME: &str = "token-monitor-lite.sqlite3";
pub const BACKUP_DIRECTORY_NAME: &str = "backups";
pub const RECOVERY_DIRECTORY_NAME: &str = "recovery";

const RECOVERY_MARKER_NAME: &str = "recovery-required";
const SCHEMA_VERSION: u32 = 4;
const PARSER_CONTRACT_REVISION: &str = "gate-2a-v1";
const COMMAND_CAPACITY: usize = 8;
const BUSY_TIMEOUT: Duration = Duration::from_millis(250);
const MAX_SETTINGS_BYTES: usize = 4 * 1024 * 1024;
const MAX_ENTRY_BYTES: usize = 16 * 1024 * 1024;
const MAX_IMPORT_SESSIONS: usize = 10_000;
const MAX_REPORT_ITEMS: usize = 1_024;
const MAX_STAGED_ARTIFACTS: usize = 256;
const MAX_CREDENTIAL_REFERENCES: usize = 512;
const MAX_CREDENTIAL_OPERATIONS: usize = 2_048;
const MAX_RUNTIME_IDENTITY_BYTES: usize = 4_096;
const MAX_RUNTIME_TOTAL_IDENTITY_BYTES: usize = 8 * 1024 * 1024;
static UNIQUE_ID: AtomicU64 = AtomicU64::new(0);

const SCHEMA_V1: &str = r#"
CREATE TABLE schema_meta (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    version INTEGER NOT NULL CHECK (version >= 0),
    applied_at INTEGER NOT NULL
) STRICT;

CREATE TABLE active_settings (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    revision INTEGER NOT NULL CHECK (revision >= 0),
    document_json BLOB NOT NULL,
    settings_generation INTEGER NOT NULL CHECK (settings_generation >= 0),
    day_generation INTEGER NOT NULL CHECK (day_generation >= 0),
    pricing_generation INTEGER NOT NULL CHECK (pricing_generation >= 0)
) STRICT;

CREATE TABLE import_runs (
    run_id TEXT PRIMARY KEY,
    committed_at INTEGER NOT NULL
) STRICT;

CREATE TABLE imported_sessions (
    client TEXT NOT NULL,
    session TEXT NOT NULL,
    original_entry_json BLOB NOT NULL,
    legacy_key TEXT NOT NULL,
    source_revision INTEGER NOT NULL CHECK (source_revision >= 0),
    archive_metadata_json BLOB,
    import_run_id TEXT NOT NULL REFERENCES import_runs(run_id),
    PRIMARY KEY (client, session)
) STRICT;

CREATE TABLE import_report_items (
    run_id TEXT NOT NULL REFERENCES import_runs(run_id),
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    artifact_kind TEXT NOT NULL,
    preservation_state TEXT NOT NULL,
    import_state TEXT NOT NULL,
    activation_state TEXT NOT NULL,
    records INTEGER NOT NULL CHECK (records >= 0),
    PRIMARY KEY (run_id, ordinal)
) STRICT;

CREATE TABLE staged_artifacts (
    run_id TEXT NOT NULL REFERENCES import_runs(run_id),
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    staging_id TEXT NOT NULL,
    artifact_kind TEXT NOT NULL,
    bytes INTEGER NOT NULL CHECK (bytes >= 0),
    PRIMARY KEY (run_id, ordinal),
    UNIQUE (run_id, staging_id)
) STRICT;

CREATE TABLE credential_references (
    credential_id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES import_runs(run_id),
    credential_kind TEXT NOT NULL
) STRICT;

CREATE TABLE credential_operations (
    operation_id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES import_runs(run_id),
    credential_id TEXT NOT NULL,
    operation TEXT NOT NULL,
    operation_state TEXT NOT NULL
) STRICT;
"#;

const SCHEMA_V2: &str = r#"
CREATE TABLE pending_credential_imports (
    run_id TEXT PRIMARY KEY,
    expected_settings_revision INTEGER NOT NULL CHECK (expected_settings_revision >= 0),
    prepared_at INTEGER NOT NULL
) STRICT;

ALTER TABLE credential_operations RENAME TO credential_operations_v1;

CREATE TABLE credential_operations (
    operation_id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL,
    credential_id TEXT NOT NULL,
    operation TEXT NOT NULL CHECK (operation IN ('set', 'delete')),
    operation_state TEXT NOT NULL CHECK (
        operation_state IN ('prepared', 'secret-written', 'verified', 'metadata-committed')
    )
) STRICT;

INSERT INTO credential_operations(operation_id, run_id, credential_id, operation, operation_state)
SELECT operation_id, run_id, credential_id, operation, operation_state
FROM credential_operations_v1;

DROP TABLE credential_operations_v1;
"#;

const SCHEMA_V3: &str = r#"
CREATE TABLE runtime_state (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    runtime_generation INTEGER NOT NULL CHECK (runtime_generation > 0),
    settings_generation INTEGER NOT NULL CHECK (settings_generation >= 0),
    day_generation INTEGER NOT NULL CHECK (day_generation >= 0),
    timezone_name TEXT NOT NULL CHECK (length(timezone_name) BETWEEN 1 AND 128),
    parser_contract_revision TEXT NOT NULL CHECK (length(parser_contract_revision) BETWEEN 1 AND 128),
    committed_at INTEGER NOT NULL
) STRICT;

CREATE TABLE runtime_sources (
    source_id TEXT PRIMARY KEY CHECK (length(source_id) = 42),
    format TEXT NOT NULL CHECK (format IN ('claude-jsonl', 'codex-jsonl')),
    client TEXT NOT NULL CHECK (length(client) BETWEEN 1 AND 64),
    parser_pin TEXT NOT NULL CHECK (length(parser_pin) BETWEEN 1 AND 128),
    source_generation INTEGER NOT NULL CHECK (source_generation > 0),
    source_state TEXT NOT NULL CHECK (source_state IN ('live', 'retained')),
    observation_count INTEGER NOT NULL CHECK (observation_count >= 0 AND observation_count <= 262144),
    session_count INTEGER NOT NULL CHECK (session_count >= 0 AND session_count <= 10000),
    last_observed_at INTEGER,
    incomplete_tail INTEGER NOT NULL CHECK (incomplete_tail IN (0, 1)),
    has_supported_usage INTEGER NOT NULL CHECK (has_supported_usage IN (0, 1)),
    UNIQUE (source_id, source_generation)
) STRICT;

CREATE TABLE source_observations (
    source_id TEXT NOT NULL,
    source_generation INTEGER NOT NULL CHECK (source_generation > 0),
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0 AND ordinal < 262144),
    client TEXT NOT NULL CHECK (length(client) BETWEEN 1 AND 64),
    session TEXT NOT NULL CHECK (length(session) BETWEEN 1 AND 4096),
    model TEXT NOT NULL CHECK (length(model) BETWEEN 1 AND 4096),
    observed_at INTEGER NOT NULL,
    input_tokens INTEGER NOT NULL CHECK (input_tokens >= 0),
    output_tokens INTEGER NOT NULL CHECK (output_tokens >= 0),
    cache_read_tokens INTEGER NOT NULL CHECK (cache_read_tokens >= 0),
    cache_write_tokens INTEGER NOT NULL CHECK (cache_write_tokens >= 0),
    reasoning_tokens INTEGER NOT NULL CHECK (reasoning_tokens >= 0),
    cache_write_1h_tokens INTEGER NOT NULL CHECK (cache_write_1h_tokens >= 0),
    dedup_identity TEXT CHECK (dedup_identity IS NULL OR length(dedup_identity) BETWEEN 1 AND 4096),
    identity_class TEXT NOT NULL CHECK (identity_class IN ('global-stable', 'source-local')),
    provider_hint TEXT CHECK (provider_hint IS NULL OR length(provider_hint) BETWEEN 1 AND 4096),
    service_tier TEXT CHECK (service_tier IS NULL OR length(service_tier) BETWEEN 1 AND 4096),
    PRIMARY KEY (source_id, source_generation, ordinal),
    FOREIGN KEY (source_id, source_generation)
        REFERENCES runtime_sources(source_id, source_generation) ON DELETE CASCADE
) STRICT;

CREATE TABLE canonical_observations (
    canonical_id TEXT PRIMARY KEY CHECK (length(canonical_id) BETWEEN 1 AND 4096),
    ordinal INTEGER NOT NULL UNIQUE CHECK (ordinal >= 0 AND ordinal < 262144),
    client TEXT NOT NULL CHECK (length(client) BETWEEN 1 AND 64),
    session TEXT NOT NULL CHECK (length(session) BETWEEN 1 AND 4096),
    model TEXT NOT NULL CHECK (length(model) BETWEEN 1 AND 4096),
    observed_at INTEGER NOT NULL,
    input_tokens INTEGER NOT NULL CHECK (input_tokens >= 0),
    output_tokens INTEGER NOT NULL CHECK (output_tokens >= 0),
    cache_read_tokens INTEGER NOT NULL CHECK (cache_read_tokens >= 0),
    cache_write_tokens INTEGER NOT NULL CHECK (cache_write_tokens >= 0),
    reasoning_tokens INTEGER NOT NULL CHECK (reasoning_tokens >= 0),
    cache_write_1h_tokens INTEGER NOT NULL CHECK (cache_write_1h_tokens >= 0),
    dedup_identity TEXT CHECK (dedup_identity IS NULL OR length(dedup_identity) BETWEEN 1 AND 4096),
    provider_hint TEXT CHECK (provider_hint IS NULL OR length(provider_hint) BETWEEN 1 AND 4096),
    service_tier TEXT CHECK (service_tier IS NULL OR length(service_tier) BETWEEN 1 AND 4096),
    source_state TEXT NOT NULL CHECK (source_state IN ('live', 'retained'))
) STRICT;

CREATE TABLE canonical_provenance (
    canonical_id TEXT NOT NULL REFERENCES canonical_observations(canonical_id) ON DELETE CASCADE,
    source_id TEXT NOT NULL,
    source_generation INTEGER NOT NULL CHECK (source_generation > 0),
    observation_ordinal INTEGER NOT NULL CHECK (observation_ordinal >= 0 AND observation_ordinal < 262144),
    PRIMARY KEY (canonical_id, source_id, source_generation, observation_ordinal),
    FOREIGN KEY (source_id, source_generation, observation_ordinal)
        REFERENCES source_observations(source_id, source_generation, ordinal)
) STRICT;
"#;

const SCHEMA_V4: &str = r#"
ALTER TABLE source_observations ADD COLUMN agent TEXT CHECK (agent IS NULL OR length(agent) BETWEEN 1 AND 4096);
ALTER TABLE canonical_observations ADD COLUMN agent TEXT CHECK (agent IS NULL OR length(agent) BETWEEN 1 AND 4096);

CREATE TABLE runtime_sqlite_sources (
    source_id TEXT PRIMARY KEY CHECK (length(source_id) = 42),
    format TEXT NOT NULL CHECK (format IN ('session-archive-sqlite', 'codex-local-usage-sqlite')),
    client TEXT NOT NULL CHECK (length(client) BETWEEN 1 AND 64),
    parser_pin TEXT NOT NULL CHECK (length(parser_pin) BETWEEN 1 AND 128),
    source_generation INTEGER NOT NULL CHECK (source_generation > 0),
    source_state TEXT NOT NULL CHECK (source_state IN ('live', 'retained')),
    observation_count INTEGER NOT NULL CHECK (observation_count >= 0 AND observation_count <= 262144),
    retained_period_count INTEGER NOT NULL CHECK (retained_period_count >= 0 AND retained_period_count <= 10000),
    session_count INTEGER NOT NULL CHECK (session_count >= 0 AND session_count <= 10000),
    last_observed_at INTEGER,
    incomplete_tail INTEGER NOT NULL CHECK (incomplete_tail IN (0, 1)),
    has_supported_usage INTEGER NOT NULL CHECK (has_supported_usage IN (0, 1)),
    UNIQUE (source_id, source_generation)
) STRICT;

CREATE TABLE source_sqlite_observations (
    source_id TEXT NOT NULL,
    source_generation INTEGER NOT NULL CHECK (source_generation > 0),
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0 AND ordinal < 262144),
    client TEXT NOT NULL CHECK (length(client) BETWEEN 1 AND 64),
    session TEXT NOT NULL CHECK (length(session) BETWEEN 1 AND 4096),
    model TEXT NOT NULL CHECK (length(model) BETWEEN 1 AND 4096),
    observed_at INTEGER NOT NULL,
    input_tokens INTEGER NOT NULL CHECK (input_tokens >= 0),
    output_tokens INTEGER NOT NULL CHECK (output_tokens >= 0),
    cache_read_tokens INTEGER NOT NULL CHECK (cache_read_tokens >= 0),
    cache_write_tokens INTEGER NOT NULL CHECK (cache_write_tokens >= 0),
    reasoning_tokens INTEGER NOT NULL CHECK (reasoning_tokens >= 0),
    cache_write_1h_tokens INTEGER NOT NULL CHECK (cache_write_1h_tokens >= 0),
    dedup_identity TEXT CHECK (dedup_identity IS NULL OR length(dedup_identity) BETWEEN 1 AND 4096),
    identity_class TEXT NOT NULL CHECK (identity_class IN ('global-stable', 'source-local')),
    provider_hint TEXT CHECK (provider_hint IS NULL OR length(provider_hint) BETWEEN 1 AND 4096),
    service_tier TEXT CHECK (service_tier IS NULL OR length(service_tier) BETWEEN 1 AND 4096),
    agent TEXT CHECK (agent IS NULL OR length(agent) BETWEEN 1 AND 4096),
    PRIMARY KEY (source_id, source_generation, ordinal),
    FOREIGN KEY (source_id, source_generation)
        REFERENCES runtime_sqlite_sources(source_id, source_generation) ON DELETE CASCADE
) STRICT;

CREATE TABLE source_retained_periods (
    source_id TEXT NOT NULL,
    source_generation INTEGER NOT NULL CHECK (source_generation > 0),
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0 AND ordinal < 10000),
    client TEXT NOT NULL CHECK (length(client) BETWEEN 1 AND 64),
    session TEXT NOT NULL CHECK (length(session) BETWEEN 1 AND 4096),
    period TEXT NOT NULL CHECK (period IN ('today', 'month', 'allTime')),
    captured_at INTEGER NOT NULL,
    day TEXT CHECK (day IS NULL OR length(day) BETWEEN 1 AND 64),
    month TEXT CHECK (month IS NULL OR length(month) BETWEEN 1 AND 64),
    total_tokens INTEGER NOT NULL CHECK (total_tokens >= 0),
    input_tokens INTEGER NOT NULL CHECK (input_tokens >= 0),
    output_tokens INTEGER NOT NULL CHECK (output_tokens >= 0),
    cache_read_tokens INTEGER NOT NULL CHECK (cache_read_tokens >= 0),
    cache_write_tokens INTEGER NOT NULL CHECK (cache_write_tokens >= 0),
    reasoning_tokens INTEGER NOT NULL CHECK (reasoning_tokens >= 0),
    unclassified_tokens INTEGER NOT NULL CHECK (unclassified_tokens >= 0),
    components_complete INTEGER NOT NULL CHECK (components_complete IN (0, 1)),
    models_complete INTEGER NOT NULL CHECK (models_complete IN (0, 1)),
    PRIMARY KEY (source_id, source_generation, ordinal),
    FOREIGN KEY (source_id, source_generation)
        REFERENCES runtime_sqlite_sources(source_id, source_generation) ON DELETE CASCADE
) STRICT;

CREATE TABLE source_retained_models (
    source_id TEXT NOT NULL,
    source_generation INTEGER NOT NULL CHECK (source_generation > 0),
    period_ordinal INTEGER NOT NULL CHECK (period_ordinal >= 0 AND period_ordinal < 10000),
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    model TEXT NOT NULL CHECK (length(model) BETWEEN 1 AND 4096),
    total_tokens INTEGER NOT NULL CHECK (total_tokens >= 0),
    input_tokens INTEGER NOT NULL CHECK (input_tokens >= 0),
    cache_read_tokens INTEGER NOT NULL CHECK (cache_read_tokens >= 0),
    cache_write_tokens INTEGER NOT NULL CHECK (cache_write_tokens >= 0),
    output_tokens INTEGER NOT NULL CHECK (output_tokens >= 0),
    PRIMARY KEY (source_id, source_generation, period_ordinal, ordinal),
    FOREIGN KEY (source_id, source_generation, period_ordinal)
        REFERENCES source_retained_periods(source_id, source_generation, ordinal) ON DELETE CASCADE
) STRICT;

CREATE TABLE canonical_sqlite_provenance (
    canonical_id TEXT NOT NULL REFERENCES canonical_observations(canonical_id) ON DELETE CASCADE,
    source_id TEXT NOT NULL,
    source_generation INTEGER NOT NULL CHECK (source_generation > 0),
    observation_ordinal INTEGER NOT NULL CHECK (observation_ordinal >= 0 AND observation_ordinal < 262144),
    PRIMARY KEY (canonical_id, source_id, source_generation, observation_ordinal),
    FOREIGN KEY (source_id, source_generation, observation_ordinal)
        REFERENCES source_sqlite_observations(source_id, source_generation, ordinal)
) STRICT;
"#;

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum StoreError {
    #[error("native storage input is invalid")]
    InvalidImport,
    #[error("persisted runtime candidate is invalid")]
    InvalidRuntime,
    #[error("native storage rejected credential material")]
    SecretRejected,
    #[error("native storage command queue is busy")]
    Busy,
    #[error("native storage is closed")]
    Closed,
    #[error("settings revision conflict")]
    Conflict { current_revision: u64 },
    #[error("persisted runtime generation conflict")]
    RuntimeConflict { current_generation: u64 },
    #[error("credential operation state changed")]
    CredentialTransitionConflict,
    #[error("credential operations are not verified")]
    CredentialsNotVerified,
    #[error("native database uses unsupported future schema {0}")]
    FutureSchema(u32),
    #[error("native database recovery is required")]
    RecoveryRequired,
    #[error("native database I/O failed")]
    Io,
    #[error("native database operation failed")]
    Sqlite,
    #[error("native storage worker failed")]
    WorkerPanicked,
}

impl From<io::Error> for StoreError {
    fn from(_: io::Error) -> Self {
        Self::Io
    }
}

fn sqlite<T>(result: rusqlite::Result<T>) -> Result<T, StoreError> {
    result.map_err(|_| StoreError::Sqlite)
}

#[derive(Clone, PartialEq, Eq)]
pub struct NativeSettings {
    json: Vec<u8>,
}

impl NativeSettings {
    pub fn from_json_bytes(bytes: &[u8]) -> Result<Self, StoreError> {
        if bytes.len() > MAX_SETTINGS_BYTES {
            return Err(StoreError::InvalidImport);
        }
        let value: Value = serde_json::from_slice(bytes).map_err(|_| StoreError::InvalidImport)?;
        if !value.is_object() {
            return Err(StoreError::InvalidImport);
        }
        validate_json_safety(&value)?;
        Ok(Self {
            json: bytes.to_vec(),
        })
    }

    pub fn json_bytes(&self) -> &[u8] {
        &self.json
    }
}

impl fmt::Debug for NativeSettings {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("NativeSettings { values: [redacted] }")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreserveState {
    Preserved,
    NotPresent,
    Failed,
}

impl PreserveState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Preserved => "preserved",
            Self::NotPresent => "not_present",
            Self::Failed => "failed",
        }
    }

    fn parse(value: &str) -> Result<Self, StoreError> {
        match value {
            "preserved" => Ok(Self::Preserved),
            "not_present" => Ok(Self::NotPresent),
            "failed" => Ok(Self::Failed),
            _ => Err(StoreError::Sqlite),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportState {
    Imported,
    Unsupported,
    Excluded,
    Failed,
    NotAttempted,
}

impl ImportState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Imported => "imported",
            Self::Unsupported => "unsupported",
            Self::Excluded => "excluded",
            Self::Failed => "failed",
            Self::NotAttempted => "not_attempted",
        }
    }

    fn parse(value: &str) -> Result<Self, StoreError> {
        match value {
            "imported" => Ok(Self::Imported),
            "unsupported" => Ok(Self::Unsupported),
            "excluded" => Ok(Self::Excluded),
            "failed" => Ok(Self::Failed),
            "not_attempted" => Ok(Self::NotAttempted),
            _ => Err(StoreError::Sqlite),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivationState {
    Activated,
    Inactive,
    Failed,
    NotApplicable,
}

impl ActivationState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Activated => "active",
            Self::Inactive => "inactive",
            Self::Failed => "failed",
            Self::NotApplicable => "not_applicable",
        }
    }

    fn parse(value: &str) -> Result<Self, StoreError> {
        match value {
            "active" => Ok(Self::Activated),
            "inactive" => Ok(Self::Inactive),
            "failed" => Ok(Self::Failed),
            "not_applicable" => Ok(Self::NotApplicable),
            _ => Err(StoreError::Sqlite),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct ImportedSession {
    pub client: String,
    pub session: String,
    pub original_entry_json: Vec<u8>,
    pub legacy_key: String,
    pub source_revision: u64,
    pub archive_metadata_json: Option<Vec<u8>>,
}

impl fmt::Debug for ImportedSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ImportedSession")
            .field("identity", &format_args!("[redacted]"))
            .field("source_revision", &self.source_revision)
            .field("json", &format_args!("[redacted]"))
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportReportItem {
    pub artifact_kind: String,
    pub preservation: PreserveState,
    pub import: ImportState,
    pub activation: ActivationState,
    pub records: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StagedArtifact {
    pub staging_id: String,
    pub artifact_kind: String,
    pub bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CredentialReference {
    pub credential_id: String,
    pub credential_kind: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialOperationKind {
    Set,
    Delete,
}

impl CredentialOperationKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Set => "set",
            Self::Delete => "delete",
        }
    }

    fn parse(value: &str) -> Result<Self, StoreError> {
        match value {
            "set" => Ok(Self::Set),
            "delete" => Ok(Self::Delete),
            _ => Err(StoreError::Sqlite),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialOperationState {
    Prepared,
    SecretWritten,
    Verified,
    MetadataCommitted,
}

impl CredentialOperationState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::SecretWritten => "secret-written",
            Self::Verified => "verified",
            Self::MetadataCommitted => "metadata-committed",
        }
    }

    fn parse(value: &str) -> Result<Self, StoreError> {
        match value {
            "prepared" => Ok(Self::Prepared),
            "secret-written" => Ok(Self::SecretWritten),
            "verified" => Ok(Self::Verified),
            "metadata-committed" => Ok(Self::MetadataCommitted),
            _ => Err(StoreError::Sqlite),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CredentialOperation {
    pub operation_id: String,
    pub credential_id: String,
    pub operation: CredentialOperationKind,
    pub state: CredentialOperationState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatedImport {
    pub run_id: String,
    pub expected_settings_revision: u64,
    pub settings: Option<NativeSettings>,
    pub sessions: Vec<ImportedSession>,
    pub report_items: Vec<ImportReportItem>,
    pub staged_artifacts: Vec<StagedArtifact>,
    pub credential_references: Vec<CredentialReference>,
    pub credential_operations: Vec<CredentialOperation>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredImportRun {
    pub run_id: String,
    pub committed_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredReportItem {
    pub run_id: String,
    pub ordinal: usize,
    pub item: ImportReportItem,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredStagedArtifact {
    pub run_id: String,
    pub ordinal: usize,
    pub artifact: StagedArtifact,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredCredentialReference {
    pub run_id: String,
    pub reference: CredentialReference,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredCredentialOperation {
    pub run_id: String,
    pub operation: CredentialOperation,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingCredentialImport {
    pub run_id: String,
    pub expected_settings_revision: u64,
    pub prepared_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PersistedRuntimeSnapshot {
    pub initialized: bool,
    pub generation: u64,
    pub settings_generation: u64,
    pub day_generation: u64,
    pub timezone_name: Option<String>,
    pub parser_contract_revision: String,
    pub sources: Arc<[Arc<SourceSnapshot>]>,
    pub canonical: Option<Arc<CanonicalObservationSnapshot>>,
}

impl PersistedRuntimeSnapshot {
    fn empty() -> Self {
        Self {
            initialized: false,
            generation: 0,
            settings_generation: 0,
            day_generation: 0,
            timezone_name: None,
            parser_contract_revision: PARSER_CONTRACT_REVISION.to_owned(),
            sources: Arc::from([]),
            canonical: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct CanonicalCommit {
    pub expected_runtime_generation: u64,
    pub settings_generation: u64,
    pub day_generation: u64,
    pub timezone_name: String,
    pub source: Arc<SourceSnapshot>,
    pub canonical: Arc<CanonicalObservationSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoreSnapshot {
    pub schema_version: u32,
    pub foreign_keys_enabled: bool,
    pub settings_revision: u64,
    pub settings_generation: u64,
    pub day_generation: u64,
    pub pricing_generation: u64,
    pub settings: Option<NativeSettings>,
    pub sessions: Vec<ImportedSession>,
    pub import_runs: Vec<StoredImportRun>,
    pub report_items: Vec<StoredReportItem>,
    pub staged_artifacts: Vec<StoredStagedArtifact>,
    pub credential_references: Vec<StoredCredentialReference>,
    pub credential_operations: Vec<StoredCredentialOperation>,
    pub pending_credential_imports: Vec<PendingCredentialImport>,
    pub runtime: Arc<PersistedRuntimeSnapshot>,
}

impl StoreSnapshot {
    fn empty() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            foreign_keys_enabled: false,
            settings_revision: 0,
            settings_generation: 0,
            day_generation: 0,
            pricing_generation: 0,
            settings: None,
            sessions: Vec::new(),
            import_runs: Vec::new(),
            report_items: Vec::new(),
            staged_artifacts: Vec::new(),
            credential_references: Vec::new(),
            credential_operations: Vec::new(),
            pending_credential_imports: Vec::new(),
            runtime: Arc::new(PersistedRuntimeSnapshot::empty()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackupInfo {
    pub id: String,
    pub schema_version: u32,
    pub settings_revision: u64,
    pub bytes: u64,
}

enum Command {
    SaveSettings {
        expected_revision: u64,
        next: NativeSettings,
        response: SyncSender<Result<Arc<StoreSnapshot>, StoreError>>,
    },
    AdoptImport {
        import: ValidatedImport,
        response: SyncSender<Result<Arc<StoreSnapshot>, StoreError>>,
    },
    PrepareCredentialImport {
        run_id: String,
        expected_settings_revision: u64,
        operations: Vec<CredentialOperation>,
        response: SyncSender<Result<Arc<StoreSnapshot>, StoreError>>,
    },
    AdvanceCredentialOperation {
        operation_id: String,
        expected: CredentialOperationState,
        next: CredentialOperationState,
        response: SyncSender<Result<Arc<StoreSnapshot>, StoreError>>,
    },
    CommitRuntime {
        commit: CanonicalCommit,
        response: SyncSender<Result<Arc<PersistedRuntimeSnapshot>, StoreError>>,
    },
    CreateBackup {
        response: SyncSender<Result<BackupInfo, StoreError>>,
    },
    Shutdown,
}

struct StoreInner {
    sender: SyncSender<Command>,
    latest: Arc<RwLock<Arc<StoreSnapshot>>>,
    admission: Mutex<bool>,
    join: Mutex<Option<JoinHandle<()>>>,
}

impl StoreInner {
    fn submit<T>(
        &self,
        make_command: impl FnOnce(SyncSender<Result<T, StoreError>>) -> Command,
    ) -> Result<T, StoreError> {
        let (response_sender, response_receiver) = mpsc::sync_channel(1);
        {
            let stopped = self.admission.lock().map_err(|_| StoreError::Closed)?;
            if *stopped {
                return Err(StoreError::Closed);
            }
            self.sender
                .try_send(make_command(response_sender))
                .map_err(|error| match error {
                    mpsc::TrySendError::Full(_) => StoreError::Busy,
                    mpsc::TrySendError::Disconnected(_) => StoreError::Closed,
                })?;
        }
        response_receiver.recv().map_err(|_| StoreError::Closed)?
    }

    fn stop_and_join(&self) -> Result<(), StoreError> {
        {
            let mut stopped = self.admission.lock().map_err(|_| StoreError::Closed)?;
            if !*stopped {
                *stopped = true;
                self.sender
                    .send(Command::Shutdown)
                    .map_err(|_| StoreError::Closed)?;
            }
        }
        let join = self.join.lock().map_err(|_| StoreError::Closed)?.take();
        if let Some(join) = join {
            join.join().map_err(|_| StoreError::WorkerPanicked)?;
        }
        Ok(())
    }
}

impl Drop for StoreInner {
    fn drop(&mut self) {
        if let Ok(stopped) = self.admission.get_mut()
            && !*stopped
        {
            *stopped = true;
            let _ = self.sender.send(Command::Shutdown);
        }
        if let Ok(join) = self.join.get_mut()
            && let Some(join) = join.take()
        {
            let _ = join.join();
        }
    }
}

#[derive(Clone)]
pub struct StoreHandle {
    inner: Arc<StoreInner>,
}

impl fmt::Debug for StoreHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("StoreHandle { database: [redacted] }")
    }
}

impl StoreHandle {
    pub fn open(paths: NativePaths) -> Result<(Self, StoreSnapshot), StoreError> {
        let root = paths.root().to_path_buf();
        let latest = Arc::new(RwLock::new(Arc::new(StoreSnapshot::empty())));
        let worker_latest = latest.clone();
        let (sender, receiver) = mpsc::sync_channel(COMMAND_CAPACITY);
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let join = thread::Builder::new()
            .name("token-monitor-sqlite-writer".to_owned())
            .spawn(move || writer_main(root, receiver, ready_sender, worker_latest))
            .map_err(|_| StoreError::Io)?;

        let initial = match ready_receiver.recv() {
            Ok(Ok(snapshot)) => snapshot,
            Ok(Err(error)) => {
                let _ = join.join();
                return Err(error);
            }
            Err(_) => {
                let _ = join.join();
                return Err(StoreError::WorkerPanicked);
            }
        };
        let handle = Self {
            inner: Arc::new(StoreInner {
                sender,
                latest,
                admission: Mutex::new(false),
                join: Mutex::new(Some(join)),
            }),
        };
        Ok((handle, (*initial).clone()))
    }

    pub fn snapshot(&self) -> Result<Arc<StoreSnapshot>, StoreError> {
        self.inner
            .latest
            .read()
            .map(|snapshot| snapshot.clone())
            .map_err(|_| StoreError::Closed)
    }

    pub fn runtime_snapshot(&self) -> Result<Arc<PersistedRuntimeSnapshot>, StoreError> {
        Ok(self.snapshot()?.runtime.clone())
    }

    pub fn commit_runtime(
        &self,
        commit: CanonicalCommit,
    ) -> Result<Arc<PersistedRuntimeSnapshot>, StoreError> {
        self.inner
            .submit(|response| Command::CommitRuntime { commit, response })
    }

    /// Submits one runtime commit and reports every `wait` interval while the
    /// writer has not answered. The callback runs on the caller's thread so a
    /// monitor can publish delayed health without touching Slint directly.
    pub fn commit_runtime_observed(
        &self,
        commit: CanonicalCommit,
        wait: Duration,
        on_wait: &mut dyn FnMut(),
    ) -> Result<Arc<PersistedRuntimeSnapshot>, StoreError> {
        let (response_sender, response_receiver) = mpsc::sync_channel(1);
        {
            let stopped = self
                .inner
                .admission
                .lock()
                .map_err(|_| StoreError::Closed)?;
            if *stopped {
                return Err(StoreError::Closed);
            }
            self.inner
                .sender
                .try_send(Command::CommitRuntime {
                    commit,
                    response: response_sender,
                })
                .map_err(|error| match error {
                    mpsc::TrySendError::Full(_) => StoreError::Busy,
                    mpsc::TrySendError::Disconnected(_) => StoreError::Closed,
                })?;
        }
        loop {
            match response_receiver.recv_timeout(wait) {
                Ok(result) => return result,
                Err(mpsc::RecvTimeoutError::Timeout) => on_wait(),
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err(StoreError::Closed),
            }
        }
    }

    pub fn save_settings(
        &self,
        expected_revision: u64,
        next: NativeSettings,
    ) -> Result<Arc<StoreSnapshot>, StoreError> {
        self.inner.submit(|response| Command::SaveSettings {
            expected_revision,
            next,
            response,
        })
    }

    pub fn adopt_import(&self, import: ValidatedImport) -> Result<Arc<StoreSnapshot>, StoreError> {
        self.inner
            .submit(|response| Command::AdoptImport { import, response })
    }

    pub fn prepare_credential_import(
        &self,
        run_id: &str,
        expected_settings_revision: u64,
        operations: Vec<CredentialOperation>,
    ) -> Result<Arc<StoreSnapshot>, StoreError> {
        let run_id = run_id.to_owned();
        self.inner
            .submit(|response| Command::PrepareCredentialImport {
                run_id,
                expected_settings_revision,
                operations,
                response,
            })
    }

    pub fn advance_credential_operation(
        &self,
        operation_id: &str,
        expected: CredentialOperationState,
        next: CredentialOperationState,
    ) -> Result<Arc<StoreSnapshot>, StoreError> {
        let operation_id = operation_id.to_owned();
        self.inner
            .submit(|response| Command::AdvanceCredentialOperation {
                operation_id,
                expected,
                next,
                response,
            })
    }

    pub fn create_backup(&self) -> Result<BackupInfo, StoreError> {
        self.inner
            .submit(|response| Command::CreateBackup { response })
    }

    pub fn shutdown(self) -> Result<(), StoreError> {
        self.inner.stop_and_join()
    }
}

fn writer_main(
    root: PathBuf,
    receiver: mpsc::Receiver<Command>,
    ready: SyncSender<Result<Arc<StoreSnapshot>, StoreError>>,
    latest: Arc<RwLock<Arc<StoreSnapshot>>>,
) {
    let (mut connection, initial) = match bootstrap(&root) {
        Ok(ready_state) => ready_state,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    let initial = Arc::new(initial);
    if let Ok(mut published) = latest.write() {
        *published = initial.clone();
    } else {
        let _ = ready.send(Err(StoreError::WorkerPanicked));
        return;
    }
    if ready.send(Ok(initial)).is_err() {
        return;
    }

    while let Ok(command) = receiver.recv() {
        match command {
            Command::SaveSettings {
                expected_revision,
                next,
                response,
            } => {
                let result = save_settings(&mut connection, expected_revision, next)
                    .and_then(|()| publish_snapshot(&connection, &latest));
                let _ = response.send(result);
            }
            Command::AdoptImport { import, response } => {
                let result = adopt_import(&mut connection, import)
                    .and_then(|()| publish_snapshot(&connection, &latest));
                let _ = response.send(result);
            }
            Command::PrepareCredentialImport {
                run_id,
                expected_settings_revision,
                operations,
                response,
            } => {
                let result = prepare_credential_import(
                    &mut connection,
                    &run_id,
                    expected_settings_revision,
                    operations,
                )
                .and_then(|()| publish_snapshot(&connection, &latest));
                let _ = response.send(result);
            }
            Command::AdvanceCredentialOperation {
                operation_id,
                expected,
                next,
                response,
            } => {
                let result =
                    advance_credential_operation(&mut connection, &operation_id, expected, next)
                        .and_then(|()| publish_snapshot(&connection, &latest));
                let _ = response.send(result);
            }
            Command::CommitRuntime { commit, response } => {
                let current = latest
                    .read()
                    .map(|snapshot| snapshot.runtime.clone())
                    .map_err(|_| StoreError::Closed);
                let result = current.and_then(|current| {
                    commit_runtime(&mut connection, commit, &current).and_then(|runtime| {
                        publish_runtime_snapshot(&latest, runtime.clone())?;
                        Ok(runtime)
                    })
                });
                let _ = response.send(result);
            }
            Command::CreateBackup { response } => {
                let revision = latest
                    .read()
                    .map(|snapshot| snapshot.settings_revision)
                    .unwrap_or(0);
                let _ = response.send(create_backup(&connection, &root, revision));
            }
            Command::Shutdown => break,
        }
    }
}

fn publish_runtime_snapshot(
    latest: &RwLock<Arc<StoreSnapshot>>,
    runtime: Arc<PersistedRuntimeSnapshot>,
) -> Result<(), StoreError> {
    let mut published = latest.write().map_err(|_| StoreError::Closed)?;
    let mut next = (**published).clone();
    next.runtime = runtime;
    *published = Arc::new(next);
    Ok(())
}

fn publish_snapshot(
    connection: &Connection,
    latest: &RwLock<Arc<StoreSnapshot>>,
) -> Result<Arc<StoreSnapshot>, StoreError> {
    let runtime = latest
        .read()
        .map_err(|_| StoreError::Closed)?
        .runtime
        .clone();
    let mut loaded = load_snapshot(connection)?;
    loaded.runtime = runtime;
    let next = Arc::new(loaded);
    *latest.write().map_err(|_| StoreError::Closed)? = next.clone();
    Ok(next)
}

fn bootstrap(root: &Path) -> Result<(Connection, StoreSnapshot), StoreError> {
    fs::create_dir_all(root)?;
    let database = root.join(DATABASE_FILE_NAME);
    let marker = root.join(RECOVERY_MARKER_NAME);

    if !database.exists() {
        if marker.exists() || recovery_evidence_exists(root)? {
            restore_latest_backup(root, &database)?;
        } else {
            let mut connection = sqlite(Connection::open(&database))?;
            configure_connection(&connection)?;
            migrate_zero_to_one(&mut connection)?;
            migrate_one_to_two(&mut connection)?;
            migrate_two_to_three(&mut connection)?;
            migrate_three_to_four(&mut connection)?;
            let snapshot = load_snapshot(&connection)?;
            return Ok((connection, snapshot));
        }
    }

    let version = match inspect_database(&database, IntegrityCheck::Full) {
        Ok(version) => version,
        Err(StoreError::FutureSchema(version)) => return Err(StoreError::FutureSchema(version)),
        Err(_) => {
            restore_latest_backup(root, &database)?;
            inspect_database(&database, IntegrityCheck::Full)?
        }
    };
    if version > SCHEMA_VERSION {
        return Err(StoreError::FutureSchema(version));
    }

    let mut connection = sqlite(Connection::open(&database))?;
    sqlite(connection.busy_timeout(BUSY_TIMEOUT))?;
    if version == 0 {
        create_backup_internal(&connection, root, "pre-migration-v0", 0, 0)?;
    } else if version == 1 {
        create_backup_internal(&connection, root, "pre-migration-v1", 1, 0)?;
    } else if version == 2 {
        create_backup_internal(&connection, root, "pre-migration-v2", 2, 0)?;
    } else if version == 3 {
        create_backup_internal(&connection, root, "pre-migration-v3", 3, 0)?;
    }
    configure_connection(&connection)?;
    if version == 0 {
        migrate_zero_to_one(&mut connection)?;
    }
    if version <= 1 {
        migrate_one_to_two(&mut connection)?;
    }
    if version <= 2 {
        migrate_two_to_three(&mut connection)?;
    }
    if version <= 3 {
        migrate_three_to_four(&mut connection)?;
    }
    let snapshot = match load_snapshot(&connection) {
        Ok(snapshot) => snapshot,
        Err(_) => {
            drop(connection);
            restore_latest_backup(root, &database)?;
            let connection = sqlite(Connection::open(&database))?;
            configure_connection(&connection)?;
            let snapshot = load_snapshot(&connection)?;
            if marker.exists() {
                fs::remove_file(marker)?;
            }
            return Ok((connection, snapshot));
        }
    };
    if marker.exists() {
        fs::remove_file(marker)?;
    }
    Ok((connection, snapshot))
}

fn configure_connection(connection: &Connection) -> Result<(), StoreError> {
    sqlite(connection.busy_timeout(BUSY_TIMEOUT))?;
    sqlite(connection.pragma_update(None, "foreign_keys", true))?;
    let journal: String =
        sqlite(connection.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0)))?;
    if !journal.eq_ignore_ascii_case("wal") {
        return Err(StoreError::Sqlite);
    }
    sqlite(connection.pragma_update(None, "synchronous", "NORMAL"))?;
    sqlite(connection.pragma_update(None, "trusted_schema", false))?;
    Ok(())
}

fn migrate_zero_to_one(connection: &mut Connection) -> Result<(), StoreError> {
    let transaction = sqlite(connection.transaction_with_behavior(TransactionBehavior::Exclusive))?;
    sqlite(transaction.execute_batch(SCHEMA_V1))?;
    sqlite(transaction.execute(
        "INSERT INTO schema_meta(id, version, applied_at) VALUES (1, ?1, ?2)",
        params![1_i64, now_seconds()?],
    ))?;
    sqlite(transaction.pragma_update(None, "user_version", 1))?;
    sqlite(transaction.commit())?;
    Ok(())
}

fn migrate_one_to_two(connection: &mut Connection) -> Result<(), StoreError> {
    let unsupported: i64 = sqlite(connection.query_row(
        "SELECT COUNT(*) FROM credential_operations
         WHERE operation NOT IN ('set', 'delete')
            OR operation_state NOT IN (
                'prepared', 'secret-written', 'verified', 'metadata-committed'
            )",
        [],
        |row| row.get(0),
    ))?;
    if unsupported != 0 {
        return Err(StoreError::Sqlite);
    }
    let transaction = sqlite(connection.transaction_with_behavior(TransactionBehavior::Exclusive))?;
    sqlite(transaction.execute_batch(SCHEMA_V2))?;
    sqlite(transaction.execute(
        "UPDATE schema_meta SET version = ?1, applied_at = ?2 WHERE id = 1",
        params![2_i64, now_seconds()?],
    ))?;
    sqlite(transaction.pragma_update(None, "user_version", 2))?;
    sqlite(transaction.commit())?;
    Ok(())
}

fn migrate_two_to_three(connection: &mut Connection) -> Result<(), StoreError> {
    let transaction = sqlite(connection.transaction_with_behavior(TransactionBehavior::Exclusive))?;
    sqlite(transaction.execute_batch(SCHEMA_V3))?;
    sqlite(transaction.execute(
        "UPDATE schema_meta SET version = ?1, applied_at = ?2 WHERE id = 1",
        params![3_i64, now_seconds()?],
    ))?;
    sqlite(transaction.pragma_update(None, "user_version", 3))?;
    sqlite(transaction.commit())?;
    Ok(())
}

fn migrate_three_to_four(connection: &mut Connection) -> Result<(), StoreError> {
    let transaction = sqlite(connection.transaction_with_behavior(TransactionBehavior::Exclusive))?;
    sqlite(transaction.execute_batch(SCHEMA_V4))?;
    sqlite(transaction.execute(
        "UPDATE schema_meta SET version = ?1, applied_at = ?2 WHERE id = 1",
        params![i64::from(SCHEMA_VERSION), now_seconds()?],
    ))?;
    sqlite(transaction.pragma_update(None, "user_version", SCHEMA_VERSION))?;
    sqlite(transaction.commit())?;
    Ok(())
}

#[derive(Clone, Copy)]
enum IntegrityCheck {
    Full,
    Quick,
}

fn inspect_database(path: &Path, check: IntegrityCheck) -> Result<u32, StoreError> {
    let connection = sqlite(Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ))?;
    sqlite(connection.busy_timeout(BUSY_TIMEOUT))?;
    sqlite(connection.pragma_update(None, "query_only", true))?;
    let pragma = match check {
        IntegrityCheck::Full => "PRAGMA integrity_check",
        IntegrityCheck::Quick => "PRAGMA quick_check",
    };
    let result: String = sqlite(connection.query_row(pragma, [], |row| row.get(0)))?;
    if result != "ok" {
        return Err(StoreError::Sqlite);
    }
    schema_version(&connection)
}

fn schema_version(connection: &Connection) -> Result<u32, StoreError> {
    let exists: i64 = sqlite(connection.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM sqlite_schema
             WHERE type = 'table' AND name = 'schema_meta'
         )",
        [],
        |row| row.get(0),
    ))?;
    if exists == 0 {
        return Ok(0);
    }
    let mut statement = sqlite(connection.prepare("SELECT version FROM schema_meta"))?;
    let versions = sqlite(statement.query_map([], |row| row.get::<_, i64>(0)))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| StoreError::Sqlite)?;
    if versions.len() != 1 || versions[0] < 0 || versions[0] > i64::from(u32::MAX) {
        return Err(StoreError::Sqlite);
    }
    let version = versions[0] as u32;
    if version > SCHEMA_VERSION {
        return Err(StoreError::FutureSchema(version));
    }
    Ok(version)
}

fn recovery_evidence_exists(root: &Path) -> Result<bool, StoreError> {
    let directory = root.join(RECOVERY_DIRECTORY_NAME);
    if !directory.exists() {
        return Ok(false);
    }
    Ok(fs::read_dir(directory)?.next().transpose()?.is_some())
}

fn restore_latest_backup(root: &Path, active: &Path) -> Result<(), StoreError> {
    let backup = latest_compatible_backup(root).ok_or(StoreError::RecoveryRequired)?;
    let restore = root.join(format!("restore-{}.tmp", unique_suffix()?));
    copy_new(&backup, &restore)?;
    if inspect_database(&restore, IntegrityCheck::Full)? != SCHEMA_VERSION {
        let _ = fs::remove_file(&restore);
        return Err(StoreError::RecoveryRequired);
    }

    let marker = root.join(RECOVERY_MARKER_NAME);
    write_marker(&marker)?;
    if active.exists() {
        let recovery_directory = root.join(RECOVERY_DIRECTORY_NAME);
        fs::create_dir_all(&recovery_directory)?;
        let suffix = unique_suffix()?;
        let retained = recovery_directory.join(format!("corrupt-{suffix}.sqlite3"));
        fs::rename(active, retained)?;
        retain_sidecar(active, &recovery_directory, &suffix, "-wal")?;
        retain_sidecar(active, &recovery_directory, &suffix, "-shm")?;
    }
    if let Err(_error) = fs::rename(&restore, active) {
        let _ = fs::remove_file(&restore);
        return Err(StoreError::RecoveryRequired);
    }
    sync_directory(root)?;
    fs::remove_file(marker)?;
    Ok(())
}

fn retain_sidecar(
    active: &Path,
    recovery_directory: &Path,
    suffix: &str,
    sidecar_suffix: &str,
) -> Result<(), StoreError> {
    let sidecar = PathBuf::from(format!("{}{}", active.display(), sidecar_suffix));
    if sidecar.exists() {
        fs::rename(
            sidecar,
            recovery_directory.join(format!("corrupt-{suffix}{sidecar_suffix}")),
        )?;
    }
    Ok(())
}

fn latest_compatible_backup(root: &Path) -> Option<PathBuf> {
    let directory = root.join(BACKUP_DIRECTORY_NAME);
    let mut candidates = fs::read_dir(directory)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "sqlite3")
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| right.file_name().cmp(&left.file_name()));
    candidates.into_iter().find(|path| {
        inspect_database(path, IntegrityCheck::Full).is_ok_and(|version| version == SCHEMA_VERSION)
    })
}

fn copy_new(source: &Path, destination: &Path) -> Result<(), StoreError> {
    let mut source = File::open(source)?;
    let mut destination = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)?;
    io::copy(&mut source, &mut destination)?;
    destination.flush()?;
    destination.sync_all()?;
    Ok(())
}

fn write_marker(path: &Path) -> Result<(), StoreError> {
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)?;
    file.write_all(b"recovery-required\n")?;
    file.flush()?;
    file.sync_all()?;
    Ok(())
}

fn create_backup(
    connection: &Connection,
    root: &Path,
    settings_revision: u64,
) -> Result<BackupInfo, StoreError> {
    create_backup_internal(
        connection,
        root,
        "backup",
        SCHEMA_VERSION,
        settings_revision,
    )
}

fn create_backup_internal(
    connection: &Connection,
    root: &Path,
    label: &str,
    expected_schema: u32,
    settings_revision: u64,
) -> Result<BackupInfo, StoreError> {
    let directory = root.join(BACKUP_DIRECTORY_NAME);
    fs::create_dir_all(&directory)?;
    let id = format!("{label}-{}", unique_suffix()?);
    let temporary = directory.join(format!(".{id}.tmp"));
    let final_path = directory.join(format!("{id}.sqlite3"));

    let result = (|| {
        let mut destination = sqlite(Connection::open_with_flags(
            &temporary,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ))?;
        {
            let backup = sqlite(Backup::new(connection, &mut destination))?;
            sqlite(backup.run_to_completion(128, Duration::from_millis(1), None))?;
        }
        drop(destination);
        if inspect_database(&temporary, IntegrityCheck::Quick)? != expected_schema {
            return Err(StoreError::Sqlite);
        }
        // Windows requires write access for FlushFileBuffers and refuses to
        // rename a file that still has an open handle.
        let file = OpenOptions::new().read(true).write(true).open(&temporary)?;
        file.sync_all()?;
        let bytes = file.metadata()?.len();
        drop(file);
        fs::rename(&temporary, &final_path)?;
        sync_directory(&directory)?;
        Ok(BackupInfo {
            id,
            schema_version: expected_schema,
            settings_revision,
            bytes,
        })
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

fn save_settings(
    connection: &mut Connection,
    expected_revision: u64,
    next: NativeSettings,
) -> Result<(), StoreError> {
    let transaction = sqlite(connection.transaction_with_behavior(TransactionBehavior::Immediate))?;
    let (revision, settings_generation, day_generation, pricing_generation) =
        current_generations(&transaction)?;
    if revision != expected_revision {
        return Err(StoreError::Conflict {
            current_revision: revision,
        });
    }
    let next_revision = checked_increment(revision)?;
    let next_settings_generation = checked_increment(settings_generation)?;
    sqlite(transaction.execute(
        "INSERT INTO active_settings(
             id, revision, document_json, settings_generation, day_generation, pricing_generation
         ) VALUES (1, ?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(id) DO UPDATE SET
             revision = excluded.revision,
             document_json = excluded.document_json,
             settings_generation = excluded.settings_generation,
             day_generation = excluded.day_generation,
             pricing_generation = excluded.pricing_generation",
        params![
            to_i64(next_revision)?,
            next.json,
            to_i64(next_settings_generation)?,
            to_i64(day_generation)?,
            to_i64(pricing_generation)?,
        ],
    ))?;
    sqlite(transaction.commit())?;
    Ok(())
}

fn prepare_credential_import(
    connection: &mut Connection,
    run_id: &str,
    expected_settings_revision: u64,
    operations: Vec<CredentialOperation>,
) -> Result<(), StoreError> {
    validate_opaque_id(run_id)?;
    if operations.is_empty() || operations.len() > MAX_CREDENTIAL_OPERATIONS {
        return Err(StoreError::InvalidImport);
    }
    let mut operation_ids = BTreeSet::new();
    let mut credential_ids = BTreeSet::new();
    for operation in &operations {
        validate_opaque_id(&operation.operation_id)?;
        validate_opaque_id(&operation.credential_id)?;
        if operation.state != CredentialOperationState::Prepared
            || !operation_ids.insert(operation.operation_id.as_str())
            || !credential_ids.insert(operation.credential_id.as_str())
        {
            return Err(StoreError::InvalidImport);
        }
    }

    let transaction = sqlite(connection.transaction_with_behavior(TransactionBehavior::Immediate))?;
    let (revision, _, _, _) = current_generations(&transaction)?;
    if revision != expected_settings_revision {
        return Err(StoreError::Conflict {
            current_revision: revision,
        });
    }
    let duplicate: i64 = sqlite(transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM import_runs WHERE run_id = ?1)
              OR EXISTS(SELECT 1 FROM pending_credential_imports WHERE run_id = ?1)",
        [run_id],
        |row| row.get(0),
    ))?;
    if duplicate != 0 {
        return Err(StoreError::InvalidImport);
    }
    sqlite(transaction.execute(
        "INSERT INTO pending_credential_imports(
             run_id, expected_settings_revision, prepared_at
         ) VALUES (?1, ?2, ?3)",
        params![run_id, to_i64(expected_settings_revision)?, now_seconds()?],
    ))?;
    for operation in operations {
        sqlite(transaction.execute(
            "INSERT INTO credential_operations(
                 operation_id, run_id, credential_id, operation, operation_state
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                operation.operation_id,
                run_id,
                operation.credential_id,
                operation.operation.as_str(),
                operation.state.as_str(),
            ],
        ))?;
    }
    sqlite(transaction.commit())?;
    Ok(())
}

fn advance_credential_operation(
    connection: &mut Connection,
    operation_id: &str,
    expected: CredentialOperationState,
    next: CredentialOperationState,
) -> Result<(), StoreError> {
    validate_opaque_id(operation_id)?;
    let valid_transition = matches!(
        (expected, next),
        (
            CredentialOperationState::Prepared,
            CredentialOperationState::SecretWritten
        ) | (
            CredentialOperationState::SecretWritten,
            CredentialOperationState::Verified
        )
    );
    if !valid_transition {
        return Err(StoreError::InvalidImport);
    }
    let transaction = sqlite(connection.transaction_with_behavior(TransactionBehavior::Immediate))?;
    let changed = sqlite(transaction.execute(
        "UPDATE credential_operations
         SET operation_state = ?1
         WHERE operation_id = ?2 AND operation_state = ?3
           AND run_id IN (SELECT run_id FROM pending_credential_imports)",
        params![next.as_str(), operation_id, expected.as_str()],
    ))?;
    if changed != 1 {
        return Err(StoreError::CredentialTransitionConflict);
    }
    sqlite(transaction.commit())?;
    Ok(())
}

fn adopt_import(
    connection: &mut Connection,
    mut import: ValidatedImport,
) -> Result<(), StoreError> {
    validate_import_capacities(&import)?;
    validate_opaque_id(&import.run_id)?;
    if !import.credential_operations.is_empty() {
        return Err(StoreError::InvalidImport);
    }
    let transaction = sqlite(connection.transaction_with_behavior(TransactionBehavior::Immediate))?;
    let (revision, settings_generation, day_generation, pricing_generation) =
        current_generations(&transaction)?;
    if revision != import.expected_settings_revision {
        return Err(StoreError::Conflict {
            current_revision: revision,
        });
    }
    let duplicate: i64 = sqlite(transaction.query_row(
        "SELECT COUNT(*) FROM import_runs WHERE run_id = ?1",
        [&import.run_id],
        |row| row.get(0),
    ))?;
    if duplicate != 0 {
        return Err(StoreError::InvalidImport);
    }
    let finalize_credentials = validate_credential_finalization(&transaction, &import)?;
    sqlite(transaction.execute(
        "INSERT INTO import_runs(run_id, committed_at) VALUES (?1, ?2)",
        params![import.run_id, now_seconds()?],
    ))?;

    if let Some(settings) = import.settings {
        sqlite(transaction.execute(
            "INSERT INTO active_settings(
                 id, revision, document_json, settings_generation, day_generation, pricing_generation
             ) VALUES (1, ?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET
                 revision = excluded.revision,
                 document_json = excluded.document_json,
                 settings_generation = excluded.settings_generation,
                 day_generation = excluded.day_generation,
                 pricing_generation = excluded.pricing_generation",
            params![
                to_i64(checked_increment(revision)?)?,
                settings.json,
                to_i64(checked_increment(settings_generation)?)?,
                to_i64(day_generation)?,
                to_i64(pricing_generation)?,
            ],
        ))?;
    }

    import.sessions.sort_by(|left, right| {
        (
            &left.client,
            &left.session,
            left.source_revision,
            &left.original_entry_json,
        )
            .cmp(&(
                &right.client,
                &right.session,
                right.source_revision,
                &right.original_entry_json,
            ))
    });
    for session in import.sessions {
        insert_session(&transaction, &import.run_id, session)?;
    }
    for (ordinal, item) in import.report_items.into_iter().enumerate() {
        validate_opaque_id(&item.artifact_kind)?;
        sqlite(transaction.execute(
            "INSERT INTO import_report_items(
                 run_id, ordinal, artifact_kind, preservation_state, import_state,
                 activation_state, records
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                import.run_id,
                to_i64(ordinal as u64)?,
                item.artifact_kind,
                item.preservation.as_str(),
                item.import.as_str(),
                item.activation.as_str(),
                to_i64(item.records)?,
            ],
        ))?;
    }
    for (ordinal, artifact) in import.staged_artifacts.into_iter().enumerate() {
        validate_staging_id(&artifact.staging_id)?;
        validate_opaque_id(&artifact.artifact_kind)?;
        sqlite(transaction.execute(
            "INSERT INTO staged_artifacts(
                 run_id, ordinal, staging_id, artifact_kind, bytes
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                import.run_id,
                to_i64(ordinal as u64)?,
                artifact.staging_id,
                artifact.artifact_kind,
                to_i64(artifact.bytes)?,
            ],
        ))?;
    }
    for reference in import.credential_references {
        validate_opaque_id(&reference.credential_id)?;
        validate_opaque_id(&reference.credential_kind)?;
        sqlite(transaction.execute(
            "INSERT INTO credential_references(credential_id, run_id, credential_kind)
             VALUES (?1, ?2, ?3)",
            params![
                reference.credential_id,
                import.run_id,
                reference.credential_kind
            ],
        ))?;
    }
    if finalize_credentials {
        let changed = sqlite(transaction.execute(
            "UPDATE credential_operations SET operation_state = ?1
             WHERE run_id = ?2 AND operation_state = ?3",
            params![
                CredentialOperationState::MetadataCommitted.as_str(),
                import.run_id,
                CredentialOperationState::Verified.as_str(),
            ],
        ))?;
        if changed == 0 {
            return Err(StoreError::CredentialsNotVerified);
        }
        let deleted = sqlite(transaction.execute(
            "DELETE FROM pending_credential_imports WHERE run_id = ?1",
            [&import.run_id],
        ))?;
        if deleted != 1 {
            return Err(StoreError::CredentialsNotVerified);
        }
    }
    sqlite(transaction.commit())?;
    Ok(())
}

fn validate_credential_finalization(
    transaction: &Transaction<'_>,
    import: &ValidatedImport,
) -> Result<bool, StoreError> {
    let pending_revision = sqlite(
        transaction
            .query_row(
                "SELECT expected_settings_revision FROM pending_credential_imports
                 WHERE run_id = ?1",
                [&import.run_id],
                |row| row.get::<_, i64>(0),
            )
            .optional(),
    )?;
    let Some(pending_revision) = pending_revision else {
        return if import.credential_references.is_empty() {
            Ok(false)
        } else {
            Err(StoreError::CredentialsNotVerified)
        };
    };
    if from_i64(pending_revision)? != import.expected_settings_revision {
        return Err(StoreError::InvalidImport);
    }

    let mut statement = sqlite(transaction.prepare(
        "SELECT credential_id, operation, operation_state
         FROM credential_operations WHERE run_id = ?1 ORDER BY operation_id",
    ))?;
    let operations = sqlite(statement.query_map([&import.run_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    }))?
    .collect::<Result<Vec<_>, _>>()
    .map_err(|_| StoreError::Sqlite)?;
    if operations.is_empty()
        || operations
            .iter()
            .any(|(_, _, state)| state != CredentialOperationState::Verified.as_str())
    {
        return Err(StoreError::CredentialsNotVerified);
    }

    let set_credentials = operations
        .iter()
        .filter(|(_, operation, _)| operation == CredentialOperationKind::Set.as_str())
        .map(|(credential_id, _, _)| credential_id.as_str())
        .collect::<BTreeSet<_>>();
    let referenced_credentials = import
        .credential_references
        .iter()
        .map(|reference| reference.credential_id.as_str())
        .collect::<BTreeSet<_>>();
    if set_credentials != referenced_credentials
        || referenced_credentials.len() != import.credential_references.len()
    {
        return Err(StoreError::InvalidImport);
    }
    Ok(true)
}

fn insert_session(
    transaction: &Transaction<'_>,
    run_id: &str,
    session: ImportedSession,
) -> Result<(), StoreError> {
    validate_identity(&session.client, 64)?;
    validate_identity(&session.session, 512)?;
    validate_legacy_key(&session.legacy_key)?;
    validate_json_document(&session.original_entry_json, MAX_ENTRY_BYTES)?;
    if let Some(metadata) = &session.archive_metadata_json {
        validate_json_document(metadata, MAX_ENTRY_BYTES)?;
    }
    let source_revision = to_i64(session.source_revision)?;
    let existing = sqlite(
        transaction
            .query_row(
                "SELECT original_entry_json, legacy_key, source_revision, archive_metadata_json
         FROM imported_sessions WHERE client = ?1 AND session = ?2",
                params![session.client, session.session],
                |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Option<Vec<u8>>>(3)?,
                    ))
                },
            )
            .optional(),
    )?;
    match existing {
        None => {
            sqlite(transaction.execute(
                "INSERT INTO imported_sessions(
                     client, session, original_entry_json, legacy_key, source_revision,
                     archive_metadata_json, import_run_id
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    session.client,
                    session.session,
                    session.original_entry_json,
                    session.legacy_key,
                    source_revision,
                    session.archive_metadata_json,
                    run_id,
                ],
            ))?;
        }
        Some((json, legacy_key, revision, metadata)) if revision == source_revision => {
            if json != session.original_entry_json
                || legacy_key != session.legacy_key
                || metadata != session.archive_metadata_json
            {
                return Err(StoreError::InvalidImport);
            }
        }
        Some((_, _, revision, _)) if revision > source_revision => {}
        Some(_) => {
            sqlite(transaction.execute(
                "UPDATE imported_sessions SET
                     original_entry_json = ?1,
                     legacy_key = ?2,
                     source_revision = ?3,
                     archive_metadata_json = ?4,
                     import_run_id = ?5
                 WHERE client = ?6 AND session = ?7",
                params![
                    session.original_entry_json,
                    session.legacy_key,
                    source_revision,
                    session.archive_metadata_json,
                    run_id,
                    session.client,
                    session.session,
                ],
            ))?;
        }
    }
    Ok(())
}

fn commit_runtime(
    connection: &mut Connection,
    commit: CanonicalCommit,
    current: &PersistedRuntimeSnapshot,
) -> Result<Arc<PersistedRuntimeSnapshot>, StoreError> {
    if commit.expected_runtime_generation != current.generation {
        return Err(StoreError::RuntimeConflict {
            current_generation: current.generation,
        });
    }
    validate_runtime_commit(&commit, current)?;
    let next_generation = commit
        .expected_runtime_generation
        .checked_add(1)
        .ok_or(StoreError::InvalidRuntime)?;
    let mut sources = current.sources.iter().cloned().collect::<Vec<_>>();
    sources.retain(|source| source.source_id != commit.source.source_id);
    sources.push(commit.source.clone());
    sources.sort_by_key(|source| source.source_id);

    let timezone = commit
        .timezone_name
        .parse::<BucketTimezone>()
        .map_err(|_| StoreError::InvalidRuntime)?;
    let rebuilt = CanonicalLedger::rehydrate(timezone, sources.iter().cloned(), next_generation)
        .map_err(|_| StoreError::InvalidRuntime)?;
    if rebuilt.canonical().as_ref() != commit.canonical.as_ref() {
        return Err(StoreError::InvalidRuntime);
    }

    let transaction = sqlite(connection.transaction_with_behavior(TransactionBehavior::Immediate))?;
    let database_generation = runtime_generation(&transaction)?;
    if database_generation != commit.expected_runtime_generation {
        return Err(StoreError::RuntimeConflict {
            current_generation: database_generation,
        });
    }
    let (_, settings_generation, day_generation, _) = current_generations(&transaction)?;
    if settings_generation != commit.settings_generation || day_generation != commit.day_generation
    {
        return Err(StoreError::InvalidRuntime);
    }

    sqlite(transaction.execute("DELETE FROM canonical_provenance", []))?;
    sqlite(transaction.execute("DELETE FROM canonical_sqlite_provenance", []))?;
    sqlite(transaction.execute("DELETE FROM canonical_observations", []))?;
    sqlite(transaction.execute(
        "DELETE FROM runtime_sources WHERE source_id = ?1",
        [commit.source.source_id.as_storage_key()],
    ))?;
    sqlite(transaction.execute(
        "DELETE FROM runtime_sqlite_sources WHERE source_id = ?1",
        [commit.source.source_id.as_storage_key()],
    ))?;
    insert_runtime_source(&transaction, &commit.source)?;
    insert_canonical_rows(&transaction, &commit.canonical)?;
    sqlite(transaction.execute(
        "INSERT INTO runtime_state(
             id, runtime_generation, settings_generation, day_generation,
             timezone_name, parser_contract_revision, committed_at
         ) VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(id) DO UPDATE SET
             runtime_generation = excluded.runtime_generation,
             settings_generation = excluded.settings_generation,
             day_generation = excluded.day_generation,
             timezone_name = excluded.timezone_name,
             parser_contract_revision = excluded.parser_contract_revision,
             committed_at = excluded.committed_at",
        params![
            runtime_i64(next_generation)?,
            runtime_i64(commit.settings_generation)?,
            runtime_i64(commit.day_generation)?,
            commit.timezone_name,
            PARSER_CONTRACT_REVISION,
            now_seconds()?,
        ],
    ))?;
    sqlite(transaction.commit())?;

    Ok(Arc::new(PersistedRuntimeSnapshot {
        initialized: true,
        generation: next_generation,
        settings_generation: commit.settings_generation,
        day_generation: commit.day_generation,
        timezone_name: Some(timezone.name().to_owned()),
        parser_contract_revision: PARSER_CONTRACT_REVISION.to_owned(),
        sources: sources.into(),
        canonical: Some(commit.canonical),
    }))
}

fn validate_runtime_commit(
    commit: &CanonicalCommit,
    current: &PersistedRuntimeSnapshot,
) -> Result<(), StoreError> {
    if commit.expected_runtime_generation != current.generation
        || commit.canonical.generation
            != commit
                .expected_runtime_generation
                .checked_add(1)
                .ok_or(StoreError::InvalidRuntime)?
        || commit.canonical.rollups.generation != commit.canonical.generation
        || commit.canonical.rollups.timezone_name != commit.timezone_name
        || commit.source.generation == 0
        || commit.source.rows.len() > MAX_CANONICAL_ROWS
        || current.sources.len() >= MAX_SOURCES
            && !current
                .sources
                .iter()
                .any(|source| source.source_id == commit.source.source_id)
    {
        return Err(StoreError::InvalidRuntime);
    }
    if let Some(previous) = current
        .sources
        .iter()
        .find(|source| source.source_id == commit.source.source_id)
        && commit.source.generation <= previous.generation
    {
        return Err(StoreError::InvalidRuntime);
    }
    validate_runtime_text(&commit.timezone_name, 128)?;
    validate_runtime_text(&commit.source.parser_pin, 128)?;
    let mut identity_bytes = 0_usize;
    for row in commit.source.rows.iter() {
        validate_usage(row, &mut identity_bytes)?;
    }
    let mut canonical_ids = BTreeSet::new();
    for row in commit.canonical.rows.iter() {
        validate_runtime_text(&row.canonical_id, MAX_RUNTIME_IDENTITY_BYTES)?;
        identity_bytes = identity_bytes
            .checked_add(row.canonical_id.len())
            .ok_or(StoreError::InvalidRuntime)?;
        validate_usage(&row.usage, &mut identity_bytes)?;
        if !canonical_ids.insert(row.canonical_id.as_str()) || row.provenance.is_empty() {
            return Err(StoreError::InvalidRuntime);
        }
    }
    if identity_bytes > MAX_RUNTIME_TOTAL_IDENTITY_BYTES {
        return Err(StoreError::InvalidRuntime);
    }
    Ok(())
}

fn validate_usage(row: &ResponseUsage, identity_bytes: &mut usize) -> Result<(), StoreError> {
    if row.cost != CostStatus::Unavailable || row.cache_write_1h > row.tokens.cache_write {
        return Err(StoreError::InvalidRuntime);
    }
    for value in [&row.key.client, &row.key.session, &row.model] {
        validate_runtime_text(value, MAX_RUNTIME_IDENTITY_BYTES)?;
        *identity_bytes = identity_bytes
            .checked_add(value.len())
            .ok_or(StoreError::InvalidRuntime)?;
    }
    for value in [
        row.dedup_key.as_ref(),
        row.billing_provider_hint.as_ref(),
        row.service_tier.as_ref(),
        row.agent.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        validate_runtime_text(value, MAX_RUNTIME_IDENTITY_BYTES)?;
        *identity_bytes = identity_bytes
            .checked_add(value.len())
            .ok_or(StoreError::InvalidRuntime)?;
    }
    Ok(())
}

fn validate_runtime_text(value: &str, maximum: usize) -> Result<(), StoreError> {
    if value.is_empty()
        || value.len() > maximum
        || value.chars().any(char::is_control)
        || is_absolute_path_text(value)
    {
        return Err(StoreError::InvalidRuntime);
    }
    Ok(())
}

fn runtime_generation(transaction: &Transaction<'_>) -> Result<u64, StoreError> {
    let value = sqlite(
        transaction
            .query_row(
                "SELECT runtime_generation FROM runtime_state WHERE id = 1",
                [],
                |row| row.get::<_, i64>(0),
            )
            .optional(),
    )?;
    value.map_or(Ok(0), from_i64)
}

fn insert_runtime_source(
    transaction: &Transaction<'_>,
    source: &SourceSnapshot,
) -> Result<(), StoreError> {
    if source.format.is_sqlite() {
        return insert_sqlite_source(transaction, source);
    }
    let source_id = source.source_id.as_storage_key();
    let sessions = source
        .rows
        .iter()
        .map(|row| (&row.key.client, &row.key.session))
        .collect::<BTreeSet<_>>();
    let last_observed_at = source.rows.iter().map(|row| row.timestamp_ms).max();
    sqlite(transaction.execute(
        "INSERT INTO runtime_sources(
             source_id, format, client, parser_pin, source_generation, source_state,
             observation_count, session_count, last_observed_at, incomplete_tail,
             has_supported_usage
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            source_id,
            source.format.storage_name(),
            source.format.client(),
            source.parser_pin,
            runtime_i64(source.generation)?,
            source.source_state.as_str(),
            runtime_i64(source.rows.len() as u64)?,
            runtime_i64(sessions.len() as u64)?,
            last_observed_at,
            i64::from(source.incomplete_tail),
            i64::from(source.has_supported_usage),
        ],
    ))?;
    for (ordinal, row) in source.rows.iter().enumerate() {
        sqlite(transaction.execute(
            "INSERT INTO source_observations(
                 source_id, source_generation, ordinal, client, session, model, observed_at,
                 input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                 reasoning_tokens, cache_write_1h_tokens, dedup_identity, identity_class,
                 provider_hint, service_tier, agent
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                       ?14, ?15, ?16, ?17, ?18)",
            params![
                source_id,
                runtime_i64(source.generation)?,
                runtime_i64(ordinal as u64)?,
                row.key.client,
                row.key.session,
                row.model,
                row.timestamp_ms,
                token_i64(row.tokens.input)?,
                token_i64(row.tokens.output)?,
                token_i64(row.tokens.cache_read)?,
                token_i64(row.tokens.cache_write)?,
                token_i64(row.tokens.reasoning)?,
                token_i64(row.cache_write_1h)?,
                row.dedup_key,
                if row.dedup_key.is_some() {
                    "global-stable"
                } else {
                    "source-local"
                },
                row.billing_provider_hint,
                row.service_tier,
                row.agent,
            ],
        ))?;
    }
    Ok(())
}

fn insert_sqlite_source(
    transaction: &Transaction<'_>,
    source: &SourceSnapshot,
) -> Result<(), StoreError> {
    let source_id = source.source_id.as_storage_key();
    let sessions = source
        .rows
        .iter()
        .map(|row| (&row.key.client, &row.key.session))
        .chain(
            source
                .retained
                .iter()
                .map(|row| (&row.client, &row.session)),
        )
        .collect::<BTreeSet<_>>();
    let last_observed_at = source
        .rows
        .iter()
        .map(|row| row.timestamp_ms)
        .chain(source.retained.iter().map(|row| row.captured_at_ms))
        .max();
    sqlite(transaction.execute(
        "INSERT INTO runtime_sqlite_sources(
             source_id, format, client, parser_pin, source_generation, source_state,
             observation_count, retained_period_count, session_count, last_observed_at,
             incomplete_tail, has_supported_usage
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            source_id,
            source.format.storage_name(),
            source.format.client(),
            source.parser_pin,
            runtime_i64(source.generation)?,
            source.source_state.as_str(),
            runtime_i64(source.rows.len() as u64)?,
            runtime_i64(source.retained.len() as u64)?,
            runtime_i64(sessions.len() as u64)?,
            last_observed_at,
            i64::from(source.incomplete_tail),
            i64::from(source.has_supported_usage),
        ],
    ))?;
    for (ordinal, row) in source.rows.iter().enumerate() {
        sqlite(transaction.execute(
            "INSERT INTO source_sqlite_observations(
                 source_id, source_generation, ordinal, client, session, model, observed_at,
                 input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                 reasoning_tokens, cache_write_1h_tokens, dedup_identity, identity_class,
                 provider_hint, service_tier, agent
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                       ?14, ?15, ?16, ?17, ?18)",
            params![
                source_id,
                runtime_i64(source.generation)?,
                runtime_i64(ordinal as u64)?,
                row.key.client,
                row.key.session,
                row.model,
                row.timestamp_ms,
                token_i64(row.tokens.input)?,
                token_i64(row.tokens.output)?,
                token_i64(row.tokens.cache_read)?,
                token_i64(row.tokens.cache_write)?,
                token_i64(row.tokens.reasoning)?,
                token_i64(row.cache_write_1h)?,
                row.dedup_key,
                if row.dedup_key.is_some() {
                    "global-stable"
                } else {
                    "source-local"
                },
                row.billing_provider_hint,
                row.service_tier,
                row.agent,
            ],
        ))?;
    }
    for (ordinal, period) in source.retained.iter().enumerate() {
        sqlite(transaction.execute(
            "INSERT INTO source_retained_periods(
                 source_id, source_generation, ordinal, client, session, period, captured_at,
                 day, month, total_tokens, input_tokens, output_tokens, cache_read_tokens,
                 cache_write_tokens, reasoning_tokens, unclassified_tokens,
                 components_complete, models_complete
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                       ?15, ?16, ?17, ?18)",
            params![
                source_id,
                runtime_i64(source.generation)?,
                runtime_i64(ordinal as u64)?,
                period.client,
                period.session,
                period.period.as_str(),
                period.captured_at_ms,
                period.day,
                period.month,
                token_i64(period.total)?,
                token_i64(period.input)?,
                token_i64(period.output)?,
                token_i64(period.cache_read)?,
                token_i64(period.cache_write)?,
                token_i64(period.reasoning)?,
                token_i64(period.unclassified)?,
                i64::from(period.components_complete),
                i64::from(period.models_complete),
            ],
        ))?;
        for (model_ordinal, model) in period.models.iter().enumerate() {
            sqlite(transaction.execute(
                "INSERT INTO source_retained_models(
                     source_id, source_generation, period_ordinal, ordinal, model,
                     total_tokens, input_tokens, cache_read_tokens, cache_write_tokens,
                     output_tokens
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    source_id,
                    runtime_i64(source.generation)?,
                    runtime_i64(ordinal as u64)?,
                    runtime_i64(model_ordinal as u64)?,
                    model.model,
                    token_i64(model.total)?,
                    token_i64(model.input)?,
                    token_i64(model.cache_read)?,
                    token_i64(model.cache_write)?,
                    token_i64(model.output)?,
                ],
            ))?;
        }
    }
    Ok(())
}

fn insert_canonical_rows(
    transaction: &Transaction<'_>,
    canonical: &CanonicalObservationSnapshot,
) -> Result<(), StoreError> {
    for (ordinal, row) in canonical.rows.iter().enumerate() {
        sqlite(transaction.execute(
            "INSERT INTO canonical_observations(
                 canonical_id, ordinal, client, session, model, observed_at, input_tokens,
                 output_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens,
                 cache_write_1h_tokens, dedup_identity, provider_hint, service_tier, agent,
                 source_state
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                       ?15, ?16, ?17)",
            params![
                row.canonical_id,
                runtime_i64(ordinal as u64)?,
                row.usage.key.client,
                row.usage.key.session,
                row.usage.model,
                row.usage.timestamp_ms,
                token_i64(row.usage.tokens.input)?,
                token_i64(row.usage.tokens.output)?,
                token_i64(row.usage.tokens.cache_read)?,
                token_i64(row.usage.tokens.cache_write)?,
                token_i64(row.usage.tokens.reasoning)?,
                token_i64(row.usage.cache_write_1h)?,
                row.usage.dedup_key,
                row.usage.billing_provider_hint,
                row.usage.service_tier,
                row.usage.agent,
                row.source_state.as_str(),
            ],
        ))?;
        for link in row.provenance.iter() {
            let source_id = link.source_id.as_storage_key();
            let in_source_observations: i64 = sqlite(transaction.query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM source_observations
                     WHERE source_id = ?1 AND source_generation = ?2 AND ordinal = ?3
                 )",
                params![
                    source_id,
                    runtime_i64(link.source_generation)?,
                    runtime_i64(link.observation_ordinal as u64)?,
                ],
                |row| row.get(0),
            ))?;
            let table = if in_source_observations == 1 {
                "canonical_provenance"
            } else {
                "canonical_sqlite_provenance"
            };
            sqlite(transaction.execute(
                &format!(
                    "INSERT INTO {table}(
                         canonical_id, source_id, source_generation, observation_ordinal
                     ) VALUES (?1, ?2, ?3, ?4)"
                ),
                params![
                    row.canonical_id,
                    source_id,
                    runtime_i64(link.source_generation)?,
                    runtime_i64(link.observation_ordinal as u64)?,
                ],
            ))?;
        }
    }
    Ok(())
}

fn token_i64(value: u64) -> Result<i64, StoreError> {
    value.try_into().map_err(|_| StoreError::InvalidRuntime)
}

fn runtime_i64(value: u64) -> Result<i64, StoreError> {
    value.try_into().map_err(|_| StoreError::InvalidRuntime)
}

fn load_snapshot(connection: &Connection) -> Result<StoreSnapshot, StoreError> {
    let foreign_keys: i64 =
        sqlite(connection.query_row("PRAGMA foreign_keys", [], |row| row.get(0)))?;
    if schema_version(connection)? != SCHEMA_VERSION {
        return Err(StoreError::Sqlite);
    }
    let settings_row = sqlite(connection.query_row(
        "SELECT revision, document_json, settings_generation, day_generation, pricing_generation
         FROM active_settings WHERE id = 1",
        [],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
            ))
        },
    ).optional())?;
    let (settings_revision, settings, settings_generation, day_generation, pricing_generation) =
        match settings_row {
            Some((revision, json, settings_generation, day_generation, pricing_generation)) => (
                from_i64(revision)?,
                Some(NativeSettings::from_json_bytes(&json)?),
                from_i64(settings_generation)?,
                from_i64(day_generation)?,
                from_i64(pricing_generation)?,
            ),
            None => (0, None, 0, 0, 0),
        };

    let sessions = {
        let mut statement = sqlite(connection.prepare(
            "SELECT client, session, original_entry_json, legacy_key, source_revision,
                    archive_metadata_json
             FROM imported_sessions ORDER BY client, session",
        ))?;
        sqlite(statement.query_map([], |row| {
            Ok(ImportedSession {
                client: row.get(0)?,
                session: row.get(1)?,
                original_entry_json: row.get(2)?,
                legacy_key: row.get(3)?,
                source_revision: row.get::<_, i64>(4)?.try_into().map_err(|_| {
                    rusqlite::Error::IntegralValueOutOfRange(4, row.get::<_, i64>(4).unwrap_or(-1))
                })?,
                archive_metadata_json: row.get(5)?,
            })
        }))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| StoreError::Sqlite)?
    };
    let import_runs = {
        let mut statement = sqlite(
            connection.prepare("SELECT run_id, committed_at FROM import_runs ORDER BY run_id"),
        )?;
        sqlite(statement.query_map([], |row| {
            Ok(StoredImportRun {
                run_id: row.get(0)?,
                committed_at: row.get(1)?,
            })
        }))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| StoreError::Sqlite)?
    };
    let report_items = load_report_items(connection)?;
    let staged_artifacts = load_staged_artifacts(connection)?;
    let credential_references = load_credential_references(connection)?;
    let credential_operations = load_credential_operations(connection)?;
    let pending_credential_imports = load_pending_credential_imports(connection)?;
    let runtime = Arc::new(load_runtime_snapshot(connection)?);

    Ok(StoreSnapshot {
        schema_version: SCHEMA_VERSION,
        foreign_keys_enabled: foreign_keys == 1,
        settings_revision,
        settings_generation,
        day_generation,
        pricing_generation,
        settings,
        sessions,
        import_runs,
        report_items,
        staged_artifacts,
        credential_references,
        credential_operations,
        pending_credential_imports,
        runtime,
    })
}

#[derive(Debug)]
struct RawUsage {
    client: String,
    session: String,
    model: String,
    observed_at: i64,
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
    reasoning: i64,
    cache_write_1h: i64,
    dedup_identity: Option<String>,
    provider_hint: Option<String>,
    service_tier: Option<String>,
    agent: Option<String>,
}

impl RawUsage {
    fn into_usage(self) -> Result<ResponseUsage, StoreError> {
        Ok(ResponseUsage {
            key: SessionKey::new(self.client, self.session),
            model: self.model,
            tokens: TokenCounts {
                input: from_i64(self.input)?,
                output: from_i64(self.output)?,
                cache_read: from_i64(self.cache_read)?,
                cache_write: from_i64(self.cache_write)?,
                reasoning: from_i64(self.reasoning)?,
            },
            cache_write_1h: from_i64(self.cache_write_1h)?,
            timestamp_ms: self.observed_at,
            dedup_key: self.dedup_identity,
            billing_provider_hint: self.provider_hint,
            service_tier: self.service_tier,
            agent: self.agent,
            cost: CostStatus::Unavailable,
        })
    }
}

fn load_runtime_snapshot(connection: &Connection) -> Result<PersistedRuntimeSnapshot, StoreError> {
    let state = sqlite(
        connection
            .query_row(
                "SELECT runtime_generation, settings_generation, day_generation,
                        timezone_name, parser_contract_revision
                 FROM runtime_state WHERE id = 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                },
            )
            .optional(),
    )?;
    let Some((generation, settings_generation, day_generation, timezone_name, revision)) = state
    else {
        let runtime_rows: i64 = sqlite(connection.query_row(
            "SELECT
                 (SELECT COUNT(*) FROM runtime_sources) +
                 (SELECT COUNT(*) FROM runtime_sqlite_sources) +
                 (SELECT COUNT(*) FROM source_observations) +
                 (SELECT COUNT(*) FROM source_sqlite_observations) +
                 (SELECT COUNT(*) FROM source_retained_periods) +
                 (SELECT COUNT(*) FROM source_retained_models) +
                 (SELECT COUNT(*) FROM canonical_observations) +
                 (SELECT COUNT(*) FROM canonical_provenance) +
                 (SELECT COUNT(*) FROM canonical_sqlite_provenance)",
            [],
            |row| row.get(0),
        ))?;
        if runtime_rows != 0 {
            return Err(StoreError::Sqlite);
        }
        return Ok(PersistedRuntimeSnapshot::empty());
    };

    if revision != PARSER_CONTRACT_REVISION {
        return Err(StoreError::Sqlite);
    }
    let generation = from_i64(generation)?;
    let settings_generation = from_i64(settings_generation)?;
    let day_generation = from_i64(day_generation)?;
    if generation == 0 {
        return Err(StoreError::Sqlite);
    }
    let timezone = timezone_name
        .parse::<BucketTimezone>()
        .map_err(|_| StoreError::Sqlite)?;
    if timezone.name() != timezone_name {
        return Err(StoreError::Sqlite);
    }

    let sources = load_runtime_sources(connection)?;
    let rebuilt = CanonicalLedger::rehydrate(timezone, sources.iter().cloned(), generation)
        .map_err(|_| StoreError::Sqlite)?;
    let canonical = rebuilt.canonical();
    let stored_canonical = load_canonical_rows(connection, generation, &canonical.rollups)?;
    if stored_canonical.as_ref() != canonical.as_ref() {
        return Err(StoreError::Sqlite);
    }

    Ok(PersistedRuntimeSnapshot {
        initialized: true,
        generation,
        settings_generation,
        day_generation,
        timezone_name: Some(timezone_name),
        parser_contract_revision: revision,
        sources: sources.into(),
        canonical: Some(canonical),
    })
}

fn load_runtime_sources(connection: &Connection) -> Result<Vec<Arc<SourceSnapshot>>, StoreError> {
    let mut sources = load_jsonl_runtime_sources(connection)?;
    sources.extend(load_sqlite_runtime_sources(connection)?);
    sources.sort_by_key(|source| source.source_id);
    Ok(sources)
}

fn load_jsonl_runtime_sources(
    connection: &Connection,
) -> Result<Vec<Arc<SourceSnapshot>>, StoreError> {
    let raw = {
        let mut statement = sqlite(connection.prepare(
            "SELECT source_id, format, client, parser_pin, source_generation, source_state,
                    observation_count, session_count, last_observed_at, incomplete_tail,
                    has_supported_usage
             FROM runtime_sources ORDER BY source_id",
        ))?;
        sqlite(statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, Option<i64>>(8)?,
                row.get::<_, i64>(9)?,
                row.get::<_, i64>(10)?,
            ))
        }))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| StoreError::Sqlite)?
    };
    if raw.len() > MAX_SOURCES {
        return Err(StoreError::Sqlite);
    }

    raw.into_iter()
        .map(
            |(
                source_key,
                format,
                client,
                parser_pin,
                source_generation,
                source_state,
                observation_count,
                session_count,
                last_observed_at,
                incomplete_tail,
                has_supported_usage,
            )| {
                let source_id =
                    SourceId::parse_storage_key(&source_key).ok_or(StoreError::Sqlite)?;
                let format = SourceFormat::parse_storage_name(&format).ok_or(StoreError::Sqlite)?;
                let source_state =
                    ObservationSourceState::parse(&source_state).ok_or(StoreError::Sqlite)?;
                let source_generation = from_i64(source_generation)?;
                if source_generation == 0 || format.client() != client {
                    return Err(StoreError::Sqlite);
                }
                validate_runtime_text(&parser_pin, 128).map_err(|_| StoreError::Sqlite)?;
                let rows = load_source_observations(connection, &source_key, source_generation)?;
                if rows.len() != from_i64(observation_count)? as usize {
                    return Err(StoreError::Sqlite);
                }
                let sessions = rows
                    .iter()
                    .map(|row| (&row.key.client, &row.key.session))
                    .collect::<BTreeSet<_>>();
                if sessions.len() != from_i64(session_count)? as usize
                    || rows.iter().map(|row| row.timestamp_ms).max() != last_observed_at
                {
                    return Err(StoreError::Sqlite);
                }
                let (totals, cache_write_1h) = rows.iter().try_fold(
                    (TokenCounts::default(), 0_u64),
                    |(totals, cache), row| {
                        Ok::<_, StoreError>((
                            totals
                                .checked_add(row.tokens)
                                .map_err(|_| StoreError::Sqlite)?,
                            cache
                                .checked_add(row.cache_write_1h)
                                .ok_or(StoreError::Sqlite)?,
                        ))
                    },
                )?;
                Ok(Arc::new(SourceSnapshot {
                    source_id,
                    format,
                    parser_pin,
                    generation: source_generation,
                    rows: rows.into(),
                    retained: Arc::from([]),
                    totals,
                    cache_write_1h,
                    incomplete_tail: parse_sql_bool(incomplete_tail)?,
                    has_supported_usage: parse_sql_bool(has_supported_usage)?,
                    source_state,
                }))
            },
        )
        .collect()
}

fn load_sqlite_runtime_sources(
    connection: &Connection,
) -> Result<Vec<Arc<SourceSnapshot>>, StoreError> {
    let raw = {
        let mut statement = sqlite(connection.prepare(
            "SELECT source_id, format, client, parser_pin, source_generation, source_state,
                    observation_count, retained_period_count, session_count, last_observed_at,
                    incomplete_tail, has_supported_usage
             FROM runtime_sqlite_sources ORDER BY source_id",
        ))?;
        sqlite(statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, Option<i64>>(9)?,
                row.get::<_, i64>(10)?,
                row.get::<_, i64>(11)?,
            ))
        }))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| StoreError::Sqlite)?
    };
    if raw.len() > MAX_SOURCES {
        return Err(StoreError::Sqlite);
    }
    raw.into_iter()
        .map(
            |(
                source_key,
                format,
                client,
                parser_pin,
                source_generation,
                source_state,
                observation_count,
                retained_period_count,
                session_count,
                last_observed_at,
                incomplete_tail,
                has_supported_usage,
            )| {
                let source_id =
                    SourceId::parse_storage_key(&source_key).ok_or(StoreError::Sqlite)?;
                let format = SourceFormat::parse_storage_name(&format).ok_or(StoreError::Sqlite)?;
                if !format.is_sqlite() || format.client() != client {
                    return Err(StoreError::Sqlite);
                }
                let source_generation = from_i64(source_generation)?;
                if source_generation == 0 {
                    return Err(StoreError::Sqlite);
                }
                let rows =
                    load_source_sqlite_observations(connection, &source_key, source_generation)?;
                let retained = load_source_retained(connection, &source_key, source_generation)?;
                if rows.len() != from_i64(observation_count)? as usize
                    || retained.len() != from_i64(retained_period_count)? as usize
                {
                    return Err(StoreError::Sqlite);
                }
                let sessions = rows
                    .iter()
                    .map(|row| (&row.key.client, &row.key.session))
                    .chain(retained.iter().map(|row| (&row.client, &row.session)))
                    .collect::<BTreeSet<_>>();
                if sessions.len() != from_i64(session_count)? as usize
                    || rows
                        .iter()
                        .map(|row| row.timestamp_ms)
                        .chain(retained.iter().map(|row| row.captured_at_ms))
                        .max()
                        != last_observed_at
                {
                    return Err(StoreError::Sqlite);
                }
                let (totals, cache_write_1h) = rows.iter().try_fold(
                    (TokenCounts::default(), 0_u64),
                    |(totals, cache), row| {
                        Ok::<_, StoreError>((
                            totals
                                .checked_add(row.tokens)
                                .map_err(|_| StoreError::Sqlite)?,
                            cache
                                .checked_add(row.cache_write_1h)
                                .ok_or(StoreError::Sqlite)?,
                        ))
                    },
                )?;
                Ok(Arc::new(SourceSnapshot {
                    source_id,
                    format,
                    parser_pin,
                    generation: source_generation,
                    rows: rows.into(),
                    retained: retained.into(),
                    totals,
                    cache_write_1h,
                    incomplete_tail: parse_sql_bool(incomplete_tail)?,
                    has_supported_usage: parse_sql_bool(has_supported_usage)?,
                    source_state: ObservationSourceState::parse(&source_state)
                        .ok_or(StoreError::Sqlite)?,
                }))
            },
        )
        .collect()
}

fn load_source_sqlite_observations(
    connection: &Connection,
    source_id: &str,
    source_generation: u64,
) -> Result<Vec<ResponseUsage>, StoreError> {
    let mut statement = sqlite(connection.prepare(
        "SELECT ordinal, client, session, model, observed_at, input_tokens, output_tokens,
                cache_read_tokens, cache_write_tokens, reasoning_tokens, cache_write_1h_tokens,
                dedup_identity, identity_class, provider_hint, service_tier, agent
         FROM source_sqlite_observations
         WHERE source_id = ?1 AND source_generation = ?2
         ORDER BY ordinal",
    ))?;
    let raw = sqlite(statement.query_map(
        params![source_id, runtime_i64(source_generation)?],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                RawUsage {
                    client: row.get(1)?,
                    session: row.get(2)?,
                    model: row.get(3)?,
                    observed_at: row.get(4)?,
                    input: row.get(5)?,
                    output: row.get(6)?,
                    cache_read: row.get(7)?,
                    cache_write: row.get(8)?,
                    reasoning: row.get(9)?,
                    cache_write_1h: row.get(10)?,
                    dedup_identity: row.get(11)?,
                    provider_hint: row.get(13)?,
                    service_tier: row.get(14)?,
                    agent: row.get(15)?,
                },
                row.get::<_, String>(12)?,
            ))
        },
    ))?
    .collect::<Result<Vec<_>, _>>()
    .map_err(|_| StoreError::Sqlite)?;
    if raw.len() > MAX_CANONICAL_ROWS {
        return Err(StoreError::Sqlite);
    }
    raw.into_iter()
        .enumerate()
        .map(|(expected, (ordinal, raw, identity_class))| {
            if from_i64(ordinal)? as usize != expected
                || (raw.dedup_identity.is_some()) != (identity_class == "global-stable")
                || (raw.dedup_identity.is_none()) != (identity_class == "source-local")
            {
                return Err(StoreError::Sqlite);
            }
            let usage = raw.into_usage()?;
            let mut identity_bytes = 0;
            validate_usage(&usage, &mut identity_bytes).map_err(|_| StoreError::Sqlite)?;
            Ok(usage)
        })
        .collect()
}

fn load_source_retained(
    connection: &Connection,
    source_id: &str,
    source_generation: u64,
) -> Result<Vec<RetainedPeriodUsage>, StoreError> {
    let mut statement = sqlite(connection.prepare(
        "SELECT ordinal, client, session, period, captured_at, day, month, total_tokens,
                input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                reasoning_tokens, unclassified_tokens, components_complete, models_complete
         FROM source_retained_periods
         WHERE source_id = ?1 AND source_generation = ?2
         ORDER BY ordinal",
    ))?;
    let raw = sqlite(statement.query_map(
        params![source_id, runtime_i64(source_generation)?],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, i64>(9)?,
                row.get::<_, i64>(10)?,
                row.get::<_, i64>(11)?,
                row.get::<_, i64>(12)?,
                row.get::<_, i64>(13)?,
                row.get::<_, i64>(14)?,
                row.get::<_, i64>(15)?,
            ))
        },
    ))?
    .collect::<Result<Vec<_>, _>>()
    .map_err(|_| StoreError::Sqlite)?;
    if raw.len() > 10_000 {
        return Err(StoreError::Sqlite);
    }
    raw.into_iter()
        .enumerate()
        .map(
            |(
                expected,
                (
                    ordinal,
                    client,
                    session,
                    period,
                    captured_at,
                    day,
                    month,
                    total,
                    input,
                    output,
                    cache_read,
                    cache_write,
                    reasoning,
                    unclassified,
                    components_complete,
                    models_complete,
                ),
            )| {
                if from_i64(ordinal)? as usize != expected {
                    return Err(StoreError::Sqlite);
                }
                let models = load_retained_models(
                    connection,
                    source_id,
                    source_generation,
                    expected as u64,
                )?;
                Ok(RetainedPeriodUsage {
                    client,
                    session,
                    period: RetainedPeriod::parse(&period).ok_or(StoreError::Sqlite)?,
                    captured_at_ms: captured_at,
                    day,
                    month,
                    total: from_i64(total)?,
                    input: from_i64(input)?,
                    output: from_i64(output)?,
                    cache_read: from_i64(cache_read)?,
                    cache_write: from_i64(cache_write)?,
                    reasoning: from_i64(reasoning)?,
                    unclassified: from_i64(unclassified)?,
                    components_complete: parse_sql_bool(components_complete)?,
                    models_complete: parse_sql_bool(models_complete)?,
                    models: models.into(),
                })
            },
        )
        .collect()
}

fn load_retained_models(
    connection: &Connection,
    source_id: &str,
    source_generation: u64,
    period_ordinal: u64,
) -> Result<Vec<RetainedModelUsage>, StoreError> {
    let mut statement = sqlite(connection.prepare(
        "SELECT ordinal, model, total_tokens, input_tokens, cache_read_tokens,
                cache_write_tokens, output_tokens
         FROM source_retained_models
         WHERE source_id = ?1 AND source_generation = ?2 AND period_ordinal = ?3
         ORDER BY ordinal",
    ))?;
    let raw = sqlite(statement.query_map(
        params![
            source_id,
            runtime_i64(source_generation)?,
            runtime_i64(period_ordinal)?
        ],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
            ))
        },
    ))?
    .collect::<Result<Vec<_>, _>>()
    .map_err(|_| StoreError::Sqlite)?;
    raw.into_iter()
        .enumerate()
        .map(
            |(expected, (ordinal, model, total, input, cache_read, cache_write, output))| {
                if from_i64(ordinal)? as usize != expected {
                    return Err(StoreError::Sqlite);
                }
                Ok(RetainedModelUsage {
                    model,
                    total: from_i64(total)?,
                    input: from_i64(input)?,
                    cache_read: from_i64(cache_read)?,
                    cache_write: from_i64(cache_write)?,
                    output: from_i64(output)?,
                })
            },
        )
        .collect()
}

fn load_source_observations(
    connection: &Connection,
    source_id: &str,
    source_generation: u64,
) -> Result<Vec<ResponseUsage>, StoreError> {
    let mut statement = sqlite(connection.prepare(
        "SELECT ordinal, client, session, model, observed_at, input_tokens, output_tokens,
                cache_read_tokens, cache_write_tokens, reasoning_tokens, cache_write_1h_tokens,
                dedup_identity, identity_class, provider_hint, service_tier, agent
         FROM source_observations
         WHERE source_id = ?1 AND source_generation = ?2
         ORDER BY ordinal",
    ))?;
    let raw = sqlite(statement.query_map(
        params![source_id, runtime_i64(source_generation)?],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                RawUsage {
                    client: row.get(1)?,
                    session: row.get(2)?,
                    model: row.get(3)?,
                    observed_at: row.get(4)?,
                    input: row.get(5)?,
                    output: row.get(6)?,
                    cache_read: row.get(7)?,
                    cache_write: row.get(8)?,
                    reasoning: row.get(9)?,
                    cache_write_1h: row.get(10)?,
                    dedup_identity: row.get(11)?,
                    provider_hint: row.get(13)?,
                    service_tier: row.get(14)?,
                    agent: row.get(15)?,
                },
                row.get::<_, String>(12)?,
            ))
        },
    ))?
    .collect::<Result<Vec<_>, _>>()
    .map_err(|_| StoreError::Sqlite)?;
    if raw.len() > MAX_CANONICAL_ROWS {
        return Err(StoreError::Sqlite);
    }
    raw.into_iter()
        .enumerate()
        .map(|(expected, (ordinal, raw, identity_class))| {
            if from_i64(ordinal)? as usize != expected
                || (raw.dedup_identity.is_some()) != (identity_class == "global-stable")
                || (raw.dedup_identity.is_none()) != (identity_class == "source-local")
            {
                return Err(StoreError::Sqlite);
            }
            let usage = raw.into_usage()?;
            let mut identity_bytes = 0;
            validate_usage(&usage, &mut identity_bytes).map_err(|_| StoreError::Sqlite)?;
            Ok(usage)
        })
        .collect()
}

fn load_canonical_rows(
    connection: &Connection,
    generation: u64,
    rollups: &Arc<CanonicalSnapshot>,
) -> Result<Arc<CanonicalObservationSnapshot>, StoreError> {
    let mut provenance: BTreeMap<String, Vec<CanonicalProvenanceLink>> = BTreeMap::new();
    {
        let mut statement = sqlite(connection.prepare(
            "SELECT canonical_id, source_id, source_generation, observation_ordinal
             FROM canonical_provenance
             ORDER BY canonical_id, source_id, source_generation, observation_ordinal",
        ))?;
        let raw = sqlite(statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        }))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| StoreError::Sqlite)?;
        for (canonical_id, source_id, source_generation, observation_ordinal) in raw {
            provenance
                .entry(canonical_id)
                .or_default()
                .push(CanonicalProvenanceLink {
                    source_id: SourceId::parse_storage_key(&source_id).ok_or(StoreError::Sqlite)?,
                    source_generation: from_i64(source_generation)?,
                    observation_ordinal: from_i64(observation_ordinal)? as usize,
                });
        }
    }
    {
        let mut statement = sqlite(connection.prepare(
            "SELECT canonical_id, source_id, source_generation, observation_ordinal
             FROM canonical_sqlite_provenance
             ORDER BY canonical_id, source_id, source_generation, observation_ordinal",
        ))?;
        let raw = sqlite(statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        }))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| StoreError::Sqlite)?;
        for (canonical_id, source_id, source_generation, observation_ordinal) in raw {
            provenance
                .entry(canonical_id)
                .or_default()
                .push(CanonicalProvenanceLink {
                    source_id: SourceId::parse_storage_key(&source_id).ok_or(StoreError::Sqlite)?,
                    source_generation: from_i64(source_generation)?,
                    observation_ordinal: from_i64(observation_ordinal)? as usize,
                });
        }
    }

    let raw = {
        let mut statement = sqlite(connection.prepare(
            "SELECT ordinal, canonical_id, client, session, model, observed_at, input_tokens,
                    output_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens,
                    cache_write_1h_tokens, dedup_identity, provider_hint, service_tier, agent,
                    source_state
             FROM canonical_observations ORDER BY ordinal",
        ))?;
        sqlite(statement.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                RawUsage {
                    client: row.get(2)?,
                    session: row.get(3)?,
                    model: row.get(4)?,
                    observed_at: row.get(5)?,
                    input: row.get(6)?,
                    output: row.get(7)?,
                    cache_read: row.get(8)?,
                    cache_write: row.get(9)?,
                    reasoning: row.get(10)?,
                    cache_write_1h: row.get(11)?,
                    dedup_identity: row.get(12)?,
                    provider_hint: row.get(13)?,
                    service_tier: row.get(14)?,
                    agent: row.get(15)?,
                },
                row.get::<_, String>(16)?,
            ))
        }))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| StoreError::Sqlite)?
    };
    if raw.len() > MAX_CANONICAL_ROWS {
        return Err(StoreError::Sqlite);
    }
    let rows = raw
        .into_iter()
        .enumerate()
        .map(|(expected, (ordinal, canonical_id, raw, source_state))| {
            if from_i64(ordinal)? as usize != expected {
                return Err(StoreError::Sqlite);
            }
            let links = provenance.remove(&canonical_id).ok_or(StoreError::Sqlite)?;
            if links.is_empty() {
                return Err(StoreError::Sqlite);
            }
            Ok(CanonicalObservation {
                canonical_id,
                usage: raw.into_usage()?,
                source_state: ObservationSourceState::parse(&source_state)
                    .ok_or(StoreError::Sqlite)?,
                provenance: links.into(),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    if !provenance.is_empty() {
        return Err(StoreError::Sqlite);
    }
    Ok(Arc::new(CanonicalObservationSnapshot {
        generation,
        rows: rows.into(),
        rollups: rollups.clone(),
    }))
}

fn parse_sql_bool(value: i64) -> Result<bool, StoreError> {
    match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(StoreError::Sqlite),
    }
}

fn load_report_items(connection: &Connection) -> Result<Vec<StoredReportItem>, StoreError> {
    let mut statement = sqlite(connection.prepare(
        "SELECT run_id, ordinal, artifact_kind, preservation_state, import_state,
                activation_state, records
         FROM import_report_items ORDER BY run_id, ordinal",
    ))?;
    let raw = sqlite(statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, i64>(6)?,
        ))
    }))?
    .collect::<Result<Vec<_>, _>>()
    .map_err(|_| StoreError::Sqlite)?;
    raw.into_iter()
        .map(
            |(run_id, ordinal, artifact_kind, preservation, import, activation, records)| {
                Ok(StoredReportItem {
                    run_id,
                    ordinal: from_i64(ordinal)? as usize,
                    item: ImportReportItem {
                        artifact_kind,
                        preservation: PreserveState::parse(&preservation)?,
                        import: ImportState::parse(&import)?,
                        activation: ActivationState::parse(&activation)?,
                        records: from_i64(records)?,
                    },
                })
            },
        )
        .collect()
}

fn load_staged_artifacts(connection: &Connection) -> Result<Vec<StoredStagedArtifact>, StoreError> {
    let mut statement = sqlite(connection.prepare(
        "SELECT run_id, ordinal, staging_id, artifact_kind, bytes
         FROM staged_artifacts ORDER BY run_id, ordinal",
    ))?;
    let raw = sqlite(statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, i64>(4)?,
        ))
    }))?
    .collect::<Result<Vec<_>, _>>()
    .map_err(|_| StoreError::Sqlite)?;
    raw.into_iter()
        .map(|(run_id, ordinal, staging_id, artifact_kind, bytes)| {
            Ok(StoredStagedArtifact {
                run_id,
                ordinal: from_i64(ordinal)? as usize,
                artifact: StagedArtifact {
                    staging_id,
                    artifact_kind,
                    bytes: from_i64(bytes)?,
                },
            })
        })
        .collect()
}

fn load_credential_references(
    connection: &Connection,
) -> Result<Vec<StoredCredentialReference>, StoreError> {
    let mut statement = sqlite(connection.prepare(
        "SELECT run_id, credential_id, credential_kind
         FROM credential_references ORDER BY credential_id",
    ))?;
    sqlite(statement.query_map([], |row| {
        Ok(StoredCredentialReference {
            run_id: row.get(0)?,
            reference: CredentialReference {
                credential_id: row.get(1)?,
                credential_kind: row.get(2)?,
            },
        })
    }))?
    .collect::<Result<Vec<_>, _>>()
    .map_err(|_| StoreError::Sqlite)
}

fn load_credential_operations(
    connection: &Connection,
) -> Result<Vec<StoredCredentialOperation>, StoreError> {
    let mut statement = sqlite(connection.prepare(
        "SELECT run_id, operation_id, credential_id, operation, operation_state
         FROM credential_operations ORDER BY operation_id",
    ))?;
    let raw = sqlite(statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
        ))
    }))?
    .collect::<Result<Vec<_>, _>>()
    .map_err(|_| StoreError::Sqlite)?;
    raw.into_iter()
        .map(|(run_id, operation_id, credential_id, operation, state)| {
            Ok(StoredCredentialOperation {
                run_id,
                operation: CredentialOperation {
                    operation_id,
                    credential_id,
                    operation: CredentialOperationKind::parse(&operation)?,
                    state: CredentialOperationState::parse(&state)?,
                },
            })
        })
        .collect()
}

fn load_pending_credential_imports(
    connection: &Connection,
) -> Result<Vec<PendingCredentialImport>, StoreError> {
    let mut statement = sqlite(connection.prepare(
        "SELECT run_id, expected_settings_revision, prepared_at
         FROM pending_credential_imports ORDER BY run_id",
    ))?;
    let raw = sqlite(statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
        ))
    }))?
    .collect::<Result<Vec<_>, _>>()
    .map_err(|_| StoreError::Sqlite)?;
    raw.into_iter()
        .map(|(run_id, expected_settings_revision, prepared_at)| {
            Ok(PendingCredentialImport {
                run_id,
                expected_settings_revision: from_i64(expected_settings_revision)?,
                prepared_at,
            })
        })
        .collect()
}

fn current_generations(transaction: &Transaction<'_>) -> Result<(u64, u64, u64, u64), StoreError> {
    let row = sqlite(
        transaction
            .query_row(
                "SELECT revision, settings_generation, day_generation, pricing_generation
         FROM active_settings WHERE id = 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional(),
    )?;
    match row {
        Some((revision, settings, day, pricing)) => Ok((
            from_i64(revision)?,
            from_i64(settings)?,
            from_i64(day)?,
            from_i64(pricing)?,
        )),
        None => Ok((0, 0, 0, 0)),
    }
}

fn validate_import_capacities(import: &ValidatedImport) -> Result<(), StoreError> {
    if import.sessions.len() > MAX_IMPORT_SESSIONS
        || import.report_items.len() > MAX_REPORT_ITEMS
        || import.staged_artifacts.len() > MAX_STAGED_ARTIFACTS
        || import.credential_references.len() > MAX_CREDENTIAL_REFERENCES
        || import.credential_operations.len() > MAX_CREDENTIAL_OPERATIONS
    {
        return Err(StoreError::InvalidImport);
    }
    Ok(())
}

fn validate_json_document(bytes: &[u8], maximum: usize) -> Result<(), StoreError> {
    if bytes.len() > maximum {
        return Err(StoreError::InvalidImport);
    }
    let value: Value = serde_json::from_slice(bytes).map_err(|_| StoreError::InvalidImport)?;
    if !value.is_object() {
        return Err(StoreError::InvalidImport);
    }
    validate_json_safety(&value)
}

fn validate_json_safety(value: &Value) -> Result<(), StoreError> {
    match value {
        Value::Object(fields) => {
            for (key, value) in fields {
                let normalized = key
                    .chars()
                    .filter(|character| character.is_ascii_alphanumeric())
                    .flat_map(char::to_lowercase)
                    .collect::<String>();
                if normalized.contains("secret")
                    || normalized.contains("password")
                    || normalized.contains("cookie")
                    || normalized.ends_with("apikey")
                    || normalized.ends_with("accesstoken")
                    || normalized.ends_with("refreshtoken")
                    || normalized.ends_with("authtoken")
                    || normalized.ends_with("bearertoken")
                    || normalized.ends_with("token")
                {
                    return Err(StoreError::SecretRejected);
                }
                validate_json_safety(value)?;
            }
        }
        Value::Array(values) => {
            for value in values {
                validate_json_safety(value)?;
            }
        }
        Value::String(value) if is_absolute_path_text(value) => {
            return Err(StoreError::InvalidImport);
        }
        _ => {}
    }
    Ok(())
}

fn is_absolute_path_text(value: &str) -> bool {
    value.starts_with('/')
        || value.starts_with("\\\\")
        || (value.len() >= 3
            && value.as_bytes()[0].is_ascii_alphabetic()
            && value.as_bytes()[1] == b':'
            && matches!(value.as_bytes()[2], b'/' | b'\\'))
}

fn validate_identity(value: &str, maximum: usize) -> Result<(), StoreError> {
    if value.is_empty()
        || value.len() > maximum
        || value
            .chars()
            .any(|character| character.is_control() || matches!(character, '/' | '\\' | ':' | '*'))
    {
        return Err(StoreError::InvalidImport);
    }
    Ok(())
}

fn validate_legacy_key(value: &str) -> Result<(), StoreError> {
    if value.is_empty()
        || value.len() > 1_024
        || value.chars().any(char::is_control)
        || is_absolute_path_text(value)
    {
        return Err(StoreError::InvalidImport);
    }
    Ok(())
}

fn validate_opaque_id(value: &str) -> Result<(), StoreError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        return Err(StoreError::InvalidImport);
    }
    Ok(())
}

fn validate_staging_id(value: &str) -> Result<(), StoreError> {
    if value.is_empty()
        || value.len() > 512
        || value.starts_with(['/', '\\'])
        || value.as_bytes().get(1) == Some(&b':')
    {
        return Err(StoreError::InvalidImport);
    }
    for component in value.split(['/', '\\']) {
        if component.is_empty()
            || component == "."
            || component == ".."
            || !component
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(StoreError::InvalidImport);
        }
    }
    Ok(())
}

fn checked_increment(value: u64) -> Result<u64, StoreError> {
    value.checked_add(1).ok_or(StoreError::InvalidImport)
}

fn to_i64(value: u64) -> Result<i64, StoreError> {
    value.try_into().map_err(|_| StoreError::InvalidImport)
}

fn from_i64(value: i64) -> Result<u64, StoreError> {
    value.try_into().map_err(|_| StoreError::Sqlite)
}

fn now_seconds() -> Result<i64, StoreError> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| StoreError::Io)?
        .as_secs();
    to_i64(seconds)
}

fn unique_suffix() -> Result<String, StoreError> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| StoreError::Io)?
        .as_nanos();
    let sequence = UNIQUE_ID.fetch_add(1, Ordering::Relaxed);
    Ok(format!("{nanos:020}-{sequence:010}"))
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), StoreError> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), StoreError> {
    Ok(())
}
