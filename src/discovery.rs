//! Deterministic, bounded discovery of native paths and read-only legacy inputs.
//! Paths stay in backend-owned types and diagnostics never render their values.

use crate::aggregation::MAX_SOURCES;
use crate::parsers::jsonl::MAX_BATCH_BYTES;
use crate::runtime::MonitoredSource;
use crate::source_inputs::{
    AntigravitySqliteAdapter, CodexLocalUsageAdapter, GooseAdapter, HermesAdapter, KiloAdapter,
    MAX_DECODED_BYTES, OpenClawAdapter, OpenCodeAdapter, SessionArchiveAdapter, SourceInput,
    SqliteSourceError, WorkBuddySqliteAdapter, ZedAdapter, has_zstd_suffix,
    is_compressed_source_path, is_opencode_db_filename,
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
            let root = join_many(inputs.platform, &inputs.home, &[".openclaw", "agents"]);
            let mut candidates = Vec::new();
            let mut inspected = 0;
            let mut overflow = false;
            collect_openclaw_candidates(&root, 0, &mut inspected, &mut candidates, &mut overflow)?;
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
            "grok" => (
                "grok",
                SourceFormat::GrokJsonl,
                vec![join_many(
                    inputs.platform,
                    &inputs.home,
                    &[".grok", "sessions"],
                )],
            ),
            "kimi" => (
                "kimi",
                SourceFormat::KimiJsonl,
                vec![join_many(
                    inputs.platform,
                    &inputs.home,
                    &[".kimi", "sessions"],
                )],
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

fn source_session(path: &Path, format: SourceFormat) -> Result<String, ()> {
    if format == SourceFormat::CodeBuddyJsonl
        && path
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
                SourceFormat::PiJsonl | SourceFormat::OmpJsonl | SourceFormat::SenpiJsonl => {
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
                // OpenClaw's session is the transcript's file stem, which the
                // caller already knows; the event stream carries no session id.
                SourceFormat::OpenClawJsonl => None,
                // Droid's session is the `<uuid>` stem of
                // `<uuid>.settings.json`; the settings document names none.
                SourceFormat::DroidJson => None,
                // Kimi's wire stream carries no session id; the caller derives
                // it from the `<GROUP>/<SESSION>/wire.jsonl` path.
                SourceFormat::KimiJsonl => None,
                // Grok's update stream names no session; the caller derives it
                // from the `<workspace>/<session-id>/updates.jsonl` path.
                SourceFormat::GrokJsonl => None,
                SourceFormat::SessionArchiveSqlite
                | SourceFormat::CodexLocalUsageSqlite
                | SourceFormat::OpenCodeSqlite
                | SourceFormat::KiloSqlite
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
