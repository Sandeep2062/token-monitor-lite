//! One-source bounded reconciliation. Run steps on the collector worker, never
//! on the UI thread. Only a successfully captured framed prefix is published;
//! incomplete final records remain explicit and deferred. This is not source
//! discovery, cross-file deduplication, period aggregation or archive retention.

use crate::accounting::{SessionKey, TokenCounts, TokenDelta};
use crate::parsers::{ReaderError, ResponseUsage, claude::ClaudeReader, codex::CodexReader};
use crate::source_inputs::{ArchivePeriod, ArchiveSession, SourceInput, SqliteSourceError};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

const PARSER_PIN: &str = "d3df620b2cdd9502c70aececdbc6ece61fe26fc9";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SourceFormat {
    ClaudeCli,
    CodexCli,
    SessionArchiveSqlite,
    CodexLocalUsageSqlite,
}

impl SourceFormat {
    pub fn client(self) -> &'static str {
        match self {
            Self::ClaudeCli => "claude",
            Self::CodexCli | Self::CodexLocalUsageSqlite => "codex",
            Self::SessionArchiveSqlite => "mixed",
        }
    }

    pub(crate) fn storage_name(self) -> &'static str {
        match self {
            Self::ClaudeCli => "claude-jsonl",
            Self::CodexCli => "codex-jsonl",
            Self::SessionArchiveSqlite => "session-archive-sqlite",
            Self::CodexLocalUsageSqlite => "codex-local-usage-sqlite",
        }
    }

    pub(crate) fn parse_storage_name(value: &str) -> Option<Self> {
        match value {
            "claude-jsonl" => Some(Self::ClaudeCli),
            "codex-jsonl" => Some(Self::CodexCli),
            "session-archive-sqlite" => Some(Self::SessionArchiveSqlite),
            "codex-local-usage-sqlite" => Some(Self::CodexLocalUsageSqlite),
            _ => None,
        }
    }

    pub(crate) fn is_sqlite(self) -> bool {
        matches!(
            self,
            Self::SessionArchiveSqlite | Self::CodexLocalUsageSqlite
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RetainedPeriod {
    Today,
    Month,
    AllTime,
}

impl RetainedPeriod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Today => "today",
            Self::Month => "month",
            Self::AllTime => "allTime",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "today" => Some(Self::Today),
            "month" => Some(Self::Month),
            "allTime" => Some(Self::AllTime),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetainedModelUsage {
    pub model: String,
    pub total: u64,
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetainedPeriodUsage {
    pub client: String,
    pub session: String,
    pub period: RetainedPeriod,
    pub captured_at_ms: i64,
    pub day: Option<String>,
    pub month: Option<String>,
    pub total: u64,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: u64,
    pub unclassified: u64,
    pub components_complete: bool,
    pub models_complete: bool,
    pub models: Arc<[RetainedModelUsage]>,
}

/// Versioned fixed-size backend identity. Exact platform path bytes feed the
/// digest, but neither those bytes nor a display path are retained here.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SourceId([u8; 16]);

impl SourceId {
    const PREFIX: &'static str = "source-v1-";

    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    pub fn for_path(format: SourceFormat, path: &Path) -> Self {
        let normalized = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let mut left = 0xcbf29ce484222325_u64;
        let mut right = 0x84222325cbf29ce4_u64;
        hash_bytes(&mut left, b"token-monitor-lite/source-id/v1\0");
        hash_bytes(&mut right, b"token-monitor-lite/source-id/v1/secondary\0");
        hash_bytes(&mut left, format.storage_name().as_bytes());
        hash_bytes(&mut right, format.storage_name().as_bytes());
        hash_bytes(&mut left, &[0]);
        hash_bytes(&mut right, &[0]);
        hash_path(&mut left, &normalized);
        hash_path(&mut right, &normalized);
        let mut bytes = [0_u8; 16];
        bytes[..8].copy_from_slice(&left.to_be_bytes());
        bytes[8..].copy_from_slice(&right.to_be_bytes());
        Self(bytes)
    }

    pub fn as_storage_key(self) -> String {
        let mut value = String::with_capacity(Self::PREFIX.len() + 32);
        value.push_str(Self::PREFIX);
        for byte in self.0 {
            use std::fmt::Write as _;
            let _ = write!(value, "{byte:02x}");
        }
        value
    }

    pub(crate) fn parse_storage_key(value: &str) -> Option<Self> {
        let hex = value.strip_prefix(Self::PREFIX)?;
        if hex.len() != 32 {
            return None;
        }
        let mut bytes = [0_u8; 16];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).ok()?;
        }
        Some(Self(bytes))
    }
}

impl fmt::Debug for SourceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SourceId([redacted])")
    }
}

impl fmt::Display for SourceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.as_storage_key())
    }
}

fn hash_bytes(state: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *state ^= u64::from(*byte);
        *state = state.wrapping_mul(0x100000001b3);
    }
}

#[cfg(unix)]
fn hash_path(state: &mut u64, path: &Path) {
    use std::os::unix::ffi::OsStrExt as _;
    hash_bytes(state, path.as_os_str().as_bytes());
}

#[cfg(windows)]
fn hash_path(state: &mut u64, path: &Path) {
    use std::os::windows::ffi::OsStrExt as _;
    for unit in path.as_os_str().encode_wide() {
        hash_bytes(state, &unit.to_le_bytes());
    }
}

#[cfg(not(any(unix, windows)))]
fn hash_path(state: &mut u64, path: &Path) {
    hash_bytes(state, path.as_os_str().as_encoded_bytes());
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ObservationSourceState {
    Live,
    Retained,
}

impl ObservationSourceState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Retained => "retained",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "live" => Some(Self::Live),
            "retained" => Some(Self::Retained),
            _ => None,
        }
    }
}

enum ResponseReader {
    Claude(Box<ClaudeReader>),
    Codex(Box<CodexReader>),
}

impl ResponseReader {
    fn ingest(&mut self, record: &[u8]) -> Result<(), ReaderError> {
        match self {
            Self::Claude(reader) => reader.ingest(record),
            Self::Codex(reader) => reader.ingest(record),
        }
    }

    fn snapshot(&self) -> Vec<ResponseUsage> {
        match self {
            Self::Claude(reader) => reader.snapshot(),
            Self::Codex(reader) => reader.snapshot(),
        }
    }

    fn finish(&mut self) -> Result<(), ReaderError> {
        match self {
            Self::Claude(_) => Ok(()),
            Self::Codex(reader) => reader.finish(),
        }
    }

    fn has_supported_usage(&self, rows: &[ResponseUsage]) -> bool {
        match self {
            Self::Claude(_) => !rows.is_empty(),
            Self::Codex(reader) => reader.has_supported_usage(),
        }
    }
}

/// Path-free immutable observations suitable for persistence and rehydration.
#[derive(Clone, PartialEq, Eq)]
pub struct SourceSnapshot {
    pub source_id: SourceId,
    pub format: SourceFormat,
    pub parser_pin: String,
    pub generation: u64,
    pub rows: Arc<[ResponseUsage]>,
    pub retained: Arc<[RetainedPeriodUsage]>,
    /// Exact components for request-like rows only, not a sum across
    /// overlapping retained periods.
    pub totals: TokenCounts,
    pub cache_write_1h: u64,
    pub incomplete_tail: bool,
    pub has_supported_usage: bool,
    pub source_state: ObservationSourceState,
}

impl fmt::Debug for SourceSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SourceSnapshot")
            .field("source_id", &self.source_id)
            .field("format", &self.format)
            .field("generation", &self.generation)
            .field("observations", &self.rows.len())
            .field("retained_periods", &self.retained.len())
            .field("incomplete_tail", &self.incomplete_tail)
            .field("source_state", &self.source_state)
            .finish()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum SourceProgress {
    Pending,
    Published { delta: TokenDelta },
    Unchanged,
}

#[derive(Debug)]
pub struct SourceStep {
    pub bytes_read: usize,
    pub physical_bytes_read: usize,
    pub decoded_bytes_read: usize,
    pub progress: SourceProgress,
}

/// Errors deliberately omit paths and captured source values.
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("source read unavailable ({0:?})")]
    Io(io::ErrorKind),
    #[error(transparent)]
    Reader(#[from] ReaderError),
    #[error("source SQLite input unavailable")]
    Sqlite(#[from] SqliteSourceError),
    #[error("source component aggregation overflow")]
    Overflow,
    #[error("source generation overflow")]
    GenerationOverflow,
}

impl From<io::Error> for SourceError {
    fn from(error: io::Error) -> Self {
        Self::Io(error.kind())
    }
}

pub struct SourceReader {
    source_id: SourceId,
    key: SessionKey,
    format: SourceFormat,
    input: SourceInput,
    candidate: Option<ResponseReader>,
    last_good: Option<Arc<SourceSnapshot>>,
    dirty: bool,
}

impl SourceReader {
    pub fn new(path: PathBuf, format: SourceFormat, session: impl Into<String>) -> Self {
        let source_id = SourceId::for_path(format, &path);
        Self::with_source_input(
            source_id,
            format,
            session,
            SourceInput::for_path(format, path),
        )
    }

    pub(crate) fn with_source_input(
        source_id: SourceId,
        format: SourceFormat,
        session: impl Into<String>,
        input: SourceInput,
    ) -> Self {
        Self {
            source_id,
            key: SessionKey::new(format.client(), session),
            format,
            input,
            candidate: None,
            last_good: None,
            dirty: true,
        }
    }

    pub(crate) fn restore_last_good(&mut self, snapshot: Arc<SourceSnapshot>) -> bool {
        if snapshot.source_id != self.source_id || snapshot.format != self.format {
            return false;
        }
        self.last_good = Some(snapshot);
        true
    }

    pub fn last_good(&self) -> Option<Arc<SourceSnapshot>> {
        self.last_good.clone()
    }

    /// Invalidate only working state. Keep the published last-good source
    /// while a replacement is being read or if that replacement fails.
    pub fn request_full_reconciliation(&mut self) {
        self.input.reset();
        self.candidate = None;
        self.dirty = true;
    }

    /// Admit one bounded JSONL batch. At EOF, materialization/checked rollup
    /// is bounded by reader response/identity caps. Calls are serial. An error
    /// forces a full retry, so successfully ingested prefixes cannot leak into
    /// the next generation after a malformed/unsupported/over-budget record.
    pub fn step(&mut self) -> Result<SourceStep, SourceError> {
        let result = self.read_step();
        if result.is_err() {
            self.request_full_reconciliation();
        }
        result
    }

    fn read_step(&mut self) -> Result<SourceStep, SourceError> {
        if self.format.is_sqlite() {
            return self.read_sqlite_step();
        }
        // A candidate spanning several batches must not mix versions. Growth
        // between batches is ambiguous too: restart rather than admitting a
        // partly old, partly new full-source observation.
        if self.dirty && self.candidate.is_some() && !self.input.is_current()? {
            self.request_full_reconciliation();
        }
        let batch = self.input.read_batch()?;
        if batch.reset || self.candidate.is_none() {
            self.candidate = Some(self.new_response_reader());
            self.dirty = true;
        }
        self.dirty |= batch.decoded_bytes_read != 0 || !batch.records.is_empty();
        let candidate = self
            .candidate
            .as_mut()
            .expect("candidate initialized above");
        for record in &batch.records {
            candidate.ingest(record)?;
        }
        if batch.more_available {
            return Ok(SourceStep {
                bytes_read: batch.physical_bytes_read,
                physical_bytes_read: batch.physical_bytes_read,
                decoded_bytes_read: batch.decoded_bytes_read,
                progress: SourceProgress::Pending,
            });
        }
        if !self.input.is_current()? {
            self.request_full_reconciliation();
            return Ok(SourceStep {
                bytes_read: batch.physical_bytes_read,
                physical_bytes_read: batch.physical_bytes_read,
                decoded_bytes_read: batch.decoded_bytes_read,
                progress: SourceProgress::Pending,
            });
        }
        if !self.dirty {
            return Ok(SourceStep {
                bytes_read: batch.physical_bytes_read,
                physical_bytes_read: batch.physical_bytes_read,
                decoded_bytes_read: batch.decoded_bytes_read,
                progress: SourceProgress::Unchanged,
            });
        }
        candidate.finish()?;
        let rows = candidate.snapshot();
        let mut totals = TokenCounts::default();
        let mut cache_write_1h = 0_u64;
        for row in &rows {
            totals = totals
                .checked_add(row.tokens)
                .map_err(|_| SourceError::Overflow)?;
            cache_write_1h = cache_write_1h
                .checked_add(row.cache_write_1h)
                .ok_or(SourceError::Overflow)?;
        }
        // Materialization may take time; reject metadata changes during it.
        if !self.input.is_current()? {
            self.request_full_reconciliation();
            return Ok(SourceStep {
                bytes_read: batch.physical_bytes_read,
                physical_bytes_read: batch.physical_bytes_read,
                decoded_bytes_read: batch.decoded_bytes_read,
                progress: SourceProgress::Pending,
            });
        }
        let generation = self
            .last_good
            .as_ref()
            .map_or(0, |snapshot| snapshot.generation)
            .checked_add(1)
            .ok_or(SourceError::GenerationOverflow)?;
        let delta = totals.difference(
            self.last_good
                .as_ref()
                .map_or(TokenCounts::default(), |snapshot| snapshot.totals),
        );
        let has_supported_usage = candidate.has_supported_usage(&rows);
        self.last_good = Some(Arc::new(SourceSnapshot {
            source_id: self.source_id,
            format: self.format,
            parser_pin: PARSER_PIN.to_owned(),
            generation,
            rows: rows.into(),
            retained: Arc::from([]),
            totals,
            cache_write_1h,
            incomplete_tail: batch.incomplete_tail,
            has_supported_usage,
            source_state: ObservationSourceState::Live,
        }));
        self.dirty = false;
        Ok(SourceStep {
            bytes_read: batch.physical_bytes_read,
            physical_bytes_read: batch.physical_bytes_read,
            decoded_bytes_read: batch.decoded_bytes_read,
            progress: SourceProgress::Published { delta },
        })
    }

    fn new_response_reader(&self) -> ResponseReader {
        match self.format {
            SourceFormat::ClaudeCli => {
                let path = self.input.source_path();
                let client = path
                    .map(claude_client_from_path)
                    .unwrap_or_else(|| "claude".to_owned());
                let fallback = path.and_then(file_mtime_ms);
                ResponseReader::Claude(Box::new(
                    ClaudeReader::with_client(client, &self.key.session)
                        .with_fallback_timestamp(fallback),
                ))
            }
            SourceFormat::CodexCli => {
                let fallback = self.input.source_path().and_then(file_mtime_ms);
                ResponseReader::Codex(Box::new(
                    CodexReader::new(&self.key.session).with_fallback_timestamp(fallback),
                ))
            }
            SourceFormat::SessionArchiveSqlite | SourceFormat::CodexLocalUsageSqlite => {
                unreachable!("SQLite sources do not use text readers")
            }
        }
    }

    fn read_sqlite_step(&mut self) -> Result<SourceStep, SourceError> {
        let Some(data) = self.input.read_sqlite()? else {
            return Err(SourceError::Reader(ReaderError::Unsupported(
                "SQLite source input missing",
            )));
        };
        let (rows, retained): (Arc<[ResponseUsage]>, Arc<[RetainedPeriodUsage]>) = match data {
            crate::source_inputs::SqliteInputData::CodexLocalUsage(rows) => (rows, Arc::from([])),
            crate::source_inputs::SqliteInputData::SessionArchive(snapshot) => {
                let mut retained = Vec::new();
                for session in snapshot.sessions.iter() {
                    retained.extend(retained_periods(session));
                }
                (Arc::from([]), Arc::from(retained))
            }
        };
        let mut totals = TokenCounts::default();
        let mut cache_write_1h = 0_u64;
        for row in rows.iter() {
            totals = totals
                .checked_add(row.tokens)
                .map_err(|_| SourceError::Overflow)?;
            cache_write_1h = cache_write_1h
                .checked_add(row.cache_write_1h)
                .ok_or(SourceError::Overflow)?;
        }
        let generation = self
            .last_good
            .as_ref()
            .map_or(0, |snapshot| snapshot.generation)
            .checked_add(1)
            .ok_or(SourceError::GenerationOverflow)?;
        let delta = totals.difference(
            self.last_good
                .as_ref()
                .map_or(TokenCounts::default(), |snapshot| snapshot.totals),
        );
        let has_supported_usage = !rows.is_empty() || !retained.is_empty();
        self.last_good = Some(Arc::new(SourceSnapshot {
            source_id: self.source_id,
            format: self.format,
            parser_pin: PARSER_PIN.to_owned(),
            generation,
            rows,
            retained,
            totals,
            cache_write_1h,
            incomplete_tail: false,
            has_supported_usage,
            source_state: ObservationSourceState::Live,
        }));
        self.dirty = false;
        Ok(SourceStep {
            bytes_read: 0,
            physical_bytes_read: 0,
            decoded_bytes_read: 0,
            progress: SourceProgress::Published { delta },
        })
    }
}

fn claude_client_from_path(path: &Path) -> String {
    let parts = path
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect::<Vec<_>>();
    for window in parts.windows(2) {
        if window[0] == ".cc-mirror" {
            let variant = window[1]
                .chars()
                .map(|character| {
                    if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                        character
                    } else {
                        '-'
                    }
                })
                .take(64)
                .collect::<String>();
            if !variant.is_empty() {
                return format!("cc-mirror/{variant}");
            }
        }
    }
    "claude".to_owned()
}

fn file_mtime_ms(path: &Path) -> Option<i64> {
    fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
}

fn retained_periods(session: &ArchiveSession) -> Vec<RetainedPeriodUsage> {
    session
        .periods
        .iter()
        .map(|period| RetainedPeriodUsage {
            client: session.client.clone(),
            session: session.session.clone(),
            period: match period.period {
                ArchivePeriod::Today => RetainedPeriod::Today,
                ArchivePeriod::Month => RetainedPeriod::Month,
                ArchivePeriod::AllTime => RetainedPeriod::AllTime,
            },
            captured_at_ms: period.captured_at_ms,
            day: period.day.clone(),
            month: period.month.clone(),
            total: period.total,
            input: period.input,
            output: period.output,
            cache_read: period.cache_read,
            cache_write: period.cache_write,
            reasoning: period.reasoning,
            unclassified: period.unclassified,
            components_complete: period.components_complete,
            models_complete: period.models_complete,
            models: period
                .models
                .iter()
                .map(|model| RetainedModelUsage {
                    model: model.model.clone(),
                    total: model.total,
                    input: model.input,
                    cache_read: model.cache_read,
                    cache_write: model.cache_write,
                    output: model.output,
                })
                .collect::<Vec<_>>()
                .into(),
        })
        .collect()
}
