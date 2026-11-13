//! Deterministic, bounded discovery of native paths and read-only legacy inputs.
//! Paths stay in backend-owned types and diagnostics never render their values.

use crate::aggregation::MAX_SOURCES;
use crate::parsers::jsonl::MAX_BATCH_BYTES;
use crate::runtime::MonitoredSource;
use crate::source_inputs::{
    CodexLocalUsageAdapter, MAX_DECODED_BYTES, SessionArchiveAdapter, SourceInput, has_zstd_suffix,
    is_compressed_source_path,
};
use crate::sources::{SourceFormat, SourceId, SourceProgress, SourceReader};
use serde_json::Value;
use std::collections::{BTreeSet, BinaryHeap};
use std::env;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

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
    pub xdg_state_home: Option<PathBuf>,
    pub legacy_shared_override: Option<PathBuf>,
    pub native_data_override: Option<PathBuf>,
    pub enabled_clients: BTreeSet<String>,
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

        Ok(Self {
            platform,
            home,
            app_data: path("APPDATA"),
            local_app_data: path("LOCALAPPDATA"),
            xdg_config_home: path("XDG_CONFIG_HOME"),
            xdg_state_home: path("XDG_STATE_HOME"),
            legacy_shared_override: path("TOKEN_MONITOR_SHARED_DIR"),
            native_data_override: path("TOKEN_MONITOR_LITE_DATA_DIR"),
            enabled_clients,
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

fn discover_sources_with_id(
    inputs: &HostPaths,
    notices: &mut Vec<DiscoveryNotice>,
    mut source_id_for: impl FnMut(SourceFormat, &Path) -> SourceId,
) -> Result<Vec<DiscoveredSource>, DiscoveryError> {
    let mut sources = Vec::new();
    let mut admitted_ids = BTreeSet::new();
    let mut rejected_ids = BTreeSet::new();
    for client in &inputs.enabled_clients {
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
    let mut input = SourceInput::for_path(format, path.to_path_buf());
    let max_steps = if is_compressed_source_path(path) {
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
                SourceFormat::SessionArchiveSqlite | SourceFormat::CodexLocalUsageSqlite => None,
            };
            if let Some(session) = session.filter(|session| !session.is_empty()) {
                return Ok(session.to_owned());
            }
        }
        if !batch.more_available {
            break;
        }
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
            xdg_state_home: None,
            legacy_shared_override: Some(root.join("legacy")),
            native_data_override: Some(root.join("native")),
            enabled_clients: BTreeSet::from(["claude".to_owned()]),
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
