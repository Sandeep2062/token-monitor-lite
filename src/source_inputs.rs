//! Closed, bounded source-input adapters. Adapters own physical I/O and source
//! mutation fences; client parsers receive only complete normalized records.
//! The SQLite archive adapter returns retained period data separately; it is
//! intentionally not folded into the request-like canonical runtime yet.

use crate::accounting::{SessionKey, TokenCounts};
use crate::parsers::antigravity::AntigravityDbReader;
use crate::parsers::catpaw;
use crate::parsers::cherrystudio;
use crate::parsers::goose::{GooseReader, GooseRow};
use crate::parsers::hermes::{HermesReader, HermesRow};
use crate::parsers::jsonl::{
    JsonlCursor, JsonlFramer, MAX_BATCH_BYTES, MAX_RECORD_BYTES, SourceStamp,
};
use crate::parsers::kilo::KiloReader;
use crate::parsers::mimo::{self, MimoReader};
use crate::parsers::openclaw::OpenClawReader;
use crate::parsers::opencode::{
    MAX_RESPONSES as MAX_OPENCODE_RESPONSES, OpenCodeKind, OpenCodeReader, OpenCodeRecord,
};
use crate::parsers::qodercn;
use crate::parsers::unsloth;
use crate::parsers::workbuddy::sqlite_row_to_usage;
use crate::parsers::zcode;
use crate::parsers::zed::ZedReader;
use crate::parsers::{CostStatus, ReaderError, ResponseUsage};
use crate::runtime::MonitoredSource;
use crate::sources::SourceFormat;
use chrono::DateTime;
use rusqlite::{Connection, ErrorCode, OpenFlags, OptionalExtension, params};
use ruzstd::decoding::errors::FrameDecoderError;
use ruzstd::decoding::{BlockDecodingStrategy, FrameDecoder, StreamingDecoder};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, UNIX_EPOCH};

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
    OpenCode,
    Kilo,
    Goose,
}

pub fn classify_sqlite_source(path: &Path) -> Option<SqliteSourceRole> {
    let name = path.file_name()?.to_str()?;
    match name {
        "session-usage-archive.sqlite" => Some(SqliteSourceRole::SessionArchive),
        "codex-local-usage.sqlite" => Some(SqliteSourceRole::CodexLocalUsage),
        "kilo.db" => Some(SqliteSourceRole::Kilo),
        "sessions.db" => Some(SqliteSourceRole::Goose),
        _ if is_opencode_db_filename(name) => Some(SqliteSourceRole::OpenCode),
        _ => None,
    }
}

/// OpenCode picks `opencode.db` or `opencode-<channel>.db` at build time.
/// Sidecars (`-wal`, `-shm`, `-journal`) do not end in `.db` and are excluded.
pub(crate) fn is_opencode_db_filename(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".db") else {
        return false;
    };
    if stem == "opencode" {
        return true;
    }
    let Some(channel) = stem.strip_prefix("opencode-") else {
        return false;
    };
    !channel.is_empty()
        && channel
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
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
    #[error("sqlite source contains an unsupported shape")]
    Unsupported,
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

pub struct OpenCodeAdapter {
    path: PathBuf,
}

impl OpenCodeAdapter {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub fn read(&self) -> Result<Arc<[ResponseUsage]>, SqliteSourceError> {
        read_opencode(&self.path)
    }
}

pub struct CatpawAdapter {
    path: PathBuf,
    edition: catpaw::Edition,
}

impl CatpawAdapter {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        // The edition is derived from the containing root's name, never from a
        // model id: the same numeric id means different models per edition.
        let edition = path
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .and_then(catpaw::Edition::from_database_name)
            .unwrap_or(catpaw::Edition::Domestic);
        Self { path, edition }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub fn read(&self) -> Result<Arc<[ResponseUsage]>, SqliteSourceError> {
        read_catpaw(&self.path, self.edition)
    }
}

pub struct CherrystudioAdapter {
    path: PathBuf,
}

impl CherrystudioAdapter {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub fn read(&self) -> Result<Arc<[ResponseUsage]>, SqliteSourceError> {
        read_cherrystudio(&self.path)
    }
}

pub struct UnslothAdapter {
    path: PathBuf,
}

impl UnslothAdapter {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub fn read(&self) -> Result<Arc<[ResponseUsage]>, SqliteSourceError> {
        read_unsloth(&self.path)
    }
}

pub struct MimoAdapter {
    path: PathBuf,
}

impl MimoAdapter {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub fn read(&self) -> Result<Arc<[ResponseUsage]>, SqliteSourceError> {
        read_mimo(&self.path)
    }
}

pub struct DshInput {
    path: PathBuf,
    inner: DshInputKind,
}

/// DSH writes the same rows to `session.jsonl.zstd` or a plain
/// `session.jsonl` depending on the backend's `compression` setting, so the
/// encoding is decided by the zstd frame magic rather than the file name.
enum DshInputKind {
    Unprobed,
    Plain(OrdinaryJsonlInput),
    Compressed(Box<CompressedJsonlInput>),
}

impl DshInput {
    fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            inner: DshInputKind::Unprobed,
        }
    }

    fn probe(&mut self) -> io::Result<()> {
        if !matches!(self.inner, DshInputKind::Unprobed) {
            return Ok(());
        }
        let mut magic = [0_u8; 4];
        let mut file = File::open(&self.path)?;
        let read = {
            use std::io::Read as _;
            let mut filled = 0;
            while filled < magic.len() {
                match file.read(&mut magic[filled..])? {
                    0 => break,
                    count => filled += count,
                }
            }
            filled
        };
        self.inner = if read == magic.len() && magic == ZSTD_MAGIC {
            DshInputKind::Compressed(Box::new(CompressedJsonlInput {
                path: self.path.clone(),
                state: None,
                framer: JsonlFramer::default(),
                buffer: vec![0; MAX_BATCH_BYTES].into_boxed_slice(),
                decoded_total: 0,
                decoded_limit: MAX_DECODED_BYTES,
                reset_pending: true,
            }))
        } else {
            DshInputKind::Plain(OrdinaryJsonlInput {
                path: self.path.clone(),
                cursor: JsonlCursor::default(),
            })
        };
        Ok(())
    }

    fn reset(&mut self) {
        match &mut self.inner {
            DshInputKind::Unprobed => {}
            DshInputKind::Plain(input) => input.cursor.reset(),
            DshInputKind::Compressed(input) => input.reset(),
        }
    }

    fn is_current(&self) -> io::Result<bool> {
        match &self.inner {
            DshInputKind::Unprobed => Ok(false),
            DshInputKind::Plain(input) => input.cursor.is_current(&input.path),
            DshInputKind::Compressed(input) => input.is_current(),
        }
    }

    fn read_batch(&mut self) -> io::Result<InputBatch> {
        self.probe()?;
        match &mut self.inner {
            DshInputKind::Unprobed => unreachable!("probe resolves the encoding"),
            DshInputKind::Plain(input) => {
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
            DshInputKind::Compressed(input) => input.read_batch(),
        }
    }
}

pub struct QodercnAdapter {
    path: PathBuf,
}

impl QodercnAdapter {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub fn read(&self) -> Result<Arc<[ResponseUsage]>, SqliteSourceError> {
        read_qodercn(&self.path)
    }
}

pub struct ZcodeAdapter {
    path: PathBuf,
}

impl ZcodeAdapter {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub fn read(&self) -> Result<Arc<[ResponseUsage]>, SqliteSourceError> {
        read_zcode(&self.path)
    }
}

pub struct KiloAdapter {
    path: PathBuf,
}

impl KiloAdapter {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub fn read(&self) -> Result<Arc<[ResponseUsage]>, SqliteSourceError> {
        read_kilo(&self.path)
    }
}

pub struct AntigravitySqliteAdapter {
    path: PathBuf,
}

impl AntigravitySqliteAdapter {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub fn read(&self) -> Result<Arc<[ResponseUsage]>, SqliteSourceError> {
        read_antigravity_sqlite(&self.path)
    }
}

pub struct WorkBuddySqliteAdapter {
    path: PathBuf,
}

impl WorkBuddySqliteAdapter {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub fn read(&self) -> Result<Arc<[ResponseUsage]>, SqliteSourceError> {
        read_workbuddy_sqlite(&self.path)
    }
}

pub struct ZedAdapter {
    path: PathBuf,
}

impl ZedAdapter {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub fn read(&self) -> Result<Arc<[ResponseUsage]>, SqliteSourceError> {
        read_zed(&self.path)
    }
}

pub struct GooseAdapter {
    path: PathBuf,
}

impl GooseAdapter {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub fn read(&self) -> Result<Arc<[ResponseUsage]>, SqliteSourceError> {
        read_goose(&self.path)
    }
}

pub struct HermesAdapter {
    path: PathBuf,
}

impl HermesAdapter {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub fn read(&self) -> Result<Arc<[ResponseUsage]>, SqliteSourceError> {
        read_hermes(&self.path)
    }
}

pub struct OpenClawAdapter {
    path: PathBuf,
}

impl OpenClawAdapter {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub fn read(&self) -> Result<Arc<[ResponseUsage]>, SqliteSourceError> {
        read_openclaw(&self.path)
    }
}

const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];
const ZSTD_SKIPPABLE_MIN: u32 = 0x184D_2A50;
const ZSTD_SKIPPABLE_MAX: u32 = 0x184D_2A5F;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputAdapterStatus {
    OrdinaryJsonl,
    KimiJsonl,
    CommandCodeJsonl,
    GrokJsonl,
    GeminiTranscript,
    CursorCacheJson,
    ClineJson,
    AmpJson,
    DroidJson,
    KiroJson,
    PromaJsonl,
    CopilotJsonl,
    CodeBuddyLog,
    CompressedCodexJsonl,
    SessionArchiveSqlite,
    CodexLocalUsageSqlite,
    OpenCodeSqlite,
    KiloSqlite,
    MimoSqlite,
    ZcodeJsonl,
    ZcodeSqlite,
    QodercnJsonl,
    QodercnSqlite,
    DshJsonl,
    LmstudioLog,
    CatpawSqlite,
    CherrystudioSqlite,
    UnslothSqlite,
    GooseSqlite,
    HermesSqlite,
    OpenClawSqlite,
    ZedSqlite,
    WorkBuddySqlite,
    AntigravitySqlite,
}

pub(crate) enum SqliteInputData {
    SessionArchive(SessionArchiveSnapshot),
    CodexLocalUsage(Arc<[ResponseUsage]>),
    OpenCode(Arc<[ResponseUsage]>),
    Kilo(Arc<[ResponseUsage]>),
    Mimo(Arc<[ResponseUsage]>),
    Zcode(Arc<[ResponseUsage]>),
    Qodercn(Arc<[ResponseUsage]>),
    Catpaw(Arc<[ResponseUsage]>),
    Cherrystudio(Arc<[ResponseUsage]>),
    Unsloth(Arc<[ResponseUsage]>),
    Goose(Arc<[ResponseUsage]>),
    Hermes(Arc<[ResponseUsage]>),
    OpenClaw(Arc<[ResponseUsage]>),
    Zed(Arc<[ResponseUsage]>),
    WorkBuddySqlite(Arc<[ResponseUsage]>),
    AntigravitySqlite(Arc<[ResponseUsage]>),
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

pub struct GeminiInput {
    path: PathBuf,
    kind: GeminiInputKind,
}

pub struct DocumentInput {
    path: PathBuf,
    cursor: DocumentCursor,
}

pub type CursorInput = DocumentInput;

enum GeminiInputKind {
    Jsonl(JsonlCursor),
    Document(DocumentCursor),
}

#[derive(Default)]
struct DocumentCursor {
    stamp: Option<SourceStamp>,
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
    Kimi(OrdinaryJsonlInput),
    CommandCode(OrdinaryJsonlInput),
    Grok(OrdinaryJsonlInput),
    ZcodeJsonl(OrdinaryJsonlInput),
    Dsh(DshInput),
    Lmstudio(DocumentInput),
    QodercnJsonl(OrdinaryJsonlInput),
    PromaJsonl(OrdinaryJsonlInput),
    Gemini(GeminiInput),
    Cursor(CursorInput),
    Cline(DocumentInput),
    Amp(DocumentInput),
    Droid(DocumentInput),
    Kiro(DocumentInput),
    Copilot(OrdinaryJsonlInput),
    CodeBuddyLog(DocumentInput),
    CompressedJsonl(Box<CompressedJsonlInput>),
    SessionArchive(SessionArchiveAdapter),
    CodexLocalUsage(CodexLocalUsageAdapter),
    OpenCode(OpenCodeAdapter),
    Kilo(KiloAdapter),
    Mimo(MimoAdapter),
    Zcode(ZcodeAdapter),
    Qodercn(QodercnAdapter),
    Catpaw(CatpawAdapter),
    Cherrystudio(CherrystudioAdapter),
    Unsloth(UnslothAdapter),
    Goose(GooseAdapter),
    Hermes(HermesAdapter),
    OpenClaw(OpenClawAdapter),
    Zed(ZedAdapter),
    WorkBuddySqlite(WorkBuddySqliteAdapter),
    AntigravitySqlite(AntigravitySqliteAdapter),
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
            SourceFormat::OpenCodeSqlite => Self::OpenCode(OpenCodeAdapter::new(path)),
            SourceFormat::KiloSqlite => Self::Kilo(KiloAdapter::new(path)),
            SourceFormat::MimoSqlite => Self::Mimo(MimoAdapter::new(path)),
            SourceFormat::ZcodeSqlite => Self::Zcode(ZcodeAdapter::new(path)),
            SourceFormat::QodercnSqlite => Self::Qodercn(QodercnAdapter::new(path)),
            SourceFormat::CatpawSqlite => Self::Catpaw(CatpawAdapter::new(path)),
            SourceFormat::CherrystudioSqlite => Self::Cherrystudio(CherrystudioAdapter::new(path)),
            SourceFormat::UnslothSqlite => Self::Unsloth(UnslothAdapter::new(path)),
            SourceFormat::GooseSqlite => Self::Goose(GooseAdapter::new(path)),
            SourceFormat::HermesSqlite => Self::Hermes(HermesAdapter::new(path)),
            SourceFormat::OpenClawSqlite => Self::OpenClaw(OpenClawAdapter::new(path)),
            SourceFormat::ZedSqlite => Self::Zed(ZedAdapter::new(path)),
            SourceFormat::WorkBuddySqlite => {
                Self::WorkBuddySqlite(WorkBuddySqliteAdapter::new(path))
            }
            SourceFormat::AntigravitySqlite => {
                Self::AntigravitySqlite(AntigravitySqliteAdapter::new(path))
            }
            SourceFormat::CommandCodeJsonl => Self::CommandCode(OrdinaryJsonlInput {
                path,
                cursor: JsonlCursor::default(),
            }),
            SourceFormat::GrokJsonl
                if path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name == "updates.jsonl") =>
            {
                Self::Grok(OrdinaryJsonlInput {
                    path,
                    cursor: JsonlCursor::default(),
                })
            }
            SourceFormat::PromaJsonl => Self::PromaJsonl(OrdinaryJsonlInput {
                path,
                cursor: JsonlCursor::default(),
            }),
            SourceFormat::ZcodeJsonl => Self::ZcodeJsonl(OrdinaryJsonlInput {
                path,
                cursor: JsonlCursor::default(),
            }),
            SourceFormat::DshJsonl => Self::Dsh(DshInput::new(path)),
            SourceFormat::QodercnJsonl => Self::QodercnJsonl(OrdinaryJsonlInput {
                path,
                cursor: JsonlCursor::default(),
            }),
            SourceFormat::KimiJsonl
                if path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name == "wire.jsonl") =>
            {
                Self::Kimi(OrdinaryJsonlInput {
                    path,
                    cursor: JsonlCursor::default(),
                })
            }
            SourceFormat::GeminiTranscript => Self::gemini(path),
            SourceFormat::CursorCacheJson => Self::cursor(path),
            SourceFormat::ClineJson => Self::document(path),
            SourceFormat::AmpJson => Self::amp(path),
            SourceFormat::DroidJson => Self::droid(path),
            SourceFormat::KiroJson => Self::kiro(path),
            SourceFormat::LmstudioLog => Self::lmstudio(path),
            SourceFormat::CopilotJsonl => Self::copilot(path),
            SourceFormat::CodeBuddyJsonl
                if path
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("log")) =>
            {
                Self::codebuddy_log(path)
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

    pub(crate) fn gemini(path: PathBuf) -> Self {
        let kind = if path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
        {
            GeminiInputKind::Jsonl(JsonlCursor::default())
        } else {
            GeminiInputKind::Document(DocumentCursor::default())
        };
        Self::Gemini(GeminiInput { path, kind })
    }

    pub(crate) fn cursor(path: PathBuf) -> Self {
        Self::Cursor(CursorInput {
            path,
            cursor: DocumentCursor::default(),
        })
    }

    pub(crate) fn document(path: PathBuf) -> Self {
        Self::Cline(DocumentInput {
            path,
            cursor: DocumentCursor::default(),
        })
    }

    pub(crate) fn amp(path: PathBuf) -> Self {
        Self::Amp(DocumentInput {
            path,
            cursor: DocumentCursor::default(),
        })
    }

    pub(crate) fn droid(path: PathBuf) -> Self {
        Self::Droid(DocumentInput {
            path,
            cursor: DocumentCursor::default(),
        })
    }

    pub(crate) fn kiro(path: PathBuf) -> Self {
        Self::Kiro(DocumentInput {
            path,
            cursor: DocumentCursor::default(),
        })
    }

    pub(crate) fn lmstudio(path: PathBuf) -> Self {
        Self::Lmstudio(DocumentInput {
            path,
            cursor: DocumentCursor::default(),
        })
    }

    pub(crate) fn codebuddy_log(path: PathBuf) -> Self {
        Self::CodeBuddyLog(DocumentInput {
            path,
            cursor: DocumentCursor::default(),
        })
    }

    pub(crate) fn copilot(path: PathBuf) -> Self {
        Self::Copilot(OrdinaryJsonlInput {
            path,
            cursor: JsonlCursor::default(),
        })
    }

    pub fn status(&self) -> InputAdapterStatus {
        match self {
            Self::OrdinaryJsonl(_) => InputAdapterStatus::OrdinaryJsonl,
            Self::Lmstudio(_) => InputAdapterStatus::LmstudioLog,
            Self::Kimi(_) => InputAdapterStatus::KimiJsonl,
            Self::CommandCode(_) => InputAdapterStatus::CommandCodeJsonl,
            Self::Grok(_) => InputAdapterStatus::GrokJsonl,
            Self::ZcodeJsonl(_) => InputAdapterStatus::ZcodeJsonl,
            Self::QodercnJsonl(_) => InputAdapterStatus::QodercnJsonl,
            Self::Dsh(_) => InputAdapterStatus::DshJsonl,
            Self::Gemini(_) => InputAdapterStatus::GeminiTranscript,
            Self::Cursor(_) => InputAdapterStatus::CursorCacheJson,
            Self::Cline(_) => InputAdapterStatus::ClineJson,
            Self::Amp(_) => InputAdapterStatus::AmpJson,
            Self::Droid(_) => InputAdapterStatus::DroidJson,
            Self::Kiro(_) => InputAdapterStatus::KiroJson,
            Self::PromaJsonl(_) => InputAdapterStatus::PromaJsonl,
            Self::Copilot(_) => InputAdapterStatus::CopilotJsonl,
            Self::CodeBuddyLog(_) => InputAdapterStatus::CodeBuddyLog,
            Self::CompressedJsonl(_) => InputAdapterStatus::CompressedCodexJsonl,
            Self::SessionArchive(_) => InputAdapterStatus::SessionArchiveSqlite,
            Self::CodexLocalUsage(_) => InputAdapterStatus::CodexLocalUsageSqlite,
            Self::OpenCode(_) => InputAdapterStatus::OpenCodeSqlite,
            Self::Kilo(_) => InputAdapterStatus::KiloSqlite,
            Self::Mimo(_) => InputAdapterStatus::MimoSqlite,
            Self::Zcode(_) => InputAdapterStatus::ZcodeSqlite,
            Self::Qodercn(_) => InputAdapterStatus::QodercnSqlite,
            Self::Catpaw(_) => InputAdapterStatus::CatpawSqlite,
            Self::Cherrystudio(_) => InputAdapterStatus::CherrystudioSqlite,
            Self::Unsloth(_) => InputAdapterStatus::UnslothSqlite,
            Self::Goose(_) => InputAdapterStatus::GooseSqlite,
            Self::Hermes(_) => InputAdapterStatus::HermesSqlite,
            Self::OpenClaw(_) => InputAdapterStatus::OpenClawSqlite,
            Self::Zed(_) => InputAdapterStatus::ZedSqlite,
            Self::WorkBuddySqlite(_) => InputAdapterStatus::WorkBuddySqlite,
            Self::AntigravitySqlite(_) => InputAdapterStatus::AntigravitySqlite,
        }
    }

    pub fn reset(&mut self) {
        match self {
            Self::OrdinaryJsonl(input) => input.cursor.reset(),
            Self::Lmstudio(input) => input.reset(),
            Self::CommandCode(input) => input.cursor.reset(),
            Self::Grok(input) => input.cursor.reset(),
            Self::ZcodeJsonl(input) => input.cursor.reset(),
            Self::QodercnJsonl(input) => input.cursor.reset(),
            Self::Dsh(input) => input.reset(),
            Self::PromaJsonl(input) => input.cursor.reset(),
            Self::Kimi(input) => input.cursor.reset(),
            Self::Gemini(input) => input.reset(),
            Self::Cursor(input) => input.reset(),
            Self::Cline(input) => input.reset(),
            Self::Amp(input) => input.reset(),
            Self::Droid(input) => input.reset(),
            Self::Kiro(input) => input.reset(),
            Self::Copilot(input) => input.cursor.reset(),
            Self::CodeBuddyLog(input) => input.reset(),
            Self::CompressedJsonl(input) => input.reset(),
            Self::SessionArchive(_)
            | Self::CodexLocalUsage(_)
            | Self::OpenCode(_)
            | Self::Kilo(_)
            | Self::Mimo(_)
            | Self::Zcode(_)
            | Self::Qodercn(_)
            | Self::Catpaw(_)
            | Self::Cherrystudio(_)
            | Self::Unsloth(_)
            | Self::Goose(_)
            | Self::Hermes(_)
            | Self::OpenClaw(_)
            | Self::Zed(_)
            | Self::WorkBuddySqlite(_)
            | Self::AntigravitySqlite(_) => {}
        }
    }

    pub fn is_current(&self) -> io::Result<bool> {
        match self {
            Self::OrdinaryJsonl(input) => input.cursor.is_current(&input.path),
            Self::Lmstudio(input) => input.is_current(),
            Self::CommandCode(input) => input.cursor.is_current(&input.path),
            Self::Grok(input) => input.cursor.is_current(&input.path),
            Self::ZcodeJsonl(input) => input.cursor.is_current(&input.path),
            Self::QodercnJsonl(input) => input.cursor.is_current(&input.path),
            Self::Dsh(input) => input.is_current(),
            Self::PromaJsonl(input) => input.cursor.is_current(&input.path),
            Self::Kimi(input) => input.cursor.is_current(&input.path),
            Self::Gemini(input) => input.is_current(),
            Self::Cursor(input) => input.is_current(),
            Self::Cline(input) => input.is_current(),
            Self::Amp(input) => input.is_current(),
            Self::Droid(input) => input.is_current(),
            Self::Kiro(input) => input.is_current(),
            Self::Copilot(input) => input.cursor.is_current(&input.path),
            Self::CodeBuddyLog(input) => input.is_current(),
            Self::CompressedJsonl(input) => input.is_current(),
            Self::SessionArchive(_)
            | Self::CodexLocalUsage(_)
            | Self::OpenCode(_)
            | Self::Kilo(_)
            | Self::Mimo(_)
            | Self::Zcode(_)
            | Self::Qodercn(_)
            | Self::Catpaw(_)
            | Self::Cherrystudio(_)
            | Self::Unsloth(_)
            | Self::Goose(_)
            | Self::Hermes(_)
            | Self::OpenClaw(_)
            | Self::Zed(_)
            | Self::WorkBuddySqlite(_)
            | Self::AntigravitySqlite(_) => Ok(true),
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
            Self::OpenCode(adapter) => Ok(Some(SqliteInputData::OpenCode(adapter.read()?))),
            Self::Kilo(adapter) => Ok(Some(SqliteInputData::Kilo(adapter.read()?))),
            Self::Mimo(adapter) => Ok(Some(SqliteInputData::Mimo(adapter.read()?))),
            Self::Zcode(adapter) => Ok(Some(SqliteInputData::Zcode(adapter.read()?))),
            Self::Qodercn(adapter) => Ok(Some(SqliteInputData::Qodercn(adapter.read()?))),
            Self::Catpaw(adapter) => Ok(Some(SqliteInputData::Catpaw(adapter.read()?))),
            Self::Cherrystudio(adapter) => Ok(Some(SqliteInputData::Cherrystudio(adapter.read()?))),
            Self::Unsloth(adapter) => Ok(Some(SqliteInputData::Unsloth(adapter.read()?))),
            Self::Goose(adapter) => Ok(Some(SqliteInputData::Goose(adapter.read()?))),
            Self::Hermes(adapter) => Ok(Some(SqliteInputData::Hermes(adapter.read()?))),
            Self::OpenClaw(adapter) => Ok(Some(SqliteInputData::OpenClaw(adapter.read()?))),
            Self::Zed(adapter) => Ok(Some(SqliteInputData::Zed(adapter.read()?))),
            Self::WorkBuddySqlite(adapter) => {
                Ok(Some(SqliteInputData::WorkBuddySqlite(adapter.read()?)))
            }
            Self::AntigravitySqlite(adapter) => {
                Ok(Some(SqliteInputData::AntigravitySqlite(adapter.read()?)))
            }
            Self::OrdinaryJsonl(_)
            | Self::Lmstudio(_)
            | Self::Kimi(_)
            | Self::CommandCode(_)
            | Self::Grok(_)
            | Self::ZcodeJsonl(_)
            | Self::QodercnJsonl(_)
            | Self::Dsh(_)
            | Self::PromaJsonl(_)
            | Self::Gemini(_)
            | Self::Cursor(_)
            | Self::Cline(_)
            | Self::Amp(_)
            | Self::Droid(_)
            | Self::Kiro(_)
            | Self::Copilot(_)
            | Self::CodeBuddyLog(_)
            | Self::CompressedJsonl(_) => Ok(None),
        }
    }

    pub(crate) fn source_path(&self) -> Option<&Path> {
        match self {
            Self::OrdinaryJsonl(input) => Some(&input.path),
            Self::Lmstudio(input) => Some(&input.path),
            Self::CommandCode(input) => Some(&input.path),
            Self::Grok(input) => Some(&input.path),
            Self::ZcodeJsonl(input) => Some(&input.path),
            Self::QodercnJsonl(input) => Some(&input.path),
            Self::Dsh(input) => Some(&input.path),
            Self::PromaJsonl(input) => Some(&input.path),
            Self::Kimi(input) => Some(&input.path),
            Self::Gemini(input) => Some(&input.path),
            Self::Cursor(input) => Some(&input.path),
            Self::Cline(input) => Some(&input.path),
            Self::Amp(input) => Some(&input.path),
            Self::Droid(input) => Some(&input.path),
            Self::Kiro(input) => Some(&input.path),
            Self::Copilot(input) => Some(&input.path),
            Self::CodeBuddyLog(input) => Some(&input.path),
            Self::CompressedJsonl(input) => Some(&input.path),
            Self::SessionArchive(adapter) => Some(adapter.path()),
            Self::CodexLocalUsage(adapter) => Some(adapter.path()),
            Self::OpenCode(adapter) => Some(adapter.path()),
            Self::Kilo(adapter) => Some(adapter.path()),
            Self::Mimo(adapter) => Some(adapter.path()),
            Self::Zcode(adapter) => Some(adapter.path()),
            Self::Qodercn(adapter) => Some(adapter.path()),
            Self::Catpaw(adapter) => Some(adapter.path()),
            Self::Cherrystudio(adapter) => Some(adapter.path()),
            Self::Unsloth(adapter) => Some(adapter.path()),
            Self::Goose(adapter) => Some(adapter.path()),
            Self::Hermes(adapter) => Some(adapter.path()),
            Self::OpenClaw(adapter) => Some(adapter.path()),
            Self::Zed(adapter) => Some(adapter.path()),
            Self::WorkBuddySqlite(adapter) => Some(adapter.path()),
            Self::AntigravitySqlite(adapter) => Some(adapter.path()),
        }
    }

    pub fn read_batch(&mut self) -> io::Result<InputBatch> {
        match self {
            Self::CommandCode(input) => {
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
            Self::Grok(input) => {
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
            Self::ZcodeJsonl(input) => {
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
            Self::Dsh(input) => input.read_batch(),
            Self::Lmstudio(input) => input.read_batch(),
            Self::QodercnJsonl(input) => {
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
            Self::PromaJsonl(input) => {
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
            Self::Gemini(input) => input.read_batch(),
            Self::Kimi(input) => {
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
            Self::Cursor(input) => input.read_batch(),
            Self::Cline(input) => input.read_batch(),
            Self::Amp(input) => input.read_batch(),
            Self::Droid(input) => input.read_batch(),
            Self::Kiro(input) => input.read_batch(),
            Self::CodeBuddyLog(input) => input.read_batch(),
            Self::Copilot(input) => {
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
            Self::SessionArchive(_)
            | Self::CodexLocalUsage(_)
            | Self::OpenCode(_)
            | Self::Kilo(_)
            | Self::Mimo(_)
            | Self::Zcode(_)
            | Self::Qodercn(_)
            | Self::Catpaw(_)
            | Self::Cherrystudio(_)
            | Self::Unsloth(_)
            | Self::Goose(_)
            | Self::Hermes(_)
            | Self::OpenClaw(_)
            | Self::Zed(_)
            | Self::WorkBuddySqlite(_)
            | Self::AntigravitySqlite(_) => Err(invalid("sqlite input has no JSONL batch")),
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

impl GeminiInput {
    fn reset(&mut self) {
        match &mut self.kind {
            GeminiInputKind::Jsonl(cursor) => cursor.reset(),
            GeminiInputKind::Document(cursor) => cursor.stamp = None,
        }
    }

    fn is_current(&self) -> io::Result<bool> {
        match &self.kind {
            GeminiInputKind::Jsonl(cursor) => cursor.is_current(&self.path),
            GeminiInputKind::Document(cursor) => {
                let stamp = current_stamp(&self.path)?;
                Ok(cursor.stamp.as_ref() == Some(&stamp))
            }
        }
    }

    fn read_batch(&mut self) -> io::Result<InputBatch> {
        match &mut self.kind {
            GeminiInputKind::Jsonl(cursor) => {
                let batch = cursor.read_batch(&self.path)?;
                Ok(InputBatch {
                    records: batch.records,
                    physical_bytes_read: batch.bytes_read,
                    decoded_bytes_read: batch.bytes_read,
                    reset: batch.reset,
                    incomplete_tail: batch.incomplete_tail,
                    more_available: batch.more_available,
                })
            }
            GeminiInputKind::Document(cursor) => cursor.read_batch(&self.path),
        }
    }
}

impl DocumentInput {
    fn reset(&mut self) {
        self.cursor.stamp = None;
    }

    fn is_current(&self) -> io::Result<bool> {
        let stamp = current_stamp(&self.path)?;
        Ok(self.cursor.stamp.as_ref() == Some(&stamp))
    }

    fn read_batch(&mut self) -> io::Result<InputBatch> {
        self.cursor.read_batch(&self.path)
    }
}

impl DocumentCursor {
    fn read_batch(&mut self, path: &Path) -> io::Result<InputBatch> {
        let mut file = File::open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(invalid("Gemini source is not a file"));
        }
        let stamp = SourceStamp::new(path, &metadata);
        if self.stamp.as_ref() == Some(&stamp) {
            return Ok(InputBatch {
                records: Vec::new(),
                physical_bytes_read: 0,
                decoded_bytes_read: 0,
                reset: false,
                incomplete_tail: false,
                more_available: false,
            });
        }
        let length = metadata.len();
        if length > MAX_RECORD_BYTES as u64 {
            return Err(invalid("Gemini JSON document exceeds 16 MiB limit"));
        }
        let mut bytes = Vec::with_capacity(length as usize);
        file.read_to_end(&mut bytes)?;
        if bytes.len() as u64 != length || current_stamp(path)? != stamp {
            return Err(invalid("Gemini JSON document changed during read"));
        }
        self.stamp = Some(stamp);
        Ok(InputBatch {
            records: vec![bytes],
            physical_bytes_read: length as usize,
            decoded_bytes_read: length as usize,
            reset: true,
            incomplete_tail: false,
            more_available: false,
        })
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
                workspace_key: None,
                workspace_label: None,
                title: None,
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

fn read_kilo(path: &Path) -> Result<Arc<[ResponseUsage]>, SqliteSourceError> {
    let snapshot = SqliteSnapshot::open(path)?;
    let transaction = snapshot.connection.unchecked_transaction()?;
    require_columns(&transaction, "message", &["id", "session_id", "data"])?;
    let candidates: i64 = transaction.query_row(
        "SELECT COUNT(*) FROM message
         WHERE json_valid(data)
           AND json_extract(data, '$.role') = 'assistant'
           AND json_extract(data, '$.tokens') IS NOT NULL",
        [],
        |row| row.get(0),
    )?;
    let candidates: usize = candidates
        .try_into()
        .map_err(|_| SqliteSourceError::Capacity)?;
    if candidates > MAX_OPENCODE_RESPONSES {
        return Err(SqliteSourceError::Capacity);
    }
    let mut reader = KiloReader::new().with_fallback_timestamp(source_modified_ms(path));
    let mut offset = 0_i64;
    loop {
        let mut statement = transaction.prepare(
            "SELECT id, session_id, data FROM message
             WHERE json_valid(data)
               AND json_extract(data, '$.role') = 'assistant'
               AND json_extract(data, '$.tokens') IS NOT NULL
             ORDER BY session_id, id LIMIT ?1 OFFSET ?2",
        )?;
        let mut rows = statement.query(params![SQLITE_PAGE_ROWS, offset])?;
        let mut page_rows = 0_usize;
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let session_id: String = row.get(1)?;
            let data = row
                .get_ref(2)?
                .as_bytes()
                .map_err(|_| SqliteSourceError::Invalid)?;
            if data.len() > MAX_RECORD_BYTES {
                return Err(SqliteSourceError::Capacity);
            }
            reader
                .ingest(&id, &session_id, data)
                .map_err(map_opencode_error)?;
            page_rows += 1;
        }
        if page_rows == 0 {
            break;
        }
        offset = offset
            .checked_add(i64::try_from(page_rows).map_err(|_| SqliteSourceError::Capacity)?)
            .ok_or(SqliteSourceError::Capacity)?;
    }
    drop(transaction);
    snapshot.verify(path)?;
    Ok(Arc::from(reader.snapshot()))
}

fn read_antigravity_sqlite(path: &Path) -> Result<Arc<[ResponseUsage]>, SqliteSourceError> {
    let snapshot = SqliteSnapshot::open(path)?;
    let transaction = snapshot.connection.unchecked_transaction()?;
    require_columns(&transaction, "gen_metadata", &["idx", "data"])?;
    let session = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .ok_or(SqliteSourceError::Invalid)?
        .to_owned();
    let mut reader = AntigravityDbReader::new(session, source_modified_ms(path).unwrap_or(0));
    let mut offset = 0_i64;
    loop {
        let mut statement =
            transaction.prepare("SELECT data FROM gen_metadata ORDER BY idx LIMIT ?1 OFFSET ?2")?;
        let mut rows = statement.query(params![SQLITE_PAGE_ROWS, offset])?;
        let mut page_rows = 0_usize;
        while let Some(row) = rows.next()? {
            let data = row
                .get_ref(0)?
                .as_bytes()
                .map_err(|_| SqliteSourceError::Invalid)?;
            if data.len() > MAX_RECORD_BYTES {
                return Err(SqliteSourceError::Capacity);
            }
            reader.ingest_blob(data).map_err(map_opencode_error)?;
            page_rows += 1;
        }
        if page_rows == 0 {
            break;
        }
        offset = offset
            .checked_add(i64::try_from(page_rows).map_err(|_| SqliteSourceError::Capacity)?)
            .ok_or(SqliteSourceError::Capacity)?;
    }
    drop(transaction);
    snapshot.verify(path)?;
    Ok(Arc::from(reader.snapshot()))
}

fn read_workbuddy_sqlite(path: &Path) -> Result<Arc<[ResponseUsage]>, SqliteSourceError> {
    let snapshot = SqliteSnapshot::open(path)?;
    let transaction = snapshot.connection.unchecked_transaction()?;
    require_columns(
        &transaction,
        "session_usage",
        &["session_id", "used", "updated_at"],
    )?;
    let sessions = table_columns(&transaction, "sessions")?;
    let model = if sessions
        .as_ref()
        .is_some_and(|columns| columns.contains("model"))
    {
        "s.model"
    } else {
        "NULL"
    };
    let mut rows = Vec::new();
    let mut offset = 0_i64;
    loop {
        let mut statement = transaction.prepare(&format!(
            "SELECT su.session_id, su.used, su.updated_at, {model}
             FROM session_usage su LEFT JOIN sessions s ON s.id = su.session_id
             WHERE su.used IS NOT NULL AND su.used > 0
               AND su.updated_at IS NOT NULL AND su.updated_at > 0
             ORDER BY su.session_id, su.updated_at LIMIT ?1 OFFSET ?2"
        ))?;
        let mut page = statement.query(params![SQLITE_PAGE_ROWS, offset])?;
        let mut page_rows = 0_usize;
        while let Some(row) = page.next()? {
            let session: String = row.get(0)?;
            let used: i64 = row.get(1)?;
            let updated_at: i64 = row.get(2)?;
            let model: Option<String> = row.get(3)?;
            let used = u64::try_from(used).map_err(|_| SqliteSourceError::Invalid)?;
            rows.push(
                sqlite_row_to_usage(&session, model.as_deref(), used, updated_at)
                    .map_err(map_opencode_error)?,
            );
            page_rows += 1;
        }
        if page_rows == 0 {
            break;
        }
        offset = offset
            .checked_add(i64::try_from(page_rows).map_err(|_| SqliteSourceError::Capacity)?)
            .ok_or(SqliteSourceError::Capacity)?;
    }
    drop(transaction);
    snapshot.verify(path)?;
    if rows.len() > MAX_LOCAL_USAGE_ROWS {
        return Err(SqliteSourceError::Capacity);
    }
    Ok(Arc::from(rows))
}

fn read_zed(path: &Path) -> Result<Arc<[ResponseUsage]>, SqliteSourceError> {
    const MAX_THREAD_JSON_BYTES: u64 = 32 * 1024 * 1024;
    let snapshot = SqliteSnapshot::open(path)?;
    let transaction = snapshot.connection.unchecked_transaction()?;
    require_columns(
        &transaction,
        "threads",
        &["id", "updated_at", "data_type", "data"],
    )?;
    let columns = table_columns(&transaction, "threads")?;
    let created_at = if columns
        .as_ref()
        .is_some_and(|columns| columns.contains("created_at"))
    {
        "created_at"
    } else {
        "NULL"
    };
    let mut reader = ZedReader::new();
    let mut offset = 0_i64;
    loop {
        let mut statement = transaction.prepare(&format!(
            "SELECT id, updated_at, {created_at}, data_type, data FROM threads ORDER BY id LIMIT ?1 OFFSET ?2"
        ))?;
        let mut rows = statement.query(params![SQLITE_PAGE_ROWS, offset])?;
        let mut page_rows = 0_usize;
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let updated_at: String = row.get(1)?;
            let created_at: Option<String> = row.get(2)?;
            let data_type: String = row.get(3)?;
            let data = row
                .get_ref(4)?
                .as_bytes()
                .map_err(|_| SqliteSourceError::Invalid)?;
            let json = match data_type.trim().to_ascii_lowercase().as_str() {
                "json" => data.to_vec(),
                "zstd" => decode_zed_zstd(data, MAX_THREAD_JSON_BYTES)?,
                _ => return Err(SqliteSourceError::Unsupported),
            };
            if json.len() as u64 > MAX_THREAD_JSON_BYTES {
                return Err(SqliteSourceError::Capacity);
            }
            let thread: Value =
                serde_json::from_slice(&json).map_err(|_| SqliteSourceError::Invalid)?;
            reader
                .ingest_thread(&id, &updated_at, created_at.as_deref(), &thread)
                .map_err(map_opencode_error)?;
            page_rows += 1;
        }
        if page_rows == 0 {
            break;
        }
        offset = offset
            .checked_add(i64::try_from(page_rows).map_err(|_| SqliteSourceError::Capacity)?)
            .ok_or(SqliteSourceError::Capacity)?;
    }
    drop(transaction);
    snapshot.verify(path)?;
    Ok(Arc::from(reader.snapshot()))
}

fn decode_zed_zstd(data: &[u8], limit: u64) -> Result<Vec<u8>, SqliteSourceError> {
    let decoder =
        StreamingDecoder::new_with_max_window_size(Cursor::new(data), MAX_ZSTD_WINDOW_BYTES)
            .map_err(|_| SqliteSourceError::Unsupported)?;
    let mut decoded = Vec::new();
    decoder
        .take(limit.saturating_add(1))
        .read_to_end(&mut decoded)
        .map_err(|_| SqliteSourceError::Invalid)?;
    if decoded.len() as u64 > limit {
        return Err(SqliteSourceError::Capacity);
    }
    Ok(decoded)
}

/// OpenClaw's `transcript-payload.ts` bounds both the zstd frame and its
/// decoded UTF-8 bytes at 4 MiB, so a damaged store cannot make the reader
/// allocate without bound.
const MAX_OPENCLAW_EVENT_BYTES: i64 = 4 * 1024 * 1024;

/// OpenClaw's per-agent `openclaw-agent.sqlite`. Usage lives in
/// `transcript_events`, one event per row, either `event_json` text or a zstd
/// `event_zstd` blob whose decoded length is `event_utf8_bytes`. The session a
/// row belongs to is the row's own `session_id`; `session_windows` supplies a
/// session-level provider/model fallback when the transcript names none.
fn read_openclaw(path: &Path) -> Result<Arc<[ResponseUsage]>, SqliteSourceError> {
    let snapshot = SqliteSnapshot::open(path)?;
    let transaction = snapshot.connection.unchecked_transaction()?;
    if table_columns(&transaction, "transcript_events")?.is_none() {
        return Err(SqliteSourceError::Schema);
    }
    require_columns(&transaction, "transcript_events", &["session_id"])?;
    let columns = table_columns(&transaction, "transcript_events")?.unwrap_or_default();
    let compressed = columns.contains("event_zstd") && columns.contains("event_utf8_bytes");
    if !compressed && !columns.contains("event_json") {
        return Err(SqliteSourceError::Schema);
    }
    let payload = if compressed {
        "event_json, event_zstd, event_utf8_bytes"
    } else {
        "event_json, NULL, NULL"
    };
    let mut reader =
        OpenClawReader::new("openclaw").with_fallback_timestamp(source_modified_ms(path));
    let mut offset = 0_i64;
    loop {
        let mut statement = transaction.prepare(&format!(
            "SELECT e.session_id, {payload}, e.created_at
             FROM transcript_events e
             ORDER BY e.session_id, e.seq LIMIT ?1 OFFSET ?2"
        ))?;
        let mut rows = statement.query(params![SQLITE_PAGE_ROWS, offset])?;
        let mut page_rows = 0_usize;
        while let Some(row) = rows.next()? {
            let session_id: String = row.get(0)?;
            let event_json: Option<String> = row.get(1)?;
            let raw_bytes: Option<i64> = row.get(3)?;
            let created_at_secs: Option<f64> = row.get(4)?;
            let json = match event_json {
                Some(json) => json,
                None => {
                    if !compressed {
                        return Err(SqliteSourceError::Invalid);
                    }
                    let blob = row
                        .get_ref(2)?
                        .as_blob()
                        .map_err(|_| SqliteSourceError::Invalid)?;
                    let declared = raw_bytes.unwrap_or(0);
                    if !(1..=MAX_OPENCLAW_EVENT_BYTES).contains(&declared) {
                        return Err(SqliteSourceError::Capacity);
                    }
                    let decoded =
                        decode_zed_zstd(blob, u64::try_from(declared).unwrap_or(u64::MAX))?;
                    if decoded.len() != declared as usize {
                        return Err(SqliteSourceError::Invalid);
                    }
                    String::from_utf8(decoded).map_err(|_| SqliteSourceError::Invalid)?
                }
            };
            // A row that does not decode is skipped on its own rather than
            // failing the whole store.
            let Ok(value) = serde_json::from_str::<Value>(&json) else {
                page_rows += 1;
                continue;
            };
            let Some(object) = value.as_object() else {
                page_rows += 1;
                continue;
            };
            let row_ms = created_at_secs
                .filter(|value| value.is_finite() && *value > 0.0)
                .map(|value| (value * 1000.0) as i64);
            reader
                .ingest_session_event(&session_id, row_ms, object)
                .map_err(map_opencode_error)?;
            page_rows += 1;
        }
        if page_rows == 0 {
            break;
        }
        offset = offset
            .checked_add(i64::try_from(page_rows).map_err(|_| SqliteSourceError::Capacity)?)
            .ok_or(SqliteSourceError::Capacity)?;
    }
    drop(transaction);
    snapshot.verify(path)?;
    Ok(Arc::from(reader.snapshot()))
}

fn read_goose(path: &Path) -> Result<Arc<[ResponseUsage]>, SqliteSourceError> {
    let snapshot = SqliteSnapshot::open(path)?;
    let transaction = snapshot.connection.unchecked_transaction()?;
    require_columns(
        &transaction,
        "sessions",
        &[
            "id",
            "model_config_json",
            "provider_name",
            "created_at",
            "total_tokens",
            "input_tokens",
            "output_tokens",
            "accumulated_total_tokens",
            "accumulated_input_tokens",
            "accumulated_output_tokens",
        ],
    )?;
    let candidates: i64 = transaction.query_row(
        "SELECT COUNT(*) FROM sessions
         WHERE model_config_json IS NOT NULL AND TRIM(model_config_json) != ''",
        [],
        |row| row.get(0),
    )?;
    let candidates: usize = candidates
        .try_into()
        .map_err(|_| SqliteSourceError::Capacity)?;
    if candidates > MAX_OPENCODE_RESPONSES {
        return Err(SqliteSourceError::Capacity);
    }
    let mut reader = GooseReader::new().with_fallback_timestamp(source_modified_ms(path));
    let mut offset = 0_i64;
    loop {
        let mut statement = transaction.prepare(
            "SELECT id, model_config_json, provider_name, created_at,
                    total_tokens, input_tokens, output_tokens,
                    accumulated_total_tokens, accumulated_input_tokens, accumulated_output_tokens
             FROM sessions
             WHERE model_config_json IS NOT NULL AND TRIM(model_config_json) != ''
             ORDER BY id LIMIT ?1 OFFSET ?2",
        )?;
        let mut rows = statement.query(params![SQLITE_PAGE_ROWS, offset])?;
        let mut page_rows = 0_usize;
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let model_config_json: String = row.get(1)?;
            let provider_name: Option<String> = row.get(2)?;
            let created_at: String = row.get(3)?;
            let total_tokens: Option<i64> = row.get(4)?;
            let input_tokens: Option<i64> = row.get(5)?;
            let output_tokens: Option<i64> = row.get(6)?;
            let accumulated_total_tokens: Option<i64> = row.get(7)?;
            let accumulated_input_tokens: Option<i64> = row.get(8)?;
            let accumulated_output_tokens: Option<i64> = row.get(9)?;
            reader
                .ingest(&GooseRow {
                    id: &id,
                    model_config_json: &model_config_json,
                    provider_name: provider_name.as_deref(),
                    created_at: &created_at,
                    total_tokens,
                    input_tokens,
                    output_tokens,
                    accumulated_total_tokens,
                    accumulated_input_tokens,
                    accumulated_output_tokens,
                })
                .map_err(map_opencode_error)?;
            page_rows += 1;
        }
        if page_rows == 0 {
            break;
        }
        offset = offset
            .checked_add(i64::try_from(page_rows).map_err(|_| SqliteSourceError::Capacity)?)
            .ok_or(SqliteSourceError::Capacity)?;
    }
    drop(transaction);
    snapshot.verify(path)?;
    Ok(Arc::from(reader.snapshot()))
}

fn source_modified_ms(path: &Path) -> Option<i64> {
    std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
}

/// Hermes keeps usage in `session_model_usage` (per model and provider) joined
/// to `sessions` (start time and totals). Builds that predate the child table
/// carry totals on `sessions` alone, so the per-model rows are preferred and a
/// session without any of them falls back to its session row. The two passes
/// are disjoint by session, which is what prevents double counting.
fn read_hermes(path: &Path) -> Result<Arc<[ResponseUsage]>, SqliteSourceError> {
    let snapshot = SqliteSnapshot::open(path)?;
    let transaction = snapshot.connection.unchecked_transaction()?;
    require_columns(
        &transaction,
        "sessions",
        &[
            "id",
            "model",
            "billing_provider",
            "started_at",
            "input_tokens",
            "output_tokens",
            "cache_read_tokens",
            "cache_write_tokens",
            "reasoning_tokens",
        ],
    )?;
    let per_model = table_columns(&transaction, "session_model_usage")?;
    let per_model = match per_model {
        Some(columns) => {
            if !has_columns(
                &columns,
                &[
                    "session_id",
                    "model",
                    "billing_provider",
                    "input_tokens",
                    "output_tokens",
                    "cache_read_tokens",
                    "cache_write_tokens",
                    "reasoning_tokens",
                ],
            ) {
                return Err(SqliteSourceError::Unsupported);
            }
            true
        }
        None => false,
    };
    let mut reader = HermesReader::new().with_fallback_timestamp(source_modified_ms(path));
    let mut covered = BTreeSet::new();
    if per_model {
        read_hermes_rows(
            &transaction,
            &mut reader,
            Some(&mut covered),
            &BTreeSet::new(),
            "SELECT smu.session_id, smu.model, smu.billing_provider, s.started_at,
                    smu.input_tokens, smu.output_tokens, smu.cache_read_tokens,
                    smu.cache_write_tokens, smu.reasoning_tokens
             FROM session_model_usage smu
             JOIN sessions s ON s.id = smu.session_id
             WHERE smu.model IS NOT NULL AND TRIM(smu.model) != ''
             ORDER BY smu.session_id, smu.model, smu.billing_provider",
        )?;
    }
    // Only sessions the per-model pass did not cover fall back to their
    // session totals, so the two result sets cannot double count.
    let uncovered = covered.clone();
    read_hermes_rows(
        &transaction,
        &mut reader,
        None,
        &uncovered,
        "SELECT id, model, billing_provider, started_at,
                input_tokens, output_tokens, cache_read_tokens,
                cache_write_tokens, reasoning_tokens
         FROM sessions
         WHERE model IS NOT NULL AND TRIM(model) != ''
         ORDER BY id",
    )
    .and_then(|()| {
        drop(transaction);
        snapshot.verify(path)?;
        Ok(Arc::from(reader.snapshot()))
    })
}

fn read_hermes_rows(
    transaction: &rusqlite::Transaction<'_>,
    reader: &mut HermesReader,
    mut covered: Option<&mut BTreeSet<String>>,
    skip: &BTreeSet<String>,
    query: &str,
) -> Result<(), SqliteSourceError> {
    let mut offset = 0_i64;
    loop {
        let mut statement = transaction.prepare(&format!("{query} LIMIT ?1 OFFSET ?2"))?;
        let mut rows = statement.query(params![SQLITE_PAGE_ROWS, offset])?;
        let mut page_rows = 0_usize;
        while let Some(row) = rows.next()? {
            page_rows += 1;
            let session_id: String = row.get(0)?;
            if skip.contains(&session_id) {
                continue;
            }
            let model: String = row.get(1)?;
            let billing_provider: Option<String> = row.get(2)?;
            let started_at: f64 = row.get(3)?;
            let input_tokens: Option<i64> = row.get(4)?;
            let output_tokens: Option<i64> = row.get(5)?;
            let cache_read_tokens: Option<i64> = row.get(6)?;
            let cache_write_tokens: Option<i64> = row.get(7)?;
            let reasoning_tokens: Option<i64> = row.get(8)?;
            // Every per-model row is its own observation; the set only records
            // that the session is covered so the session-totals pass skips it.
            if let Some(set) = covered.as_deref_mut() {
                set.insert(session_id.clone());
            }
            reader
                .ingest(&HermesRow {
                    session_id: &session_id,
                    model: &model,
                    billing_provider: billing_provider.as_deref(),
                    started_at,
                    input_tokens,
                    output_tokens,
                    cache_read_tokens,
                    cache_write_tokens,
                    reasoning_tokens,
                })
                .map_err(map_opencode_error)?;
        }
        if page_rows == 0 {
            break;
        }
        offset = offset
            .checked_add(i64::try_from(page_rows).map_err(|_| SqliteSourceError::Capacity)?)
            .ok_or(SqliteSourceError::Capacity)?;
    }
    Ok(())
}

fn read_opencode(path: &Path) -> Result<Arc<[ResponseUsage]>, SqliteSourceError> {
    let snapshot = SqliteSnapshot::open(path)?;
    let transaction = snapshot.connection.unchecked_transaction()?;
    let session_message = table_columns(&transaction, "session_message")?;
    let message = table_columns(&transaction, "message")?;
    let current = session_message
        .as_ref()
        .is_some_and(|columns| has_columns(columns, &["id", "session_id", "data", "type"]));
    let legacy = message
        .as_ref()
        .is_some_and(|columns| has_columns(columns, &["id", "session_id", "data"]));
    if session_message.is_some() && !current {
        return Err(SqliteSourceError::Unsupported);
    }
    if message.is_some() && !legacy {
        return Err(SqliteSourceError::Unsupported);
    }
    if !current && !legacy {
        return Err(SqliteSourceError::Schema);
    }

    let mut reader = OpenCodeReader::new();
    let current_sessions = if current {
        read_opencode_current(&transaction, &mut reader)?
    } else {
        BTreeSet::new()
    };
    if legacy {
        let columns = message.as_ref().expect("legacy columns validated");
        if current && !current_sessions.is_empty() {
            detect_opencode_legacy_supplements(&transaction)?;
        }
        read_opencode_legacy(&transaction, columns, &current_sessions, &mut reader)?;
    }
    drop(transaction);
    snapshot.verify(path)?;
    Ok(Arc::from(reader.snapshot()))
}

/// Read MiMo Code's OpenCode-shaped `message` table.
///
/// MiMo stores assistant turns in OpenCode's schema, so this selects the same
/// rows `read_opencode_legacy` does, with two MiMo-specific changes: a missing
/// `session` table is not an error (the join is optional upstream), and the
/// database file name is threaded into each record so channel-suffixed
/// databases with overlapping row ids keep distinct identities.
fn read_mimo(path: &Path) -> Result<Arc<[ResponseUsage]>, SqliteSourceError> {
    let snapshot = SqliteSnapshot::open(path)?;
    let transaction = snapshot.connection.unchecked_transaction()?;
    let Some(columns) = table_columns(&transaction, "message")? else {
        return Err(SqliteSourceError::Schema);
    };
    if !has_columns(&columns, &["id", "session_id", "data"]) {
        return Err(SqliteSourceError::Unsupported);
    }
    let database = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(SqliteSourceError::Unsupported)?
        .to_owned();

    let candidates: i64 = connection_query_count(
        &transaction,
        "SELECT COUNT(*) FROM message
         WHERE json_valid(data) AND json_extract(data, '$.role') = 'assistant'
           AND json_extract(data, '$.tokens') IS NOT NULL",
    )?;
    let candidates: usize = candidates
        .try_into()
        .map_err(|_| SqliteSourceError::Capacity)?;
    if candidates > mimo::MAX_RESPONSES {
        return Err(SqliteSourceError::Capacity);
    }

    let time_column = if columns.contains("time_created") {
        "time_created"
    } else {
        "NULL"
    };
    let sql = format!(
        "SELECT id, session_id, data, {time_column}
         FROM message
         WHERE json_valid(data) AND json_extract(data, '$.role') = 'assistant'
           AND json_extract(data, '$.tokens') IS NOT NULL
         ORDER BY session_id, id LIMIT ?1 OFFSET ?2"
    );
    let mut reader = MimoReader::new();
    let mut offset = 0_i64;
    loop {
        let mut statement = transaction.prepare(&sql)?;
        let mut rows = statement.query(params![SQLITE_PAGE_ROWS, offset])?;
        let mut page_rows = 0_usize;
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let session: String = row.get(1)?;
            let data = row
                .get_ref(2)?
                .as_bytes()
                .map_err(|_| SqliteSourceError::Invalid)?;
            if data.len() > MAX_RECORD_BYTES {
                return Err(SqliteSourceError::Capacity);
            }
            reader
                .ingest(mimo::MimoRecord {
                    message_id: &id,
                    session_id: &session,
                    data,
                    fallback_timestamp_ms: row.get(3)?,
                    database: &database,
                })
                .map_err(map_mimo_error)?;
            page_rows += 1;
        }
        if page_rows == 0 {
            break;
        }
        offset = offset
            .checked_add(i64::try_from(page_rows).map_err(|_| SqliteSourceError::Capacity)?)
            .ok_or(SqliteSourceError::Capacity)?;
    }
    drop(transaction);
    snapshot.verify(path)?;
    Ok(Arc::from(reader.snapshot()))
}

fn read_catpaw(
    path: &Path,
    edition: catpaw::Edition,
) -> Result<Arc<[ResponseUsage]>, SqliteSourceError> {
    let snapshot = SqliteSnapshot::open(path)?;
    let transaction = snapshot.connection.unchecked_transaction()?;
    require_columns(
        &transaction,
        "ui_sdk_messages",
        &["conversation_id", "message_id", "payload", "role"],
    )?;
    let namespace = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(SqliteSourceError::Unsupported)?
        .to_owned();
    // Only assistant rows with a non-null usage block are candidates. The
    // session selection is joined on `conversation_id` so each message is
    // labelled with the model that session currently names.
    let candidates: i64 = connection_query_count(
        &transaction,
        "SELECT COUNT(*) FROM ui_sdk_messages
         WHERE role = 'assistant'
           AND json_type(payload, '$.extra.contextInfo.usage') <> 'null'",
    )?;
    let candidates: usize = candidates
        .try_into()
        .map_err(|_| SqliteSourceError::Capacity)?;
    if candidates > catpaw::MAX_RESPONSES {
        return Err(SqliteSourceError::Capacity);
    }
    let sql = "SELECT m.conversation_id, m.message_id, m.created_at_ms, m.updated_at_ms,
                      m.schema_version,
                      json_extract(m.payload, '$.extra.contextInfo.usage'),
                      CASE WHEN json_type(m.payload, '$.extra.contextInfo.totalUsageTokens')
                                IN ('integer', 'real')
                           THEN json_extract(m.payload, '$.extra.contextInfo.totalUsageTokens')
                      END,
                      (SELECT json_object(
                          'persistedModelId', json_extract(s.extra, '$.persistedModelId'),
                          'persistedModelMode', json_extract(s.extra, '$.persistedModelMode'),
                          'persistedModelSelection', json_extract(s.extra, '$.persistedModelSelection'),
                          'initialSelectedModelId', json_extract(s.extra, '$.initialSelectedModelId'),
                          'initialModelMode', json_extract(s.extra, '$.initialModelMode'),
                          'initialModelSelection', json_extract(s.extra, '$.initialModelSelection'))
                       FROM sessions s WHERE s.conversation_id = m.conversation_id LIMIT 1)
               FROM ui_sdk_messages m
               WHERE m.role = 'assistant'
                 AND json_type(m.payload, '$.extra.contextInfo.usage') <> 'null'
               ORDER BY m.updated_at_ms, m.seq LIMIT ?1 OFFSET ?2";
    let mut reader = catpaw::CatpawReader::new(edition);
    let mut offset = 0_i64;
    loop {
        let mut statement = transaction.prepare(sql)?;
        let mut rows = statement.query(params![SQLITE_PAGE_ROWS, offset])?;
        let mut page_rows = 0_usize;
        while let Some(row) = rows.next()? {
            let conversation: String = row.get(0)?;
            let message: String = row.get(1)?;
            let created: Option<i64> = row.get(2)?;
            let updated: Option<i64> = row.get(3)?;
            let schema: i64 = row.get(4)?;
            let usage: Option<String> = row.get(5)?;
            let recorded: Option<i64> = row.get(6)?;
            let selection: Option<String> = row.get(7)?;
            let Some(usage) = usage else {
                page_rows += 1;
                continue;
            };
            reader
                .ingest(catpaw::CatpawRow {
                    conversation_id: &conversation,
                    message_id: &message,
                    created_at_ms: created,
                    updated_at_ms: updated,
                    schema_version: schema,
                    usage: usage.as_bytes(),
                    recorded_total: recorded,
                    selection: selection.as_deref().map(str::as_bytes),
                    namespace: &namespace,
                })
                .map_err(map_catpaw_error)?;
            page_rows += 1;
        }
        if page_rows == 0 {
            break;
        }
        offset = offset
            .checked_add(i64::try_from(page_rows).map_err(|_| SqliteSourceError::Capacity)?)
            .ok_or(SqliteSourceError::Capacity)?;
    }
    drop(transaction);
    snapshot.verify(path)?;
    Ok(Arc::from(reader.snapshot()))
}

fn map_catpaw_error(error: ReaderError) -> SqliteSourceError {
    match error {
        ReaderError::Capacity(_) => SqliteSourceError::Capacity,
        ReaderError::Overflow => SqliteSourceError::Capacity,
        ReaderError::InvalidRecord
        | ReaderError::InvalidTimestamp
        | ReaderError::Unsupported(_) => SqliteSourceError::Invalid,
    }
}

fn read_zcode(path: &Path) -> Result<Arc<[ResponseUsage]>, SqliteSourceError> {
    let snapshot = SqliteSnapshot::open(path)?;
    let transaction = snapshot.connection.unchecked_transaction()?;
    require_columns(
        &transaction,
        "model_usage",
        &["id", "session_id", "input_tokens", "output_tokens"],
    )?;
    // Probe the column directly rather than inferring a legacy schema from the
    // modern query failing to prepare: that query also LEFT JOINs `session`, so
    // it can fail for reasons unrelated to this column. Conflating the two would
    // send modern rows with NULL totals through the unconditional subtraction.
    let legacy_schema = {
        let mut statement = transaction.prepare("PRAGMA table_info(model_usage)")?;
        let mut rows = statement.query([])?;
        let mut columns = BTreeSet::new();
        while let Some(row) = rows.next()? {
            columns.insert(row.get::<_, String>(1)?);
        }
        !columns.contains("computed_total_tokens")
    };
    let candidates: i64 = connection_query_count(
        &transaction,
        "SELECT COUNT(*) FROM model_usage
         WHERE COALESCE(input_tokens, 0) + COALESCE(output_tokens, 0)
             + COALESCE(reasoning_tokens, 0)
             + COALESCE(cache_read_input_tokens, 0)
             + COALESCE(cache_creation_input_tokens, 0) > 0",
    )?;
    let candidates: usize = candidates
        .try_into()
        .map_err(|_| SqliteSourceError::Capacity)?;
    if candidates > zcode::MAX_RESPONSES {
        return Err(SqliteSourceError::Capacity);
    }
    let total_column = if legacy_schema {
        "NULL"
    } else {
        "computed_total_tokens"
    };
    let sql = format!(
        "SELECT id, session_id, model_id, started_at, completed_at,
                input_tokens, output_tokens, reasoning_tokens,
                cache_read_input_tokens, cache_creation_input_tokens,
                {total_column}
         FROM model_usage
         WHERE COALESCE(input_tokens, 0) + COALESCE(output_tokens, 0)
             + COALESCE(reasoning_tokens, 0)
             + COALESCE(cache_read_input_tokens, 0)
             + COALESCE(cache_creation_input_tokens, 0) > 0
         ORDER BY COALESCE(completed_at, started_at, 0), id LIMIT ?1 OFFSET ?2"
    );
    let mut responses = Vec::new();
    let mut offset = 0_i64;
    loop {
        let mut statement = transaction.prepare(&sql)?;
        let mut rows = statement.query(params![SQLITE_PAGE_ROWS, offset])?;
        let mut page_rows = 0_usize;
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let session: Option<String> = row.get(1)?;
            let model: Option<String> = row.get(2)?;
            let Some(usage) = zcode::normalize_row(&zcode::ZcodeUsageRow {
                id: &id,
                session_id: session.as_deref(),
                model_id: model.as_deref(),
                started_at: row.get(3)?,
                completed_at: row.get(4)?,
                fallback_timestamp_ms: source_modified_ms(path).unwrap_or(0),
                input: row.get(5)?,
                output: row.get(6)?,
                reasoning: row.get(7)?,
                cache_read: row.get(8)?,
                cache_write: row.get(9)?,
                computed_total: row.get(10)?,
                legacy_schema,
            })
            .map_err(map_zcode_error)?
            else {
                page_rows += 1;
                continue;
            };
            if responses.len() == zcode::MAX_RESPONSES {
                return Err(SqliteSourceError::Capacity);
            }
            responses.push(usage);
            page_rows += 1;
        }
        if page_rows == 0 {
            break;
        }
        offset = offset
            .checked_add(i64::try_from(page_rows).map_err(|_| SqliteSourceError::Capacity)?)
            .ok_or(SqliteSourceError::Capacity)?;
    }
    drop(transaction);
    snapshot.verify(path)?;
    Ok(Arc::from(responses))
}

fn read_unsloth(path: &Path) -> Result<Arc<[ResponseUsage]>, SqliteSourceError> {
    let snapshot = SqliteSnapshot::open(path)?;
    let transaction = snapshot.connection.unchecked_transaction()?;
    let mut reader = unsloth::UnslothReader::new();

    // The internal chat lane. Only scalar usage and routing columns are
    // selected: message content, prompts and previews never cross the boundary.
    if table_exists(&transaction, "chat_messages")? {
        let sql = "SELECT m.id, m.thread_id, m.created_at,
                          json_extract(m.metadata_json, '$.contextUsage.promptTokens'),
                          json_extract(m.metadata_json, '$.contextUsage.completionTokens'),
                          json_extract(m.metadata_json, '$.contextUsage.totalTokens'),
                          json_extract(m.metadata_json, '$.contextUsage.cachedTokens'),
                          json_extract(m.metadata_json, '$.contextUsage.cacheWriteTokens'),
                          json_extract(m.metadata_json, '$.contextUsage.reasoningTokens'),
                          json_extract(m.metadata_json, '$.contextUsage.modelId'),
                          json_extract(m.metadata_json, '$.responseDetails.responseModelId'),
                          json_extract(m.metadata_json, '$.responseDetails.providerType'),
                          t.model_id
                   FROM chat_messages m
                   LEFT JOIN chat_threads t ON t.id = m.thread_id
                   WHERE m.role = 'assistant'
                     AND json_valid(m.metadata_json)
                   ORDER BY m.created_at, m.id LIMIT ?1 OFFSET ?2";
        read_unsloth_lane(&transaction, sql, unsloth::Lane::Chat, &mut reader)?;
    }

    // The external API lane. Older Studio builds have no such table, which is
    // not an error: their chat usage still imports.
    if table_exists(&transaction, "api_usage_events")? {
        let sql = "SELECT id, endpoint, model, prompt_tokens, completion_tokens,
                          total_tokens, created_at
                   FROM api_usage_events
                   ORDER BY created_at, id LIMIT ?1 OFFSET ?2";
        read_unsloth_lane(&transaction, sql, unsloth::Lane::Api, &mut reader)?;
    }

    drop(transaction);
    snapshot.verify(path)?;
    Ok(Arc::from(reader.snapshot()))
}

fn read_unsloth_lane(
    transaction: &rusqlite::Transaction<'_>,
    sql: &str,
    lane: unsloth::Lane,
    reader: &mut unsloth::UnslothReader,
) -> Result<(), SqliteSourceError> {
    let mut offset = 0_i64;
    loop {
        let mut statement = transaction.prepare(sql)?;
        let mut rows = statement.query(params![SQLITE_PAGE_ROWS, offset])?;
        let mut page_rows = 0_usize;
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            // Bind every borrowed string before building the row: the row holds
            // references, so the temporaries have to outlive it.
            let (endpoint, thread_id, requested, response, provider_type, thread_model) = match lane
            {
                unsloth::Lane::Chat => (
                    None,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, Option<String>>(10)?,
                    row.get::<_, Option<String>>(11)?,
                    row.get::<_, Option<String>>(12)?,
                ),
                unsloth::Lane::Api => (
                    row.get::<_, Option<String>>(1)?,
                    None,
                    None,
                    None,
                    None,
                    None,
                ),
            };
            let row = match lane {
                unsloth::Lane::Chat => unsloth::UnslothRow {
                    lane,
                    id: &id,
                    thread_id: thread_id.as_deref(),
                    endpoint: None,
                    created_at_secs: row.get(2)?,
                    prompt_tokens: row.get::<_, Option<i64>>(3)?.unwrap_or(0),
                    completion_tokens: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
                    total_tokens: row.get::<_, Option<i64>>(5)?.unwrap_or(0),
                    cached_tokens: row.get::<_, Option<i64>>(6)?.unwrap_or(0),
                    cache_write_tokens: row.get::<_, Option<i64>>(7)?.unwrap_or(0),
                    reasoning_tokens: row.get::<_, Option<i64>>(8)?.unwrap_or(0),
                    requested_model: requested.as_deref(),
                    response_model: response.as_deref(),
                    provider_type: provider_type.as_deref(),
                    thread_model: thread_model.as_deref(),
                },
                unsloth::Lane::Api => unsloth::UnslothRow {
                    lane,
                    id: &id,
                    thread_id: None,
                    endpoint: endpoint.as_deref(),
                    created_at_secs: row.get(6)?,
                    prompt_tokens: row.get(3)?,
                    completion_tokens: row.get(4)?,
                    total_tokens: row.get(5)?,
                    cached_tokens: 0,
                    cache_write_tokens: 0,
                    reasoning_tokens: 0,
                    requested_model: None,
                    response_model: None,
                    provider_type: None,
                    thread_model: None,
                },
            };
            reader.ingest(row).map_err(map_unsloth_error)?;
            page_rows += 1;
        }
        if page_rows == 0 {
            break;
        }
        offset = offset
            .checked_add(i64::try_from(page_rows).map_err(|_| SqliteSourceError::Capacity)?)
            .ok_or(SqliteSourceError::Capacity)?;
    }
    Ok(())
}

fn map_unsloth_error(error: ReaderError) -> SqliteSourceError {
    match error {
        ReaderError::Capacity(_) => SqliteSourceError::Capacity,
        ReaderError::Overflow => SqliteSourceError::Capacity,
        ReaderError::InvalidRecord
        | ReaderError::InvalidTimestamp
        | ReaderError::Unsupported(_) => SqliteSourceError::Invalid,
    }
}

fn read_cherrystudio(path: &Path) -> Result<Arc<[ResponseUsage]>, SqliteSourceError> {
    let snapshot = SqliteSnapshot::open(path)?;
    let transaction = snapshot.connection.unchecked_transaction()?;
    // The table has shipped several spellings, so every optional column is
    // probed. Naming a column this build lacks would fail the whole statement
    // to prepare and silently take the lane to zero.
    let mut columns = {
        let mut statement = transaction.prepare("PRAGMA table_info(ai_usage_record)")?;
        let mut rows = statement.query([])?;
        let mut names = BTreeSet::new();
        while let Some(row) = rows.next()? {
            names.insert(row.get::<_, String>(1)?);
        }
        names
    };
    if !columns.contains("id") || !columns.contains("created_at") {
        return Err(SqliteSourceError::Unsupported);
    }
    let has = |columns: &BTreeSet<String>, name: &str| columns.contains(name);
    let column = |columns: &BTreeSet<String>, name: &str, qualifier: &str| {
        if has(columns, name) {
            format!("{qualifier}.{name}")
        } else {
            "NULL".to_owned()
        }
    };
    // The cache-write bucket was renamed; either spelling may be present.
    let cache_write = if has(&columns, "cache_write_tokens") {
        "r.cache_write_tokens"
    } else if has(&columns, "cache_creation_tokens") {
        "r.cache_creation_tokens"
    } else {
        "NULL"
    };
    // Conversation identity lives on `message` (its title on `topic`), so the
    // join is what lets a row name the conversation it belongs to. Each table
    // is probed separately because a build may carry one without the other.
    let link_topic = has(&columns, "message_id")
        && table_has_column_for(&transaction, "message", "id")?
        && table_has_column_for(&transaction, "message", "topic_id")?
        && table_has_column_for(&transaction, "topic", "id")?
        && table_has_column_for(&transaction, "topic", "name")?;
    let title = if link_topic { "t.name" } else { "NULL" };
    let joins = if link_topic {
        "LEFT JOIN message m ON m.id = r.message_id
         LEFT JOIN topic t ON t.id = m.topic_id"
    } else {
        ""
    };
    // `message_kind` separates the two surfaces. A build without it cannot tell
    // them apart, so it must report every row: the transcripts keep each
    // invocation distinct, and guessing would be worse than reporting.
    let (kind, kind_filter) = if has(&columns, "message_kind") {
        (
            "r.message_kind",
            " AND (r.message_kind IS NULL OR r.message_kind = 'chat')",
        )
    } else {
        ("NULL", "")
    };
    // `legacy-aggregate` rows are pre-summed history for the same calls, so
    // reading them beside per-invocation rows double counts.
    let record_kind_filter = if has(&columns, "record_kind") {
        "r.record_kind = 'invocation'"
    } else {
        "1 = 1"
    };

    let sql = format!(
        "SELECT r.id, {message_id}, {title}, {provider}, {model},
                {input}, {output}, {cache_read}, {cache_write}, {reasoning},
                {cost}, r.created_at, {kind}, {no_cache}, {currency}
         FROM ai_usage_record r
         {joins}
         WHERE {record_kind_filter}{kind_filter}
         ORDER BY r.created_at, r.id LIMIT ?1 OFFSET ?2",
        message_id = column(&columns, "message_id", "r"),
        title = title,
        provider = column(&columns, "provider_id", "r"),
        model = column(&columns, "model_id", "r"),
        input = column(&columns, "input_tokens", "r"),
        output = column(&columns, "output_tokens", "r"),
        cache_read = column(&columns, "cache_read_tokens", "r"),
        cache_write = cache_write,
        reasoning = column(&columns, "reasoning_tokens", "r"),
        cost = column(&columns, "cost", "r"),
        kind = kind,
        no_cache = column(&columns, "no_cache_tokens", "r"),
        currency = column(&columns, "cost_currency", "r"),
    );
    columns.clear();

    let mut reader = cherrystudio::CherryReader::new();
    let mut offset = 0_i64;
    loop {
        let mut statement = transaction.prepare(&sql)?;
        let mut rows = statement.query(params![SQLITE_PAGE_ROWS, offset])?;
        let mut page_rows = 0_usize;
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let message_id: Option<String> = row.get(1)?;
            let session_title: Option<String> = row.get(2)?;
            let provider_id: Option<String> = row.get(3)?;
            let model_id: Option<String> = row.get(4)?;
            let cost_currency: Option<String> = row.get(14)?;
            let message_kind: Option<String> = row.get(12)?;
            reader
                .ingest(cherrystudio::CherryRow {
                    id: &id,
                    message_id: message_id.as_deref(),
                    session_title: session_title.as_deref(),
                    provider_id: provider_id.as_deref(),
                    model_id: model_id.as_deref(),
                    input_tokens: row.get(5)?,
                    output_tokens: row.get(6)?,
                    cache_read_tokens: row.get(7)?,
                    cache_write_tokens: row.get(8)?,
                    reasoning_tokens: row.get(9)?,
                    no_cache_tokens: row.get(13)?,
                    cost: row.get(10)?,
                    cost_currency: cost_currency.as_deref(),
                    created_at: row.get(11)?,
                    message_kind: message_kind.as_deref(),
                })
                .map_err(map_cherrystudio_error)?;
            page_rows += 1;
        }
        if page_rows == 0 {
            break;
        }
        offset = offset
            .checked_add(i64::try_from(page_rows).map_err(|_| SqliteSourceError::Capacity)?)
            .ok_or(SqliteSourceError::Capacity)?;
    }
    drop(transaction);
    snapshot.verify(path)?;
    Ok(Arc::from(reader.snapshot()))
}

/// Whether `table` exists in this database.
fn table_exists(
    transaction: &rusqlite::Transaction<'_>,
    table: &str,
) -> Result<bool, SqliteSourceError> {
    let mut statement =
        transaction.prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1")?;
    let mut rows = statement.query([table])?;
    Ok(rows.next()?.is_some())
}

/// Whether `table` exists and carries `name`, for probing optional joins.
fn table_has_column_for(
    transaction: &rusqlite::Transaction<'_>,
    table: &str,
    name: &str,
) -> Result<bool, SqliteSourceError> {
    let mut statement = transaction.prepare(&format!("PRAGMA table_info('{table}')"))?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        if row.get::<_, String>(1)? == name {
            return Ok(true);
        }
    }
    Ok(false)
}

fn map_cherrystudio_error(error: ReaderError) -> SqliteSourceError {
    match error {
        ReaderError::Capacity(_) => SqliteSourceError::Capacity,
        ReaderError::Overflow => SqliteSourceError::Capacity,
        ReaderError::InvalidRecord
        | ReaderError::InvalidTimestamp
        | ReaderError::Unsupported(_) => SqliteSourceError::Invalid,
    }
}

fn read_qodercn(path: &Path) -> Result<Arc<[ResponseUsage]>, SqliteSourceError> {
    let snapshot = SqliteSnapshot::open(path)?;
    let transaction = snapshot.connection.unchecked_transaction()?;
    require_columns(
        &transaction,
        "chat_message",
        &["id", "session_id", "token_info"],
    )?;
    // `chat_session.project_name` is optional in older builds, so it is probed
    // rather than required: the query falls back to a NULL project column.
    let with_project = transaction
        .query_row(
            "SELECT 1 FROM sqlite_master
             WHERE type = 'table' AND name = 'chat_session'
               AND EXISTS (SELECT 1 FROM pragma_table_info('chat_session')
                           WHERE name = 'project_name')
             LIMIT 1",
            [],
            |_| Ok(()),
        )
        .is_ok();
    let source = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.rsplit_once('.').map(|(stem, _)| stem))
        .unwrap_or("local");
    let project_column = if with_project {
        "(SELECT cs.project_name FROM chat_session cs
           WHERE cs.session_id = chat_message.session_id LIMIT 1)"
    } else {
        "NULL"
    };
    let sql = format!(
        "SELECT rowid, id, session_id, request_id, token_info, model_info, gmt_create,
                {project_column}
         FROM chat_message
         WHERE role = 'assistant'
           AND token_info IS NOT NULL
           AND trim(token_info) NOT IN ('', '{{}}')
         ORDER BY gmt_create, rowid LIMIT ?1 OFFSET ?2"
    );
    let candidates: i64 = connection_query_count(
        &transaction,
        "SELECT COUNT(*) FROM chat_message
         WHERE role = 'assistant'
           AND token_info IS NOT NULL
           AND trim(token_info) NOT IN ('', '{}')",
    )?;
    let candidates: usize = candidates
        .try_into()
        .map_err(|_| SqliteSourceError::Capacity)?;
    if candidates > qodercn::MAX_RESPONSES {
        return Err(SqliteSourceError::Capacity);
    }
    let mut reader = qodercn::QodercnReader::new(source);
    let mut offset = 0_i64;
    loop {
        let mut statement = transaction.prepare(&sql)?;
        let mut rows = statement.query(params![SQLITE_PAGE_ROWS, offset])?;
        let mut page_rows = 0_usize;
        while let Some(row) = rows.next()? {
            let row_id: i64 = row.get(0)?;
            let id: Option<String> = row.get(1)?;
            let session: Option<String> = row.get(2)?;
            let request: Option<String> = row.get(3)?;
            let token_info: Option<String> = row.get(4)?;
            let model_info: Option<String> = row.get(5)?;
            let created_at: Option<i64> = row.get(6)?;
            let project: Option<String> = row.get(7)?;
            page_rows += 1;
            // A row whose `token_info` is not a JSON object is a visible
            // rejection, not a silent skip: the counters cannot be proven.
            let Some(token_info) = token_info else {
                continue;
            };
            let Ok(token_value) = serde_json::from_str::<serde_json::Value>(&token_info) else {
                return Err(SqliteSourceError::Invalid);
            };
            let (prompt, cached, output) =
                qodercn::database_counters(&token_value).map_err(map_qodercn_error)?;
            let model = model_info
                .as_deref()
                .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
                .and_then(|value| {
                    ["model_key", "modelKey"].iter().find_map(|field| {
                        value
                            .get(*field)
                            .and_then(serde_json::Value::as_str)
                            .filter(|text| !text.is_empty())
                            .map(str::to_owned)
                    })
                });
            let session_id = session
                .as_deref()
                .filter(|value| !value.is_empty())
                .or(request.as_deref().filter(|value| !value.is_empty()))
                .or(id.as_deref().filter(|value| !value.is_empty()))
                .map(str::to_owned)
                .unwrap_or_else(|| row_id.to_string());
            let message_id = id
                .as_deref()
                .filter(|value| !value.is_empty())
                .or(request.as_deref().filter(|value| !value.is_empty()))
                .map(str::to_owned)
                .unwrap_or_else(|| row_id.to_string());
            reader
                .ingest(qodercn::QodercnRow {
                    lane: qodercn::Lane::Database,
                    source,
                    session_id: Some(&session_id),
                    message_id: Some(&message_id),
                    prompt,
                    cached,
                    output,
                    model: model.as_deref(),
                    workspace: project.as_deref(),
                    created_at_ms: created_at.unwrap_or(0),
                })
                .map_err(map_qodercn_error)?;
        }
        if page_rows == 0 {
            break;
        }
        offset = offset
            .checked_add(i64::try_from(page_rows).map_err(|_| SqliteSourceError::Capacity)?)
            .ok_or(SqliteSourceError::Capacity)?;
    }
    drop(transaction);
    snapshot.verify(path)?;
    Ok(Arc::from(reader.snapshot()))
}

fn map_qodercn_error(error: ReaderError) -> SqliteSourceError {
    match error {
        ReaderError::Capacity(_) => SqliteSourceError::Capacity,
        ReaderError::Overflow => SqliteSourceError::Capacity,
        ReaderError::InvalidRecord
        | ReaderError::InvalidTimestamp
        | ReaderError::Unsupported(_) => SqliteSourceError::Invalid,
    }
}

fn map_zcode_error(error: ReaderError) -> SqliteSourceError {
    match error {
        ReaderError::Capacity(_) => SqliteSourceError::Capacity,
        ReaderError::Overflow => SqliteSourceError::Capacity,
        ReaderError::InvalidRecord
        | ReaderError::InvalidTimestamp
        | ReaderError::Unsupported(_) => SqliteSourceError::Invalid,
    }
}

fn connection_query_count(connection: &Connection, sql: &str) -> Result<i64, SqliteSourceError> {
    Ok(connection.query_row(sql, [], |row| row.get(0))?)
}

fn map_mimo_error(error: ReaderError) -> SqliteSourceError {
    match error {
        ReaderError::Capacity(_) => SqliteSourceError::Capacity,
        ReaderError::InvalidRecord
        | ReaderError::InvalidTimestamp
        | ReaderError::Unsupported(_)
        | ReaderError::Overflow => SqliteSourceError::Invalid,
    }
}

fn read_opencode_current(
    connection: &Connection,
    reader: &mut OpenCodeReader,
) -> Result<BTreeSet<String>, SqliteSourceError> {
    let candidates: i64 = connection.query_row(
        "SELECT COUNT(*) FROM session_message WHERE type IN ('assistant', 'compaction')",
        [],
        |row| row.get(0),
    )?;
    let candidates: usize = candidates
        .try_into()
        .map_err(|_| SqliteSourceError::Capacity)?;
    if candidates > MAX_OPENCODE_RESPONSES {
        return Err(SqliteSourceError::Capacity);
    }

    let mut sessions = BTreeSet::new();
    let mut statement = connection.prepare("SELECT DISTINCT session_id FROM session_message")?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        sessions.insert(row.get::<_, String>(0)?);
    }
    drop(rows);
    drop(statement);

    let time_created = table_columns(connection, "session_message")?
        .is_some_and(|columns| columns.contains("time_created"));
    let time_column = if time_created { "time_created" } else { "NULL" };
    let sql = format!(
        "SELECT id, session_id, type, data, {time_column}
         FROM session_message
         WHERE type IN ('assistant', 'compaction')
         ORDER BY session_id, id LIMIT ?1 OFFSET ?2"
    );
    let mut offset = 0_i64;
    loop {
        let mut statement = connection.prepare(&sql)?;
        let mut rows = statement.query(params![SQLITE_PAGE_ROWS, offset])?;
        let mut page_rows = 0_usize;
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let session: String = row.get(1)?;
            let kind: String = row.get(2)?;
            let data = row
                .get_ref(3)?
                .as_bytes()
                .map_err(|_| SqliteSourceError::Invalid)?;
            if data.len() > MAX_RECORD_BYTES {
                return Err(SqliteSourceError::Capacity);
            }
            let kind = match kind.as_str() {
                "assistant" => OpenCodeKind::Assistant,
                "compaction" => OpenCodeKind::Compaction,
                _ => continue,
            };
            reader
                .ingest(OpenCodeRecord {
                    message_id: &id,
                    session_id: &session,
                    kind,
                    data,
                    fallback_timestamp_ms: row.get(4)?,
                })
                .map_err(map_opencode_error)?;
            page_rows += 1;
        }
        if page_rows == 0 {
            break;
        }
        offset = offset
            .checked_add(i64::try_from(page_rows).map_err(|_| SqliteSourceError::Capacity)?)
            .ok_or(SqliteSourceError::Capacity)?;
    }
    Ok(sessions)
}

fn read_opencode_legacy(
    connection: &Connection,
    columns: &BTreeSet<String>,
    current_sessions: &BTreeSet<String>,
    reader: &mut OpenCodeReader,
) -> Result<(), SqliteSourceError> {
    let filter = if current_sessions.is_empty() {
        String::new()
    } else {
        "AND session_id NOT IN (SELECT DISTINCT session_id FROM session_message)".to_owned()
    };
    let candidates: i64 = connection.query_row(
        &format!(
            "SELECT COUNT(*) FROM message
             WHERE json_valid(data) AND json_extract(data, '$.role') = 'assistant' {filter}"
        ),
        [],
        |row| row.get(0),
    )?;
    let candidates: usize = candidates
        .try_into()
        .map_err(|_| SqliteSourceError::Capacity)?;
    if candidates > MAX_OPENCODE_RESPONSES {
        return Err(SqliteSourceError::Capacity);
    }
    let time_column = if columns.contains("time_created") {
        "time_created"
    } else {
        "NULL"
    };
    let sql = format!(
        "SELECT id, session_id, data, {time_column}
         FROM message
         WHERE json_valid(data) AND json_extract(data, '$.role') = 'assistant' {filter}
         ORDER BY session_id, id LIMIT ?1 OFFSET ?2"
    );
    let mut offset = 0_i64;
    loop {
        let mut statement = connection.prepare(&sql)?;
        let mut rows = statement.query(params![SQLITE_PAGE_ROWS, offset])?;
        let mut page_rows = 0_usize;
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let session: String = row.get(1)?;
            let data = row
                .get_ref(2)?
                .as_bytes()
                .map_err(|_| SqliteSourceError::Invalid)?;
            if data.len() > MAX_RECORD_BYTES {
                return Err(SqliteSourceError::Capacity);
            }
            reader
                .ingest(OpenCodeRecord {
                    message_id: &id,
                    session_id: &session,
                    kind: OpenCodeKind::Assistant,
                    data,
                    fallback_timestamp_ms: row.get(3)?,
                })
                .map_err(map_opencode_error)?;
            page_rows += 1;
        }
        if page_rows == 0 {
            break;
        }
        offset = offset
            .checked_add(i64::try_from(page_rows).map_err(|_| SqliteSourceError::Capacity)?)
            .ok_or(SqliteSourceError::Capacity)?;
    }
    Ok(())
}

/// The v1 migration replaced compacted prompt/summary usage and dropped
/// subtask assistants from the current transcript. Restoring either form is
/// unsupported here, so a restorable non-current legacy row rejects the whole
/// candidate instead of silently undercounting it.
fn detect_opencode_legacy_supplements(connection: &Connection) -> Result<(), SqliteSourceError> {
    let part_columns = table_columns(connection, "part")?;
    let parts = part_columns
        .as_ref()
        .is_some_and(|columns| has_columns(columns, &["message_id", "session_id", "data"]));
    let parent_compaction_part = if parts {
        "OR EXISTS (
             SELECT 1 FROM message AS parent
             JOIN part AS parent_part
               ON parent_part.message_id = parent.id AND parent_part.session_id = parent.session_id
             WHERE parent.session_id = m.session_id
               AND parent.id = json_extract(m.data, '$.parentID')
               AND json_valid(parent.data) AND json_extract(parent.data, '$.role') = 'user'
               AND json_valid(parent_part.data) AND json_extract(parent_part.data, '$.type') = 'compaction'
         )"
    } else {
        ""
    };
    let summary = format!(
        "SELECT 1 FROM message AS m
         WHERE m.session_id IN (SELECT DISTINCT session_id FROM session_message)
           AND NOT EXISTS (
             SELECT 1 FROM session_message AS current
             WHERE current.id = m.id AND current.session_id = m.session_id
           )
           AND json_valid(m.data)
           AND json_extract(m.data, '$.role') = 'assistant'
           AND json_extract(m.data, '$.summary') = 1
           AND (
             EXISTS (
               SELECT 1 FROM session_message AS compaction
               WHERE compaction.session_id = m.session_id
                 AND compaction.type = 'compaction'
                 AND compaction.id = json_extract(m.data, '$.parentID')
             )
             {parent_compaction_part}
           )
         LIMIT 1"
    );
    if connection
        .query_row(&summary, [], |_| Ok(()))
        .optional()?
        .is_some()
    {
        return Err(SqliteSourceError::Unsupported);
    }
    if !parts {
        return Ok(());
    }
    let subtask = "SELECT 1 FROM message AS m
         WHERE m.session_id IN (SELECT DISTINCT session_id FROM session_message)
           AND NOT EXISTS (
             SELECT 1 FROM session_message AS current
             WHERE current.id = m.id AND current.session_id = m.session_id
           )
           AND json_valid(m.data)
           AND json_extract(m.data, '$.role') = 'assistant'
           AND EXISTS (
             SELECT 1 FROM part AS owned
             WHERE owned.session_id = m.session_id AND owned.message_id = m.id
               AND json_valid(owned.data) AND json_extract(owned.data, '$.type') = 'tool'
               AND json_extract(owned.data, '$.tool') = 'task'
           )
           AND EXISTS (
             SELECT 1 FROM message AS parent
             JOIN part AS parent_part
               ON parent_part.message_id = parent.id AND parent_part.session_id = parent.session_id
             WHERE parent.session_id = m.session_id
               AND parent.id = json_extract(m.data, '$.parentID')
               AND json_valid(parent.data) AND json_extract(parent.data, '$.role') = 'user'
               AND json_valid(parent_part.data) AND json_extract(parent_part.data, '$.type') = 'subtask'
           )
         LIMIT 1";
    if connection
        .query_row(subtask, [], |_| Ok(()))
        .optional()?
        .is_some()
    {
        return Err(SqliteSourceError::Unsupported);
    }
    Ok(())
}

fn map_opencode_error(error: ReaderError) -> SqliteSourceError {
    match error {
        ReaderError::Unsupported(_) => SqliteSourceError::Unsupported,
        ReaderError::Capacity(_) | ReaderError::Overflow => SqliteSourceError::Capacity,
        ReaderError::InvalidRecord | ReaderError::InvalidTimestamp => SqliteSourceError::Invalid,
    }
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

/// `None` means the table is absent; an empty set is impossible for a real table.
fn table_columns(
    connection: &Connection,
    table: &str,
) -> Result<Option<BTreeSet<String>>, SqliteSourceError> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = statement.query([])?;
    let mut columns = BTreeSet::new();
    while let Some(row) = rows.next()? {
        columns.insert(row.get::<_, String>(1)?);
    }
    Ok((!columns.is_empty()).then_some(columns))
}

fn has_columns(columns: &BTreeSet<String>, required: &[&str]) -> bool {
    required.iter().all(|column| columns.contains(*column))
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
