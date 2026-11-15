//! One-source bounded reconciliation. Run steps on the collector worker, never
//! on the UI thread. Only a successfully captured framed prefix is published;
//! incomplete final records remain explicit and deferred. This is not source
//! discovery, cross-file deduplication, period aggregation or archive retention.

use crate::accounting::{SessionKey, TokenCounts, TokenDelta};
use crate::parsers::{
    ReaderError, ResponseUsage,
    amp::AmpReader,
    antigravity::AntigravityReader,
    claude::ClaudeReader,
    cline::ClineReader,
    codebuddy::{CodeBuddyLogReader, CodeBuddyReader},
    codex::CodexReader,
    commandcode::CommandCodeReader,
    copilot::CopilotReader,
    cursor::CursorReader,
    droid::DroidReader,
    gemini::GeminiReader,
    grok::GrokReader,
    kimi::KimiReader,
    kiro::KiroReader,
    mcode::McodeReader,
    muse::MuseReader,
    omp::OmpReader,
    openclaw::OpenClawReader,
    pi::PiReader,
    qwen::QwenReader,
    reasonix::ReasonixReader,
    senpi::SenpiReader,
    workbuddy::WorkBuddyReader,
    zcode::ZcodeReader,
};
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
    GeminiTranscript,
    CursorCacheJson,
    ClineJson,
    AmpJson,
    DroidJson,
    KiroJson,
    KimiJsonl,
    CopilotJsonl,
    CodeBuddyJsonl,
    WorkBuddyJsonl,
    PiJsonl,
    OmpJsonl,
    SenpiJsonl,
    AntigravityJsonl,
    QwenJsonl,
    ReasonixJsonl,
    MuseJsonl,
    McodeJsonl,
    MimoSqlite,
    ZcodeJsonl,
    ZcodeSqlite,
    CommandCodeJsonl,
    GrokJsonl,
    OpenClawJsonl,
    ZedSqlite,
    WorkBuddySqlite,
    AntigravitySqlite,
    SessionArchiveSqlite,
    CodexLocalUsageSqlite,
    OpenCodeSqlite,
    KiloSqlite,
    GooseSqlite,
    HermesSqlite,
    OpenClawSqlite,
}

impl SourceFormat {
    pub fn client(self) -> &'static str {
        match self {
            Self::ClaudeCli => "claude",
            Self::CodexCli | Self::CodexLocalUsageSqlite => "codex",
            Self::GeminiTranscript => "gemini",
            Self::CursorCacheJson => "cursor",
            Self::ClineJson => "cline",
            Self::AmpJson => "amp",
            Self::DroidJson => "droid",
            Self::KiroJson => "kiro",
            Self::KimiJsonl => "kimi",
            Self::CommandCodeJsonl => "commandcode",
            Self::GrokJsonl => "grok",
            Self::CopilotJsonl => "copilot",
            Self::CodeBuddyJsonl => "codebuddy",
            Self::WorkBuddyJsonl => "workbuddy",
            Self::PiJsonl => "pi",
            Self::OmpJsonl => "omp",
            Self::SenpiJsonl => "senpi",
            Self::AntigravityJsonl => "antigravity",
            Self::QwenJsonl => "qwen",
            Self::ReasonixJsonl => "reasonix",
            Self::MuseJsonl => "muse",
            Self::McodeJsonl => "mcode",
            Self::MimoSqlite => "mimo",
            Self::ZcodeJsonl | Self::ZcodeSqlite => "zcode",
            Self::ZedSqlite => "zed",
            Self::WorkBuddySqlite => "workbuddy",
            Self::AntigravitySqlite => "antigravity",
            Self::SessionArchiveSqlite => "mixed",
            Self::OpenCodeSqlite => "opencode",
            Self::KiloSqlite => "kilo",
            Self::GooseSqlite => "goose",
            Self::HermesSqlite => "hermes",
            Self::OpenClawJsonl | Self::OpenClawSqlite => "openclaw",
        }
    }

    pub(crate) fn storage_name(self) -> &'static str {
        match self {
            Self::ClaudeCli => "claude-jsonl",
            Self::CodexCli => "codex-jsonl",
            Self::GeminiTranscript => "gemini-transcript",
            Self::CursorCacheJson => "cursor-cache-json",
            Self::ClineJson => "cline-json",
            Self::AmpJson => "amp-json",
            Self::DroidJson => "droid-json",
            Self::KiroJson => "kiro-json",
            Self::KimiJsonl => "kimi-jsonl",
            Self::CommandCodeJsonl => "commandcode-jsonl",
            Self::GrokJsonl => "grok-jsonl",
            Self::CopilotJsonl => "copilot-jsonl",
            Self::CodeBuddyJsonl => "codebuddy-jsonl",
            Self::WorkBuddyJsonl => "workbuddy-jsonl",
            Self::PiJsonl => "pi-jsonl",
            Self::OmpJsonl => "omp-jsonl",
            Self::SenpiJsonl => "senpi-jsonl",
            Self::AntigravityJsonl => "antigravity-jsonl",
            Self::QwenJsonl => "qwen-jsonl",
            Self::ReasonixJsonl => "reasonix-jsonl",
            Self::MuseJsonl => "muse-jsonl",
            Self::McodeJsonl => "mcode-jsonl",
            Self::MimoSqlite => "mimo-sqlite",
            Self::ZcodeJsonl => "zcode-jsonl",
            Self::ZcodeSqlite => "zcode-sqlite",
            Self::OpenClawJsonl => "openclaw-jsonl",
            Self::ZedSqlite => "zed-sqlite",
            Self::WorkBuddySqlite => "workbuddy-sqlite",
            Self::AntigravitySqlite => "antigravity-sqlite",
            Self::SessionArchiveSqlite => "session-archive-sqlite",
            Self::CodexLocalUsageSqlite => "codex-local-usage-sqlite",
            Self::OpenCodeSqlite => "opencode-sqlite",
            Self::KiloSqlite => "kilo-sqlite",
            Self::GooseSqlite => "goose-sqlite",
            Self::HermesSqlite => "hermes-sqlite",
            Self::OpenClawSqlite => "openclaw-sqlite",
        }
    }

    pub(crate) fn parse_storage_name(value: &str) -> Option<Self> {
        match value {
            "claude-jsonl" => Some(Self::ClaudeCli),
            "codex-jsonl" => Some(Self::CodexCli),
            "gemini-transcript" => Some(Self::GeminiTranscript),
            "cursor-cache-json" => Some(Self::CursorCacheJson),
            "cline-json" => Some(Self::ClineJson),
            "amp-json" => Some(Self::AmpJson),
            "droid-json" => Some(Self::DroidJson),
            "kiro-json" => Some(Self::KiroJson),
            "kimi-jsonl" => Some(Self::KimiJsonl),
            "commandcode-jsonl" => Some(Self::CommandCodeJsonl),
            "grok-jsonl" => Some(Self::GrokJsonl),
            "copilot-jsonl" => Some(Self::CopilotJsonl),
            "codebuddy-jsonl" => Some(Self::CodeBuddyJsonl),
            "workbuddy-jsonl" => Some(Self::WorkBuddyJsonl),
            "pi-jsonl" => Some(Self::PiJsonl),
            "omp-jsonl" => Some(Self::OmpJsonl),
            "senpi-jsonl" => Some(Self::SenpiJsonl),
            "antigravity-jsonl" => Some(Self::AntigravityJsonl),
            "qwen-jsonl" => Some(Self::QwenJsonl),
            "reasonix-jsonl" => Some(Self::ReasonixJsonl),
            "muse-jsonl" => Some(Self::MuseJsonl),
            "mcode-jsonl" => Some(Self::McodeJsonl),
            "mimo-sqlite" => Some(Self::MimoSqlite),
            "zcode-jsonl" => Some(Self::ZcodeJsonl),
            "zcode-sqlite" => Some(Self::ZcodeSqlite),
            "openclaw-jsonl" => Some(Self::OpenClawJsonl),
            "zed-sqlite" => Some(Self::ZedSqlite),
            "workbuddy-sqlite" => Some(Self::WorkBuddySqlite),
            "antigravity-sqlite" => Some(Self::AntigravitySqlite),
            "session-archive-sqlite" => Some(Self::SessionArchiveSqlite),
            "codex-local-usage-sqlite" => Some(Self::CodexLocalUsageSqlite),
            "opencode-sqlite" => Some(Self::OpenCodeSqlite),
            "kilo-sqlite" => Some(Self::KiloSqlite),
            "goose-sqlite" => Some(Self::GooseSqlite),
            "hermes-sqlite" => Some(Self::HermesSqlite),
            "openclaw-sqlite" => Some(Self::OpenClawSqlite),
            _ => None,
        }
    }

    pub(crate) fn is_sqlite(self) -> bool {
        matches!(
            self,
            Self::SessionArchiveSqlite
                | Self::CodexLocalUsageSqlite
                | Self::OpenCodeSqlite
                | Self::KiloSqlite
                | Self::MimoSqlite
                | Self::ZcodeSqlite
                | Self::GooseSqlite
                | Self::HermesSqlite
                | Self::OpenClawSqlite
                | Self::ZedSqlite
                | Self::WorkBuddySqlite
                | Self::AntigravitySqlite
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
    Gemini(Box<GeminiReader>),
    Cursor(Box<CursorReader>),
    Cline(Box<ClineReader>),
    Amp(Box<AmpReader>),
    Droid(Box<DroidReader>),
    Kiro(Box<KiroReader>),
    Kimi(Box<KimiReader>),
    CommandCode(Box<CommandCodeReader>),
    Grok(Box<GrokReader>),
    Copilot(Box<CopilotReader>),
    CodeBuddy(Box<CodeBuddyReader>),
    CodeBuddyLog(Box<CodeBuddyLogReader>),
    WorkBuddy(Box<WorkBuddyReader>),
    Pi(Box<PiReader>),
    Omp(Box<OmpReader>),
    Senpi(Box<SenpiReader>),
    Antigravity(Box<AntigravityReader>),
    Qwen(Box<QwenReader>),
    Reasonix(Box<ReasonixReader>),
    Muse(Box<MuseReader>),
    Mcode(Box<McodeReader>),
    OpenClaw(Box<OpenClawReader>),
    Zcode(Box<ZcodeReader>),
}

impl ResponseReader {
    fn ingest(&mut self, record: &[u8]) -> Result<(), ReaderError> {
        match self {
            Self::Claude(reader) => reader.ingest(record),
            Self::Codex(reader) => reader.ingest(record),
            Self::Gemini(reader) => reader.ingest(record),
            Self::Cursor(reader) => reader.ingest(record),
            Self::Cline(reader) => reader.ingest(record),
            Self::Amp(reader) => reader.ingest(record),
            Self::Droid(reader) => reader.ingest(record),
            Self::Kiro(reader) => reader.ingest(record),
            Self::Kimi(reader) => reader.ingest(record),
            Self::CommandCode(reader) => reader.ingest(record),
            Self::Grok(reader) => reader.ingest(record),
            Self::Copilot(reader) => reader.ingest(record),
            Self::CodeBuddy(reader) => reader.ingest(record),
            Self::CodeBuddyLog(reader) => reader.ingest(record),
            Self::WorkBuddy(reader) => reader.ingest(record),
            Self::Pi(reader) => reader.ingest(record),
            Self::Omp(reader) => reader.ingest(record),
            Self::Senpi(reader) => reader.ingest(record),
            Self::Antigravity(reader) => reader.ingest(record),
            Self::Qwen(reader) => reader.ingest(record),
            Self::Reasonix(reader) => reader.ingest(record),
            Self::Muse(reader) => reader.ingest(record),
            Self::Mcode(reader) => reader.ingest(record),
            Self::OpenClaw(reader) => reader.ingest(record),
            Self::Zcode(reader) => reader.ingest(record),
        }
    }

    fn snapshot(&self) -> Vec<ResponseUsage> {
        match self {
            Self::Claude(reader) => reader.snapshot(),
            Self::Codex(reader) => reader.snapshot(),
            Self::Gemini(reader) => reader.snapshot(),
            Self::Cursor(reader) => reader.snapshot(),
            Self::Cline(reader) => reader.snapshot(),
            Self::Amp(reader) => reader.snapshot(),
            Self::Droid(reader) => reader.snapshot(),
            Self::Kiro(reader) => reader.snapshot(),
            Self::Kimi(reader) => reader.snapshot(),
            Self::CommandCode(reader) => reader.snapshot(),
            Self::Grok(reader) => reader.snapshot(),
            Self::Copilot(reader) => reader.snapshot(),
            Self::CodeBuddy(reader) => reader.snapshot(),
            Self::CodeBuddyLog(reader) => reader.snapshot(),
            Self::WorkBuddy(reader) => reader.snapshot(),
            Self::Pi(reader) => reader.snapshot(),
            Self::Omp(reader) => reader.snapshot(),
            Self::Senpi(reader) => reader.snapshot(),
            Self::Antigravity(reader) => reader.snapshot(),
            Self::Qwen(reader) => reader.snapshot(),
            Self::Reasonix(reader) => reader.snapshot(),
            Self::Muse(reader) => reader.snapshot(),
            Self::Mcode(reader) => reader.snapshot(),
            Self::OpenClaw(reader) => reader.snapshot(),
            Self::Zcode(reader) => reader.snapshot(),
        }
    }

    fn finish(&mut self) -> Result<(), ReaderError> {
        match self {
            Self::Claude(_) => Ok(()),
            Self::Codex(reader) => reader.finish(),
            Self::Gemini(_)
            | Self::Cursor(_)
            | Self::Cline(_)
            | Self::Amp(_)
            | Self::Droid(_)
            | Self::Kiro(_)
            | Self::Kimi(_)
            | Self::CommandCode(_)
            | Self::Grok(_)
            | Self::Copilot(_)
            | Self::CodeBuddy(_)
            | Self::CodeBuddyLog(_)
            | Self::WorkBuddy(_)
            | Self::Pi(_)
            | Self::Omp(_)
            | Self::Senpi(_)
            | Self::Antigravity(_)
            | Self::Qwen(_)
            | Self::Reasonix(_)
            | Self::Muse(_)
            | Self::Mcode(_)
            | Self::OpenClaw(_)
            | Self::Zcode(_) => Ok(()),
        }
    }

    fn has_supported_usage(&self, rows: &[ResponseUsage]) -> bool {
        match self {
            Self::Claude(_) => !rows.is_empty(),
            Self::Codex(reader) => reader.has_supported_usage(),
            Self::Gemini(reader) => reader.has_supported_usage(),
            Self::Cursor(reader) => reader.has_supported_usage(),
            Self::Cline(reader) => reader.has_supported_usage(),
            Self::Amp(reader) => reader.has_supported_usage(),
            Self::Droid(reader) => reader.has_supported_usage(),
            Self::Kiro(reader) => reader.has_supported_usage(),
            Self::Kimi(reader) => reader.has_supported_usage(),
            Self::CommandCode(reader) => reader.has_supported_usage(),
            Self::Grok(reader) => reader.has_supported_usage(),
            Self::Copilot(reader) => reader.has_supported_usage(),
            Self::CodeBuddy(reader) => reader.has_supported_usage(),
            Self::CodeBuddyLog(reader) => reader.has_supported_usage(),
            Self::WorkBuddy(reader) => reader.has_supported_usage(),
            Self::Pi(reader) => reader.has_supported_usage(),
            Self::Omp(reader) => reader.has_supported_usage(),
            Self::Senpi(reader) => reader.has_supported_usage(),
            Self::Antigravity(reader) => reader.has_supported_usage(),
            Self::Qwen(reader) => reader.has_supported_usage(),
            Self::Reasonix(reader) => reader.has_supported_usage(),
            Self::Muse(reader) => reader.has_supported_usage(),
            Self::Mcode(reader) => reader.has_supported_usage(),
            Self::OpenClaw(reader) => reader.has_supported_usage(),
            Self::Zcode(reader) => reader.has_supported_usage(),
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
            SourceFormat::GeminiTranscript => {
                let fallback = self.input.source_path().and_then(file_mtime_ms);
                ResponseReader::Gemini(Box::new(
                    GeminiReader::new(&self.key.session).with_fallback_timestamp(fallback),
                ))
            }
            SourceFormat::CursorCacheJson => {
                let account = self
                    .input
                    .source_path()
                    .map(cursor_account_from_path)
                    .unwrap_or_else(|| "unknown".to_owned());
                ResponseReader::Cursor(Box::new(CursorReader::new(account)))
            }
            SourceFormat::ClineJson => {
                let fallback = self.input.source_path().and_then(file_mtime_ms);
                ResponseReader::Cline(Box::new(
                    ClineReader::new(&self.key.session).with_fallback_timestamp(fallback),
                ))
            }
            SourceFormat::AmpJson => {
                let fallback = self.input.source_path().and_then(file_mtime_ms);
                ResponseReader::Amp(Box::new(
                    AmpReader::new(&self.key.session).with_fallback_timestamp(fallback),
                ))
            }
            SourceFormat::DroidJson => {
                let fallback = self.input.source_path().and_then(file_mtime_ms);
                ResponseReader::Droid(Box::new(
                    DroidReader::new(&self.key.session).with_fallback_timestamp(fallback),
                ))
            }
            SourceFormat::KiroJson => {
                let fallback = self.input.source_path().and_then(file_mtime_ms);
                ResponseReader::Kiro(Box::new(
                    KiroReader::new(&self.key.session).with_fallback_timestamp(fallback),
                ))
            }
            SourceFormat::CopilotJsonl => {
                let fallback = self.input.source_path().and_then(file_mtime_ms);
                ResponseReader::Copilot(Box::new(
                    CopilotReader::new(&self.key.session).with_fallback_timestamp(fallback),
                ))
            }
            SourceFormat::CodeBuddyJsonl => {
                let fallback = self.input.source_path().and_then(file_mtime_ms);
                let log = self
                    .input
                    .source_path()
                    .and_then(Path::extension)
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("log"));
                if log {
                    ResponseReader::CodeBuddyLog(Box::new(
                        CodeBuddyLogReader::new("codebuddy", "codebuddy", &self.key.session)
                            .with_fallback_timestamp(fallback),
                    ))
                } else {
                    ResponseReader::CodeBuddy(Box::new(
                        CodeBuddyReader::new("codebuddy", "codebuddy", &self.key.session)
                            .with_fallback_timestamp(fallback),
                    ))
                }
            }
            SourceFormat::WorkBuddyJsonl => {
                let fallback = self.input.source_path().and_then(file_mtime_ms);
                ResponseReader::WorkBuddy(Box::new(
                    WorkBuddyReader::new("workbuddy", "workbuddy", &self.key.session)
                        .with_fallback_timestamp(fallback),
                ))
            }
            SourceFormat::PiJsonl => {
                let fallback = self.input.source_path().and_then(file_mtime_ms);
                ResponseReader::Pi(Box::new(
                    PiReader::new("pi", &self.key.session).with_fallback_timestamp(fallback),
                ))
            }
            SourceFormat::OmpJsonl => {
                let fallback = self.input.source_path().and_then(file_mtime_ms);
                ResponseReader::Omp(Box::new(
                    OmpReader::new("omp", &self.key.session).with_fallback_timestamp(fallback),
                ))
            }
            SourceFormat::SenpiJsonl => {
                let fallback = self.input.source_path().and_then(file_mtime_ms);
                ResponseReader::Senpi(Box::new(
                    SenpiReader::new("senpi", &self.key.session).with_fallback_timestamp(fallback),
                ))
            }
            SourceFormat::AntigravityJsonl => {
                let fallback = self.input.source_path().and_then(file_mtime_ms);
                ResponseReader::Antigravity(Box::new(
                    AntigravityReader::new(&self.key.session).with_fallback_timestamp(fallback),
                ))
            }
            SourceFormat::QwenJsonl => {
                let fallback = self.input.source_path().and_then(file_mtime_ms);
                ResponseReader::Qwen(Box::new(
                    QwenReader::new(&self.key.session).with_fallback_timestamp(fallback),
                ))
            }
            SourceFormat::ReasonixJsonl => {
                ResponseReader::Reasonix(Box::new(ReasonixReader::new(&self.key.session)))
            }
            SourceFormat::MuseJsonl => {
                let fallback = self.input.source_path().and_then(file_mtime_ms);
                ResponseReader::Muse(Box::new(
                    MuseReader::new(&self.key.session).with_fallback_timestamp(fallback),
                ))
            }
            SourceFormat::McodeJsonl => {
                let fallback = self.input.source_path().and_then(file_mtime_ms);
                ResponseReader::Mcode(Box::new(
                    McodeReader::new().with_fallback_timestamp(fallback),
                ))
            }
            SourceFormat::OpenClawJsonl => {
                let fallback = self.input.source_path().and_then(file_mtime_ms);
                ResponseReader::OpenClaw(Box::new(
                    OpenClawReader::new("openclaw").with_fallback_timestamp(fallback),
                ))
            }
            SourceFormat::GrokJsonl => {
                let fallback = self.input.source_path().and_then(file_mtime_ms);
                ResponseReader::Grok(Box::new(
                    GrokReader::new(&self.key.session).with_fallback_timestamp(fallback),
                ))
            }
            SourceFormat::CommandCodeJsonl => {
                let fallback = self.input.source_path().and_then(file_mtime_ms);
                let path = self.input.source_path();
                ResponseReader::CommandCode(Box::new(
                    CommandCodeReader::new(&self.key.session)
                        .with_fallback_timestamp(fallback)
                        .with_workspace(
                            path.and_then(commandcode_workspace_key),
                            path.and_then(commandcode_workspace_label),
                        )
                        .with_config_model(path.and_then(commandcode_config_model)),
                ))
            }
            SourceFormat::KimiJsonl => {
                let fallback = self.input.source_path().and_then(file_mtime_ms);
                ResponseReader::Kimi(Box::new(
                    KimiReader::new(&self.key.session).with_fallback_timestamp(fallback),
                ))
            }
            SourceFormat::ZcodeJsonl => {
                let fallback = self.input.source_path().and_then(file_mtime_ms);
                ResponseReader::Zcode(Box::new(
                    ZcodeReader::new(&self.key.session).with_fallback_timestamp(fallback),
                ))
            }
            SourceFormat::SessionArchiveSqlite
            | SourceFormat::CodexLocalUsageSqlite
            | SourceFormat::OpenCodeSqlite
            | SourceFormat::KiloSqlite
            | SourceFormat::MimoSqlite
            | SourceFormat::ZcodeSqlite
            | SourceFormat::GooseSqlite
            | SourceFormat::HermesSqlite
            | SourceFormat::OpenClawSqlite
            | SourceFormat::ZedSqlite
            | SourceFormat::WorkBuddySqlite
            | SourceFormat::AntigravitySqlite => {
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
            crate::source_inputs::SqliteInputData::OpenCode(rows) => (rows, Arc::from([])),
            crate::source_inputs::SqliteInputData::Kilo(rows) => (rows, Arc::from([])),
            crate::source_inputs::SqliteInputData::Mimo(rows) => (rows, Arc::from([])),
            crate::source_inputs::SqliteInputData::Zcode(rows) => (rows, Arc::from([])),
            crate::source_inputs::SqliteInputData::Goose(rows) => (rows, Arc::from([])),
            crate::source_inputs::SqliteInputData::Hermes(rows) => (rows, Arc::from([])),
            crate::source_inputs::SqliteInputData::OpenClaw(rows) => (rows, Arc::from([])),
            crate::source_inputs::SqliteInputData::Zed(rows) => (rows, Arc::from([])),
            crate::source_inputs::SqliteInputData::WorkBuddySqlite(rows) => (rows, Arc::from([])),
            crate::source_inputs::SqliteInputData::AntigravitySqlite(rows) => (rows, Arc::from([])),
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

fn cursor_account_from_path(path: &Path) -> String {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("usage.json");
    if name == "usage.json" {
        return "active".to_owned();
    }
    name.strip_prefix("usage.")
        .and_then(|name| name.strip_suffix(".json"))
        .filter(|name| !name.is_empty())
        .map(|name| {
            name.chars()
                .map(|character| {
                    if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                        character
                    } else {
                        '-'
                    }
                })
                .collect()
        })
        .filter(|name: &String| !name.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
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

/// Command Code transcripts live at
/// `<root>/.commandcode/projects/<slug>/<session>.jsonl`, so the project slug
/// names the workspace.
fn commandcode_workspace_key(path: &Path) -> Option<String> {
    let parts = path
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect::<Vec<_>>();
    let projects = parts.iter().position(|part| *part == "projects")?;
    let slug = parts.get(projects + 1)?;
    if slug.is_empty() || slug.chars().any(char::is_control) {
        return None;
    }
    Some((*slug).to_owned())
}

/// The last path segment of the workspace key, which is the readable project
/// name rather than the full encoded path.
fn commandcode_workspace_label(path: &Path) -> Option<String> {
    let key = commandcode_workspace_key(path)?;
    let label = key.rsplit('/').find(|segment| !segment.is_empty())?;
    Some(label.to_owned())
}

/// Read the configured agent model from `<root>/config.json`.
///
/// The transcript can sit at any depth under `projects`, so counting parents
/// would land on a different `config.json` for nested paths. The `projects`
/// directory is the anchor instead: the Command Code root is its parent.
fn commandcode_config_model(path: &Path) -> Option<String> {
    let root = path
        .ancestors()
        .find(|ancestor| ancestor.file_name().and_then(|name| name.to_str()) == Some("projects"))?
        .parent()?;
    let bytes = fs::read(root.join("config.json")).ok()?;
    let config: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    config
        .get("model")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(str::to_owned)
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
