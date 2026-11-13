//! Closed, bounded source-input adapters. Adapters own physical I/O and source
//! mutation fences; client parsers receive only complete normalized records.
//! The SQLite archive adapter returns retained period data separately; it is
//! intentionally not folded into the request-like canonical runtime yet.

use crate::accounting::{SessionKey, TokenCounts};
use crate::parsers::jsonl::{JsonlCursor, JsonlFramer, MAX_BATCH_BYTES, SourceStamp};
use crate::parsers::{CostStatus, ResponseUsage};
use crate::runtime::MonitoredSource;
use crate::sources::SourceFormat;
use chrono::DateTime;
use rusqlite::{Connection, ErrorCode, OpenFlags, OptionalExtension, params};
use ruzstd::decoding::errors::FrameDecoderError;
use ruzstd::decoding::{BlockDecodingStrategy, FrameDecoder};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

pub const MAX_COMPRESSED_BYTES: u64 = 256 * 1024 * 1024;
pub const MAX_DECODED_BYTES: u64 = 1024 * 1024 * 1024;
pub const MAX_ZSTD_WINDOW_BYTES: u64 = 1 << 26;
pub const MAX_ARCHIVE_ROWS: usize = 10_000;
pub const MAX_ARCHIVE_ENTRY_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_LOCAL_USAGE_ROWS: usize = 16_384;
const SQLITE_PAGE_ROWS: i64 = 1_024;
const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_millis(250);
const MAX_SQLITE_SNAPSHOT_BYTES: u64 = 512 * 1024 * 1024;
static NEXT_SQLITE_SNAPSHOT: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SqliteSourceRole {
    SessionArchive,
    CodexLocalUsage,
}

pub fn classify_sqlite_source(path: &Path) -> Option<SqliteSourceRole> {
    match path.file_name()?.to_str()? {
        "session-usage-archive.sqlite" => Some(SqliteSourceRole::SessionArchive),
        "codex-local-usage.sqlite" => Some(SqliteSourceRole::CodexLocalUsage),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArchivePeriod {
    Today,
    Month,
    AllTime,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchiveModelUsage {
    pub model: String,
    pub total: u64,
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchivePeriodUsage {
    pub period: ArchivePeriod,
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
    pub models: Arc<[ArchiveModelUsage]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchiveSession {
    pub client: String,
    pub session: String,
    pub captured_at_ms: i64,
    pub day: Option<String>,
    pub month: Option<String>,
    pub periods: Arc<[ArchivePeriodUsage]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionArchiveSnapshot {
    pub revision: u64,
    pub pruned_day: Option<String>,
    pub pruned_month: Option<String>,
    pub sessions: Arc<[ArchiveSession]>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SqliteSourceError {
    #[error("sqlite source unavailable")]
    Unavailable,
    #[error("sqlite source I/O failed ({0:?})")]
    Io(io::ErrorKind),
    #[error("sqlite source busy or locked")]
    Busy,
    #[error("sqlite source schema is unsupported")]
    Schema,
    #[error("sqlite source data is invalid")]
    Invalid,
    #[error("sqlite source capacity exceeded")]
    Capacity,
    #[error("sqlite source changed during read")]
    Changed,
    #[error("sqlite source identity conflicts")]
    Conflict,
}

impl From<io::Error> for SqliteSourceError {
    fn from(error: io::Error) -> Self {
        Self::Io(error.kind())
    }
}

impl From<rusqlite::Error> for SqliteSourceError {
    fn from(error: rusqlite::Error) -> Self {
        match error {
            rusqlite::Error::SqliteFailure(code, _)
                if matches!(
                    code.code,
                    ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked
                ) =>
            {
                Self::Busy
            }
            rusqlite::Error::SqliteFailure(code, _)
                if matches!(
                    code.code,
                    ErrorCode::CannotOpen
                        | ErrorCode::PermissionDenied
                        | ErrorCode::ReadOnly
                        | ErrorCode::NotADatabase
                ) =>
            {
                Self::Unavailable
            }
            _ => Self::Invalid,
        }
    }
}

pub struct SessionArchiveAdapter {
    path: PathBuf,
}

impl SessionArchiveAdapter {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub fn read(&self) -> Result<SessionArchiveSnapshot, SqliteSourceError> {
        read_session_archive(&self.path)
    }
}

pub struct CodexLocalUsageAdapter {
    path: PathBuf,
}

impl CodexLocalUsageAdapter {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub fn read(&self) -> Result<Arc<[ResponseUsage]>, SqliteSourceError> {
        read_codex_local_usage(&self.path)
    }
}

const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];
const ZSTD_SKIPPABLE_MIN: u32 = 0x184D_2A50;
const ZSTD_SKIPPABLE_MAX: u32 = 0x184D_2A5F;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputAdapterStatus {
    OrdinaryJsonl,
    CompressedCodexJsonl,
    SessionArchiveSqlite,
    CodexLocalUsageSqlite,
}

pub(crate) enum SqliteInputData {
    SessionArchive(SessionArchiveSnapshot),
    CodexLocalUsage(Arc<[ResponseUsage]>),
}

pub struct InputBatch {
    pub records: Vec<Vec<u8>>,
    pub physical_bytes_read: usize,
    pub decoded_bytes_read: usize,
    pub reset: bool,
    pub incomplete_tail: bool,
    pub more_available: bool,
}

impl fmt::Debug for InputBatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InputBatch")
            .field("records", &self.records.len())
            .field("physical_bytes_read", &self.physical_bytes_read)
            .field("decoded_bytes_read", &self.decoded_bytes_read)
            .field("reset", &self.reset)
            .field("incomplete_tail", &self.incomplete_tail)
            .field("more_available", &self.more_available)
            .finish()
    }
}

/// Public so the enum remains an ordinary closed enum, but intentionally has
/// no public constructor or path accessor. Production construction is fenced
/// through `MonitoredSource`.
pub struct OrdinaryJsonlInput {
    path: PathBuf,
    cursor: JsonlCursor,
}

pub struct CompressedJsonlInput {
    path: PathBuf,
    state: Option<CompressedState>,
    framer: JsonlFramer,
    buffer: Box<[u8]>,
    decoded_total: u64,
    decoded_limit: u64,
    reset_pending: bool,
}

struct CompressedState {
    stream: CompressedStream<BoundedReader<File>>,
    stamp: SourceStamp,
}

struct BoundedReader<R> {
    inner: R,
    bytes: u64,
    limit: u64,
}

struct CompressedStream<R> {
    reader: R,
    frame: FrameDecoder,
    frame_open: bool,
    seen_standard_frame: bool,
    finished: bool,
}

pub enum SourceInput {
    OrdinaryJsonl(OrdinaryJsonlInput),
    CompressedJsonl(Box<CompressedJsonlInput>),
    SessionArchive(SessionArchiveAdapter),
    CodexLocalUsage(CodexLocalUsageAdapter),
}

impl SourceInput {
    pub fn from_monitored_source(source: &MonitoredSource) -> Self {
        Self::for_path(source.format, source.path.clone())
    }

    pub(crate) fn for_path(format: SourceFormat, path: PathBuf) -> Self {
        match format {
            SourceFormat::SessionArchiveSqlite => {
                Self::SessionArchive(SessionArchiveAdapter::new(path))
            }
            SourceFormat::CodexLocalUsageSqlite => {
                Self::CodexLocalUsage(CodexLocalUsageAdapter::new(path))
            }
            SourceFormat::CodexCli if is_compressed_source_path(&path) => {
                Self::compressed_jsonl(path)
            }
            _ => Self::ordinary_jsonl(path),
        }
    }

    /// Discovery validation and focused source-reader tests occur before a
    /// `MonitoredSource` exists. Runtime monitoring must use the public typed
    /// constructor above.
    pub(crate) fn ordinary_jsonl(path: PathBuf) -> Self {
        Self::OrdinaryJsonl(OrdinaryJsonlInput {
            path,
            cursor: JsonlCursor::default(),
        })
    }

    pub(crate) fn compressed_jsonl(path: PathBuf) -> Self {
        Self::CompressedJsonl(Box::new(CompressedJsonlInput {
            path,
            state: None,
            framer: JsonlFramer::default(),
            buffer: vec![0; MAX_BATCH_BYTES].into_boxed_slice(),
            decoded_total: 0,
            decoded_limit: MAX_DECODED_BYTES,
            reset_pending: true,
        }))
    }

    pub fn status(&self) -> InputAdapterStatus {
        match self {
            Self::OrdinaryJsonl(_) => InputAdapterStatus::OrdinaryJsonl,
            Self::CompressedJsonl(_) => InputAdapterStatus::CompressedCodexJsonl,
            Self::SessionArchive(_) => InputAdapterStatus::SessionArchiveSqlite,
            Self::CodexLocalUsage(_) => InputAdapterStatus::CodexLocalUsageSqlite,
        }
    }

    pub fn reset(&mut self) {
        match self {
            Self::OrdinaryJsonl(input) => input.cursor.reset(),
            Self::CompressedJsonl(input) => input.reset(),
            Self::SessionArchive(_) | Self::CodexLocalUsage(_) => {}
        }
    }

    pub fn is_current(&self) -> io::Result<bool> {
        match self {
            Self::OrdinaryJsonl(input) => input.cursor.is_current(&input.path),
            Self::CompressedJsonl(input) => input.is_current(),
            Self::SessionArchive(_) | Self::CodexLocalUsage(_) => Ok(true),
        }
    }

    pub(crate) fn read_sqlite(&self) -> Result<Option<SqliteInputData>, SqliteSourceError> {
        match self {
            Self::SessionArchive(adapter) => {
                Ok(Some(SqliteInputData::SessionArchive(adapter.read()?)))
            }
            Self::CodexLocalUsage(adapter) => {
                Ok(Some(SqliteInputData::CodexLocalUsage(adapter.read()?)))
            }
            Self::OrdinaryJsonl(_) | Self::CompressedJsonl(_) => Ok(None),
        }
    }

    pub(crate) fn source_path(&self) -> Option<&Path> {
        match self {
            Self::OrdinaryJsonl(input) => Some(&input.path),
            Self::CompressedJsonl(input) => Some(&input.path),
            Self::SessionArchive(adapter) => Some(adapter.path()),
            Self::CodexLocalUsage(adapter) => Some(adapter.path()),
        }
    }

    pub fn read_batch(&mut self) -> io::Result<InputBatch> {
        match self {
            Self::OrdinaryJsonl(input) => {
                let batch = input.cursor.read_batch(&input.path)?;
                Ok(InputBatch {
                    records: batch.records,
                    physical_bytes_read: batch.bytes_read,
                    decoded_bytes_read: batch.bytes_read,
                    reset: batch.reset,
                    incomplete_tail: batch.incomplete_tail,
                    more_available: batch.more_available,
                })
            }
            Self::CompressedJsonl(input) => input.read_batch(),
            Self::SessionArchive(_) | Self::CodexLocalUsage(_) => {
                Err(invalid("sqlite input has no JSONL batch"))
            }
        }
    }
}

impl fmt::Debug for SourceInput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SourceInput")
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

impl CompressedJsonlInput {
    fn reset(&mut self) {
        self.state = None;
        self.framer.reset();
        self.decoded_total = 0;
        self.reset_pending = true;
    }

    fn is_current(&self) -> io::Result<bool> {
        let Some(state) = &self.state else {
            return Ok(false);
        };
        current_stamp(&self.path).map(|stamp| stamp == state.stamp)
    }

    fn read_batch(&mut self) -> io::Result<InputBatch> {
        if self.state.is_none() {
            self.state = Some(CompressedState::open(&self.path)?);
        }
        let state = self.state.as_mut().expect("compressed state opened above");
        let physical_before = state.stream.reader.bytes;
        let decoded_before = self.decoded_total;

        let framed = if self.framer.has_buffered() {
            self.framer.admit(&[])?
        } else {
            let remaining = self.decoded_limit.saturating_sub(self.decoded_total);
            if remaining == 0 {
                let mut probe = [0_u8; 1];
                let decoded = state.stream.fill(&mut probe)?;
                if decoded != 0 {
                    return Err(invalid("decoded zstd source exceeds 1 GiB limit"));
                }
                self.framer.admit(&[])?
            } else {
                let limit = usize::try_from(remaining.min(MAX_BATCH_BYTES as u64))
                    .unwrap_or(MAX_BATCH_BYTES);
                let decoded = state.stream.fill(&mut self.buffer[..limit])?;
                self.decoded_total += decoded as u64;
                self.framer.admit(&self.buffer[..decoded])?
            }
        };

        let physical_after = state.stream.reader.bytes;
        let physical = physical_after - physical_before;
        let more_available = framed.more_buffered || !state.stream.finished;
        let reset = self.reset_pending;
        self.reset_pending = false;
        Ok(InputBatch {
            records: framed.records,
            physical_bytes_read: usize::try_from(physical).unwrap_or(usize::MAX),
            decoded_bytes_read: usize::try_from(self.decoded_total - decoded_before)
                .unwrap_or(usize::MAX),
            reset,
            incomplete_tail: framed.incomplete_tail,
            more_available,
        })
    }
}

impl CompressedState {
    fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "compressed source is not a file",
            ));
        }
        if metadata.len() > MAX_COMPRESSED_BYTES {
            return Err(invalid("compressed source exceeds 256 MiB limit"));
        }
        let stamp = SourceStamp::new(path, &metadata);
        let reader = BoundedReader {
            inner: file,
            bytes: 0,
            limit: MAX_COMPRESSED_BYTES,
        };
        Ok(Self {
            stream: CompressedStream::new(reader),
            stamp,
        })
    }
}

impl<R: Read> Read for BoundedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.bytes < self.limit {
            let max = usize::try_from((self.limit - self.bytes).min(buf.len() as u64))
                .unwrap_or(buf.len());
            let read = self.inner.read(&mut buf[..max])?;
            self.bytes += read as u64;
            return Ok(read);
        }
        let mut probe = [0_u8; 1];
        if self.inner.read(&mut probe)? == 0 {
            Ok(0)
        } else {
            self.bytes += 1;
            Err(invalid("compressed source exceeds 256 MiB limit"))
        }
    }
}

impl<R: Read> CompressedStream<R> {
    fn new(reader: R) -> Self {
        let mut frame = FrameDecoder::new();
        frame.set_max_window_size(MAX_ZSTD_WINDOW_BYTES);
        Self {
            reader,
            frame,
            frame_open: false,
            seen_standard_frame: false,
            finished: false,
        }
    }

    fn fill(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() || self.finished {
            return Ok(0);
        }
        let mut written = 0;
        loop {
            if !self.frame_open && !self.open_next_frame()? {
                self.finished = true;
                break;
            }
            while written < out.len() && self.frame.can_collect() > 0 {
                let read = self.frame.read(&mut out[written..])?;
                if read == 0 {
                    break;
                }
                written += read;
            }
            if written == out.len() {
                break;
            }
            if self.frame.is_finished() {
                if self.frame.can_collect() != 0 {
                    return Err(invalid("zstd frame output could not be drained"));
                }
                self.finish_frame()?;
                self.frame_open = false;
                continue;
            }
            self.frame
                .decode_blocks(&mut self.reader, BlockDecodingStrategy::UptoBlocks(1))
                .map_err(frame_error)?;
        }
        Ok(written)
    }

    fn open_next_frame(&mut self) -> io::Result<bool> {
        loop {
            let Some(magic) = read_magic(&mut self.reader)? else {
                if !self.seen_standard_frame {
                    return Err(invalid("compressed source has no standard zstd frame"));
                }
                return Ok(false);
            };
            let magic_number = u32::from_le_bytes(magic);
            if (ZSTD_SKIPPABLE_MIN..=ZSTD_SKIPPABLE_MAX).contains(&magic_number) {
                let mut length = [0_u8; 4];
                self.reader
                    .read_exact(&mut length)
                    .map_err(|_| invalid("truncated zstd skippable frame"))?;
                let length = u32::from_le_bytes(length);
                let skipped = io::copy(
                    &mut self.reader.by_ref().take(u64::from(length)),
                    &mut io::sink(),
                )
                .map_err(|_| invalid("truncated zstd skippable frame"))?;
                if skipped != u64::from(length) {
                    return Err(invalid("truncated zstd skippable frame"));
                }
                continue;
            }
            if magic != ZSTD_MAGIC {
                return Err(invalid("unsupported zstd frame magic"));
            }
            let mut descriptor = [0_u8; 1];
            self.reader
                .read_exact(&mut descriptor)
                .map_err(|_| invalid("truncated zstd frame header"))?;
            if descriptor[0] & 0x08 != 0 {
                return Err(invalid("reserved zstd frame descriptor bit"));
            }
            let mut header = [0_u8; 5];
            header[..4].copy_from_slice(&magic);
            header[4] = descriptor[0];
            let prefix = Cursor::new(header);
            let mut chained = prefix.chain(&mut self.reader);
            self.frame.init(&mut chained).map_err(frame_error)?;
            self.frame_open = true;
            self.seen_standard_frame = true;
            return Ok(true);
        }
    }

    fn finish_frame(&mut self) -> io::Result<()> {
        if let Some(expected) = self.frame.get_checksum_from_data() {
            let calculated = self
                .frame
                .get_calculated_checksum()
                .ok_or_else(|| invalid("zstd checksum unavailable"))?;
            if expected != calculated {
                return Err(invalid("zstd checksum mismatch"));
            }
        }
        Ok(())
    }
}

fn read_magic(reader: &mut impl Read) -> io::Result<Option<[u8; 4]>> {
    let mut magic = [0_u8; 4];
    if reader.read(&mut magic[..1])? == 0 {
        return Ok(None);
    }
    reader
        .read_exact(&mut magic[1..])
        .map_err(|_| invalid("truncated zstd frame magic"))?;
    Ok(Some(magic))
}

fn current_stamp(path: &Path) -> io::Result<SourceStamp> {
    let file = File::open(path)?;
    let metadata = file.metadata()?;
    Ok(SourceStamp::new(path, &metadata))
}

pub(crate) fn is_compressed_source_path(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let Some((stem, extension)) = name.rsplit_once('.') else {
        return false;
    };
    extension.eq_ignore_ascii_case("zst") && stem.ends_with(".jsonl")
}

pub(crate) fn has_zstd_suffix(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("zst"))
}

#[derive(PartialEq, Eq)]
struct SqliteFence {
    main: SourceStamp,
    wal: Option<SourceStamp>,
    shm: Option<SourceStamp>,
}

impl SqliteFence {
    fn capture(path: &Path) -> Result<Self, SqliteSourceError> {
        let main = required_stamp(path)?;
        let wal = optional_stamp(&sidecar(path, "-wal"))?;
        let shm = optional_stamp(&sidecar(path, "-shm"))?;
        if wal.is_some() && shm.is_none() {
            return Err(SqliteSourceError::Unavailable);
        }
        Ok(Self { main, wal, shm })
    }

    fn is_current(&self, path: &Path) -> Result<bool, SqliteSourceError> {
        Ok(Self::capture(path)? == *self)
    }
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_owned();
    value.push(suffix);
    PathBuf::from(value)
}

fn required_stamp(path: &Path) -> Result<SourceStamp, SqliteSourceError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            SqliteSourceError::Unavailable
        } else {
            SqliteSourceError::Io(error.kind())
        }
    })?;
    if !metadata.file_type().is_file() {
        return Err(SqliteSourceError::Invalid);
    }
    Ok(SourceStamp::new(path, &metadata))
}

fn optional_stamp(path: &Path) -> Result<Option<SourceStamp>, SqliteSourceError> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(SqliteSourceError::Io(error.kind())),
    };
    if !metadata.file_type().is_file() {
        return Err(SqliteSourceError::Invalid);
    }
    Ok(Some(SourceStamp::new(path, &metadata)))
}

struct SqliteSnapshot {
    connection: Connection,
    fence: SqliteFence,
    _temp: TempDirectory,
}

impl SqliteSnapshot {
    fn open(path: &Path) -> Result<Self, SqliteSourceError> {
        let fence = SqliteFence::capture(path)?;
        let total = fence
            .main
            .len()
            .checked_add(fence.wal.as_ref().map_or(0, SourceStamp::len))
            .and_then(|value| value.checked_add(fence.shm.as_ref().map_or(0, SourceStamp::len)))
            .ok_or(SqliteSourceError::Capacity)?;
        if total > MAX_SQLITE_SNAPSHOT_BYTES {
            return Err(SqliteSourceError::Capacity);
        }
        let temp = TempDirectory::create()?;
        let main = temp.path.join("source.sqlite");
        copy_source(path, &main)?;
        if fence.wal.is_some() {
            copy_source(&sidecar(path, "-wal"), &sidecar(&main, "-wal"))?;
        }
        if fence.shm.is_some() {
            copy_source(&sidecar(path, "-shm"), &sidecar(&main, "-shm"))?;
        }
        if !fence.is_current(path)? {
            return Err(SqliteSourceError::Changed);
        }
        let connection = Connection::open_with_flags(
            &main,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        connection.busy_timeout(SQLITE_BUSY_TIMEOUT)?;
        connection.pragma_update(None, "query_only", true)?;
        let quick_check: String =
            connection.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
        if quick_check != "ok" {
            return Err(SqliteSourceError::Invalid);
        }
        if !fence.is_current(path)? {
            return Err(SqliteSourceError::Changed);
        }
        Ok(Self {
            connection,
            fence,
            _temp: temp,
        })
    }

    fn verify(&self, path: &Path) -> Result<(), SqliteSourceError> {
        if self.fence.is_current(path)? {
            Ok(())
        } else {
            Err(SqliteSourceError::Changed)
        }
    }
}

struct TempDirectory {
    path: PathBuf,
}

impl TempDirectory {
    fn create() -> Result<Self, SqliteSourceError> {
        let path = std::env::temp_dir().join(format!(
            "token-monitor-lite-sqlite-{}-{}",
            std::process::id(),
            NEXT_SQLITE_SNAPSHOT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(error) = fs::set_permissions(&path, fs::Permissions::from_mode(0o700)) {
                let _ = fs::remove_dir_all(&path);
                return Err(SqliteSourceError::Io(error.kind()));
            }
        }
        Ok(Self { path })
    }
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn copy_source(source: &Path, target: &Path) -> Result<(), SqliteSourceError> {
    let metadata = fs::symlink_metadata(source)?;
    if !metadata.file_type().is_file() {
        return Err(SqliteSourceError::Invalid);
    }
    if metadata.len() > MAX_SQLITE_SNAPSHOT_BYTES {
        return Err(SqliteSourceError::Capacity);
    }
    fs::copy(source, target)?;
    Ok(())
}

fn read_session_archive(path: &Path) -> Result<SessionArchiveSnapshot, SqliteSourceError> {
    let snapshot = SqliteSnapshot::open(path)?;
    let transaction = snapshot.connection.unchecked_transaction()?;
    require_columns(&transaction, "metadata", &["key", "value"])?;
    require_columns(
        &transaction,
        "sessions",
        &["session_key", "entry_json", "revision"],
    )?;
    require_index(
        &transaction,
        "sessions",
        "sessions_revision_idx",
        &["revision", "session_key"],
    )?;

    let schema_version = metadata_value(&transaction, "schema-version")?;
    if schema_version.as_deref() != Some("1") {
        return Err(SqliteSourceError::Schema);
    }
    if metadata_value(&transaction, "legacy-migrated")?.as_deref() != Some("1") {
        return Err(SqliteSourceError::Schema);
    }
    let revision = metadata_value(&transaction, "revision")?
        .ok_or(SqliteSourceError::Schema)?
        .parse::<u64>()
        .map_err(|_| SqliteSourceError::Schema)?;
    let pruned_day = metadata_value(&transaction, "pruned-day")?;
    let pruned_month = metadata_value(&transaction, "pruned-month")?;

    let expected: i64 =
        transaction.query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))?;
    let expected: usize = expected
        .try_into()
        .map_err(|_| SqliteSourceError::Capacity)?;
    if expected > MAX_ARCHIVE_ROWS {
        return Err(SqliteSourceError::Capacity);
    }

    let mut sessions = Vec::with_capacity(expected);
    let mut identities = BTreeSet::new();
    let mut offset = 0_i64;
    loop {
        let mut statement = transaction.prepare(
            "SELECT session_key, entry_json, revision
             FROM sessions ORDER BY session_key LIMIT ?1 OFFSET ?2",
        )?;
        let mut rows = statement.query(params![SQLITE_PAGE_ROWS, offset])?;
        let mut page_rows = 0_usize;
        while let Some(row) = rows.next()? {
            if sessions.len() == MAX_ARCHIVE_ROWS {
                return Err(SqliteSourceError::Capacity);
            }
            let session_key: String = row.get(0)?;
            if !valid_sqlite_key(&session_key) {
                return Err(SqliteSourceError::Invalid);
            }
            let entry = row
                .get_ref(1)?
                .as_bytes()
                .map_err(|_| SqliteSourceError::Invalid)?;
            if entry.len() > MAX_ARCHIVE_ENTRY_BYTES {
                return Err(SqliteSourceError::Capacity);
            }
            let source_revision: i64 = row.get(2)?;
            let source_revision: u64 = source_revision
                .try_into()
                .map_err(|_| SqliteSourceError::Changed)?;
            if source_revision > revision {
                return Err(SqliteSourceError::Changed);
            }
            let session = parse_archive_entry(entry)?;
            if !identities.insert((session.client.clone(), session.session.clone())) {
                return Err(SqliteSourceError::Conflict);
            }
            sessions.push(session);
            page_rows += 1;
        }
        if page_rows == 0 {
            break;
        }
        offset = offset
            .checked_add(i64::try_from(page_rows).map_err(|_| SqliteSourceError::Capacity)?)
            .ok_or(SqliteSourceError::Capacity)?;
    }
    if sessions.len() != expected {
        return Err(SqliteSourceError::Changed);
    }
    drop(transaction);
    snapshot.verify(path)?;
    Ok(SessionArchiveSnapshot {
        revision,
        pruned_day,
        pruned_month,
        sessions: sessions.into(),
    })
}

fn read_codex_local_usage(path: &Path) -> Result<Arc<[ResponseUsage]>, SqliteSourceError> {
    let snapshot = SqliteSnapshot::open(path)?;
    let transaction = snapshot.connection.unchecked_transaction()?;
    require_columns(
        &transaction,
        "threads",
        &["account_key", "thread_id", "metadata", "checkpoint"],
    )?;
    require_columns(
        &transaction,
        "requests",
        &[
            "account_key",
            "thread_id",
            "checkpoint_total",
            "observed_at",
            "turn_id",
            "model",
            "cwd",
            "usage",
        ],
    )?;
    require_columns(
        &transaction,
        "observer",
        &["singleton", "owner", "pid", "expires_at"],
    )?;
    require_columns(
        &transaction,
        "agent_observer_intent",
        &["singleton", "pid", "expires_at"],
    )?;
    require_index(
        &transaction,
        "requests",
        "requests_thread",
        &["thread_id", "observed_at"],
    )?;

    let expected: i64 =
        transaction.query_row("SELECT COUNT(*) FROM requests", [], |row| row.get(0))?;
    let expected: usize = expected
        .try_into()
        .map_err(|_| SqliteSourceError::Capacity)?;
    if expected > MAX_LOCAL_USAGE_ROWS {
        return Err(SqliteSourceError::Capacity);
    }

    let mut usage_rows = Vec::with_capacity(expected);
    let mut offset = 0_i64;
    loop {
        let mut statement = transaction.prepare(
            "SELECT r.account_key, r.thread_id, r.checkpoint_total, r.observed_at,
                    r.model, r.usage, t.metadata
             FROM requests r
             JOIN threads t
               ON r.account_key = t.account_key AND r.thread_id = t.thread_id
             ORDER BY r.account_key, r.thread_id, r.checkpoint_total
             LIMIT ?1 OFFSET ?2",
        )?;
        let mut rows = statement.query(params![SQLITE_PAGE_ROWS, offset])?;
        let mut page_rows = 0_usize;
        while let Some(row) = rows.next()? {
            let account_key: String = row.get(0)?;
            let thread_id: String = row.get(1)?;
            let checkpoint_total: i64 = row.get(2)?;
            let observed_at: String = row.get(3)?;
            let model: String = row.get(4)?;
            let usage: String = row.get(5)?;
            let metadata: String = row.get(6)?;
            page_rows += 1;
            if !valid_sqlite_key(&account_key)
                || !valid_sqlite_key(&thread_id)
                || !valid_sqlite_string(&model, 4_096)
            {
                return Err(SqliteSourceError::Invalid);
            }
            let checkpoint_total =
                u64::try_from(checkpoint_total).map_err(|_| SqliteSourceError::Invalid)?;
            let metadata: Value =
                serde_json::from_str(&metadata).map_err(|_| SqliteSourceError::Invalid)?;
            let metadata = metadata.as_object().ok_or(SqliteSourceError::Invalid)?;
            if metadata.get("nativeBacked").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            let usage: Value =
                serde_json::from_str(&usage).map_err(|_| SqliteSourceError::Invalid)?;
            let usage = usage.as_object().ok_or(SqliteSourceError::Invalid)?;
            let input = required_u64(usage, "input")?;
            let cache_read = optional_u64(usage, "cacheRead")?;
            let cache_write = optional_u64(usage, "cacheWrite")?;
            let output = required_u64(usage, "output")?;
            let reasoning = optional_u64(usage, "reasoning")?;
            let total = required_u64(usage, "total")?;
            let summed = input
                .checked_add(cache_read)
                .and_then(|value| value.checked_add(cache_write))
                .and_then(|value| value.checked_add(output))
                .ok_or(SqliteSourceError::Invalid)?;
            if summed != total || reasoning > output {
                return Err(SqliteSourceError::Invalid);
            }
            if usage_rows.len() == MAX_LOCAL_USAGE_ROWS {
                return Err(SqliteSourceError::Capacity);
            }
            let identity = local_usage_identity(&account_key, &thread_id, checkpoint_total);
            usage_rows.push(ResponseUsage {
                key: SessionKey::new("codex", thread_id),
                model,
                tokens: TokenCounts {
                    input,
                    output,
                    cache_read,
                    cache_write,
                    reasoning,
                },
                cache_write_1h: 0,
                timestamp_ms: parse_timestamp_ms(&observed_at)?,
                dedup_key: Some(identity),
                billing_provider_hint: None,
                service_tier: None,
                agent: None,
                cost: CostStatus::Unavailable,
            });
        }
        if page_rows == 0 {
            break;
        }
        offset = offset
            .checked_add(i64::try_from(page_rows).map_err(|_| SqliteSourceError::Capacity)?)
            .ok_or(SqliteSourceError::Capacity)?;
    }
    if usage_rows.len() > expected {
        return Err(SqliteSourceError::Changed);
    }
    drop(transaction);
    snapshot.verify(path)?;
    Ok(usage_rows.into())
}

fn parse_archive_entry(entry: &[u8]) -> Result<ArchiveSession, SqliteSourceError> {
    let value: Value = serde_json::from_slice(entry).map_err(|_| SqliteSourceError::Invalid)?;
    let object = value.as_object().ok_or(SqliteSourceError::Invalid)?;
    let client = required_string(object, "client", 64)?;
    let session = required_string(object, "sessionId", 512)?;
    let captured_at = required_string(object, "capturedAt", 128)?;
    let captured_at_ms = parse_timestamp_ms(captured_at)?;
    let day = optional_string(object, "day", 64)?;
    let month = optional_string(object, "month", 64)?;
    let periods = object
        .get("periods")
        .and_then(Value::as_object)
        .ok_or(SqliteSourceError::Invalid)?;
    let windows = object.get("periodWindows").and_then(Value::as_object);
    let mut parsed_periods = Vec::new();
    for (name, period) in [
        ("today", ArchivePeriod::Today),
        ("month", ArchivePeriod::Month),
        ("allTime", ArchivePeriod::AllTime),
    ] {
        let Some(value) = periods.get(name) else {
            continue;
        };
        let fields = value.as_object().ok_or(SqliteSourceError::Invalid)?;
        let window = windows.and_then(|windows| windows.get(name));
        let captured = window
            .and_then(|window| window.get("capturedAt"))
            .and_then(Value::as_str)
            .unwrap_or(captured_at);
        let period_day = window
            .and_then(|window| window.get("day"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| day.clone());
        let period_month = window
            .and_then(|window| window.get("month"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| month.clone());
        let total = required_u64(fields, "totalTokens")?;
        let input = optional_u64(fields, "inputTokens")?;
        let output = optional_u64(fields, "outputTokens")?;
        let cache_read = optional_u64(fields, "cacheReadTokens")?;
        let cache_write = optional_u64(fields, "cacheWriteTokens")?;
        let reasoning = optional_u64(fields, "reasoningTokens")?;
        let classified = input
            .checked_add(cache_read)
            .and_then(|value| value.checked_add(cache_write))
            .and_then(|value| value.checked_add(output))
            .ok_or(SqliteSourceError::Capacity)?;
        if classified > total || reasoning > output {
            return Err(SqliteSourceError::Invalid);
        }
        let models =
            parse_archive_models(fields, total, &[input, cache_read, cache_write, output])?;
        let model_total = models
            .iter()
            .try_fold(0_u64, |sum, model| sum.checked_add(model.total))
            .ok_or(SqliteSourceError::Capacity)?;
        parsed_periods.push(ArchivePeriodUsage {
            period,
            captured_at_ms: parse_timestamp_ms(captured)?,
            day: period_day,
            month: period_month,
            total,
            input,
            output,
            cache_read,
            cache_write,
            reasoning,
            unclassified: total - classified,
            components_complete: classified == total,
            models_complete: model_total == total,
            models: models.into(),
        });
    }
    if parsed_periods.is_empty() {
        return Err(SqliteSourceError::Invalid);
    }
    Ok(ArchiveSession {
        client: client.to_owned(),
        session: session.to_owned(),
        captured_at_ms,
        day,
        month,
        periods: parsed_periods.into(),
    })
}

fn parse_archive_models(
    fields: &serde_json::Map<String, Value>,
    period_total: u64,
    components: &[u64; 4],
) -> Result<Vec<ArchiveModelUsage>, SqliteSourceError> {
    let Some(models) = fields.get("models").and_then(Value::as_object) else {
        return Ok(Vec::new());
    };
    let model_components = fields
        .get("modelTokenComponents")
        .and_then(Value::as_object);
    let mut rows = Vec::with_capacity(models.len());
    let mut model_total = 0_u64;
    let mut component_totals = [0_u64; 4];
    for (model, value) in models {
        if !valid_sqlite_string(model, 4_096) {
            return Err(SqliteSourceError::Invalid);
        }
        let total = value.as_u64().ok_or(SqliteSourceError::Invalid)?;
        if total > period_total {
            return Err(SqliteSourceError::Invalid);
        }
        let (input, cache_read, cache_write, output) = model_components
            .and_then(|values| values.get(model))
            .and_then(Value::as_object)
            .map(|values| {
                Ok::<_, SqliteSourceError>((
                    optional_u64(values, "inputTokens")?,
                    optional_u64(values, "cacheReadTokens")?,
                    optional_u64(values, "cacheWriteTokens")?,
                    optional_u64(values, "outputTokens")?,
                ))
            })
            .transpose()?
            .unwrap_or((0, 0, 0, 0));
        let classified = input
            .checked_add(cache_read)
            .and_then(|value| value.checked_add(cache_write))
            .and_then(|value| value.checked_add(output))
            .ok_or(SqliteSourceError::Capacity)?;
        if classified > total {
            return Err(SqliteSourceError::Invalid);
        }
        model_total = model_total
            .checked_add(total)
            .ok_or(SqliteSourceError::Capacity)?;
        for (index, value) in [input, cache_read, cache_write, output]
            .into_iter()
            .enumerate()
        {
            component_totals[index] = component_totals[index]
                .checked_add(value)
                .ok_or(SqliteSourceError::Capacity)?;
        }
        rows.push(ArchiveModelUsage {
            model: model.clone(),
            total,
            input,
            cache_read,
            cache_write,
            output,
        });
    }
    if model_total > period_total
        || component_totals
            .into_iter()
            .zip(components.iter())
            .any(|(model, period)| model > *period)
    {
        return Err(SqliteSourceError::Invalid);
    }
    rows.sort_by(|left, right| left.model.cmp(&right.model));
    Ok(rows)
}

fn metadata_value(connection: &Connection, key: &str) -> Result<Option<String>, SqliteSourceError> {
    connection
        .query_row("SELECT value FROM metadata WHERE key = ?1", [key], |row| {
            row.get(0)
        })
        .optional()
        .map_err(SqliteSourceError::from)
}

fn require_columns(
    connection: &Connection,
    table: &str,
    required: &[&str],
) -> Result<(), SqliteSourceError> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = statement.query([])?;
    let mut columns = BTreeSet::new();
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        columns.insert(name);
    }
    if required.iter().any(|column| !columns.contains(*column)) {
        return Err(SqliteSourceError::Schema);
    }
    Ok(())
}

fn require_index(
    connection: &Connection,
    table: &str,
    index: &str,
    columns: &[&str],
) -> Result<(), SqliteSourceError> {
    let mut statement = connection.prepare(&format!("PRAGMA index_list({table})"))?;
    let mut rows = statement.query([])?;
    let mut found = false;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == index {
            found = true;
            break;
        }
    }
    if !found {
        return Err(SqliteSourceError::Schema);
    }
    let mut statement = connection.prepare(&format!("PRAGMA index_info({index})"))?;
    let mut rows = statement.query([])?;
    let mut actual = Vec::new();
    while let Some(row) = rows.next()? {
        actual.push(row.get::<_, String>(2)?);
    }
    if actual != columns {
        return Err(SqliteSourceError::Schema);
    }
    Ok(())
}

fn required_string<'a>(
    object: &'a serde_json::Map<String, Value>,
    key: &str,
    maximum: usize,
) -> Result<&'a str, SqliteSourceError> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| valid_sqlite_string(value, maximum))
        .ok_or(SqliteSourceError::Invalid)
}

fn optional_string(
    object: &serde_json::Map<String, Value>,
    key: &str,
    maximum: usize,
) -> Result<Option<String>, SqliteSourceError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if valid_sqlite_string(value, maximum) => {
            Ok(Some(value.clone()))
        }
        Some(_) => Err(SqliteSourceError::Invalid),
    }
}

fn required_u64(
    object: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<u64, SqliteSourceError> {
    object
        .get(key)
        .and_then(Value::as_u64)
        .ok_or(SqliteSourceError::Invalid)
}

fn optional_u64(
    object: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<u64, SqliteSourceError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(0),
        Some(value) => value.as_u64().ok_or(SqliteSourceError::Invalid),
    }
}

fn parse_timestamp_ms(value: &str) -> Result<i64, SqliteSourceError> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.timestamp_millis())
        .map_err(|_| SqliteSourceError::Invalid)
}

fn valid_sqlite_key(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 1_024
        && !value.chars().any(char::is_control)
        && !value.starts_with('/')
        && !value.starts_with("\\\\")
}

fn valid_sqlite_string(value: &str, maximum: usize) -> bool {
    !value.is_empty() && value.len() <= maximum && !value.chars().any(char::is_control)
}

fn local_usage_identity(account_key: &str, thread_id: &str, checkpoint_total: u64) -> String {
    let mut left = 0xcbf29ce484222325_u64;
    let mut right = 0x84222325cbf29ce4_u64;
    for byte in b"token-monitor-lite/codex-local/v1\0"
        .iter()
        .chain(account_key.as_bytes())
        .chain([0_u8].iter())
        .chain(thread_id.as_bytes())
        .chain(checkpoint_total.to_le_bytes().iter())
    {
        left ^= u64::from(*byte);
        left = left.wrapping_mul(0x100000001b3);
        right ^= u64::from(*byte).rotate_left(1);
        right = right.wrapping_mul(0x100000001b3);
    }
    format!("codex-local:{left:016x}{right:016x}")
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn frame_error(_: FrameDecoderError) -> io::Error {
    invalid("zstd frame decode failed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    struct Chunked {
        bytes: Vec<u8>,
        position: usize,
        chunk: usize,
    }

    impl Chunked {
        fn new(bytes: Vec<u8>, chunk: usize) -> Self {
            Self {
                bytes,
                position: 0,
                chunk,
            }
        }
    }

    impl Read for Chunked {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.position == self.bytes.len() {
                return Ok(0);
            }
            let end = (self.position + self.chunk.min(buf.len())).min(self.bytes.len());
            let length = end - self.position;
            buf[..length].copy_from_slice(&self.bytes[self.position..end]);
            self.position = end;
            Ok(length)
        }
    }

    fn compressed(bytes: &[u8]) -> Vec<u8> {
        ruzstd::encoding::compress_to_vec(bytes, ruzstd::encoding::CompressionLevel::Fastest)
    }

    #[test]
    fn streaming_decoder_handles_tiny_chunks_across_record_boundaries() {
        let payload = b"first record\nsecond record\nthird record\n";
        let mut stream = CompressedStream::new(Chunked::new(compressed(payload), 3));
        let mut decoded = Vec::new();
        let mut buffer = [0_u8; 7];
        loop {
            let read = stream.fill(&mut buffer).unwrap();
            if read == 0 {
                break;
            }
            decoded.extend_from_slice(&buffer[..read]);
        }
        assert_eq!(decoded, payload);
        assert!(stream.finished);
        assert!(stream.seen_standard_frame);
    }

    #[test]
    fn decoded_ceiling_rejects_a_bomb_without_materializing_it() {
        let payload = vec![b'\n'; 16 * 1024];
        let path = std::env::temp_dir().join(format!(
            "token-monitor-zstd-limit-{}-{}.jsonl.zst",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&path, compressed(&payload)).unwrap();
        let SourceInput::CompressedJsonl(mut input) = SourceInput::compressed_jsonl(path.clone())
        else {
            panic!("expected compressed input");
        };
        input.decoded_limit = 1024;
        let first = input.read_batch().unwrap();
        assert_eq!(first.decoded_bytes_read, 1024);
        assert!(first.more_available);
        assert!(input.read_batch().is_err());
        fs::remove_file(path).unwrap();
    }
}
