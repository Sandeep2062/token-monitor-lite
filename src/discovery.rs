//! Deterministic, bounded discovery of native paths and read-only legacy inputs.
//! Paths stay in backend-owned types and diagnostics never render their values.

use crate::aggregation::MAX_SOURCES;
use crate::parsers::jsonl::MAX_BATCH_BYTES;
use crate::runtime::MonitoredSource;
use crate::source_inputs::{
    AntigravitySqliteAdapter, BbAdapter, CatpawAdapter, CherrystudioAdapter,
    CodexLocalUsageAdapter, DevinAdapter, GooseAdapter, HermesAdapter, KiloAdapter,
    MAX_DECODED_BYTES, MimoAdapter, OpenClawAdapter, OpenCodeAdapter, QodercnAdapter,
    SessionArchiveAdapter, SourceInput, SqliteSourceError, UnslothAdapter, WorkBuddySqliteAdapter,
    ZcodeAdapter, ZedAdapter, has_zstd_suffix, is_compressed_source_path, is_opencode_db_filename,
};
use crate::sources::{SourceFormat, SourceId, SourceProgress, SourceReader};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};
use std::env;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

pub const MAX_ARTIFACTS: usize = 64;
pub const MAX_ARTIFACT_DEPTH: usize = 8;
pub const MAX_ARTIFACT_FILE_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_ARTIFACT_TOTAL_BYTES: u64 = 512 * 1024 * 1024;
const MAX_SOURCE_DEPTH: usize = 16;
const MAX_SOURCE_CANDIDATES: usize = MAX_SOURCES * 2;
const MAX_SOURCE_FILE_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostPlatform {
    Windows,
    MacOs,
    Linux,
}

#[derive(Clone, PartialEq, Eq)]
pub struct HostPaths {
    pub platform: HostPlatform,
    pub home: PathBuf,
    pub app_data: Option<PathBuf>,
    pub local_app_data: Option<PathBuf>,
    pub xdg_config_home: Option<PathBuf>,
    pub xdg_data_home: Option<PathBuf>,
    pub xdg_state_home: Option<PathBuf>,
    pub legacy_shared_override: Option<PathBuf>,
    pub native_data_override: Option<PathBuf>,
    pub enabled_clients: BTreeSet<String>,
    pub env_paths: BTreeMap<String, PathBuf>,
}

impl HostPaths {
    /// Acquire host inputs once. Path resolution itself remains pure and testable.
    pub fn current() -> Result<Self, DiscoveryError> {
        let platform = if cfg!(target_os = "windows") {
            HostPlatform::Windows
        } else if cfg!(target_os = "macos") {
            HostPlatform::MacOs
        } else if cfg!(target_os = "linux") {
            HostPlatform::Linux
        } else {
            return Err(DiscoveryError::UnsupportedPlatform);
        };

        let path = |name| env::var_os(name).map(PathBuf::from);
        let home = path(if platform == HostPlatform::Windows {
            "USERPROFILE"
        } else {
            "HOME"
        })
        .ok_or(DiscoveryError::RequiredRootUnavailable)?;
        let enabled_clients = env::var("TOKEN_MONITOR_CLIENTS")
            .ok()
            .into_iter()
            .flat_map(|value| {
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_lowercase)
                    .collect::<Vec<_>>()
            })
            .collect();
        let env_paths = [
            "CLINE_DIR",
            "CLINE_DATA_DIR",
            "CLINE_SESSION_DATA_DIR",
            "GEMINI_CLI_HOME",
            "GOOSE_PATH_ROOT",
            "TOKSCALE_CONFIG_DIR",
        ]
        .into_iter()
        .filter_map(|name| {
            env::var_os(name)
                .filter(|value| !value.to_string_lossy().trim().is_empty())
                .map(|value| (name.to_owned(), PathBuf::from(value)))
        })
        .collect();

        Ok(Self {
            platform,
            home,
            app_data: path("APPDATA"),
            local_app_data: path("LOCALAPPDATA"),
            xdg_config_home: path("XDG_CONFIG_HOME"),
            xdg_data_home: path("XDG_DATA_HOME"),
            xdg_state_home: path("XDG_STATE_HOME"),
            legacy_shared_override: path("TOKEN_MONITOR_SHARED_DIR"),
            native_data_override: path("TOKEN_MONITOR_LITE_DATA_DIR"),
            enabled_clients,
            env_paths,
        })
    }
}

impl fmt::Debug for HostPaths {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HostPaths")
            .field("platform", &self.platform)
            .field("paths", &format_args!("[redacted]"))
            .field("enabled_clients", &self.enabled_clients)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct NativePaths {
    root: PathBuf,
}

impl NativePaths {
    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl fmt::Debug for NativePaths {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("NativePaths { paths: [redacted] }")
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct ResolvedPaths {
    pub native: NativePaths,
    pub legacy_user_data: PathBuf,
    pub legacy_shared_data: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ArtifactKind {
    Settings,
    Credentials,
    LimitsHistory,
    HubDevices,
    IcloudRevisions,
    SessionUsageArchive,
    SessionUsageArchiveWal,
    SessionUsageArchiveShm,
    SessionUsageArchiveJson,
    DailyHistoryArchive,
    CodexLocalUsage,
    CodexLocalUsageWal,
    CodexLocalUsageShm,
    PluginManager,
    PluginPackage,
    PluginSettings,
    PluginData,
    PluginCredentials,
}

#[derive(Clone, PartialEq, Eq)]
pub struct DiscoveredArtifact {
    pub kind: ArtifactKind,
    pub ordinal: usize,
    pub bytes: u64,
    pub entries: usize,
    pub eligible_for_staging: bool,
    path: PathBuf,
}

impl DiscoveredArtifact {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl fmt::Debug for DiscoveredArtifact {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DiscoveredArtifact")
            .field("kind", &self.kind)
            .field("ordinal", &self.ordinal)
            .field("bytes", &self.bytes)
            .field("entries", &self.entries)
            .field("eligible_for_staging", &self.eligible_for_staging)
            .field("path", &format_args!("[redacted]"))
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct LegacyInstallation {
    user_data: PathBuf,
    shared_data: PathBuf,
    pub artifacts: Vec<DiscoveredArtifact>,
}

impl LegacyInstallation {
    pub fn user_data(&self) -> &Path {
        &self.user_data
    }

    pub fn shared_data(&self) -> &Path {
        &self.shared_data
    }
}

impl fmt::Debug for LegacyInstallation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LegacyInstallation")
            .field("paths", &format_args!("[redacted]"))
            .field("artifacts", &self.artifacts)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct DiscoveredSource {
    pub source_id: SourceId,
    pub format: SourceFormat,
    pub session: String,
    path: PathBuf,
}

impl DiscoveredSource {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn monitored_source(&self) -> MonitoredSource {
        MonitoredSource {
            source_id: self.source_id,
            path: self.path.clone(),
            format: self.format,
            session: self.session.clone(),
            companions: self.companions(),
        }
    }

    /// Files that belong to this source but sit beside it.
    ///
    /// Derived from `path` rather than stored on the struct so the discovery
    /// sites stay unaware of it, and so a companion that is absent needs no
    /// directory scan to rule out.
    fn companions(&self) -> Vec<PathBuf> {
        match self.format {
            // A Jcode session's journal amends its snapshot: it appends the
            // unsaved suffix and may repeat an id to correct it. They are one
            // session, so they are one source.
            SourceFormat::JcodeJson => match self.path.file_stem().and_then(|stem| stem.to_str()) {
                Some(stem) => vec![self.path.with_file_name(format!("{stem}.journal.jsonl"))],
                None => Vec::new(),
            },
            _ => Vec::new(),
        }
    }
}

impl fmt::Debug for DiscoveredSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DiscoveredSource")
            .field("format", &self.format)
            .field("identity", &format_args!("[redacted]"))
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiscoveryNotice {
    ArtifactNotRegular {
        kind: ArtifactKind,
        ordinal: usize,
    },
    ArtifactFileTooLarge {
        kind: ArtifactKind,
        ordinal: usize,
    },
    ArtifactDepthExceeded {
        kind: ArtifactKind,
        ordinal: usize,
    },
    ArtifactEntryLimitExceeded {
        kind: ArtifactKind,
        ordinal: usize,
    },
    ArtifactTotalBytesExceeded {
        kind: ArtifactKind,
        ordinal: usize,
    },
    ArtifactLimitReached,
    UnsupportedClient {
        client: String,
    },
    SourceNotRegular {
        client: &'static str,
        ordinal: usize,
    },
    SourceInvalid {
        client: &'static str,
        ordinal: usize,
    },
    SourceIdentityCollision,
    SourceLimitReached,
    SourceScanLimitReached,
    SourceUnsupportedCompression {
        client: &'static str,
        ordinal: usize,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveryReport {
    pub native: NativePaths,
    pub legacy: Option<LegacyInstallation>,
    pub sources: Vec<DiscoveredSource>,
    pub notices: Vec<DiscoveryNotice>,
}

impl fmt::Debug for ResolvedPaths {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ResolvedPaths { paths: [redacted] }")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DiscoveryError {
    #[error("required discovery root is unavailable")]
    RequiredRootUnavailable,
    #[error("discovery root is invalid")]
    InvalidRoot,
    #[error("native and legacy roots overlap")]
    OverlappingRoots,
    #[error("host platform is unsupported")]
    UnsupportedPlatform,
    #[error("discovery input/output unavailable ({0:?})")]
    Io(io::ErrorKind),
}

impl From<io::Error> for DiscoveryError {
    fn from(error: io::Error) -> Self {
        Self::Io(error.kind())
    }
}

pub fn discover(inputs: &HostPaths) -> Result<DiscoveryReport, DiscoveryError> {
    let resolved = resolve_paths(inputs)?;
    let mut notices = Vec::new();
    let legacy = discover_legacy(&resolved, &mut notices)?;
    let mut sources = discover_sources(inputs, &mut notices)?;
    if let Some(legacy) = &legacy {
        sources.extend(discover_sqlite_sources(legacy.shared_data(), &mut notices)?);
    }
    if sources.len() > MAX_SOURCES {
        sources.truncate(MAX_SOURCES);
        notices.push(DiscoveryNotice::SourceLimitReached);
    }
    Ok(DiscoveryReport {
        native: resolved.native,
        legacy,
        sources,
        notices,
    })
}

pub fn resolve_paths(inputs: &HostPaths) -> Result<ResolvedPaths, DiscoveryError> {
    let home = checked_root(inputs.platform, &inputs.home, true)?;
    let default_native = match inputs.platform {
        HostPlatform::Windows => join(
            inputs.platform,
            &checked_optional(inputs.platform, inputs.local_app_data.as_deref())?,
            "Token Monitor Lite",
        ),
        HostPlatform::MacOs => join_many(
            inputs.platform,
            &home,
            &["Library", "Application Support", "Token Monitor Lite"],
        ),
        HostPlatform::Linux => join(
            inputs.platform,
            inputs
                .xdg_state_home
                .as_deref()
                .map(|root| checked_root(inputs.platform, root, false))
                .transpose()?
                .unwrap_or_else(|| join_many(inputs.platform, &home, &[".local", "state"]))
                .as_path(),
            "token-monitor-lite",
        ),
    };
    let default_legacy = match inputs.platform {
        HostPlatform::Windows => join(
            inputs.platform,
            &checked_optional(inputs.platform, inputs.app_data.as_deref())?,
            "Token Monitor",
        ),
        HostPlatform::MacOs => join_many(
            inputs.platform,
            &home,
            &["Library", "Application Support", "Token Monitor"],
        ),
        HostPlatform::Linux => join(
            inputs.platform,
            inputs
                .xdg_config_home
                .as_deref()
                .map(|root| checked_root(inputs.platform, root, false))
                .transpose()?
                .unwrap_or_else(|| join(inputs.platform, &home, ".config"))
                .as_path(),
            "Token Monitor",
        ),
    };

    let native_root = inputs
        .native_data_override
        .as_deref()
        .map(|root| checked_root(inputs.platform, root, false))
        .transpose()?
        .unwrap_or(default_native);
    let legacy_shared_data = inputs
        .legacy_shared_override
        .as_deref()
        .map(|root| checked_root(inputs.platform, root, false))
        .transpose()?
        .unwrap_or_else(|| default_legacy.clone());

    if overlaps(inputs.platform, &native_root, &default_legacy)
        || overlaps(inputs.platform, &native_root, &legacy_shared_data)
    {
        return Err(DiscoveryError::OverlappingRoots);
    }

    Ok(ResolvedPaths {
        native: NativePaths { root: native_root },
        legacy_user_data: default_legacy,
        legacy_shared_data,
    })
}

fn checked_optional(
    platform: HostPlatform,
    value: Option<&Path>,
) -> Result<PathBuf, DiscoveryError> {
    let value = value.ok_or(DiscoveryError::RequiredRootUnavailable)?;
    checked_root(platform, value, true)
}

fn checked_root(
    platform: HostPlatform,
    value: &Path,
    required: bool,
) -> Result<PathBuf, DiscoveryError> {
    if value.as_os_str().is_empty() {
        return Err(if required {
            DiscoveryError::RequiredRootUnavailable
        } else {
            DiscoveryError::InvalidRoot
        });
    }
    match platform {
        HostPlatform::Windows => normalize_windows(value),
        HostPlatform::MacOs | HostPlatform::Linux => normalize_unix(value),
    }
}

fn normalize_unix(value: &Path) -> Result<PathBuf, DiscoveryError> {
    if !value.is_absolute()
        || value
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(DiscoveryError::InvalidRoot);
    }
    Ok(value.components().collect())
}

fn normalize_windows(value: &Path) -> Result<PathBuf, DiscoveryError> {
    let value = value.to_str().ok_or(DiscoveryError::InvalidRoot)?;
    let absolute = value.starts_with("\\\\")
        || (value.len() >= 3
            && value.as_bytes()[0].is_ascii_alphabetic()
            && value.as_bytes()[1] == b':'
            && matches!(value.as_bytes()[2], b'\\' | b'/'));
    if !absolute
        || value
            .split(['\\', '/'])
            .any(|component| matches!(component, "." | ".."))
    {
        return Err(DiscoveryError::InvalidRoot);
    }
    let normalized = value.trim_end_matches(['\\', '/']);
    if normalized.ends_with(':') {
        Ok(PathBuf::from(format!("{normalized}\\")))
    } else {
        Ok(PathBuf::from(normalized))
    }
}

fn join(platform: HostPlatform, root: &Path, child: &str) -> PathBuf {
    if platform == HostPlatform::Windows {
        PathBuf::from(format!(
            "{}\\{}",
            root.to_string_lossy().trim_end_matches(['\\', '/']),
            child
        ))
    } else {
        root.join(child)
    }
}

fn join_many(platform: HostPlatform, root: &Path, children: &[&str]) -> PathBuf {
    children.iter().fold(root.to_path_buf(), |path, child| {
        join(platform, &path, child)
    })
}

fn overlaps(platform: HostPlatform, left: &Path, right: &Path) -> bool {
    if platform == HostPlatform::Windows {
        let left = left.to_string_lossy().replace('/', "\\").to_lowercase();
        let right = right.to_string_lossy().replace('/', "\\").to_lowercase();
        left == right
            || left.starts_with(&format!("{right}\\"))
            || right.starts_with(&format!("{left}\\"))
    } else {
        left == right || left.starts_with(right) || right.starts_with(left)
    }
}

const USER_FILES: &[(ArtifactKind, &str)] = &[
    (ArtifactKind::Settings, "settings.json"),
    (ArtifactKind::Credentials, "credentials.json"),
    (ArtifactKind::LimitsHistory, "limits-history.sqlite"),
    (ArtifactKind::HubDevices, "hub-devices.json"),
    (ArtifactKind::IcloudRevisions, "icloud-revisions.json"),
];
const SHARED_FILES: &[(ArtifactKind, &str)] = &[
    (
        ArtifactKind::SessionUsageArchive,
        "session-usage-archive.sqlite",
    ),
    (
        ArtifactKind::SessionUsageArchiveWal,
        "session-usage-archive.sqlite-wal",
    ),
    (
        ArtifactKind::SessionUsageArchiveShm,
        "session-usage-archive.sqlite-shm",
    ),
    (
        ArtifactKind::SessionUsageArchiveJson,
        "session-usage-archive.json",
    ),
    (
        ArtifactKind::DailyHistoryArchive,
        "daily-history-archive.json",
    ),
    (ArtifactKind::CodexLocalUsage, "codex-local-usage.sqlite"),
    (
        ArtifactKind::CodexLocalUsageWal,
        "codex-local-usage.sqlite-wal",
    ),
    (
        ArtifactKind::CodexLocalUsageShm,
        "codex-local-usage.sqlite-shm",
    ),
];
const USER_DIRECTORIES: &[(ArtifactKind, &str)] = &[
    (ArtifactKind::PluginManager, "plugin-manager"),
    (ArtifactKind::PluginSettings, "plugin-settings"),
    (ArtifactKind::PluginData, "plugin-data"),
    (ArtifactKind::PluginCredentials, "plugin-credentials"),
];
const MAX_ARTIFACT_ENTRIES: usize = 4_096;

fn discover_legacy(
    resolved: &ResolvedPaths,
    notices: &mut Vec<DiscoveryNotice>,
) -> Result<Option<LegacyInstallation>, DiscoveryError> {
    let user_exists = existing_directory(&resolved.legacy_user_data)?;
    let shared_exists = existing_directory(&resolved.legacy_shared_data)?;
    if !user_exists && !shared_exists {
        return Ok(None);
    }

    let mut builder = ManifestBuilder {
        artifacts: Vec::new(),
        notices,
        total_bytes: 0,
        limit_reported: false,
    };
    if user_exists {
        for &(kind, name) in USER_FILES {
            builder.add(kind, resolved.legacy_user_data.join(name), false)?;
        }
        for &(kind, name) in USER_DIRECTORIES {
            builder.add(kind, resolved.legacy_user_data.join(name), true)?;
        }
        builder.add_plugin_packages(&resolved.legacy_user_data.join("plugins"))?;
    }
    if shared_exists {
        for &(kind, name) in SHARED_FILES {
            builder.add(kind, resolved.legacy_shared_data.join(name), false)?;
        }
    }

    Ok(Some(LegacyInstallation {
        user_data: resolved.legacy_user_data.clone(),
        shared_data: resolved.legacy_shared_data.clone(),
        artifacts: builder.artifacts,
    }))
}

fn existing_directory(path: &Path) -> Result<bool, DiscoveryError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(true),
        Ok(_) => Err(DiscoveryError::InvalidRoot),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

struct ManifestBuilder<'a> {
    artifacts: Vec<DiscoveredArtifact>,
    notices: &'a mut Vec<DiscoveryNotice>,
    total_bytes: u64,
    limit_reported: bool,
}

impl ManifestBuilder<'_> {
    fn add(
        &mut self,
        kind: ArtifactKind,
        path: PathBuf,
        directory: bool,
    ) -> Result<(), DiscoveryError> {
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        if self.artifacts.len() == MAX_ARTIFACTS {
            if !self.limit_reported {
                self.notices.push(DiscoveryNotice::ArtifactLimitReached);
                self.limit_reported = true;
            }
            return Ok(());
        }
        let ordinal = self
            .artifacts
            .iter()
            .filter(|artifact| artifact.kind == kind)
            .count()
            + 1;
        let expected_type = if directory {
            metadata.file_type().is_dir()
        } else {
            metadata.file_type().is_file()
        };
        if !expected_type {
            self.notices
                .push(DiscoveryNotice::ArtifactNotRegular { kind, ordinal });
            self.artifacts.push(DiscoveredArtifact {
                kind,
                ordinal,
                bytes: 0,
                entries: 1,
                eligible_for_staging: false,
                path,
            });
            return Ok(());
        }

        let scan = if directory {
            scan_artifact_directory(&path)?
        } else {
            ArtifactScan {
                bytes: metadata.len(),
                entries: 1,
                file_too_large: metadata.len() > MAX_ARTIFACT_FILE_BYTES,
                depth_exceeded: false,
                entry_limit_exceeded: false,
                non_regular: false,
            }
        };
        let mut eligible = true;
        if scan.non_regular {
            self.notices
                .push(DiscoveryNotice::ArtifactNotRegular { kind, ordinal });
            eligible = false;
        }
        if scan.file_too_large {
            self.notices
                .push(DiscoveryNotice::ArtifactFileTooLarge { kind, ordinal });
            eligible = false;
        }
        if scan.depth_exceeded {
            self.notices
                .push(DiscoveryNotice::ArtifactDepthExceeded { kind, ordinal });
            eligible = false;
        }
        if scan.entry_limit_exceeded {
            self.notices
                .push(DiscoveryNotice::ArtifactEntryLimitExceeded { kind, ordinal });
            eligible = false;
        }
        let next_total = self.total_bytes.saturating_add(scan.bytes);
        if next_total > MAX_ARTIFACT_TOTAL_BYTES {
            self.notices
                .push(DiscoveryNotice::ArtifactTotalBytesExceeded { kind, ordinal });
            eligible = false;
        }
        self.total_bytes = next_total;
        self.artifacts.push(DiscoveredArtifact {
            kind,
            ordinal,
            bytes: scan.bytes,
            entries: scan.entries,
            eligible_for_staging: eligible,
            path,
        });
        Ok(())
    }

    fn add_plugin_packages(&mut self, root: &Path) -> Result<(), DiscoveryError> {
        let metadata = match fs::symlink_metadata(root) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        if !metadata.file_type().is_dir() {
            return self.add(ArtifactKind::PluginPackage, root.to_path_buf(), true);
        }
        let keep = MAX_ARTIFACTS + 1;
        let mut children = BinaryHeap::with_capacity(keep);
        for entry in fs::read_dir(root)? {
            let path = entry?.path();
            if children.len() < keep {
                children.push(path);
            } else if children.peek().is_some_and(|largest| path < *largest) {
                children.pop();
                children.push(path);
            }
        }
        for child in children.into_sorted_vec() {
            self.add(ArtifactKind::PluginPackage, child, true)?;
        }
        Ok(())
    }
}

#[derive(Default)]
struct ArtifactScan {
    bytes: u64,
    entries: usize,
    file_too_large: bool,
    depth_exceeded: bool,
    entry_limit_exceeded: bool,
    non_regular: bool,
}

fn scan_artifact_directory(root: &Path) -> Result<ArtifactScan, DiscoveryError> {
    let mut scan = ArtifactScan {
        entries: 1,
        ..ArtifactScan::default()
    };
    scan_artifact_children(root, 0, &mut scan)?;
    Ok(scan)
}

fn scan_artifact_children(
    directory: &Path,
    depth: usize,
    scan: &mut ArtifactScan,
) -> Result<(), DiscoveryError> {
    if scan.entry_limit_exceeded {
        return Ok(());
    }
    let mut children = Vec::new();
    for entry in fs::read_dir(directory)? {
        children.push(entry?.path());
        if scan.entries.saturating_add(children.len()) > MAX_ARTIFACT_ENTRIES {
            scan.entry_limit_exceeded = true;
            scan.bytes = 0;
            scan.entries = MAX_ARTIFACT_ENTRIES + 1;
            return Ok(());
        }
    }
    children.sort();
    for child in children {
        scan.entries += 1;
        let metadata = fs::symlink_metadata(&child)?;
        let file_type = metadata.file_type();
        if file_type.is_file() {
            scan.bytes = scan.bytes.saturating_add(metadata.len());
            scan.file_too_large |= metadata.len() > MAX_ARTIFACT_FILE_BYTES;
        } else if file_type.is_dir() {
            let child_depth = depth + 1;
            if child_depth > MAX_ARTIFACT_DEPTH {
                scan.depth_exceeded = true;
            } else {
                scan_artifact_children(&child, child_depth, scan)?;
                if scan.entry_limit_exceeded {
                    return Ok(());
                }
            }
        } else {
            scan.non_regular = true;
        }
    }
    Ok(())
}

fn discover_sources(
    inputs: &HostPaths,
    notices: &mut Vec<DiscoveryNotice>,
) -> Result<Vec<DiscoveredSource>, DiscoveryError> {
    discover_sources_with_id(inputs, notices, SourceId::for_path)
}

/// OpenCode resolves its data directory through the XDG data root on every
/// supported host (`XDG_DATA_HOME`, else `<home>/.local/share`), then appends
/// `opencode`. The reference implementation does not use an Application
/// Support/AppData root, so none is guessed here.
fn opencode_data_dir(inputs: &HostPaths) -> Result<PathBuf, DiscoveryError> {
    let base = match inputs.xdg_data_home.as_deref() {
        Some(root) if !root.as_os_str().is_empty() => checked_root(inputs.platform, root, false)?,
        _ => join_many(inputs.platform, &inputs.home, &[".local", "share"]),
    };
    Ok(join(inputs.platform, &base, "opencode"))
}

fn collect_opencode_candidates(
    root: &Path,
    candidates: &mut Vec<PathBuf>,
) -> Result<(), DiscoveryError> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        if entry
            .file_name()
            .to_str()
            .is_some_and(is_opencode_db_filename)
        {
            candidates.push(entry.path());
        }
    }
    Ok(())
}

fn is_cursor_cache_filename(name: &str) -> bool {
    name == "usage.json"
        || (name.starts_with("usage.")
            && name.ends_with(".json")
            && name.len() > "usage..json".len())
}

fn cline_cli_sessions_root(inputs: &HostPaths) -> Result<PathBuf, DiscoveryError> {
    if let Some(root) = inputs.env_paths.get("CLINE_SESSION_DATA_DIR") {
        return checked_root(inputs.platform, root, false);
    }
    if let Some(root) = inputs.env_paths.get("CLINE_DATA_DIR") {
        let root = checked_root(inputs.platform, root, false)?;
        return Ok(join(inputs.platform, &root, "sessions"));
    }
    if let Some(root) = inputs.env_paths.get("CLINE_DIR") {
        let root = checked_root(inputs.platform, root, false)?;
        return Ok(join_many(inputs.platform, &root, &["data", "sessions"]));
    }
    Ok(join_many(
        inputs.platform,
        &inputs.home,
        &[".cline", "data", "sessions"],
    ))
}

/// Every VS Code `User` directory for this host.
///
/// VS Code keeps extension state under a platform-specific `User` directory, and
/// a remote or container session keeps a separate one, so a user can have the
/// same extension's state in more than one place. Paths are returned whether or
/// not they exist; the collectors treat an absent root as empty.
fn vscode_user_roots(inputs: &HostPaths) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let mut push = |base: Option<&Path>, user: &[&str]| {
        let Some(base) = base else {
            return;
        };
        let mut path = base.to_path_buf();
        for component in user {
            path.push(component);
        }
        roots.push(path);
    };
    match inputs.platform {
        HostPlatform::Windows => {
            if let Some(app_data) = inputs.app_data.as_deref() {
                push(Some(app_data), &["Code", "User"]);
                push(Some(app_data), &["Code - Insiders", "User"]);
            }
        }
        HostPlatform::MacOs => {
            push(
                Some(&inputs.home),
                &["Library", "Application Support", "Code", "User"],
            );
        }
        HostPlatform::Linux => {
            let config = inputs
                .xdg_config_home
                .as_deref()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| inputs.home.join(".config"));
            push(Some(&config), &["Code", "User"]);
        }
    }
    // A Remote-SSH/WSL/container window stores extension state in the server
    // directory, which lives in the home directory on every platform.
    push(Some(&inputs.home), &[".vscode-server", "data", "User"]);
    roots
}

/// Collect `<root>/<taskId>/ui_messages.json` for the Cline family.
fn collect_task_log_candidates(
    root: &Path,
    depth: usize,
    inspected: &mut usize,
    candidates: &mut Vec<PathBuf>,
    overflow: &mut bool,
) -> Result<(), DiscoveryError> {
    if *overflow {
        return Ok(());
    }
    if depth > 1 {
        *overflow = true;
        candidates.clear();
        return Ok(());
    }
    let metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_dir() {
        return Ok(());
    }
    let mut children = Vec::new();
    for entry in fs::read_dir(root)? {
        *inspected += 1;
        if *inspected > MAX_SOURCE_CANDIDATES {
            *overflow = true;
            candidates.clear();
            return Ok(());
        }
        children.push(entry?.path());
    }
    children.sort();
    for child in children {
        let child_metadata = fs::symlink_metadata(&child)?;
        if !child_metadata.file_type().is_dir() {
            continue;
        }
        // Only the task's own message log is usage. `api_conversation_history`
        // is the provider-facing conversation and carries no counters, and the
        // sibling metadata files carry none either.
        let candidate = child.join("ui_messages.json");
        match fs::symlink_metadata(&candidate) {
            Ok(metadata) if metadata.file_type().is_file() => {
                *inspected += 1;
                if *inspected > MAX_SOURCE_CANDIDATES {
                    *overflow = true;
                    candidates.clear();
                    return Ok(());
                }
                candidates.push(candidate);
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

/// Collect `<root>/*.jsonl` for the empty-window chat session store.
fn collect_copilot_chat_candidates(
    root: &Path,
    inspected: &mut usize,
    candidates: &mut Vec<PathBuf>,
    overflow: &mut bool,
) -> Result<(), DiscoveryError> {
    collect_jsonl_children(root, inspected, candidates, overflow)
}

/// Collect `<workspaceStorage>/<workspace>/chatSessions/*.jsonl`.
fn collect_copilot_workspace_candidates(
    root: &Path,
    depth: usize,
    inspected: &mut usize,
    candidates: &mut Vec<PathBuf>,
    overflow: &mut bool,
) -> Result<(), DiscoveryError> {
    if *overflow {
        return Ok(());
    }
    if depth > 1 {
        *overflow = true;
        candidates.clear();
        return Ok(());
    }
    let metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_dir() {
        return Ok(());
    }
    let mut children = Vec::new();
    for entry in fs::read_dir(root)? {
        *inspected += 1;
        if *inspected > MAX_SOURCE_CANDIDATES {
            *overflow = true;
            candidates.clear();
            return Ok(());
        }
        children.push(entry?.path());
    }
    children.sort();
    for child in children {
        let child_metadata = fs::symlink_metadata(&child)?;
        if child_metadata.file_type().is_dir() {
            collect_jsonl_children(&child.join("chatSessions"), inspected, candidates, overflow)?;
            if *overflow {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// Collect the `*.jsonl` files directly inside `directory`.
fn collect_jsonl_children(
    directory: &Path,
    inspected: &mut usize,
    candidates: &mut Vec<PathBuf>,
    overflow: &mut bool,
) -> Result<(), DiscoveryError> {
    if *overflow {
        return Ok(());
    }
    let metadata = match fs::symlink_metadata(directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_dir() {
        return Ok(());
    }
    let mut children = Vec::new();
    for entry in fs::read_dir(directory)? {
        *inspected += 1;
        if *inspected > MAX_SOURCE_CANDIDATES {
            *overflow = true;
            candidates.clear();
            return Ok(());
        }
        children.push(entry?.path());
    }
    children.sort();
    for child in children {
        let child_metadata = fs::symlink_metadata(&child)?;
        if !child_metadata.file_type().is_file() {
            continue;
        }
        if child
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
        {
            candidates.push(child);
        }
    }
    Ok(())
}

fn collect_cline_candidates(
    directory: &Path,
    depth: usize,
    inspected: &mut usize,
    candidates: &mut Vec<PathBuf>,
    overflow: &mut bool,
) -> Result<(), DiscoveryError> {
    if *overflow {
        return Ok(());
    }
    if depth > MAX_SOURCE_DEPTH {
        *overflow = true;
        candidates.clear();
        return Ok(());
    }
    let metadata = match fs::symlink_metadata(directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_dir() {
        return Ok(());
    }
    let mut children = Vec::new();
    for entry in fs::read_dir(directory)? {
        *inspected += 1;
        if *inspected > MAX_SOURCE_CANDIDATES {
            *overflow = true;
            candidates.clear();
            return Ok(());
        }
        children.push(entry?.path());
    }
    children.sort();
    for child in children {
        let metadata = fs::symlink_metadata(&child)?;
        if metadata.file_type().is_dir() {
            collect_cline_candidates(&child, depth + 1, inspected, candidates, overflow)?;
            if *overflow {
                return Ok(());
            }
        } else if child
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".messages.json"))
        {
            candidates.push(child);
        }
    }
    Ok(())
}

fn amp_thread_roots(inputs: &HostPaths) -> Result<Vec<PathBuf>, DiscoveryError> {
    let data_base = match inputs.xdg_data_home.as_deref() {
        Some(root) if !root.as_os_str().is_empty() => checked_root(inputs.platform, root, false)?,
        _ => join_many(inputs.platform, &inputs.home, &[".local", "share"]),
    };
    let config_base = match inputs.env_paths.get("TOKSCALE_CONFIG_DIR") {
        Some(root) => checked_root(inputs.platform, root, false)?,
        None => join_many(inputs.platform, &inputs.home, &[".config", "tokscale"]),
    };
    Ok(vec![
        join_many(inputs.platform, &data_base, &["amp", "threads"]),
        join_many(inputs.platform, &config_base, &["amp-cache", "threads"]),
    ])
}

fn collect_amp_candidates(
    directory: &Path,
    depth: usize,
    inspected: &mut usize,
    candidates: &mut Vec<PathBuf>,
    overflow: &mut bool,
) -> Result<(), DiscoveryError> {
    if *overflow {
        return Ok(());
    }
    if depth > MAX_SOURCE_DEPTH {
        *overflow = true;
        candidates.clear();
        return Ok(());
    }
    let metadata = match fs::symlink_metadata(directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_dir() {
        return Ok(());
    }
    let mut children = Vec::new();
    for entry in fs::read_dir(directory)? {
        *inspected += 1;
        if *inspected > MAX_SOURCE_CANDIDATES {
            *overflow = true;
            candidates.clear();
            return Ok(());
        }
        children.push(entry?.path());
    }
    children.sort();
    for child in children {
        let metadata = fs::symlink_metadata(&child)?;
        if metadata.file_type().is_dir() {
            collect_amp_candidates(&child, depth + 1, inspected, candidates, overflow)?;
            if *overflow {
                return Ok(());
            }
        } else if child
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
        {
            candidates.push(child);
        }
    }
    Ok(())
}

fn goose_database_path(inputs: &HostPaths) -> Option<PathBuf> {
    if let Some(root) = inputs.env_paths.get("GOOSE_PATH_ROOT") {
        let text = root.to_string_lossy();
        if !text.trim().is_empty()
            && !text.chars().next().is_some_and(char::is_whitespace)
            && root.is_absolute()
        {
            return Some(join_many(
                inputs.platform,
                root,
                &["data", "sessions", "sessions.db"],
            ));
        }
    }
    let data_base = match inputs.xdg_data_home.as_deref() {
        Some(root) if !root.as_os_str().is_empty() && !root.to_string_lossy().trim().is_empty() => {
            root.to_path_buf()
        }
        _ => join_many(inputs.platform, &inputs.home, &[".local", "share"]),
    };
    let mut candidates = Vec::new();
    if inputs.platform == HostPlatform::Windows {
        if let Some(app_data) = inputs.app_data.as_deref() {
            candidates.push(join_many(
                inputs.platform,
                app_data,
                &["Block", "goose", "data", "sessions", "sessions.db"],
            ));
        }
        candidates.push(join_many(
            inputs.platform,
            &inputs.home,
            &[
                "AppData",
                "Roaming",
                "Block",
                "goose",
                "data",
                "sessions",
                "sessions.db",
            ],
        ));
    }
    candidates.push(join_many(
        inputs.platform,
        &data_base,
        &["goose", "sessions", "sessions.db"],
    ));
    candidates.push(join_many(
        inputs.platform,
        &inputs.home,
        &[
            "Library",
            "Application Support",
            "goose",
            "sessions",
            "sessions.db",
        ],
    ));
    candidates.push(join_many(
        inputs.platform,
        &inputs.home,
        &[
            "Library",
            "Application Support",
            "Block",
            "goose",
            "sessions",
            "sessions.db",
        ],
    ));
    candidates.push(join_many(
        inputs.platform,
        &inputs.home,
        &[
            ".local",
            "share",
            "Block",
            "goose",
            "sessions",
            "sessions.db",
        ],
    ));
    candidates.into_iter().find(|candidate| candidate.is_file())
}

/// Hermes resolves its home from `HERMES_HOME`, then `~/.hermes`, then the
/// Windows-native `%LOCALAPPDATA%\hermes` (reference `resolveHermesHome`). The
/// native resolver keeps the same precedence and requires each candidate to
/// actually hold a `state.db`, so a stale `~/.hermes` directory does not shadow
/// a real Windows install.
fn hermes_home(inputs: &HostPaths) -> PathBuf {
    if let Some(root) = inputs.env_paths.get("HERMES_HOME") {
        let text = root.to_string_lossy();
        if !text.trim().is_empty() && root.is_absolute() {
            return root.to_path_buf();
        }
    }
    let dot_hermes = join_many(inputs.platform, &inputs.home, &[".hermes"]);
    if dot_hermes.join("state.db").is_file() {
        return dot_hermes;
    }
    if inputs.platform == HostPlatform::Windows {
        let local_app_data = inputs
            .env_paths
            .get("LOCALAPPDATA")
            .filter(|path| path.is_absolute() && !path.to_string_lossy().trim().is_empty())
            .cloned()
            .unwrap_or_else(|| join_many(inputs.platform, &inputs.home, &["AppData", "Local"]));
        let win_native = join_many(inputs.platform, &local_app_data, &["hermes"]);
        if win_native.join("state.db").is_file() {
            return win_native;
        }
    }
    dot_hermes
}

/// The base `state.db` plus each `profiles/<name>/state.db` the reference
/// watches. Sorted and deduplicated by the caller.
fn hermes_state_databases(root: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let base = root.join("state.db");
    if base.is_file() {
        paths.push(base);
    }
    let Ok(entries) = fs::read_dir(root.join("profiles")) else {
        return paths;
    };
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let candidate = entry.path().join("state.db");
        if candidate.is_file() {
            paths.push(candidate);
        }
    }
    paths
}

/// Which OpenClaw source a candidate file is, if any. An agent's live
/// transcript store is `agent/openclaw-agent.sqlite`; the transcripts beside it
/// are `sessions/<id>.jsonl` and the `.deleted`/`.reset` archives OpenClaw
/// publishes there. Everything else under `agents/` is not usage.
fn openclaw_candidate_format(path: &Path) -> Option<SourceFormat> {
    let name = path.file_name()?.to_str()?;
    if name == "openclaw-agent.sqlite" {
        return Some(SourceFormat::OpenClawSqlite);
    }
    // The process-held incognito store is a lexical sentinel, not a file to read.
    if name == "incognito-openclaw-agent.sqlite" {
        return None;
    }
    let in_sessions = path
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|dir| dir == "sessions");
    // `sessions.json` is the JSON index, not a line-oriented transcript.
    let transcript_shaped = name != "sessions.json"
        && name.split_once(".jsonl").is_some_and(|(stem, rest)| {
            !stem.is_empty() && (rest.is_empty() || rest.starts_with('.') || name.ends_with(".zst"))
        });
    (in_sessions && transcript_shaped).then_some(SourceFormat::OpenClawJsonl)
}

fn probe_openclaw_jsonl(path: &Path) -> Result<bool, SqliteSourceError> {
    let fallback = std::fs::metadata(path)
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .and_then(|duration| i64::try_from(duration.as_millis()).ok());
    let mut reader = crate::parsers::openclaw::OpenClawReader::new(
        path.file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("openclaw"),
    )
    .with_fallback_timestamp(fallback);
    let mut input = SourceInput::for_path(SourceFormat::OpenClawJsonl, path.to_path_buf());
    loop {
        let batch = match input.read_batch() {
            Ok(batch) => batch,
            Err(_) => return Err(SqliteSourceError::Invalid),
        };
        for record in &batch.records {
            // A single malformed line does not disqualify the file; the reader
            // skips only what it cannot interpret.
            let _ = reader.ingest(record);
        }
        if !batch.more_available {
            break;
        }
    }
    Ok(reader.has_supported_usage())
}

/// Walk an OpenClaw agents root. Its transcripts are not all `.jsonl`: the
/// `.deleted`/`.reset` archives keep the stamp in the name after the extension,
/// so the whole file name is filtered by [`openclaw_candidate_format`] instead
/// of by extension alone.
fn collect_openclaw_candidates(
    directory: &Path,
    depth: usize,
    inspected: &mut usize,
    candidates: &mut Vec<PathBuf>,
    overflow: &mut bool,
) -> Result<(), DiscoveryError> {
    if *overflow {
        return Ok(());
    }
    if depth > MAX_SOURCE_DEPTH {
        *overflow = true;
        candidates.clear();
        return Ok(());
    }
    let metadata = match fs::symlink_metadata(directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_dir() {
        return Ok(());
    }
    let mut children = Vec::new();
    for entry in fs::read_dir(directory)? {
        *inspected += 1;
        if *inspected > MAX_SOURCE_CANDIDATES {
            *overflow = true;
            candidates.clear();
            return Ok(());
        }
        children.push(entry?.path());
    }
    children.sort();
    for child in children {
        let metadata = fs::symlink_metadata(&child)?;
        if metadata.file_type().is_dir() {
            collect_openclaw_candidates(&child, depth + 1, inspected, candidates, overflow)?;
            if *overflow {
                return Ok(());
            }
        } else if openclaw_candidate_format(&child).is_some() {
            candidates.push(child);
        }
    }
    Ok(())
}

fn collect_copilot_candidates(
    directory: &Path,
    depth: usize,
    inspected: &mut usize,
    candidates: &mut Vec<PathBuf>,
    overflow: &mut bool,
) -> Result<(), DiscoveryError> {
    if *overflow {
        return Ok(());
    }
    if depth > MAX_SOURCE_DEPTH {
        *overflow = true;
        candidates.clear();
        return Ok(());
    }
    let metadata = match fs::symlink_metadata(directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_dir() {
        return Ok(());
    }
    let mut children = Vec::new();
    for entry in fs::read_dir(directory)? {
        *inspected += 1;
        if *inspected > MAX_SOURCE_CANDIDATES {
            *overflow = true;
            candidates.clear();
            return Ok(());
        }
        children.push(entry?.path());
    }
    children.sort();
    for child in children {
        let metadata = fs::symlink_metadata(&child)?;
        if metadata.file_type().is_dir() {
            collect_copilot_candidates(&child, depth + 1, inspected, candidates, overflow)?;
            if *overflow {
                return Ok(());
            }
        } else if child
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
        {
            candidates.push(child);
        }
    }
    Ok(())
}

fn collect_gemini_candidates(
    root: &Path,
    directory: &Path,
    depth: usize,
    inspected: &mut usize,
    candidates: &mut Vec<PathBuf>,
    overflow: &mut bool,
) -> Result<(), DiscoveryError> {
    if *overflow {
        return Ok(());
    }
    if depth > MAX_SOURCE_DEPTH {
        *overflow = true;
        candidates.clear();
        return Ok(());
    }
    let metadata = match fs::symlink_metadata(directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_dir() {
        return Ok(());
    }
    let mut children = Vec::new();
    for entry in fs::read_dir(directory)? {
        *inspected += 1;
        if *inspected > MAX_SOURCE_CANDIDATES {
            *overflow = true;
            candidates.clear();
            return Ok(());
        }
        children.push(entry?.path());
    }
    children.sort();
    for child in children {
        let metadata = fs::symlink_metadata(&child)?;
        if metadata.file_type().is_dir() {
            collect_gemini_candidates(root, &child, depth + 1, inspected, candidates, overflow)?;
            if *overflow {
                return Ok(());
            }
        } else if is_gemini_transcript_path(root, &child) {
            candidates.push(child);
        }
    }
    Ok(())
}

fn antigravity_database_roots(inputs: &HostPaths) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let gemini = inputs
        .env_paths
        .get("GEMINI_CLI_HOME")
        .cloned()
        .unwrap_or_else(|| inputs.home.join(".gemini"));
    for base in ["antigravity", "antigravity-ide", "antigravity-backup"] {
        roots.push(join_many(
            inputs.platform,
            &inputs.home,
            &[".gemini", base, "conversations"],
        ));
    }
    roots.push(join_many(
        inputs.platform,
        &gemini,
        &["antigravity-cli", "conversations"],
    ));
    for name in ["ANTIGRAVITY_HOME", "ANTIGRAVITY_DATA_DIR"] {
        if let Some(dir) = inputs
            .env_paths
            .get(name)
            .filter(|path| !path.as_os_str().is_empty())
        {
            roots.push(join_many(inputs.platform, dir, &["conversations"]));
        }
    }
    roots.sort();
    roots.dedup();
    roots
}

fn collect_db_candidates(
    directory: &Path,
    depth: usize,
    inspected: &mut usize,
    candidates: &mut Vec<PathBuf>,
    overflow: &mut bool,
) -> Result<(), DiscoveryError> {
    if *overflow {
        return Ok(());
    }
    if depth > MAX_SOURCE_DEPTH {
        *overflow = true;
        candidates.clear();
        return Ok(());
    }
    let metadata = match fs::symlink_metadata(directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_dir() {
        return Ok(());
    }
    let mut children = Vec::new();
    for entry in fs::read_dir(directory)? {
        *inspected += 1;
        if *inspected > MAX_SOURCE_CANDIDATES {
            *overflow = true;
            candidates.clear();
            return Ok(());
        }
        children.push(entry?.path());
    }
    children.sort();
    for child in children {
        let metadata = fs::symlink_metadata(&child)?;
        if metadata.file_type().is_dir() {
            collect_db_candidates(&child, depth + 1, inspected, candidates, overflow)?;
            if *overflow {
                return Ok(());
            }
        } else if child
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("db"))
        {
            candidates.push(child);
        }
    }
    Ok(())
}

fn workbuddy_database_paths(inputs: &HostPaths) -> Vec<PathBuf> {
    [".workbuddy", ".workbuddy-ai"]
        .into_iter()
        .map(|base| join_many(inputs.platform, &inputs.home, &[base, "workbuddy.db"]))
        .collect()
}

fn codebuddy_log_roots(inputs: &HostPaths) -> Vec<PathBuf> {
    let local = inputs
        .local_app_data
        .clone()
        .unwrap_or_else(|| inputs.home.join("AppData").join("Local"));
    let xdg = inputs
        .xdg_data_home
        .clone()
        .unwrap_or_else(|| inputs.home.join(".local").join("share"));
    [
        join_many(inputs.platform, &local, &["CodeBuddyExtension", "Logs"]),
        join_many(
            inputs.platform,
            &inputs.home,
            &[
                "Library",
                "Application Support",
                "CodeBuddyExtension",
                "Logs",
            ],
        ),
        join_many(inputs.platform, &xdg, &["CodeBuddyExtension", "Logs"]),
    ]
    .into_iter()
    .collect::<BTreeSet<_>>()
    .into_iter()
    .collect()
}

fn zed_database_paths(inputs: &HostPaths) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let data_base = inputs
        .xdg_data_home
        .clone()
        .unwrap_or_else(|| inputs.home.join(".local").join("share"));
    paths.push(join_many(
        inputs.platform,
        &data_base,
        &["zed", "threads", "threads.db"],
    ));
    paths.push(join_many(
        inputs.platform,
        &inputs.home,
        &[
            "Library",
            "Application Support",
            "Zed",
            "threads",
            "threads.db",
        ],
    ));
    let local = inputs
        .local_app_data
        .clone()
        .unwrap_or_else(|| inputs.home.join("AppData").join("Local"));
    paths.push(join_many(
        inputs.platform,
        &local,
        &["Zed", "threads", "threads.db"],
    ));
    paths.sort();
    paths.dedup();
    paths
}

fn tokscale_config_dir(inputs: &HostPaths) -> PathBuf {
    if let Some(dir) = inputs
        .env_paths
        .get("TOKSCALE_CONFIG_DIR")
        .filter(|path| !path.as_os_str().is_empty())
    {
        return dir.clone();
    }
    match inputs.platform {
        HostPlatform::Windows => {
            let base = inputs
                .app_data
                .clone()
                .unwrap_or_else(|| inputs.home.join("AppData").join("Roaming"));
            join_many(inputs.platform, &base, &["tokscale"])
        }
        _ => {
            let base = inputs
                .xdg_config_home
                .clone()
                .unwrap_or_else(|| inputs.home.join(".config"));
            join_many(inputs.platform, &base, &["tokscale"])
        }
    }
}

fn reasonix_home(inputs: &HostPaths) -> PathBuf {
    for name in ["REASONIX_STATE_HOME", "REASONIX_HOME"] {
        if let Some(dir) = inputs
            .env_paths
            .get(name)
            .filter(|path| !path.as_os_str().is_empty())
        {
            return dir.clone();
        }
    }
    match inputs.platform {
        HostPlatform::Windows => {
            let base = inputs
                .app_data
                .clone()
                .unwrap_or_else(|| inputs.home.join("AppData").join("Roaming"));
            join_many(inputs.platform, &base, &["reasonix"])
        }
        _ => join_many(inputs.platform, &inputs.home, &[".reasonix"]),
    }
}

fn mcode_session_roots(inputs: &HostPaths) -> Vec<PathBuf> {
    for name in ["MINIMAX_DATA_DIR", "MAVIS_DATA_DIR"] {
        if let Some(dir) = inputs
            .env_paths
            .get(name)
            .filter(|path| !path.as_os_str().is_empty())
        {
            return vec![join_many(inputs.platform, dir, &["v2", "sessions"])];
        }
    }
    let mut roots = [".minimax", ".mavis"]
        .into_iter()
        .map(|base| join_many(inputs.platform, &inputs.home, &[base, "v2", "sessions"]))
        .collect::<Vec<_>>();
    if let Ok(entries) = fs::read_dir(&inputs.home) {
        let mut profiles = entries
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
            .filter(|name| {
                [".minimax-", ".mavis-"]
                    .iter()
                    .any(|prefix| name.starts_with(prefix) && name.len() > prefix.len())
            })
            .collect::<Vec<_>>();
        profiles.sort();
        roots.extend(
            profiles
                .into_iter()
                .map(|name| join_many(inputs.platform, &inputs.home, &[&name, "v2", "sessions"])),
        );
    }
    if let Some(dir) = inputs
        .env_paths
        .get("TOKSCALE_HEADLESS_DIR")
        .filter(|path| !path.as_os_str().is_empty())
    {
        roots.push(join_many(inputs.platform, dir, &["mcode"]));
    } else {
        roots.push(join_many(
            inputs.platform,
            &inputs.home,
            &[".config", "tokscale", "headless", "mcode"],
        ));
        roots.push(join_many(
            inputs.platform,
            &inputs.home,
            &[
                "Library",
                "Application Support",
                "tokscale",
                "headless",
                "mcode",
            ],
        ));
    }
    roots.sort();
    roots.dedup();
    roots
}

/// Kimchi's agent directory, honouring its own override variable.
fn kimchi_agent_dir(inputs: &HostPaths) -> PathBuf {
    inputs
        .env_paths
        .get("KIMCHI_CODING_AGENT_DIR")
        .filter(|dir| !dir.as_os_str().is_empty())
        .cloned()
        .unwrap_or_else(|| join_many(inputs.platform, &inputs.home, &[".kimchi", "agent"]))
}

fn senpi_session_roots(inputs: &HostPaths) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    match inputs
        .env_paths
        .get("SENPI_CODING_AGENT_DIR")
        .filter(|path| !path.as_os_str().is_empty())
    {
        Some(dir) => roots.push(join(inputs.platform, dir, "sessions")),
        None => roots.push(join_many(
            inputs.platform,
            &inputs.home,
            &[".senpi", "agent", "sessions"],
        )),
    }
    if let Some(dir) = inputs
        .env_paths
        .get("SENPI_CODING_AGENT_SESSION_DIR")
        .filter(|path| !path.as_os_str().is_empty())
    {
        roots.push(dir.clone());
    }
    for parts in [
        &[".omo", "agent", "sessions"][..],
        &[".omo", "senpi-task", "children"],
        &[".omo", "memory"],
        &[".omo", "agent", "projects"],
    ] {
        roots.push(join_many(inputs.platform, &inputs.home, parts));
    }
    roots.sort();
    roots.dedup();
    roots
}

fn is_gemini_transcript_path(root: &Path, path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let Some(extension) = path.extension().and_then(|extension| extension.to_str()) else {
        return false;
    };
    if extension.eq_ignore_ascii_case("jsonl") {
        return true;
    }
    if !extension.eq_ignore_ascii_case("json") {
        return false;
    }
    if name.starts_with("session-") {
        return true;
    }
    path.strip_prefix(root)
        .ok()
        .map(|relative| {
            let components = relative
                .components()
                .map(|component| component.as_os_str())
                .collect::<Vec<_>>();
            components.len() == 3 && components[1].to_str() == Some("chats")
        })
        .unwrap_or(false)
}

fn discover_sources_with_id(
    inputs: &HostPaths,
    notices: &mut Vec<DiscoveryNotice>,
    mut source_id_for: impl FnMut(SourceFormat, &Path) -> SourceId,
) -> Result<Vec<DiscoveredSource>, DiscoveryError> {
    let mut sources = Vec::new();
    let mut admitted_ids = BTreeSet::new();
    let mut rejected_ids = BTreeSet::new();
    for client in &inputs.enabled_clients {
        if client == "opencode" {
            let root = opencode_data_dir(inputs)?;
            let mut candidates = Vec::new();
            collect_opencode_candidates(&root, &mut candidates)?;
            candidates.sort();
            candidates.dedup();
            for (index, path) in candidates.into_iter().enumerate() {
                if sources.len() == MAX_SOURCES {
                    notices.push(DiscoveryNotice::SourceLimitReached);
                    return Ok(sources);
                }
                let ordinal = index + 1;
                let metadata = fs::symlink_metadata(&path)?;
                if !metadata.file_type().is_file() {
                    notices.push(DiscoveryNotice::SourceNotRegular {
                        client: "opencode",
                        ordinal,
                    });
                    continue;
                }
                if metadata.len() > MAX_SOURCE_FILE_BYTES {
                    notices.push(DiscoveryNotice::SourceInvalid {
                        client: "opencode",
                        ordinal,
                    });
                    continue;
                }
                match OpenCodeAdapter::new(&path).read() {
                    Ok(rows) if !rows.is_empty() => {
                        let source_id = source_id_for(SourceFormat::OpenCodeSqlite, &path);
                        if rejected_ids.contains(&source_id) {
                            notices.push(DiscoveryNotice::SourceIdentityCollision);
                            continue;
                        }
                        if !admitted_ids.insert(source_id) {
                            sources
                                .retain(|source: &DiscoveredSource| source.source_id != source_id);
                            admitted_ids.remove(&source_id);
                            rejected_ids.insert(source_id);
                            notices.push(DiscoveryNotice::SourceIdentityCollision);
                            continue;
                        }
                        sources.push(DiscoveredSource {
                            source_id,
                            format: SourceFormat::OpenCodeSqlite,
                            session: "opencode".to_owned(),
                            path,
                        });
                    }
                    _ => notices.push(DiscoveryNotice::SourceInvalid {
                        client: "opencode",
                        ordinal,
                    }),
                }
            }
            continue;
        }
        if client == "gemini" {
            let root = join_many(inputs.platform, &inputs.home, &[".gemini", "tmp"]);
            let mut candidates = Vec::new();
            let mut inspected = 0;
            let mut overflow = false;
            collect_gemini_candidates(
                &root,
                &root,
                0,
                &mut inspected,
                &mut candidates,
                &mut overflow,
            )?;
            if overflow {
                notices.push(DiscoveryNotice::SourceScanLimitReached);
                continue;
            }
            candidates.sort();
            candidates.dedup();
            for (index, path) in candidates.into_iter().enumerate() {
                if sources.len() == MAX_SOURCES {
                    notices.push(DiscoveryNotice::SourceLimitReached);
                    return Ok(sources);
                }
                let ordinal = index + 1;
                let metadata = fs::symlink_metadata(&path)?;
                if !metadata.file_type().is_file() {
                    notices.push(DiscoveryNotice::SourceNotRegular {
                        client: "gemini",
                        ordinal,
                    });
                    continue;
                }
                if metadata.len() > MAX_SOURCE_FILE_BYTES {
                    notices.push(DiscoveryNotice::SourceInvalid {
                        client: "gemini",
                        ordinal,
                    });
                    continue;
                }
                match validate_source(&path, SourceFormat::GeminiTranscript, metadata.len()) {
                    Ok(session) => {
                        let source_id = source_id_for(SourceFormat::GeminiTranscript, &path);
                        if !admitted_ids.insert(source_id) {
                            notices.push(DiscoveryNotice::SourceIdentityCollision);
                            continue;
                        }
                        sources.push(DiscoveredSource {
                            source_id,
                            format: SourceFormat::GeminiTranscript,
                            session,
                            path,
                        });
                    }
                    Err(()) => notices.push(DiscoveryNotice::SourceInvalid {
                        client: "gemini",
                        ordinal,
                    }),
                }
            }
            continue;
        }
        if client == "cursor" {
            let root = join_many(
                inputs.platform,
                &inputs.home,
                &[".config", "tokscale", "cursor-cache"],
            );
            let mut candidates = Vec::new();
            let entries = match fs::read_dir(&root) {
                Ok(entries) => entries,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            for entry in entries {
                let entry = entry?;
                if entry
                    .file_name()
                    .to_str()
                    .is_some_and(is_cursor_cache_filename)
                {
                    candidates.push(entry.path());
                }
            }
            candidates.sort();
            candidates.dedup();
            for (index, path) in candidates.into_iter().enumerate() {
                if sources.len() == MAX_SOURCES {
                    notices.push(DiscoveryNotice::SourceLimitReached);
                    return Ok(sources);
                }
                let ordinal = index + 1;
                let metadata = fs::symlink_metadata(&path)?;
                if !metadata.file_type().is_file() {
                    notices.push(DiscoveryNotice::SourceNotRegular {
                        client: "cursor",
                        ordinal,
                    });
                    continue;
                }
                if metadata.len() > MAX_SOURCE_FILE_BYTES {
                    notices.push(DiscoveryNotice::SourceInvalid {
                        client: "cursor",
                        ordinal,
                    });
                    continue;
                }
                match validate_source(&path, SourceFormat::CursorCacheJson, metadata.len()) {
                    Ok(_) => {
                        let source_id = source_id_for(SourceFormat::CursorCacheJson, &path);
                        if !admitted_ids.insert(source_id) {
                            notices.push(DiscoveryNotice::SourceIdentityCollision);
                            continue;
                        }
                        sources.push(DiscoveredSource {
                            source_id,
                            format: SourceFormat::CursorCacheJson,
                            session: "cursor".to_owned(),
                            path,
                        });
                    }
                    Err(()) => notices.push(DiscoveryNotice::SourceInvalid {
                        client: "cursor",
                        ordinal,
                    }),
                }
            }
            continue;
        }
        if matches!(client.as_str(), "roocode" | "cline" | "kilo") {
            // Three forks of the same VS Code task log, one directory per task
            // under the extension's `globalStorage`, with `ui_messages.json`
            // the only file in it that carries usage.
            let (extension, format, notice_client) = match client.as_str() {
                "cline" => (
                    "saoudrizwan.claude-dev",
                    SourceFormat::ClineTaskJson,
                    "cline",
                ),
                "kilo" => ("kilocode.kilo-code", SourceFormat::KiloTaskJson, "kilo"),
                _ => (
                    "rooveterinaryinc.roo-cline",
                    SourceFormat::RooCodeJson,
                    "roocode",
                ),
            };
            let mut candidates = Vec::new();
            let mut inspected = 0;
            let mut overflow = false;
            for user_root in vscode_user_roots(inputs) {
                let root = user_root
                    .join("globalStorage")
                    .join(extension)
                    .join("tasks");
                collect_task_log_candidates(
                    &root,
                    0,
                    &mut inspected,
                    &mut candidates,
                    &mut overflow,
                )?;
            }
            candidates.sort();
            candidates.dedup();
            for (ordinal, path) in candidates.into_iter().enumerate() {
                let ordinal = ordinal + 1;
                if sources.len() >= MAX_SOURCES {
                    notices.push(DiscoveryNotice::SourceLimitReached);
                    break;
                }
                let session = match validate_source(
                    &path,
                    format,
                    fs::symlink_metadata(&path)
                        .map(|meta| meta.len())
                        .unwrap_or(0),
                ) {
                    Ok(session) => session,
                    Err(()) => {
                        notices.push(DiscoveryNotice::SourceInvalid {
                            client: notice_client,
                            ordinal,
                        });
                        continue;
                    }
                };
                let source_id = source_id_for(format, &path);
                if !admitted_ids.insert(source_id) {
                    notices.push(DiscoveryNotice::SourceIdentityCollision);
                    continue;
                }
                sources.push(DiscoveredSource {
                    source_id,
                    format,
                    session,
                    path,
                });
            }
            if overflow {
                notices.push(DiscoveryNotice::SourceScanLimitReached);
            }
        }
        if client == "copilot" {
            // Copilot Chat sessions are operation logs, one per session: a
            // workspace session under `workspaceStorage/<hash>/chatSessions`
            // and an empty-window session under
            // `globalStorage/emptyWindowChatSessions`.
            let mut candidates = Vec::new();
            let mut inspected = 0;
            let mut overflow = false;
            for user_root in vscode_user_roots(inputs) {
                collect_copilot_chat_candidates(
                    &user_root
                        .join("globalStorage")
                        .join("emptyWindowChatSessions"),
                    &mut inspected,
                    &mut candidates,
                    &mut overflow,
                )?;
                collect_copilot_workspace_candidates(
                    &user_root.join("workspaceStorage"),
                    0,
                    &mut inspected,
                    &mut candidates,
                    &mut overflow,
                )?;
            }
            candidates.sort();
            candidates.dedup();
            for (ordinal, path) in candidates.into_iter().enumerate() {
                let ordinal = ordinal + 1;
                if sources.len() >= MAX_SOURCES {
                    notices.push(DiscoveryNotice::SourceLimitReached);
                    break;
                }
                let session = match validate_source(
                    &path,
                    SourceFormat::CopilotChatSessionJsonl,
                    fs::symlink_metadata(&path)
                        .map(|meta| meta.len())
                        .unwrap_or(0),
                ) {
                    Ok(session) => session,
                    Err(()) => {
                        notices.push(DiscoveryNotice::SourceInvalid {
                            client: "copilot",
                            ordinal,
                        });
                        continue;
                    }
                };
                let source_id = source_id_for(SourceFormat::CopilotChatSessionJsonl, &path);
                if !admitted_ids.insert(source_id) {
                    notices.push(DiscoveryNotice::SourceIdentityCollision);
                    continue;
                }
                sources.push(DiscoveredSource {
                    source_id,
                    format: SourceFormat::CopilotChatSessionJsonl,
                    session,
                    path,
                });
            }
            if overflow {
                notices.push(DiscoveryNotice::SourceScanLimitReached);
            }
        }
        if client == "cline" {
            let root = cline_cli_sessions_root(inputs)?;
            let mut candidates = Vec::new();
            let mut inspected = 0;
            let mut overflow = false;
            collect_cline_candidates(&root, 0, &mut inspected, &mut candidates, &mut overflow)?;
            if overflow {
                notices.push(DiscoveryNotice::SourceScanLimitReached);
                continue;
            }
            candidates.sort();
            candidates.dedup();
            for (index, path) in candidates.into_iter().enumerate() {
                if sources.len() == MAX_SOURCES {
                    notices.push(DiscoveryNotice::SourceLimitReached);
                    return Ok(sources);
                }
                let ordinal = index + 1;
                let metadata = fs::symlink_metadata(&path)?;
                if !metadata.file_type().is_file() {
                    notices.push(DiscoveryNotice::SourceNotRegular {
                        client: "cline",
                        ordinal,
                    });
                    continue;
                }
                if metadata.len() > MAX_SOURCE_FILE_BYTES {
                    notices.push(DiscoveryNotice::SourceInvalid {
                        client: "cline",
                        ordinal,
                    });
                    continue;
                }
                match validate_source(&path, SourceFormat::ClineJson, metadata.len()) {
                    Ok(session) => {
                        let source_id = source_id_for(SourceFormat::ClineJson, &path);
                        if !admitted_ids.insert(source_id) {
                            notices.push(DiscoveryNotice::SourceIdentityCollision);
                            continue;
                        }
                        sources.push(DiscoveredSource {
                            source_id,
                            format: SourceFormat::ClineJson,
                            session,
                            path,
                        });
                    }
                    Err(()) => notices.push(DiscoveryNotice::SourceInvalid {
                        client: "cline",
                        ordinal,
                    }),
                }
            }
            continue;
        }
        if client == "amp" {
            let roots = amp_thread_roots(inputs)?;
            let mut candidates = Vec::new();
            let mut inspected = 0;
            let mut overflow = false;
            for root in roots {
                collect_amp_candidates(&root, 0, &mut inspected, &mut candidates, &mut overflow)?;
                if overflow {
                    break;
                }
            }
            if overflow {
                notices.push(DiscoveryNotice::SourceScanLimitReached);
                continue;
            }
            candidates.sort();
            candidates.dedup();
            for (index, path) in candidates.into_iter().enumerate() {
                if sources.len() == MAX_SOURCES {
                    notices.push(DiscoveryNotice::SourceLimitReached);
                    return Ok(sources);
                }
                let ordinal = index + 1;
                let metadata = fs::symlink_metadata(&path)?;
                if !metadata.file_type().is_file() {
                    notices.push(DiscoveryNotice::SourceNotRegular {
                        client: "amp",
                        ordinal,
                    });
                    continue;
                }
                if metadata.len() > MAX_SOURCE_FILE_BYTES {
                    notices.push(DiscoveryNotice::SourceInvalid {
                        client: "amp",
                        ordinal,
                    });
                    continue;
                }
                match validate_source(&path, SourceFormat::AmpJson, metadata.len()) {
                    Ok(session) => {
                        let source_id = source_id_for(SourceFormat::AmpJson, &path);
                        if !admitted_ids.insert(source_id) {
                            notices.push(DiscoveryNotice::SourceIdentityCollision);
                            continue;
                        }
                        sources.push(DiscoveredSource {
                            source_id,
                            format: SourceFormat::AmpJson,
                            session,
                            path,
                        });
                    }
                    Err(()) => notices.push(DiscoveryNotice::SourceInvalid {
                        client: "amp",
                        ordinal,
                    }),
                }
            }
            continue;
        }
        if client == "kilo" {
            let data_base = match inputs.xdg_data_home.as_deref() {
                Some(root) if !root.as_os_str().is_empty() => {
                    checked_root(inputs.platform, root, false)?
                }
                _ => join_many(inputs.platform, &inputs.home, &[".local", "share"]),
            };
            let path = join_many(inputs.platform, &data_base, &["kilo", "kilo.db"]);
            match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.file_type().is_file() => {
                    if metadata.len() <= MAX_SOURCE_FILE_BYTES
                        && KiloAdapter::new(&path).read().is_ok()
                        && sources.len() < MAX_SOURCES
                    {
                        let source_id = source_id_for(SourceFormat::KiloSqlite, &path);
                        if admitted_ids.insert(source_id) {
                            sources.push(DiscoveredSource {
                                source_id,
                                format: SourceFormat::KiloSqlite,
                                session: "kilo".to_owned(),
                                path,
                            });
                        } else {
                            notices.push(DiscoveryNotice::SourceIdentityCollision);
                        }
                    } else if sources.len() >= MAX_SOURCES {
                        notices.push(DiscoveryNotice::SourceLimitReached);
                    } else {
                        notices.push(DiscoveryNotice::SourceInvalid {
                            client: "kilo",
                            ordinal: 1,
                        });
                    }
                }
                Ok(_) => notices.push(DiscoveryNotice::SourceNotRegular {
                    client: "kilo",
                    ordinal: 1,
                }),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            continue;
        }
        if client == "bb" {
            // BB keeps one SQLite database per installation: `~/.bb/bb.db`,
            // `$BB_DATA_DIR/bb.db`, and development instances at
            // `~/.bb-dev/<instance>/bb.db`.
            let mut roots = Vec::new();
            if let Some(root) = inputs.env_paths.get("BB_DATA_DIR") {
                let root = checked_root(inputs.platform, root, false)?;
                roots.push(root);
            }
            roots.push(join_many(inputs.platform, &inputs.home, &[".bb"]));
            roots.push(join_many(inputs.platform, &inputs.home, &[".bb-dev"]));

            let mut candidates = Vec::new();
            let mut inspected = 0;
            let mut overflow = false;
            for root in &roots {
                collect_db_candidates(root, 0, &mut inspected, &mut candidates, &mut overflow)?;
            }
            candidates.retain(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name == "bb.db")
            });
            candidates.sort();
            for (ordinal, path) in candidates.into_iter().enumerate() {
                let ordinal = ordinal + 1;
                let metadata = match fs::symlink_metadata(&path) {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error.into()),
                };
                if !metadata.file_type().is_file() {
                    notices.push(DiscoveryNotice::SourceNotRegular {
                        client: "bb",
                        ordinal,
                    });
                    continue;
                }
                if sources.len() >= MAX_SOURCES {
                    notices.push(DiscoveryNotice::SourceLimitReached);
                    break;
                }
                if metadata.len() > MAX_SOURCE_FILE_BYTES || BbAdapter::new(&path).read().is_err() {
                    notices.push(DiscoveryNotice::SourceInvalid {
                        client: "bb",
                        ordinal,
                    });
                    continue;
                }
                let source_id = source_id_for(SourceFormat::BbSqlite, &path);
                if !admitted_ids.insert(source_id) {
                    notices.push(DiscoveryNotice::SourceIdentityCollision);
                    continue;
                }
                // Threads are the per-session identity inside the reader; the
                // source itself is the database file.
                let session = path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .filter(|stem| !stem.is_empty())
                    .unwrap_or("bb")
                    .to_owned();
                sources.push(DiscoveredSource {
                    source_id,
                    format: SourceFormat::BbSqlite,
                    session,
                    path,
                });
            }
            if overflow {
                notices.push(DiscoveryNotice::SourceScanLimitReached);
            }
            continue;
        }
        if client == "mimo" {
            // MiMo Code and Xiaomi MiMo AI share one engine, so both resolve the
            // same XDG data directory. Channel-suffixed databases
            // (`mimocode-<channel>.db`) sit beside `mimocode.db` and each is its
            // own source, which is why the reader namespaces identities by file.
            let data_base = match inputs.xdg_data_home.as_deref() {
                Some(root) if !root.as_os_str().is_empty() => {
                    checked_root(inputs.platform, root, false)?
                }
                _ => join_many(inputs.platform, &inputs.home, &[".local", "share"]),
            };
            let root = join_many(inputs.platform, &data_base, &["mimocode"]);
            let mut candidates = Vec::new();
            let mut inspected = 0;
            let mut overflow = false;
            collect_db_candidates(&root, 0, &mut inspected, &mut candidates, &mut overflow)?;
            candidates.sort();
            for (ordinal, path) in candidates.into_iter().enumerate() {
                let ordinal = ordinal + 1;
                let metadata = match fs::symlink_metadata(&path) {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error.into()),
                };
                if !metadata.file_type().is_file() {
                    notices.push(DiscoveryNotice::SourceNotRegular {
                        client: "mimo",
                        ordinal,
                    });
                    continue;
                }
                if sources.len() >= MAX_SOURCES {
                    notices.push(DiscoveryNotice::SourceLimitReached);
                    break;
                }
                if metadata.len() > MAX_SOURCE_FILE_BYTES || MimoAdapter::new(&path).read().is_err()
                {
                    notices.push(DiscoveryNotice::SourceInvalid {
                        client: "mimo",
                        ordinal,
                    });
                    continue;
                }
                let source_id = source_id_for(SourceFormat::MimoSqlite, &path);
                if !admitted_ids.insert(source_id) {
                    notices.push(DiscoveryNotice::SourceIdentityCollision);
                    continue;
                }
                let session = path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .filter(|stem| !stem.is_empty())
                    .unwrap_or("mimo")
                    .to_owned();
                sources.push(DiscoveredSource {
                    source_id,
                    format: SourceFormat::MimoSqlite,
                    session,
                    path,
                });
            }
            if overflow {
                notices.push(DiscoveryNotice::SourceScanLimitReached);
            }
            continue;
        }
        if client == "catpaw" {
            // Both editions sit directly under the platform app-data root. Only
            // immediate `catpaw-memory-<scope>.db` files with a non-empty
            // `[A-Za-z0-9_-]` scope are admitted, and the exact scope `anon` is
            // excluded. Non-anonymous historical account databases are kept:
            // that is this machine's retained history, not the current login.
            let support = match inputs.platform {
                HostPlatform::MacOs => join_many(
                    inputs.platform,
                    &inputs.home,
                    &["Library", "Application Support"],
                ),
                HostPlatform::Windows => {
                    join_many(inputs.platform, &inputs.home, &["AppData", "Roaming"])
                }
                // No Linux product root is verified, so nothing is read there.
                HostPlatform::Linux => {
                    continue;
                }
            };
            for edition in ["catpaw-moon", "catpaw-overseas"] {
                let root = join_many(inputs.platform, &support, &[edition]);
                let mut candidates = Vec::new();
                let mut inspected = 0;
                let mut overflow = false;
                collect_db_candidates(&root, 0, &mut inspected, &mut candidates, &mut overflow)?;
                candidates.retain(|path| {
                    path.parent() == Some(root.as_path())
                        && path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(catpaw_database_scope)
                });
                candidates.sort();
                for (ordinal, path) in candidates.into_iter().enumerate() {
                    let ordinal = ordinal + 1;
                    if sources.len() >= MAX_SOURCES {
                        notices.push(DiscoveryNotice::SourceLimitReached);
                        break;
                    }
                    let metadata = match fs::symlink_metadata(&path) {
                        Ok(metadata) => metadata,
                        Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                        Err(error) => return Err(error.into()),
                    };
                    if !metadata.file_type().is_file() {
                        notices.push(DiscoveryNotice::SourceNotRegular {
                            client: "catpaw",
                            ordinal,
                        });
                        continue;
                    }
                    if metadata.len() > MAX_SOURCE_FILE_BYTES
                        || CatpawAdapter::new(&path).read().is_err()
                    {
                        notices.push(DiscoveryNotice::SourceInvalid {
                            client: "catpaw",
                            ordinal,
                        });
                        continue;
                    }
                    let source_id = source_id_for(SourceFormat::CatpawSqlite, &path);
                    if !admitted_ids.insert(source_id) {
                        notices.push(DiscoveryNotice::SourceIdentityCollision);
                        continue;
                    }
                    let session = path
                        .file_stem()
                        .and_then(|stem| stem.to_str())
                        .filter(|stem| !stem.is_empty())
                        .unwrap_or("catpaw")
                        .to_owned();
                    sources.push(DiscoveredSource {
                        source_id,
                        format: SourceFormat::CatpawSqlite,
                        session,
                        path,
                    });
                }
                if overflow {
                    notices.push(DiscoveryNotice::SourceScanLimitReached);
                }
            }
            continue;
        }
        if client == "proma" {
            // Proma applies the same three source trees under both `.proma` and
            // `.proma-dev`: the `agent-sessions` display journals, and the two
            // legacy SDK transcript roots. Each file is its own source; the
            // pinned reader's cross-file reconciliation over the sibling
            // `agent-sessions.json` index is not performed.
            let mut roots = Vec::new();
            for home_name in [".proma", ".proma-dev"] {
                let root = join_many(inputs.platform, &inputs.home, &[home_name]);
                for subtree in [
                    "agent-sessions",
                    "sdk-config/sessions",
                    "sdk-config/projects",
                ] {
                    roots.push(join_many(
                        inputs.platform,
                        &root,
                        &subtree.split('/').collect::<Vec<_>>(),
                    ));
                }
            }
            let mut candidates = Vec::new();
            let mut inspected = 0;
            let mut overflow = false;
            for root in &roots {
                collect_source_candidates(root, 0, &mut inspected, &mut candidates, &mut overflow)?;
                if overflow {
                    break;
                }
            }
            candidates.retain(|path| {
                path.extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
            });
            candidates.sort();
            candidates.dedup();
            for (ordinal, path) in candidates.into_iter().enumerate() {
                let ordinal = ordinal + 1;
                if sources.len() >= MAX_SOURCES {
                    notices.push(DiscoveryNotice::SourceLimitReached);
                    break;
                }
                let bytes = fs::symlink_metadata(&path)
                    .map(|meta| meta.len())
                    .unwrap_or(0);
                let session = match validate_source(&path, SourceFormat::PromaJsonl, bytes) {
                    Ok(session) => session,
                    Err(()) => {
                        notices.push(DiscoveryNotice::SourceInvalid {
                            client: "proma",
                            ordinal,
                        });
                        continue;
                    }
                };
                let source_id = source_id_for(SourceFormat::PromaJsonl, &path);
                if !admitted_ids.insert(source_id) {
                    notices.push(DiscoveryNotice::SourceIdentityCollision);
                    continue;
                }
                sources.push(DiscoveredSource {
                    source_id,
                    format: SourceFormat::PromaJsonl,
                    session,
                    path,
                });
            }
            if overflow {
                notices.push(DiscoveryNotice::SourceScanLimitReached);
            }
            continue;
        }
        if client == "zcode" {
            // ZCode keeps two stores: transcript JSONL under
            // `~/.zcode/projects/<slug>/<session>.jsonl`, and the authoritative
            // per-request `model_usage` table at `~/.zcode/cli/db/db.sqlite`.
            // Both are scanned; each file is its own source.
            let projects = join_many(inputs.platform, &inputs.home, &[".zcode", "projects"]);
            let mut jsonl = Vec::new();
            let mut inspected = 0;
            let mut overflow = false;
            collect_source_candidates(&projects, 0, &mut inspected, &mut jsonl, &mut overflow)?;
            jsonl.retain(|path| {
                path.extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
            });
            jsonl.sort();
            for (ordinal, path) in jsonl.into_iter().enumerate() {
                let ordinal = ordinal + 1;
                if sources.len() >= MAX_SOURCES {
                    notices.push(DiscoveryNotice::SourceLimitReached);
                    break;
                }
                let session = match validate_source(
                    &path,
                    SourceFormat::ZcodeJsonl,
                    fs::symlink_metadata(&path)
                        .map(|meta| meta.len())
                        .unwrap_or(0),
                ) {
                    Ok(session) => session,
                    Err(()) => {
                        notices.push(DiscoveryNotice::SourceInvalid {
                            client: "zcode",
                            ordinal,
                        });
                        continue;
                    }
                };
                let source_id = source_id_for(SourceFormat::ZcodeJsonl, &path);
                if !admitted_ids.insert(source_id) {
                    notices.push(DiscoveryNotice::SourceIdentityCollision);
                    continue;
                }
                sources.push(DiscoveredSource {
                    source_id,
                    format: SourceFormat::ZcodeJsonl,
                    session,
                    path,
                });
            }
            let database = join_many(
                inputs.platform,
                &inputs.home,
                &[".zcode", "cli", "db", "db.sqlite"],
            );
            let metadata = match fs::symlink_metadata(&database) {
                Ok(metadata) => Some(metadata),
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            };
            if let Some(metadata) = metadata {
                if !metadata.file_type().is_file() {
                    notices.push(DiscoveryNotice::SourceNotRegular {
                        client: "zcode",
                        ordinal: 1,
                    });
                } else if sources.len() >= MAX_SOURCES {
                    notices.push(DiscoveryNotice::SourceLimitReached);
                } else if metadata.len() > MAX_SOURCE_FILE_BYTES
                    || ZcodeAdapter::new(&database).read().is_err()
                {
                    notices.push(DiscoveryNotice::SourceInvalid {
                        client: "zcode",
                        ordinal: 1,
                    });
                } else {
                    let source_id = source_id_for(SourceFormat::ZcodeSqlite, &database);
                    if admitted_ids.insert(source_id) {
                        sources.push(DiscoveredSource {
                            source_id,
                            format: SourceFormat::ZcodeSqlite,
                            session: "db".to_owned(),
                            path: database,
                        });
                    } else {
                        notices.push(DiscoveryNotice::SourceIdentityCollision);
                    }
                }
            }
            continue;
        }
        if client == "qodercn" {
            // Qoder CN keeps two stores: legacy SQLite at
            // `<data>/SharedClientCache/cache/db/local.db`, and the
            // Claude-compatible transcripts that 2026-09+ builds write under
            // `~/.qoder-cn/projects`. Both are scanned; each file is a source.
            let projects = join_many(inputs.platform, &inputs.home, &[".qoder-cn", "projects"]);
            let mut jsonl = Vec::new();
            let mut inspected = 0;
            let mut overflow = false;
            collect_source_candidates(&projects, 0, &mut inspected, &mut jsonl, &mut overflow)?;
            jsonl.retain(|path| {
                path.extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
            });
            jsonl.sort();
            for (ordinal, path) in jsonl.into_iter().enumerate() {
                let ordinal = ordinal + 1;
                if sources.len() >= MAX_SOURCES {
                    notices.push(DiscoveryNotice::SourceLimitReached);
                    break;
                }
                let session = match validate_source(
                    &path,
                    SourceFormat::QodercnJsonl,
                    fs::symlink_metadata(&path)
                        .map(|meta| meta.len())
                        .unwrap_or(0),
                ) {
                    Ok(session) => session,
                    Err(()) => {
                        notices.push(DiscoveryNotice::SourceInvalid {
                            client: "qodercn",
                            ordinal,
                        });
                        continue;
                    }
                };
                let source_id = source_id_for(SourceFormat::QodercnJsonl, &path);
                if !admitted_ids.insert(source_id) {
                    notices.push(DiscoveryNotice::SourceIdentityCollision);
                    continue;
                }
                sources.push(DiscoveredSource {
                    source_id,
                    format: SourceFormat::QodercnJsonl,
                    session,
                    path,
                });
            }
            let database = {
                // The legacy store lives under the platform app-support root,
                // inside a `QoderCN` directory: the reference resolves macOS to
                // `Application Support`, Windows to the roaming app-data
                // directory, and every other host to `XDG_CONFIG_HOME` (falling
                // back to `<home>/.config`).
                let support = match inputs.platform {
                    HostPlatform::MacOs => join_many(
                        inputs.platform,
                        &inputs.home,
                        &["Library", "Application Support"],
                    ),
                    HostPlatform::Windows => {
                        join_many(inputs.platform, &inputs.home, &["AppData", "Roaming"])
                    }
                    _ => match inputs.xdg_config_home.as_deref() {
                        Some(root) if !root.as_os_str().is_empty() => {
                            checked_root(inputs.platform, root, false)?
                        }
                        _ => join_many(inputs.platform, &inputs.home, &[".config"]),
                    },
                };
                join_many(
                    inputs.platform,
                    &support,
                    &["QoderCN", "SharedClientCache", "cache", "db", "local.db"],
                )
            };
            let metadata = match fs::symlink_metadata(&database) {
                Ok(metadata) => Some(metadata),
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            };
            if let Some(metadata) = metadata {
                if !metadata.file_type().is_file() {
                    notices.push(DiscoveryNotice::SourceNotRegular {
                        client: "qodercn",
                        ordinal: 1,
                    });
                } else if sources.len() >= MAX_SOURCES {
                    notices.push(DiscoveryNotice::SourceLimitReached);
                } else if metadata.len() > MAX_SOURCE_FILE_BYTES
                    || QodercnAdapter::new(&database).read().is_err()
                {
                    notices.push(DiscoveryNotice::SourceInvalid {
                        client: "qodercn",
                        ordinal: 1,
                    });
                } else {
                    let source_id = source_id_for(SourceFormat::QodercnSqlite, &database);
                    if admitted_ids.insert(source_id) {
                        sources.push(DiscoveredSource {
                            source_id,
                            format: SourceFormat::QodercnSqlite,
                            session: "local".to_owned(),
                            path: database,
                        });
                    } else {
                        notices.push(DiscoveryNotice::SourceIdentityCollision);
                    }
                }
            }
            continue;
        }
        if client == "cherrystudio" {
            // Cherry Studio keeps its built-in chat ledger at
            // `<app-data>/CherryStudio/Data/cherrystudio.sqlite`. The Agent
            // transcripts under `.claude/projects` are deliberately not scanned
            // here: they are the same billed calls the `claude` client already
            // reports, so reading them would count Agent mode twice.
            let support = match inputs.platform {
                HostPlatform::MacOs => join_many(
                    inputs.platform,
                    &inputs.home,
                    &["Library", "Application Support"],
                ),
                HostPlatform::Windows => {
                    join_many(inputs.platform, &inputs.home, &["AppData", "Roaming"])
                }
                _ => match inputs.xdg_config_home.as_deref() {
                    Some(root) if !root.as_os_str().is_empty() => {
                        checked_root(inputs.platform, root, false)?
                    }
                    _ => join_many(inputs.platform, &inputs.home, &[".config"]),
                },
            };
            let database = join_many(
                inputs.platform,
                &support,
                &["CherryStudio", "Data", "cherrystudio.sqlite"],
            );
            let metadata = match fs::symlink_metadata(&database) {
                Ok(metadata) => Some(metadata),
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            };
            if let Some(metadata) = metadata {
                if !metadata.file_type().is_file() {
                    notices.push(DiscoveryNotice::SourceNotRegular {
                        client: "cherrystudio",
                        ordinal: 1,
                    });
                } else if sources.len() >= MAX_SOURCES {
                    notices.push(DiscoveryNotice::SourceLimitReached);
                } else if metadata.len() > MAX_SOURCE_FILE_BYTES
                    || CherrystudioAdapter::new(&database).read().is_err()
                {
                    notices.push(DiscoveryNotice::SourceInvalid {
                        client: "cherrystudio",
                        ordinal: 1,
                    });
                } else {
                    let source_id = source_id_for(SourceFormat::CherrystudioSqlite, &database);
                    if admitted_ids.insert(source_id) {
                        sources.push(DiscoveredSource {
                            source_id,
                            format: SourceFormat::CherrystudioSqlite,
                            session: "cherrystudio".to_owned(),
                            path: database,
                        });
                    } else {
                        notices.push(DiscoveryNotice::SourceIdentityCollision);
                    }
                }
            }
            continue;
        }
        if client == "lmstudio" {
            // The local server writes pretty-printed responses beneath
            // `~/.lmstudio/server-logs/`. There is no ledger and nothing
            // rotates these files, so each admitted file is one source.
            let root = join_many(inputs.platform, &inputs.home, &[".lmstudio", "server-logs"]);
            let mut candidates = Vec::new();
            let mut inspected = 0;
            let mut overflow = false;
            collect_log_candidates(&root, 0, &mut inspected, &mut candidates, &mut overflow)?;
            candidates.sort();
            for (ordinal, path) in candidates.into_iter().enumerate() {
                let ordinal = ordinal + 1;
                if sources.len() >= MAX_SOURCES {
                    notices.push(DiscoveryNotice::SourceLimitReached);
                    break;
                }
                let session = match validate_source(
                    &path,
                    SourceFormat::LmstudioLog,
                    fs::symlink_metadata(&path)
                        .map(|meta| meta.len())
                        .unwrap_or(0),
                ) {
                    Ok(session) => session,
                    Err(()) => {
                        notices.push(DiscoveryNotice::SourceInvalid {
                            client: "lmstudio",
                            ordinal,
                        });
                        continue;
                    }
                };
                let source_id = source_id_for(SourceFormat::LmstudioLog, &path);
                if !admitted_ids.insert(source_id) {
                    notices.push(DiscoveryNotice::SourceIdentityCollision);
                    continue;
                }
                sources.push(DiscoveredSource {
                    source_id,
                    format: SourceFormat::LmstudioLog,
                    session,
                    path,
                });
            }
            if overflow {
                notices.push(DiscoveryNotice::SourceScanLimitReached);
            }
            continue;
        }
        if client == "unsloth" {
            // Unsloth Studio keeps durable chat and API usage in `studio.db`
            // beneath `$UNSLOTH_STUDIO_HOME` (normally `~/.unsloth/studio`).
            // Older builds may lack either lane's table, which the reader
            // probes silently.
            let database = join_many(
                inputs.platform,
                &inputs.home,
                &[".unsloth", "studio", "studio.db"],
            );
            let metadata = match fs::symlink_metadata(&database) {
                Ok(metadata) => Some(metadata),
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            };
            if let Some(metadata) = metadata {
                if !metadata.file_type().is_file() {
                    notices.push(DiscoveryNotice::SourceNotRegular {
                        client: "unsloth",
                        ordinal: 1,
                    });
                } else if sources.len() >= MAX_SOURCES {
                    notices.push(DiscoveryNotice::SourceLimitReached);
                } else if metadata.len() > MAX_SOURCE_FILE_BYTES
                    || UnslothAdapter::new(&database).read().is_err()
                {
                    notices.push(DiscoveryNotice::SourceInvalid {
                        client: "unsloth",
                        ordinal: 1,
                    });
                } else {
                    let source_id = source_id_for(SourceFormat::UnslothSqlite, &database);
                    if admitted_ids.insert(source_id) {
                        sources.push(DiscoveredSource {
                            source_id,
                            format: SourceFormat::UnslothSqlite,
                            session: "studio".to_owned(),
                            path: database,
                        });
                    } else {
                        notices.push(DiscoveryNotice::SourceIdentityCollision);
                    }
                }
            }
            continue;
        }
        if client == "devin" {
            // Devin CLI keeps sessions in `~/.local/share/devin/cli/sessions.db`.
            // Desktop's `acp-events` streams are not scanned: their file names
            // are independent UUIDs, so they can only be attributed through a
            // title map built from this database, which a per-source reader
            // cannot do.
            let xdg = inputs
                .xdg_data_home
                .clone()
                .filter(|root| !root.as_os_str().is_empty())
                .unwrap_or_else(|| join_many(inputs.platform, &inputs.home, &[".local", "share"]));
            let database = join_many(inputs.platform, &xdg, &["devin", "cli", "sessions.db"]);
            let metadata = match fs::symlink_metadata(&database) {
                Ok(metadata) => Some(metadata),
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            };
            if let Some(metadata) = metadata {
                if !metadata.file_type().is_file() {
                    notices.push(DiscoveryNotice::SourceNotRegular {
                        client: "devin",
                        ordinal: 1,
                    });
                } else if sources.len() >= MAX_SOURCES {
                    notices.push(DiscoveryNotice::SourceLimitReached);
                } else if metadata.len() > MAX_SOURCE_FILE_BYTES
                    || DevinAdapter::new(&database).read().is_err()
                {
                    notices.push(DiscoveryNotice::SourceInvalid {
                        client: "devin",
                        ordinal: 1,
                    });
                } else {
                    let source_id = source_id_for(SourceFormat::DevinSqlite, &database);
                    if admitted_ids.insert(source_id) {
                        sources.push(DiscoveredSource {
                            source_id,
                            format: SourceFormat::DevinSqlite,
                            session: "sessions".to_owned(),
                            path: database,
                        });
                    } else {
                        notices.push(DiscoveryNotice::SourceIdentityCollision);
                    }
                }
            }
            continue;
        }
        if client == "fx" {
            // fx aggregates one snapshot per session at
            // `~/.fx/sessions/<sessionId>/usage-v2.json`. The sibling
            // `session.json` and the shared `index.json` are not read, because a
            // per-source reader cannot open them.
            let root = join_many(inputs.platform, &inputs.home, &[".fx", "sessions"]);
            let mut candidates = Vec::new();
            let mut inspected = 0;
            let mut overflow = false;
            collect_source_candidates(&root, 0, &mut inspected, &mut candidates, &mut overflow)?;
            candidates.retain(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name == "usage-v2.json")
            });
            candidates.sort();
            for (ordinal, path) in candidates.into_iter().enumerate() {
                let ordinal = ordinal + 1;
                if sources.len() >= MAX_SOURCES {
                    notices.push(DiscoveryNotice::SourceLimitReached);
                    break;
                }
                let session = match validate_source(
                    &path,
                    SourceFormat::FxUsageJson,
                    fs::symlink_metadata(&path)
                        .map(|meta| meta.len())
                        .unwrap_or(0),
                ) {
                    Ok(session) => session,
                    Err(()) => {
                        notices.push(DiscoveryNotice::SourceInvalid {
                            client: "fx",
                            ordinal,
                        });
                        continue;
                    }
                };
                let source_id = source_id_for(SourceFormat::FxUsageJson, &path);
                if !admitted_ids.insert(source_id) {
                    notices.push(DiscoveryNotice::SourceIdentityCollision);
                    continue;
                }
                sources.push(DiscoveredSource {
                    source_id,
                    format: SourceFormat::FxUsageJson,
                    session,
                    path,
                });
            }
            if overflow {
                notices.push(DiscoveryNotice::SourceScanLimitReached);
            }
            continue;
        }
        if client == "mistral" {
            // Mistral Vibe stores session logs under `~/.vibe/logs/session/`
            // (or `$VIBE_HOME/logs/session/`): a `session_*/` directory with a
            // `meta.json` for Vibe 2.x, or a flat `session_*.json` for 1.x.
            let root = match inputs.env_paths.get("VIBE_HOME") {
                Some(home) if !home.as_os_str().is_empty() => {
                    join_many(inputs.platform, home, &["logs", "session"])
                }
                _ => join_many(inputs.platform, &inputs.home, &[".vibe", "logs", "session"]),
            };
            let mut candidates = Vec::new();
            let mut inspected = 0;
            let mut overflow = false;
            collect_source_candidates(&root, 0, &mut inspected, &mut candidates, &mut overflow)?;
            let mut meta_files = Vec::new();
            let mut flat_files = Vec::new();
            for path in candidates {
                let name = path.file_name().and_then(|name| name.to_str());
                if name == Some("meta.json") {
                    meta_files.push(path);
                } else if path
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
                    && name
                        .is_some_and(|name| name.starts_with("session_") && name.ends_with(".json"))
                {
                    flat_files.push(path);
                }
            }
            meta_files.sort();
            flat_files.sort();
            for (ordinal, path) in meta_files.into_iter().chain(flat_files).enumerate() {
                let ordinal = ordinal + 1;
                if sources.len() >= MAX_SOURCES {
                    notices.push(DiscoveryNotice::SourceLimitReached);
                    break;
                }
                let session = match validate_source(
                    &path,
                    SourceFormat::VibeJson,
                    fs::symlink_metadata(&path)
                        .map(|meta| meta.len())
                        .unwrap_or(0),
                ) {
                    Ok(session) => session,
                    Err(()) => {
                        notices.push(DiscoveryNotice::SourceInvalid {
                            client: "mistral",
                            ordinal,
                        });
                        continue;
                    }
                };
                let source_id = source_id_for(SourceFormat::VibeJson, &path);
                if !admitted_ids.insert(source_id) {
                    notices.push(DiscoveryNotice::SourceIdentityCollision);
                    continue;
                }
                sources.push(DiscoveredSource {
                    source_id,
                    format: SourceFormat::VibeJson,
                    session,
                    path,
                });
            }
            if overflow {
                notices.push(DiscoveryNotice::SourceScanLimitReached);
            }
            continue;
        }
        if client == "prime-agent" {
            // Prime Agent stores root sessions in `~/.prime/agent/sessions/*.jsonl`
            // and RLM child sessions below the sibling `session-artifacts` tree.
            // Both use the Pi append-only JSONL format and are each scanned as
            // their own source, which is why a `child_usage_attributed` record
            // is never turned into a message of its own.
            let root = join_many(
                inputs.platform,
                &inputs.home,
                &[".prime", "agent", "sessions"],
            );
            let artifacts = join_many(
                inputs.platform,
                &inputs.home,
                &[".prime", "agent", "session-artifacts"],
            );
            let mut candidates = Vec::new();
            let mut inspected = 0;
            let mut overflow = false;
            for directory in [&root, &artifacts] {
                collect_source_candidates(
                    directory,
                    0,
                    &mut inspected,
                    &mut candidates,
                    &mut overflow,
                )?;
            }
            candidates.retain(|path| {
                path.extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
            });
            candidates.sort();
            for (ordinal, path) in candidates.into_iter().enumerate() {
                let ordinal = ordinal + 1;
                if sources.len() >= MAX_SOURCES {
                    notices.push(DiscoveryNotice::SourceLimitReached);
                    break;
                }
                let session = match validate_source(
                    &path,
                    SourceFormat::PrimeAgentJsonl,
                    fs::symlink_metadata(&path)
                        .map(|meta| meta.len())
                        .unwrap_or(0),
                ) {
                    Ok(session) => session,
                    Err(()) => {
                        notices.push(DiscoveryNotice::SourceInvalid {
                            client: "prime-agent",
                            ordinal,
                        });
                        continue;
                    }
                };
                let source_id = source_id_for(SourceFormat::PrimeAgentJsonl, &path);
                if !admitted_ids.insert(source_id) {
                    notices.push(DiscoveryNotice::SourceIdentityCollision);
                    continue;
                }
                sources.push(DiscoveredSource {
                    source_id,
                    format: SourceFormat::PrimeAgentJsonl,
                    session,
                    path,
                });
            }
            if overflow {
                notices.push(DiscoveryNotice::SourceScanLimitReached);
            }
            continue;
        }
        if client == "gjc" {
            // gajae-code JSONL sessions at `~/.gjc/agent/sessions`, including depth-2 per-pass children.
            let root = join_many(
                inputs.platform,
                &inputs.home,
                &[".gjc", "agent", "sessions"],
            );
            let mut candidates = Vec::new();
            let mut inspected = 0;
            let mut overflow = false;
            collect_source_candidates(&root, 0, &mut inspected, &mut candidates, &mut overflow)?;
            candidates.sort();
            for (ordinal, path) in candidates.into_iter().enumerate() {
                let ordinal = ordinal + 1;
                if sources.len() >= MAX_SOURCES {
                    notices.push(DiscoveryNotice::SourceLimitReached);
                    break;
                }
                let session = match validate_source(
                    &path,
                    SourceFormat::GjcJsonl,
                    fs::symlink_metadata(&path)
                        .map(|meta| meta.len())
                        .unwrap_or(0),
                ) {
                    Ok(session) => session,
                    Err(()) => {
                        notices.push(DiscoveryNotice::SourceInvalid {
                            client: "gjc",
                            ordinal,
                        });
                        continue;
                    }
                };
                let source_id = source_id_for(SourceFormat::GjcJsonl, &path);
                if !admitted_ids.insert(source_id) {
                    notices.push(DiscoveryNotice::SourceIdentityCollision);
                    continue;
                }
                sources.push(DiscoveredSource {
                    source_id,
                    format: SourceFormat::GjcJsonl,
                    session,
                    path,
                });
            }
            if overflow {
                notices.push(DiscoveryNotice::SourceScanLimitReached);
            }
            continue;
        }
        if client == "opencodereview" {
            // OpenCodeReview JSONL sessions at `~/.opencodereview/sessions/<encoded-repo>/<session>.jsonl`.
            let root = join_many(
                inputs.platform,
                &inputs.home,
                &[".opencodereview", "sessions"],
            );
            let mut candidates = Vec::new();
            let mut inspected = 0;
            let mut overflow = false;
            collect_source_candidates(&root, 0, &mut inspected, &mut candidates, &mut overflow)?;
            candidates.sort();
            for (ordinal, path) in candidates.into_iter().enumerate() {
                let ordinal = ordinal + 1;
                if sources.len() >= MAX_SOURCES {
                    notices.push(DiscoveryNotice::SourceLimitReached);
                    break;
                }
                let session = match validate_source(
                    &path,
                    SourceFormat::OpenCodeReviewJsonl,
                    fs::symlink_metadata(&path)
                        .map(|meta| meta.len())
                        .unwrap_or(0),
                ) {
                    Ok(session) => session,
                    Err(()) => {
                        notices.push(DiscoveryNotice::SourceInvalid {
                            client: "opencodereview",
                            ordinal,
                        });
                        continue;
                    }
                };
                let source_id = source_id_for(SourceFormat::OpenCodeReviewJsonl, &path);
                if !admitted_ids.insert(source_id) {
                    notices.push(DiscoveryNotice::SourceIdentityCollision);
                    continue;
                }
                sources.push(DiscoveredSource {
                    source_id,
                    format: SourceFormat::OpenCodeReviewJsonl,
                    session,
                    path,
                });
            }
            if overflow {
                notices.push(DiscoveryNotice::SourceScanLimitReached);
            }
            continue;
        }
        if client == "codebuff" {
            // Codebuff keeps chat history under
            // `<config>/<channel>/projects/<project>/chats/<chatId>/chat-messages.json`,
            // with `manicode` plus the `-dev` and `-staging` channels.
            let mut candidates = Vec::new();
            let mut inspected = 0;
            let mut overflow = false;
            for channel in ["manicode", "manicode-dev", "manicode-staging"] {
                let root = join_many(
                    inputs.platform,
                    &inputs.home,
                    &[".config", channel, "projects"],
                );
                collect_source_candidates(
                    &root,
                    0,
                    &mut inspected,
                    &mut candidates,
                    &mut overflow,
                )?;
            }
            candidates.retain(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name == "chat-messages.json")
                    && !crate::parsers::freebuff::is_freebuff_path(path)
            });
            candidates.sort();
            for (ordinal, path) in candidates.into_iter().enumerate() {
                let ordinal = ordinal + 1;
                if sources.len() >= MAX_SOURCES {
                    notices.push(DiscoveryNotice::SourceLimitReached);
                    break;
                }
                let session = match validate_source(
                    &path,
                    SourceFormat::CodebuffJson,
                    fs::symlink_metadata(&path)
                        .map(|meta| meta.len())
                        .unwrap_or(0),
                ) {
                    Ok(session) => session,
                    Err(()) => {
                        notices.push(DiscoveryNotice::SourceInvalid {
                            client: "codebuff",
                            ordinal,
                        });
                        continue;
                    }
                };
                let source_id = source_id_for(SourceFormat::CodebuffJson, &path);
                if !admitted_ids.insert(source_id) {
                    notices.push(DiscoveryNotice::SourceIdentityCollision);
                    continue;
                }
                sources.push(DiscoveredSource {
                    source_id,
                    format: SourceFormat::CodebuffJson,
                    session,
                    path,
                });
            }
            if overflow {
                notices.push(DiscoveryNotice::SourceScanLimitReached);
            }
            continue;
        }
        if client == "freebuff" {
            // The same tree and the same file shape as Codebuff; only the lane
            // marker separates them, so a chat can never be counted twice.
            let mut candidates = Vec::new();
            let mut inspected = 0;
            let mut overflow = false;
            for channel in ["manicode", "manicode-dev", "manicode-staging"] {
                let root = join_many(
                    inputs.platform,
                    &inputs.home,
                    &[".config", channel, "projects"],
                );
                collect_source_candidates(
                    &root,
                    0,
                    &mut inspected,
                    &mut candidates,
                    &mut overflow,
                )?;
            }
            candidates.retain(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name == "chat-messages.json")
                    && crate::parsers::freebuff::is_freebuff_path(path)
            });
            candidates.sort();
            for (ordinal, path) in candidates.into_iter().enumerate() {
                let ordinal = ordinal + 1;
                if sources.len() >= MAX_SOURCES {
                    notices.push(DiscoveryNotice::SourceLimitReached);
                    break;
                }
                let session = match validate_source(
                    &path,
                    SourceFormat::FreebuffJson,
                    fs::symlink_metadata(&path)
                        .map(|meta| meta.len())
                        .unwrap_or(0),
                ) {
                    Ok(session) => session,
                    Err(()) => {
                        notices.push(DiscoveryNotice::SourceInvalid {
                            client: "freebuff",
                            ordinal,
                        });
                        continue;
                    }
                };
                let source_id = source_id_for(SourceFormat::FreebuffJson, &path);
                if !admitted_ids.insert(source_id) {
                    notices.push(DiscoveryNotice::SourceIdentityCollision);
                    continue;
                }
                sources.push(DiscoveredSource {
                    source_id,
                    format: SourceFormat::FreebuffJson,
                    session,
                    path,
                });
            }
            if overflow {
                notices.push(DiscoveryNotice::SourceScanLimitReached);
            }
            continue;
        }
        if client == "trae" {
            // Trae's usage-API dump lands in the collector cache directory, the
            // same convention the cursor and antigravity `*-cache` lanes use.
            let root = join_many(
                inputs.platform,
                &tokscale_config_dir(inputs),
                &["trae-cache", "sessions"],
            );
            let mut candidates = Vec::new();
            let mut inspected = 0;
            let mut overflow = false;
            collect_source_candidates(&root, 0, &mut inspected, &mut candidates, &mut overflow)?;
            candidates.sort();
            for (ordinal, path) in candidates.into_iter().enumerate() {
                let ordinal = ordinal + 1;
                if sources.len() >= MAX_SOURCES {
                    notices.push(DiscoveryNotice::SourceLimitReached);
                    break;
                }
                let session = match validate_source(
                    &path,
                    SourceFormat::TraeJson,
                    fs::symlink_metadata(&path)
                        .map(|meta| meta.len())
                        .unwrap_or(0),
                ) {
                    Ok(session) => session,
                    Err(()) => {
                        notices.push(DiscoveryNotice::SourceInvalid {
                            client: "trae",
                            ordinal,
                        });
                        continue;
                    }
                };
                let source_id = source_id_for(SourceFormat::TraeJson, &path);
                if !admitted_ids.insert(source_id) {
                    notices.push(DiscoveryNotice::SourceIdentityCollision);
                    continue;
                }
                sources.push(DiscoveredSource {
                    source_id,
                    format: SourceFormat::TraeJson,
                    session,
                    path,
                });
            }
            if overflow {
                notices.push(DiscoveryNotice::SourceScanLimitReached);
            }
            continue;
        }
        if client == "augment" {
            // Auggie writes one session snapshot per session at
            // `~/.augment/sessions/<sessionId>.json`.
            let root = join_many(inputs.platform, &inputs.home, &[".augment", "sessions"]);
            let mut candidates = Vec::new();
            let mut inspected = 0;
            let mut overflow = false;
            collect_source_candidates(&root, 0, &mut inspected, &mut candidates, &mut overflow)?;
            candidates.retain(|path| {
                path.extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
            });
            candidates.sort();
            for (ordinal, path) in candidates.into_iter().enumerate() {
                let ordinal = ordinal + 1;
                if sources.len() >= MAX_SOURCES {
                    notices.push(DiscoveryNotice::SourceLimitReached);
                    break;
                }
                let session = match validate_source(
                    &path,
                    SourceFormat::AugmentJson,
                    fs::symlink_metadata(&path)
                        .map(|meta| meta.len())
                        .unwrap_or(0),
                ) {
                    Ok(session) => session,
                    Err(()) => {
                        notices.push(DiscoveryNotice::SourceInvalid {
                            client: "augment",
                            ordinal,
                        });
                        continue;
                    }
                };
                let source_id = source_id_for(SourceFormat::AugmentJson, &path);
                if !admitted_ids.insert(source_id) {
                    notices.push(DiscoveryNotice::SourceIdentityCollision);
                    continue;
                }
                sources.push(DiscoveredSource {
                    source_id,
                    format: SourceFormat::AugmentJson,
                    session,
                    path,
                });
            }
            if overflow {
                notices.push(DiscoveryNotice::SourceScanLimitReached);
            }
            continue;
        }
        if client == "junie" {
            // Junie stores local sessions under
            // `~/.junie/sessions/<session-id>/events.jsonl`.
            let root = join_many(inputs.platform, &inputs.home, &[".junie", "sessions"]);
            let mut candidates = Vec::new();
            let mut inspected = 0;
            let mut overflow = false;
            collect_source_candidates(&root, 0, &mut inspected, &mut candidates, &mut overflow)?;
            candidates.retain(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name == "events.jsonl")
            });
            candidates.sort();
            for (ordinal, path) in candidates.into_iter().enumerate() {
                let ordinal = ordinal + 1;
                if sources.len() >= MAX_SOURCES {
                    notices.push(DiscoveryNotice::SourceLimitReached);
                    break;
                }
                let session = match validate_source(
                    &path,
                    SourceFormat::JunieJsonl,
                    fs::symlink_metadata(&path)
                        .map(|meta| meta.len())
                        .unwrap_or(0),
                ) {
                    Ok(session) => session,
                    Err(()) => {
                        notices.push(DiscoveryNotice::SourceInvalid {
                            client: "junie",
                            ordinal,
                        });
                        continue;
                    }
                };
                let source_id = source_id_for(SourceFormat::JunieJsonl, &path);
                if !admitted_ids.insert(source_id) {
                    notices.push(DiscoveryNotice::SourceIdentityCollision);
                    continue;
                }
                sources.push(DiscoveredSource {
                    source_id,
                    format: SourceFormat::JunieJsonl,
                    session,
                    path,
                });
            }
            if overflow {
                notices.push(DiscoveryNotice::SourceScanLimitReached);
            }
            continue;
        }
        if client == "mux" {
            // Mux keeps one aggregate counter document per workspace at
            // `~/.mux/sessions/<workspaceId>/session-usage.json`.
            let root = join_many(inputs.platform, &inputs.home, &[".mux", "sessions"]);
            let mut candidates = Vec::new();
            let mut inspected = 0;
            let mut overflow = false;
            collect_source_candidates(&root, 0, &mut inspected, &mut candidates, &mut overflow)?;
            candidates.retain(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name == "session-usage.json")
            });
            candidates.sort();
            for (ordinal, path) in candidates.into_iter().enumerate() {
                let ordinal = ordinal + 1;
                if sources.len() >= MAX_SOURCES {
                    notices.push(DiscoveryNotice::SourceLimitReached);
                    break;
                }
                let session = match validate_source(
                    &path,
                    SourceFormat::MuxJson,
                    fs::symlink_metadata(&path)
                        .map(|meta| meta.len())
                        .unwrap_or(0),
                ) {
                    Ok(session) => session,
                    Err(()) => {
                        notices.push(DiscoveryNotice::SourceInvalid {
                            client: "mux",
                            ordinal,
                        });
                        continue;
                    }
                };
                let source_id = source_id_for(SourceFormat::MuxJson, &path);
                if !admitted_ids.insert(source_id) {
                    notices.push(DiscoveryNotice::SourceIdentityCollision);
                    continue;
                }
                sources.push(DiscoveredSource {
                    source_id,
                    format: SourceFormat::MuxJson,
                    session,
                    path,
                });
            }
            if overflow {
                notices.push(DiscoveryNotice::SourceScanLimitReached);
            }
            continue;
        }
        if client == "jcode" {
            // Jcode writes one compact JSON session snapshot per session at
            // `~/.jcode/sessions/session_*.json`. The sibling journal is not
            // read, because a per-source reader cannot reconcile across files.
            let root = join_many(inputs.platform, &inputs.home, &[".jcode", "sessions"]);
            let mut candidates = Vec::new();
            let mut inspected = 0;
            let mut overflow = false;
            collect_source_candidates(&root, 0, &mut inspected, &mut candidates, &mut overflow)?;
            candidates.retain(|path| {
                path.extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
                    && path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with("session_"))
            });
            candidates.sort();
            for (ordinal, path) in candidates.into_iter().enumerate() {
                let ordinal = ordinal + 1;
                if sources.len() >= MAX_SOURCES {
                    notices.push(DiscoveryNotice::SourceLimitReached);
                    break;
                }
                let session = match validate_source(
                    &path,
                    SourceFormat::JcodeJson,
                    fs::symlink_metadata(&path)
                        .map(|meta| meta.len())
                        .unwrap_or(0),
                ) {
                    Ok(session) => session,
                    Err(()) => {
                        notices.push(DiscoveryNotice::SourceInvalid {
                            client: "jcode",
                            ordinal,
                        });
                        continue;
                    }
                };
                let source_id = source_id_for(SourceFormat::JcodeJson, &path);
                if !admitted_ids.insert(source_id) {
                    notices.push(DiscoveryNotice::SourceIdentityCollision);
                    continue;
                }
                sources.push(DiscoveredSource {
                    source_id,
                    format: SourceFormat::JcodeJson,
                    session,
                    path,
                });
            }
            if overflow {
                notices.push(DiscoveryNotice::SourceScanLimitReached);
            }
            continue;
        }
        if client == "dsh" {
            // DSH writes one transcript per session under
            // `<DSH_HOME>/sessions/<encoded-cwd>/<session-id>/session.jsonl`
            // (`<DSH_HOME>` defaults to `~/.dsh`) and appends `.zstd` when the
            // backend compresses. The suffix marks the physical encoding only,
            // so both are admitted and the input layer dispatches on the zstd
            // frame magic rather than on the name.
            let root = join_many(inputs.platform, &inputs.home, &[".dsh", "sessions"]);
            let mut candidates = Vec::new();
            let mut inspected = 0;
            let mut overflow = false;
            collect_source_candidates(&root, 0, &mut inspected, &mut candidates, &mut overflow)?;
            candidates.retain(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        name == "session.jsonl" || name.eq_ignore_ascii_case("session.jsonl.zstd")
                    })
            });
            candidates.sort();
            for (ordinal, path) in candidates.into_iter().enumerate() {
                let ordinal = ordinal + 1;
                if sources.len() >= MAX_SOURCES {
                    notices.push(DiscoveryNotice::SourceLimitReached);
                    break;
                }
                let session = match source_session(&path, SourceFormat::DshJsonl) {
                    Ok(session) => session,
                    Err(()) => {
                        notices.push(DiscoveryNotice::SourceInvalid {
                            client: "dsh",
                            ordinal,
                        });
                        continue;
                    }
                };
                let bytes = fs::symlink_metadata(&path)
                    .map(|meta| meta.len())
                    .unwrap_or(0);
                let session = match validate_source(&path, SourceFormat::DshJsonl, bytes) {
                    Ok(_) => session,
                    Err(()) => {
                        notices.push(DiscoveryNotice::SourceInvalid {
                            client: "dsh",
                            ordinal,
                        });
                        continue;
                    }
                };
                let source_id = source_id_for(SourceFormat::DshJsonl, &path);
                if !admitted_ids.insert(source_id) {
                    notices.push(DiscoveryNotice::SourceIdentityCollision);
                    continue;
                }
                sources.push(DiscoveredSource {
                    source_id,
                    format: SourceFormat::DshJsonl,
                    session,
                    path,
                });
            }
            continue;
        }
        if client == "zed" {
            for (ordinal, path) in zed_database_paths(inputs).into_iter().enumerate() {
                let ordinal = ordinal + 1;
                let metadata = match fs::symlink_metadata(&path) {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error.into()),
                };
                if !metadata.file_type().is_file() {
                    notices.push(DiscoveryNotice::SourceNotRegular {
                        client: "zed",
                        ordinal,
                    });
                    continue;
                }
                if metadata.len() > MAX_SOURCE_FILE_BYTES {
                    notices.push(DiscoveryNotice::SourceInvalid {
                        client: "zed",
                        ordinal,
                    });
                    continue;
                }
                match ZedAdapter::new(&path).read() {
                    Ok(rows) if !rows.is_empty() => {
                        if sources.len() == MAX_SOURCES {
                            notices.push(DiscoveryNotice::SourceLimitReached);
                            return Ok(sources);
                        }
                        let source_id = source_id_for(SourceFormat::ZedSqlite, &path);
                        if admitted_ids.insert(source_id) {
                            sources.push(DiscoveredSource {
                                source_id,
                                format: SourceFormat::ZedSqlite,
                                session: "zed".to_owned(),
                                path,
                            });
                        } else {
                            notices.push(DiscoveryNotice::SourceIdentityCollision);
                        }
                    }
                    _ => notices.push(DiscoveryNotice::SourceInvalid {
                        client: "zed",
                        ordinal,
                    }),
                }
            }
            continue;
        }
        if client == "goose" {
            if let Some(path) = goose_database_path(inputs) {
                match fs::symlink_metadata(&path) {
                    Ok(metadata) if metadata.file_type().is_file() => {
                        if metadata.len() <= MAX_SOURCE_FILE_BYTES && sources.len() < MAX_SOURCES {
                            match GooseAdapter::new(&path).read() {
                                Ok(rows) if !rows.is_empty() => {
                                    let source_id = source_id_for(SourceFormat::GooseSqlite, &path);
                                    if admitted_ids.insert(source_id) {
                                        sources.push(DiscoveredSource {
                                            source_id,
                                            format: SourceFormat::GooseSqlite,
                                            session: "goose".to_owned(),
                                            path,
                                        });
                                    } else {
                                        notices.push(DiscoveryNotice::SourceIdentityCollision);
                                    }
                                }
                                _ => notices.push(DiscoveryNotice::SourceInvalid {
                                    client: "goose",
                                    ordinal: 1,
                                }),
                            }
                        } else if sources.len() >= MAX_SOURCES {
                            notices.push(DiscoveryNotice::SourceLimitReached);
                        } else {
                            notices.push(DiscoveryNotice::SourceInvalid {
                                client: "goose",
                                ordinal: 1,
                            });
                        }
                    }
                    Ok(_) => notices.push(DiscoveryNotice::SourceNotRegular {
                        client: "goose",
                        ordinal: 1,
                    }),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
            continue;
        }
        if client == "openclaw" {
            // The tool has been renamed twice, so an older install keeps its
            // history under the previous home directory. All four are read.
            let mut candidates = Vec::new();
            let mut inspected = 0;
            let mut overflow = false;
            for home in [".openclaw", ".clawdbot", ".moltbot", ".moldbot"] {
                let root = join_many(inputs.platform, &inputs.home, &[home, "agents"]);
                collect_openclaw_candidates(
                    &root,
                    0,
                    &mut inspected,
                    &mut candidates,
                    &mut overflow,
                )?;
            }
            if overflow {
                notices.push(DiscoveryNotice::SourceScanLimitReached);
                continue;
            }
            candidates.sort();
            candidates.dedup();
            for (index, path) in candidates.into_iter().enumerate() {
                let ordinal = index + 1;
                let format = match openclaw_candidate_format(&path) {
                    Some(format) => format,
                    None => continue,
                };
                match fs::symlink_metadata(&path) {
                    Ok(metadata) if metadata.file_type().is_file() => {
                        if metadata.len() > MAX_SOURCE_FILE_BYTES {
                            notices.push(DiscoveryNotice::SourceInvalid {
                                client: "openclaw",
                                ordinal,
                            });
                            continue;
                        }
                        if sources.len() >= MAX_SOURCES {
                            notices.push(DiscoveryNotice::SourceLimitReached);
                            continue;
                        }
                        let probe = match format {
                            SourceFormat::OpenClawJsonl => probe_openclaw_jsonl(&path),
                            _ => OpenClawAdapter::new(&path)
                                .read()
                                .map(|rows| !rows.is_empty()),
                        };
                        match probe {
                            Ok(true) => {
                                let source_id = source_id_for(format, &path);
                                if admitted_ids.insert(source_id) {
                                    sources.push(DiscoveredSource {
                                        source_id,
                                        format,
                                        session: "openclaw".to_owned(),
                                        path,
                                    });
                                } else {
                                    notices.push(DiscoveryNotice::SourceIdentityCollision);
                                }
                            }
                            _ => notices.push(DiscoveryNotice::SourceInvalid {
                                client: "openclaw",
                                ordinal,
                            }),
                        }
                    }
                    Ok(_) => notices.push(DiscoveryNotice::SourceNotRegular {
                        client: "openclaw",
                        ordinal,
                    }),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
            continue;
        }
        if client == "hermes" {
            let root = hermes_home(inputs);
            let mut candidates = hermes_state_databases(&root);
            candidates.sort();
            candidates.dedup();
            for (index, path) in candidates.into_iter().enumerate() {
                let ordinal = index + 1;
                match fs::symlink_metadata(&path) {
                    Ok(metadata) if metadata.file_type().is_file() => {
                        if metadata.len() <= MAX_SOURCE_FILE_BYTES && sources.len() < MAX_SOURCES {
                            match HermesAdapter::new(&path).read() {
                                Ok(rows) if !rows.is_empty() => {
                                    let source_id =
                                        source_id_for(SourceFormat::HermesSqlite, &path);
                                    if admitted_ids.insert(source_id) {
                                        sources.push(DiscoveredSource {
                                            source_id,
                                            format: SourceFormat::HermesSqlite,
                                            session: "hermes".to_owned(),
                                            path,
                                        });
                                    } else {
                                        notices.push(DiscoveryNotice::SourceIdentityCollision);
                                    }
                                }
                                _ => notices.push(DiscoveryNotice::SourceInvalid {
                                    client: "hermes",
                                    ordinal,
                                }),
                            }
                        } else if sources.len() >= MAX_SOURCES {
                            notices.push(DiscoveryNotice::SourceLimitReached);
                        } else {
                            notices.push(DiscoveryNotice::SourceInvalid {
                                client: "hermes",
                                ordinal,
                            });
                        }
                    }
                    Ok(_) => notices.push(DiscoveryNotice::SourceNotRegular {
                        client: "hermes",
                        ordinal,
                    }),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
            continue;
        }
        if client == "copilot" {
            let root = join_many(inputs.platform, &inputs.home, &[".copilot", "otel"]);
            let mut candidates = Vec::new();
            let mut inspected = 0;
            let mut overflow = false;
            collect_copilot_candidates(&root, 0, &mut inspected, &mut candidates, &mut overflow)?;
            if overflow {
                notices.push(DiscoveryNotice::SourceScanLimitReached);
                continue;
            }
            candidates.sort();
            candidates.dedup();
            for (index, path) in candidates.into_iter().enumerate() {
                if sources.len() == MAX_SOURCES {
                    notices.push(DiscoveryNotice::SourceLimitReached);
                    return Ok(sources);
                }
                let ordinal = index + 1;
                let metadata = fs::symlink_metadata(&path)?;
                if !metadata.file_type().is_file() {
                    notices.push(DiscoveryNotice::SourceNotRegular {
                        client: "copilot",
                        ordinal,
                    });
                    continue;
                }
                if metadata.len() > MAX_SOURCE_FILE_BYTES {
                    notices.push(DiscoveryNotice::SourceInvalid {
                        client: "copilot",
                        ordinal,
                    });
                    continue;
                }
                match validate_source(&path, SourceFormat::CopilotJsonl, metadata.len()) {
                    Ok(session) => {
                        let source_id = source_id_for(SourceFormat::CopilotJsonl, &path);
                        if admitted_ids.insert(source_id) {
                            sources.push(DiscoveredSource {
                                source_id,
                                format: SourceFormat::CopilotJsonl,
                                session,
                                path,
                            });
                        } else {
                            notices.push(DiscoveryNotice::SourceIdentityCollision);
                        }
                    }
                    Err(()) => notices.push(DiscoveryNotice::SourceInvalid {
                        client: "copilot",
                        ordinal,
                    }),
                }
            }
            continue;
        }
        let (client_id, format, roots) = match client.as_str() {
            "claude" => (
                "claude",
                SourceFormat::ClaudeCli,
                vec![
                    join_many(inputs.platform, &inputs.home, &[".claude", "projects"]),
                    join_many(inputs.platform, &inputs.home, &[".claude", "transcripts"]),
                ],
            ),
            "codex" => (
                "codex",
                SourceFormat::CodexCli,
                vec![
                    join_many(inputs.platform, &inputs.home, &[".codex", "sessions"]),
                    join_many(
                        inputs.platform,
                        &inputs.home,
                        &[".codex", "archived_sessions"],
                    ),
                ],
            ),
            "codebuddy" => ("codebuddy", SourceFormat::CodeBuddyJsonl, {
                let mut roots = vec![join_many(
                    inputs.platform,
                    &inputs.home,
                    &[".codebuddy", "projects"],
                )];
                roots.extend(codebuddy_log_roots(inputs));
                roots
            }),
            "workbuddy" => (
                "workbuddy",
                SourceFormat::WorkBuddyJsonl,
                vec![
                    join_many(inputs.platform, &inputs.home, &[".workbuddy", "projects"]),
                    join_many(
                        inputs.platform,
                        &inputs.home,
                        &[".workbuddy-ai", "projects"],
                    ),
                ],
            ),
            "pi" => (
                "pi",
                SourceFormat::PiJsonl,
                vec![join_many(
                    inputs.platform,
                    &inputs.home,
                    &[".pi", "agent", "sessions"],
                )],
            ),
            "omp" => (
                "omp",
                SourceFormat::OmpJsonl,
                vec![join_many(
                    inputs.platform,
                    &inputs.home,
                    &[".omp", "agent", "sessions"],
                )],
            ),
            "senpi" => (
                "senpi",
                SourceFormat::SenpiJsonl,
                senpi_session_roots(inputs),
            ),
            // Kimchi writes the Pi record format under its own agent
            // directory, so it is discovered and read exactly like senpi/omp
            // and only the client id differs.
            "kimchi" => (
                "kimchi",
                SourceFormat::KimchiJsonl,
                vec![join_many(
                    inputs.platform,
                    &kimchi_agent_dir(inputs),
                    &["sessions"],
                )],
            ),
            "antigravity" => (
                "antigravity",
                SourceFormat::AntigravityJsonl,
                vec![join(
                    inputs.platform,
                    &tokscale_config_dir(inputs),
                    "antigravity-cache",
                )],
            ),
            "qwen" => (
                "qwen",
                SourceFormat::QwenJsonl,
                vec![join_many(
                    inputs.platform,
                    &inputs.home,
                    &[".qwen", "projects"],
                )],
            ),
            "reasonix" => (
                "reasonix",
                SourceFormat::ReasonixJsonl,
                vec![join(inputs.platform, &reasonix_home(inputs), "stats")],
            ),
            "muse" => (
                "muse",
                SourceFormat::MuseJsonl,
                vec![join_many(
                    inputs.platform,
                    &inputs
                        .xdg_data_home
                        .clone()
                        .unwrap_or_else(|| inputs.home.join(".local").join("share")),
                    &["muse", "sessions"],
                )],
            ),
            "mcode" => (
                "mcode",
                SourceFormat::McodeJsonl,
                mcode_session_roots(inputs),
            ),
            "droid" => (
                "droid",
                SourceFormat::DroidJson,
                vec![join_many(
                    inputs.platform,
                    &inputs.home,
                    &[".factory", "sessions"],
                )],
            ),
            "commandcode" => (
                "commandcode",
                SourceFormat::CommandCodeJsonl,
                vec![join_many(
                    inputs.platform,
                    &inputs.home,
                    &[".commandcode", "projects"],
                )],
            ),
            "grok" => (
                "grok",
                SourceFormat::GrokJsonl,
                vec![join_many(
                    inputs.platform,
                    &inputs.home,
                    &[".grok", "sessions"],
                )],
            ),
            "kiro" => (
                "kiro",
                SourceFormat::KiroJson,
                vec![join_many(
                    inputs.platform,
                    &inputs.home,
                    &[".kiro", "sessions", "cli"],
                )],
            ),
            "kimi" => (
                "kimi",
                SourceFormat::KimiJsonl,
                vec![
                    join_many(inputs.platform, &inputs.home, &[".kimi", "sessions"]),
                    // The coding-focused build keeps its sessions in a
                    // sibling home directory.
                    join_many(inputs.platform, &inputs.home, &[".kimi-code", "sessions"]),
                ],
            ),
            _ => {
                notices.push(DiscoveryNotice::UnsupportedClient {
                    client: redacted_client_id(client),
                });
                continue;
            }
        };
        let mut candidates = Vec::new();
        let mut inspected = 0;
        let mut overflow = false;
        for root in roots {
            collect_source_candidates(&root, 0, &mut inspected, &mut candidates, &mut overflow)?;
            if client_id == "codebuddy" {
                collect_log_candidates(&root, 0, &mut inspected, &mut candidates, &mut overflow)?;
            }
            if client_id == "commandcode" {
                // `<session>.checkpoints.jsonl` is a snapshot stream beside the
                // transcript, not a transcript; only the session JSONL is usage.
                candidates.retain(|path| {
                    !path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.ends_with(".checkpoints.jsonl"))
                });
            }
            if client_id == "grok" {
                // Only the per-session update stream is usage; signals.json is
                // a rollup the reader does not consume.
                candidates.retain(|path| {
                    path.file_name().and_then(|name| name.to_str()) == Some("updates.jsonl")
                });
            }
            if client_id == "kimi" {
                // Only `wire.jsonl` is the usage stream; the session directory
                // also holds `context.jsonl` and other non-usage records.
                candidates.retain(|path| {
                    path.file_name().and_then(|name| name.to_str()) == Some("wire.jsonl")
                });
            }
            if overflow {
                break;
            }
        }
        if overflow {
            notices.push(DiscoveryNotice::SourceScanLimitReached);
            continue;
        }
        candidates.sort();
        candidates.dedup();
        for (index, path) in candidates.into_iter().enumerate() {
            if sources.len() == MAX_SOURCES {
                notices.push(DiscoveryNotice::SourceLimitReached);
                return Ok(sources);
            }
            let ordinal = index + 1;
            let metadata = fs::symlink_metadata(&path)?;
            if !metadata.file_type().is_file() {
                notices.push(DiscoveryNotice::SourceNotRegular {
                    client: client_id,
                    ordinal,
                });
                continue;
            }
            if metadata.len() > MAX_SOURCE_FILE_BYTES {
                notices.push(DiscoveryNotice::SourceInvalid {
                    client: client_id,
                    ordinal,
                });
                continue;
            }
            let compressed = is_compressed_source_path(&path);
            if has_zstd_suffix(&path) && !compressed {
                notices.push(DiscoveryNotice::SourceUnsupportedCompression {
                    client: client_id,
                    ordinal,
                });
                continue;
            }
            if compressed && format != SourceFormat::CodexCli {
                notices.push(DiscoveryNotice::SourceUnsupportedCompression {
                    client: client_id,
                    ordinal,
                });
                continue;
            }
            match validate_source(&path, format, metadata.len()) {
                Ok(session) => {
                    let source_id = source_id_for(format, &path);
                    if rejected_ids.contains(&source_id) {
                        notices.push(DiscoveryNotice::SourceIdentityCollision);
                        continue;
                    }
                    if !admitted_ids.insert(source_id) {
                        sources.retain(|source: &DiscoveredSource| source.source_id != source_id);
                        admitted_ids.remove(&source_id);
                        rejected_ids.insert(source_id);
                        notices.push(DiscoveryNotice::SourceIdentityCollision);
                        continue;
                    }
                    sources.push(DiscoveredSource {
                        source_id,
                        format,
                        session,
                        path,
                    });
                }
                Err(()) => notices.push(DiscoveryNotice::SourceInvalid {
                    client: client_id,
                    ordinal,
                }),
            }
        }
    }
    if inputs.enabled_clients.contains("antigravity") {
        let mut candidates = Vec::new();
        let mut inspected = 0;
        let mut overflow = false;
        for root in antigravity_database_roots(inputs) {
            collect_db_candidates(&root, 0, &mut inspected, &mut candidates, &mut overflow)?;
            if overflow {
                break;
            }
        }
        if overflow {
            notices.push(DiscoveryNotice::SourceScanLimitReached);
        } else {
            candidates.sort();
            candidates.dedup();
            for (ordinal, path) in candidates.into_iter().enumerate() {
                if sources.len() == MAX_SOURCES {
                    notices.push(DiscoveryNotice::SourceLimitReached);
                    break;
                }
                let metadata = match fs::symlink_metadata(&path) {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error.into()),
                };
                if !metadata.file_type().is_file() {
                    notices.push(DiscoveryNotice::SourceNotRegular {
                        client: "antigravity",
                        ordinal: ordinal + 1,
                    });
                    continue;
                }
                if metadata.len() > MAX_SOURCE_FILE_BYTES {
                    notices.push(DiscoveryNotice::SourceInvalid {
                        client: "antigravity",
                        ordinal: ordinal + 1,
                    });
                    continue;
                }
                match AntigravitySqliteAdapter::new(&path).read() {
                    Ok(rows) if !rows.is_empty() => {
                        let source_id = source_id_for(SourceFormat::AntigravitySqlite, &path);
                        if admitted_ids.insert(source_id) {
                            let session = path
                                .file_stem()
                                .and_then(|stem| stem.to_str())
                                .unwrap_or("antigravity")
                                .to_owned();
                            sources.push(DiscoveredSource {
                                source_id,
                                format: SourceFormat::AntigravitySqlite,
                                session,
                                path,
                            });
                        } else {
                            notices.push(DiscoveryNotice::SourceIdentityCollision);
                        }
                    }
                    _ => notices.push(DiscoveryNotice::SourceInvalid {
                        client: "antigravity",
                        ordinal: ordinal + 1,
                    }),
                }
            }
        }
    }
    if inputs.enabled_clients.contains("workbuddy")
        && !sources
            .iter()
            .any(|source: &DiscoveredSource| source.format == SourceFormat::WorkBuddyJsonl)
    {
        for (ordinal, path) in workbuddy_database_paths(inputs).into_iter().enumerate() {
            let ordinal = ordinal + 1;
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            if !metadata.file_type().is_file() {
                notices.push(DiscoveryNotice::SourceNotRegular {
                    client: "workbuddy",
                    ordinal,
                });
                continue;
            }
            if metadata.len() > MAX_SOURCE_FILE_BYTES {
                notices.push(DiscoveryNotice::SourceInvalid {
                    client: "workbuddy",
                    ordinal,
                });
                continue;
            }
            match WorkBuddySqliteAdapter::new(&path).read() {
                Ok(rows) if !rows.is_empty() => {
                    if sources.len() == MAX_SOURCES {
                        notices.push(DiscoveryNotice::SourceLimitReached);
                        return Ok(sources);
                    }
                    let source_id = source_id_for(SourceFormat::WorkBuddySqlite, &path);
                    if admitted_ids.insert(source_id) {
                        sources.push(DiscoveredSource {
                            source_id,
                            format: SourceFormat::WorkBuddySqlite,
                            session: "workbuddy".to_owned(),
                            path,
                        });
                        break;
                    }
                    notices.push(DiscoveryNotice::SourceIdentityCollision);
                }
                _ => notices.push(DiscoveryNotice::SourceInvalid {
                    client: "workbuddy",
                    ordinal,
                }),
            }
        }
    }
    Ok(sources)
}

fn discover_sqlite_sources(
    root: &Path,
    _notices: &mut Vec<DiscoveryNotice>,
) -> Result<Vec<DiscoveredSource>, DiscoveryError> {
    let mut sources = Vec::new();
    for (name, format, session) in [
        (
            "session-usage-archive.sqlite",
            SourceFormat::SessionArchiveSqlite,
            "archive",
        ),
        (
            "codex-local-usage.sqlite",
            SourceFormat::CodexLocalUsageSqlite,
            "codex-local",
        ),
    ] {
        let path = root.join(name);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if !metadata.file_type().is_file() || metadata.len() > MAX_SOURCE_FILE_BYTES {
            continue;
        }
        let valid = match format {
            SourceFormat::SessionArchiveSqlite => SessionArchiveAdapter::new(&path)
                .read()
                .map(|_| ())
                .map_err(|_| ()),
            SourceFormat::CodexLocalUsageSqlite => CodexLocalUsageAdapter::new(&path)
                .read()
                .map(|_| ())
                .map_err(|_| ()),
            _ => Err(()),
        };
        if valid.is_err() {
            continue;
        }
        sources.push(DiscoveredSource {
            source_id: SourceId::for_path(format, &path),
            format,
            session: session.to_owned(),
            path,
        });
    }
    Ok(sources)
}

fn collect_source_candidates(
    directory: &Path,
    depth: usize,
    inspected: &mut usize,
    candidates: &mut Vec<PathBuf>,
    overflow: &mut bool,
) -> Result<(), DiscoveryError> {
    if *overflow {
        return Ok(());
    }
    if depth > MAX_SOURCE_DEPTH {
        *overflow = true;
        candidates.clear();
        return Ok(());
    }
    let metadata = match fs::symlink_metadata(directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_dir() {
        return Ok(());
    }
    let mut children = Vec::new();
    for entry in fs::read_dir(directory)? {
        *inspected += 1;
        if *inspected > MAX_SOURCE_CANDIDATES {
            *overflow = true;
            candidates.clear();
            return Ok(());
        }
        children.push(entry?.path());
    }
    children.sort();
    for child in children {
        let metadata = fs::symlink_metadata(&child)?;
        if metadata.file_type().is_dir() {
            collect_source_candidates(&child, depth + 1, inspected, candidates, overflow)?;
            if *overflow {
                return Ok(());
            }
        } else if child.extension().is_some_and(|extension| {
            extension == "jsonl"
                || extension
                    .to_str()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("zst"))
                || extension == "json"
        }) {
            candidates.push(child);
        }
    }
    Ok(())
}

/// Collect `*.log` files, which the shared collector does not admit.
///
/// LM Studio's server logs are plain text named `.log`; keeping this separate
/// keeps the JSONL collector's extension list honest rather than widening it
/// for one client.
fn collect_log_candidates(
    directory: &Path,
    depth: usize,
    inspected: &mut usize,
    candidates: &mut Vec<PathBuf>,
    overflow: &mut bool,
) -> Result<(), DiscoveryError> {
    if *overflow {
        return Ok(());
    }
    if depth > MAX_SOURCE_DEPTH {
        *overflow = true;
        candidates.clear();
        return Ok(());
    }
    let metadata = match fs::symlink_metadata(directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_dir() {
        return Ok(());
    }
    let mut children = Vec::new();
    for entry in fs::read_dir(directory)? {
        *inspected += 1;
        if *inspected > MAX_SOURCE_CANDIDATES {
            *overflow = true;
            candidates.clear();
            return Ok(());
        }
        children.push(entry?.path());
    }
    children.sort();
    for child in children {
        let metadata = fs::symlink_metadata(&child)?;
        if metadata.file_type().is_dir() {
            collect_log_candidates(&child, depth + 1, inspected, candidates, overflow)?;
            if *overflow {
                return Ok(());
            }
        } else if child
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("log"))
        {
            candidates.push(child);
        }
    }
    Ok(())
}

fn validate_source(path: &Path, format: SourceFormat, bytes: u64) -> Result<String, ()> {
    let session = source_session(path, format)?;
    let mut reader = SourceReader::new(path.to_path_buf(), format, session.clone());
    let bounded_bytes = if is_compressed_source_path(path) {
        MAX_DECODED_BYTES
    } else {
        bytes
    };
    let max_steps = usize::try_from(bounded_bytes / MAX_BATCH_BYTES as u64)
        .unwrap_or(usize::MAX)
        .saturating_add(4);
    for _ in 0..max_steps {
        let step = reader.step().map_err(|_| ())?;
        match step.progress {
            SourceProgress::Pending => continue,
            SourceProgress::Published { .. } | SourceProgress::Unchanged => {
                let snapshot = reader.last_good().ok_or(())?;
                return if snapshot.has_supported_usage && !snapshot.incomplete_tail {
                    Ok(session)
                } else {
                    Err(())
                };
            }
        }
    }
    Err(())
}

/// A CatPaw account database name: `catpaw-memory-<scope>.db`, where the scope
/// is non-empty and limited to ASCII letters, digits, `_` and `-`, and is not
/// the exact scope `anon`.
fn catpaw_database_scope(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("catpaw-memory-") else {
        return false;
    };
    let Some(scope) = rest.strip_suffix(".db") else {
        return false;
    };
    if scope.is_empty() || scope == "anon" {
        return false;
    }
    scope
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn source_session(path: &Path, format: SourceFormat) -> Result<String, ()> {
    if format == SourceFormat::CodebuffJson || format == SourceFormat::FreebuffJson {
        let session = crate::parsers::codebuff::session_from_path(path);
        return Ok(session);
    }
    if matches!(
        format,
        SourceFormat::RooCodeJson | SourceFormat::ClineTaskJson | SourceFormat::KiloTaskJson
    ) {
        // `ui_messages.json` is a bare array with no task identity inside, so
        // the session is the task directory it sits in — `<tasks>/<taskId>/`.
        return path
            .parent()
            .and_then(|directory| directory.file_name())
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty() && !name.chars().any(char::is_control))
            .map(str::to_owned)
            .ok_or(());
    }
    if format == SourceFormat::CopilotChatSessionJsonl {
        // `<sessionId>.jsonl`: the session id is the file stem, and the
        // workspace hash above it is not part of the session identity.
        return path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .filter(|stem| !stem.is_empty() && !stem.chars().any(char::is_control))
            .map(str::to_owned)
            .ok_or(());
    }
    if matches!(
        format,
        SourceFormat::CodeBuddyJsonl | SourceFormat::LmstudioLog
    ) && path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("log"))
    {
        return path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .filter(|stem| !stem.is_empty() && !stem.chars().any(char::is_control))
            .map(str::to_owned)
            .ok_or(());
    }
    let mut input = SourceInput::for_path(format, path.to_path_buf());
    let max_steps = if matches!(
        format,
        SourceFormat::CodexCli | SourceFormat::McodeJsonl | SourceFormat::GeminiTranscript
    ) || is_compressed_source_path(path)
        || (format == SourceFormat::GeminiTranscript
            && path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl")))
    {
        4
    } else {
        1
    };
    for _ in 0..max_steps {
        let batch = input.read_batch().map_err(|_| ())?;
        for record in batch.records {
            let value: Value = serde_json::from_slice(&record).map_err(|_| ())?;
            let session = match format {
                // Resolved from the path before this point.
                SourceFormat::RooCodeJson
                | SourceFormat::ClineTaskJson
                | SourceFormat::KiloTaskJson
                | SourceFormat::CopilotChatSessionJsonl => None,
                SourceFormat::BbSqlite => None,
                SourceFormat::CodebuffJson => None,
                SourceFormat::FreebuffJson => None,
                SourceFormat::ClaudeCli => value.get("sessionId").and_then(Value::as_str),
                SourceFormat::CodexCli => {
                    if value.get("type").and_then(Value::as_str) == Some("session_meta") {
                        value
                            .get("payload")
                            .and_then(|payload| payload.get("id"))
                            .and_then(Value::as_str)
                    } else {
                        None
                    }
                }
                SourceFormat::GeminiTranscript => value
                    .get("sessionId")
                    .or_else(|| value.get("session_id"))
                    .and_then(Value::as_str),
                SourceFormat::ClineJson => value.get("sessionId").and_then(Value::as_str),
                SourceFormat::AmpJson => value.get("id").and_then(Value::as_str),
                SourceFormat::CopilotJsonl => value
                    .get("attributes")
                    .and_then(Value::as_object)
                    .and_then(|attributes| {
                        [
                            "gen_ai.conversation.id",
                            "copilot_chat.session_id",
                            "copilot_chat.chat_session_id",
                            "session.id",
                            "github.copilot.interaction_id",
                        ]
                        .iter()
                        .find_map(|key| attributes.get(*key).and_then(Value::as_str))
                    })
                    .or_else(|| value.get("traceId").and_then(Value::as_str)),
                SourceFormat::CodeBuddyJsonl | SourceFormat::WorkBuddyJsonl => {
                    value.get("sessionId").and_then(Value::as_str)
                }
                SourceFormat::PiJsonl
                | SourceFormat::OmpJsonl
                | SourceFormat::SenpiJsonl
                | SourceFormat::KimchiJsonl
                | SourceFormat::CommandCodeJsonl
                | SourceFormat::PrimeAgentJsonl => {
                    if value.get("type").and_then(Value::as_str) == Some("session") {
                        value.get("id").and_then(Value::as_str)
                    } else {
                        None
                    }
                }
                SourceFormat::AntigravityJsonl | SourceFormat::QwenJsonl => {
                    value.get("sessionId").and_then(Value::as_str)
                }
                SourceFormat::MuseJsonl => value
                    .pointer("/stream/id")
                    .and_then(Value::as_str)
                    .or_else(|| value.get("sessionId").and_then(Value::as_str)),
                SourceFormat::McodeJsonl => {
                    value.get("sessionId").and_then(Value::as_str).or_else(|| {
                        value
                            .get("message")
                            .and_then(|message| message.get("sessionId"))
                            .and_then(Value::as_str)
                    })
                }
                SourceFormat::ReasonixJsonl => None,
                // Mux is handled above: its session is the workspace directory.
                SourceFormat::MuxJson => None,
                // Junie is handled above: its session is the containing directory.
                SourceFormat::JunieJsonl => None,
                // Augment is handled above: its session is the file stem.
                SourceFormat::AugmentJson => None,
                // trae is handled above.
                SourceFormat::TraeJson => None,
                // opencodereview is handled above.
                SourceFormat::OpenCodeReviewJsonl => None,
                // gjc is handled above.
                SourceFormat::GjcJsonl => None,
                // OpenClaw's session is the transcript's file stem, which the
                // caller already knows; the event stream carries no session id.
                SourceFormat::OpenClawJsonl => None,
                // Droid's session is the `<uuid>` stem of
                // `<uuid>.settings.json`; the settings document names none.
                SourceFormat::DroidJson => None,
                // Kiro's session document names its own session_id; the caller
                // derives one from the <session>.json stem when absent.
                SourceFormat::KiroJson => None,
                // Proma names no session in its SDK records beyond an optional
                // session_id; the caller derives one from the file stem.
                SourceFormat::PromaJsonl => None,
                // CatPaw session ids come from the database row, not the path.
                SourceFormat::CatpawSqlite => None,
                // LM Studio has no session concept: the log file is the unit.
                SourceFormat::LmstudioLog => None,
                // Cherry Studio's ledger names its session inside the record.
                SourceFormat::CherrystudioSqlite => None,
                // Unsloth names its session inside the ledger row.
                SourceFormat::UnslothSqlite => None,
                // Devin names its session inside the ledger row.
                SourceFormat::DevinSqlite => None,
                // fx names its session in the snapshot, else the directory.
                SourceFormat::FxUsageJson => None,
                // Vibe names its session inside the metadata document.
                SourceFormat::VibeJson => None,
                // Qoder CN names its session inside the record; the caller
                // derives one from the <slug>/<session>.jsonl path when absent.
                SourceFormat::QodercnJsonl => None,
                SourceFormat::QodercnSqlite => None,
                // Jcode names its session inside the snapshot.
                SourceFormat::JcodeJson => None,
                // DSH's transcript directory is named after its session id.
                SourceFormat::DshJsonl => None,
                // Kimi's wire stream carries no session id; the caller derives
                // it from the `<GROUP>/<SESSION>/wire.jsonl` path.
                SourceFormat::KimiJsonl => None,
                // Grok's update stream names no session; the caller derives it
                // from the `<workspace>/<session-id>/updates.jsonl` path.
                SourceFormat::GrokJsonl => None,
                // ZCode's transcript carries an optional sessionId; the caller
                // derives one from the <slug>/<session>.jsonl path when absent.
                SourceFormat::ZcodeJsonl => None,
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
                | SourceFormat::AntigravitySqlite
                | SourceFormat::CursorCacheJson => None,
            };
            if let Some(session) = session.filter(|session| !session.is_empty()) {
                return Ok(session.to_owned());
            }
        }
        if !batch.more_available {
            break;
        }
    }
    if format == SourceFormat::CursorCacheJson {
        return Ok("cursor".to_owned());
    }
    if format == SourceFormat::ClineJson {
        return path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map(|stem| stem.strip_suffix(".messages").unwrap_or(stem))
            .filter(|stem| !stem.is_empty() && !stem.chars().any(char::is_control))
            .map(str::to_owned)
            .ok_or(());
    }
    if format == SourceFormat::AmpJson {
        return path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .filter(|stem| !stem.is_empty() && !stem.chars().any(char::is_control))
            .map(str::to_owned)
            .ok_or(());
    }
    if format == SourceFormat::MuseJsonl {
        return path
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty() && !name.chars().any(char::is_control))
            .map(str::to_owned)
            .ok_or(());
    }
    // `<uuid>.settings.json` holds the session's cumulative usage, so the
    // session id keeps the stem before `.settings`.
    if format == SourceFormat::DroidJson {
        return path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|stem| stem.strip_suffix(".settings"))
            .filter(|stem| !stem.is_empty() && !stem.chars().any(char::is_control))
            .map(str::to_owned)
            .ok_or(());
    }
    // `<workspace>/<session-id>/updates.jsonl`: the session is the containing
    // directory, since the update stream names none.
    if format == SourceFormat::GrokJsonl {
        return path
            .parent()
            .filter(|parent| parent.file_name().and_then(|name| name.to_str()) != Some("sessions"))
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty() && !name.chars().any(char::is_control))
            .map(str::to_owned)
            .ok_or(());
    }
    // `~/.vibe/logs/session/session_*/meta.json`: the session is the containing
    // directory. A flat legacy `session_*.json` names it by stem instead.
    if format == SourceFormat::VibeJson {
        return path
            .parent()
            .filter(|parent| parent.file_name().and_then(|name| name.to_str()) != Some("session"))
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty() && !name.chars().any(char::is_control))
            .map(str::to_owned)
            .or_else(|| {
                path.file_stem()
                    .and_then(|stem| stem.to_str())
                    .filter(|stem| !stem.is_empty() && !stem.chars().any(char::is_control))
                    .map(str::to_owned)
            })
            .ok_or(());
    }
    if format == SourceFormat::GjcJsonl {
        // `gajae-code JSONL sessions at `~/.gjc/agent/sessions`, including depth-2 per-pass children.`: the record names its session; the stem is the fallback.
        return path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .filter(|stem| !stem.is_empty() && !stem.chars().any(char::is_control))
            .map(str::to_owned)
            .ok_or(());
    }
    if format == SourceFormat::OpenCodeReviewJsonl {
        // `OpenCodeReview JSONL sessions at `~/.opencodereview/sessions/<encoded-repo>/<session>.jsonl`.`: the file stem names the session.
        return path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .filter(|stem| !stem.is_empty() && !stem.chars().any(char::is_control))
            .map(str::to_owned)
            .ok_or(());
    }
    if format == SourceFormat::TraeJson {
        // `Trae usage-API cache dumps at `~/.local/share/trae-cache/sessions/*.json`.`: the record names its session; the stem is the fallback.
        return path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .filter(|stem| !stem.is_empty() && !stem.chars().any(char::is_control))
            .map(str::to_owned)
            .ok_or(());
    }
    // `~/.augment/sessions/<sessionId>.json`: the snapshot names its session,
    // so the caller falls back to the file stem when it is blank.
    if format == SourceFormat::AugmentJson {
        return path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .filter(|stem| !stem.is_empty() && !stem.chars().any(char::is_control))
            .map(str::to_owned)
            .ok_or(());
    }
    // `~/.junie/sessions/<session-id>/events.jsonl`: the session is the
    // containing directory, since the file itself is always `events.jsonl`.
    if format == SourceFormat::JunieJsonl {
        return path
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty() && !name.chars().any(char::is_control))
            .map(str::to_owned)
            .ok_or(());
    }
    // `~/.mux/sessions/<workspaceId>/session-usage.json`: the session is the
    // workspace, which is the containing directory.
    if format == SourceFormat::MuxJson {
        return path
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty() && !name.chars().any(char::is_control))
            .map(str::to_owned)
            .ok_or(());
    }
    // `~/.jcode/sessions/session_<stamp>_<id>.json`: the snapshot names its
    // session, so the caller falls back to the file stem when it is blank.
    if format == SourceFormat::JcodeJson {
        return path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .filter(|stem| !stem.is_empty() && !stem.chars().any(char::is_control))
            .map(str::to_owned)
            .ok_or(());
    }
    // `~/.fx/sessions/<session-id>/usage-v2.json`: the session is the containing
    // directory, because the file itself is always `usage-v2.json`.
    if format == SourceFormat::FxUsageJson {
        return path
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty() && !name.chars().any(char::is_control))
            .map(str::to_owned)
            .ok_or(());
    }
    // `<encoded-cwd>/<session-id>/session.jsonl[.zstd]`: the session is the
    // containing directory, since the file itself is always `session.jsonl`.
    if format == SourceFormat::DshJsonl {
        return path
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty() && !name.chars().any(char::is_control))
            .map(str::to_owned)
            .ok_or(());
    }
    // `<GROUP>/<SESSION>/wire.jsonl`: the session is the containing directory,
    // since the wire itself names none.
    if format == SourceFormat::KimiJsonl {
        return path
            .parent()
            .filter(|parent| parent.file_name().and_then(|name| name.to_str()) != Some("sessions"))
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty() && !name.chars().any(char::is_control))
            .map(str::to_owned)
            .ok_or(());
    }
    if matches!(
        format,
        SourceFormat::CopilotJsonl
            | SourceFormat::GeminiTranscript
            | SourceFormat::CodeBuddyJsonl
            | SourceFormat::WorkBuddyJsonl
            | SourceFormat::PiJsonl
            | SourceFormat::OmpJsonl
            | SourceFormat::SenpiJsonl
            | SourceFormat::AntigravityJsonl
            | SourceFormat::QwenJsonl
            | SourceFormat::ReasonixJsonl
            | SourceFormat::McodeJsonl
            | SourceFormat::OpenClawJsonl
            | SourceFormat::CommandCodeJsonl
            | SourceFormat::ZcodeJsonl
            | SourceFormat::KiroJson
            | SourceFormat::PromaJsonl
            | SourceFormat::QodercnJsonl
    ) {
        return path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .filter(|stem| !stem.is_empty() && !stem.chars().any(char::is_control))
            .map(str::to_owned)
            .ok_or(());
    }
    Err(())
}

fn redacted_client_id(client: &str) -> String {
    if client.len() <= 40
        && client
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        client.to_owned()
    } else {
        "invalid".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_COLLISION_TREE: AtomicUsize = AtomicUsize::new(0);

    fn host(platform: HostPlatform, home: &str) -> HostPaths {
        HostPaths {
            platform,
            home: PathBuf::from(home),
            app_data: None,
            local_app_data: None,
            xdg_config_home: None,
            xdg_data_home: None,
            xdg_state_home: None,
            legacy_shared_override: None,
            native_data_override: None,
            enabled_clients: BTreeSet::new(),
            env_paths: BTreeMap::new(),
        }
    }

    #[test]
    #[cfg(unix)]
    fn opencode_data_dir_uses_xdg_then_home_on_unix_hosts() {
        assert_eq!(
            opencode_data_dir(&host(HostPlatform::Linux, "/home/tester")).unwrap(),
            PathBuf::from("/home/tester/.local/share/opencode")
        );
        assert_eq!(
            opencode_data_dir(&host(HostPlatform::MacOs, "/Users/tester")).unwrap(),
            PathBuf::from("/Users/tester/.local/share/opencode")
        );
        let mut linux = host(HostPlatform::Linux, "/home/tester");
        linux.xdg_data_home = Some(PathBuf::from("/xdg-data"));
        assert_eq!(
            opencode_data_dir(&linux).unwrap(),
            PathBuf::from("/xdg-data/opencode")
        );
    }

    #[test]
    #[cfg(windows)]
    fn opencode_data_dir_uses_xdg_then_home_on_windows() {
        assert_eq!(
            opencode_data_dir(&host(HostPlatform::Windows, r"C:\Users\tester")).unwrap(),
            PathBuf::from(r"C:\Users\tester\.local\share\opencode")
        );
        let mut windows = host(HostPlatform::Windows, r"C:\Users\tester");
        windows.xdg_data_home = Some(PathBuf::from(r"D:\xdg-data"));
        assert_eq!(
            opencode_data_dir(&windows).unwrap(),
            PathBuf::from(r"D:\xdg-data\opencode")
        );
    }

    #[test]
    fn injected_source_id_collision_rejects_every_alias() {
        let root = env::temp_dir().join(format!(
            "token-monitor-discovery-collision-{}-{}",
            std::process::id(),
            NEXT_COLLISION_TREE.fetch_add(1, Ordering::Relaxed)
        ));
        let sources = root.join(".claude/projects/project");
        fs::create_dir_all(&sources).unwrap();
        for (name, session) in [("a.jsonl", "session-a"), ("b.jsonl", "session-b")] {
            fs::write(
                sources.join(name),
                format!(
                    r#"{{"type":"assistant","sessionId":"{session}","timestamp":"2026-11-09T10:00:00Z","message":{{"id":"message-{session}","model":"claude","usage":{{"input_tokens":1}}}}}}
"#
                ),
            )
            .unwrap();
        }
        let inputs = HostPaths {
            platform: HostPlatform::Linux,
            home: root.clone(),
            app_data: None,
            local_app_data: None,
            xdg_config_home: None,
            xdg_data_home: None,
            xdg_state_home: None,
            legacy_shared_override: Some(root.join("legacy")),
            native_data_override: Some(root.join("native")),
            enabled_clients: BTreeSet::from(["claude".to_owned()]),
            env_paths: BTreeMap::new(),
        };
        let forced = SourceId::from_bytes([9; 16]);
        let mut notices = Vec::new();
        let discovered = discover_sources_with_id(&inputs, &mut notices, |_, _| forced).unwrap();

        assert!(discovered.is_empty());
        assert_eq!(notices, vec![DiscoveryNotice::SourceIdentityCollision]);
        assert!(!format!("{notices:?}").contains(root.to_str().unwrap()));
        fs::remove_dir_all(root).unwrap();
    }
}
