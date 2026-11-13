//! Private, lossless staging of recognized legacy artifacts.
//!
//! Source paths and file contents stay behind this backend boundary. Semantic
//! readers operate only on verified private copies.

use crate::config::inspect_settings;
use crate::credentials::{CredentialImportPlan, plan_import};
use crate::discovery::{
    ArtifactKind, DiscoveredArtifact, LegacyInstallation, MAX_ARTIFACT_FILE_BYTES, NativePaths,
};
use crate::storage::{
    ActivationState, CredentialReference, ImportReportItem, ImportState, ImportedSession,
    NativeSettings, PreserveState, StagedArtifact, StoreError, ValidatedImport,
};
use rusqlite::backup::Backup;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;

pub const STAGING_DIRECTORY_NAME: &str = "migration-staging";
pub const SEMANTIC_ARCHIVE_FILE_NAME: &str = "semantic.sqlite3";
const CREDENTIAL_RECOVERY_FILE_NAME: &str = "credential-recovery.json";

const COPY_BUFFER_BYTES: usize = 64 * 1024;
const MAX_ARCHIVE_ROWS: usize = 10_000;
const MAX_ARCHIVE_ENTRY_BYTES: usize = 16 * 1024 * 1024;
static NEXT_RUN: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MigrationReason {
    InvalidRoot,
    NotRegular,
    SourceChanged,
    CopyMismatch,
    SqliteBackupFailed,
    ArchiveSchema,
    UnsupportedArchiveVersion,
    MalformedArchiveRow,
    UnresolvedArchiveIdentity,
    ConflictingArchiveIdentity,
    ArchiveRevisionMismatch,
    ArchiveCapacity,
    InvalidCredentials,
}

#[derive(Debug)]
pub struct StagedCredentialImport {
    pub import: ValidatedImport,
    pub credentials: CredentialImportPlan,
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum MigrationError {
    #[error("migration staging I/O failed")]
    Io(io::ErrorKind),
    #[error("migration validation failed for {kind:?} ({reason:?})")]
    Validation {
        kind: ArtifactKind,
        reason: MigrationReason,
    },
}

impl From<io::Error> for MigrationError {
    fn from(error: io::Error) -> Self {
        Self::Io(error.kind())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileStamp {
    bytes: u64,
    modified: Option<SystemTime>,
    identity: FileIdentity,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum FileIdentity {
    #[cfg(unix)]
    Unix { device: u64, inode: u64 },
    #[cfg(not(unix))]
    Portable { created: Option<SystemTime> },
}

pub fn stage_and_validate(
    legacy: &LegacyInstallation,
    native: &NativePaths,
) -> Result<ValidatedImport, MigrationError> {
    let run_id = next_run_id()?;
    let staging_root = native.root().join(STAGING_DIRECTORY_NAME);
    create_private_directory_all(native.root())?;
    create_private_directory_all(&staging_root)?;
    let run_root = staging_root.join(&run_id);
    fs::create_dir(&run_root)?;
    make_private_directory(&run_root)?;

    let mut staged_artifacts = Vec::new();
    let mut report_items = Vec::with_capacity(legacy.artifacts.len());
    let mut settings = None;
    let mut sessions = Vec::new();
    let mut credential_references = Vec::new();
    for artifact in &legacy.artifacts {
        let kind = artifact_kind_id(artifact.kind);
        if !artifact.eligible_for_staging {
            report_items.push(ImportReportItem {
                artifact_kind: kind.to_owned(),
                preservation: PreserveState::Failed,
                import: ImportState::NotAttempted,
                activation: ActivationState::Inactive,
                records: 0,
            });
            continue;
        }
        ensure_artifact_below_legacy_root(legacy, artifact)?;
        let artifact_directory = artifact_directory_name(artifact.kind, artifact.ordinal);
        let artifact_root = run_root.join(&artifact_directory);
        fs::create_dir_all(&artifact_root)?;
        make_private_directory(&artifact_root)?;
        let source_metadata = fs::symlink_metadata(artifact.path())?;
        let staging_id = if source_metadata.file_type().is_file() {
            let relative = format!("{artifact_directory}/{}", artifact_file_name(artifact.kind));
            let destination = run_root.join(&relative);
            if let Err(reason) = copy_verified_file(artifact.path(), &destination) {
                let _ = fs::remove_file(&destination);
                if is_semantically_supported(artifact.kind) {
                    return Err(MigrationError::Validation {
                        kind: artifact.kind,
                        reason,
                    });
                }
                report_items.push(failed_preservation(kind));
                continue;
            }
            relative
        } else if source_metadata.file_type().is_dir() {
            let relative = format!("{artifact_directory}/files");
            let destination = run_root.join(&relative);
            if let Err(reason) = copy_verified_directory(
                artifact.path(),
                &destination,
                artifact.entries,
                artifact.bytes,
            ) {
                let _ = fs::remove_dir_all(&artifact_root);
                report_items.push(failed_preservation(kind));
                if is_semantically_supported(artifact.kind) {
                    return Err(MigrationError::Validation {
                        kind: artifact.kind,
                        reason,
                    });
                }
                continue;
            }
            relative
        } else {
            let _ = fs::remove_dir_all(&artifact_root);
            report_items.push(failed_preservation(kind));
            continue;
        };

        staged_artifacts.push(StagedArtifact {
            staging_id,
            artifact_kind: kind.to_owned(),
            bytes: artifact.bytes,
        });
        let mut report = ImportReportItem {
            artifact_kind: kind.to_owned(),
            preservation: PreserveState::Preserved,
            import: if is_semantically_supported(artifact.kind) {
                ImportState::NotAttempted
            } else {
                ImportState::Unsupported
            },
            activation: ActivationState::Inactive,
            records: artifact.entries as u64,
        };
        match artifact.kind {
            ArtifactKind::Settings => {
                let staged =
                    run_root.join(&staged_artifacts.last().expect("staged artifact").staging_id);
                match inspect_settings(&staged) {
                    Ok(inspection) => {
                        let fields = inspection.preferences().recognized_credential_fields();
                        if fields.is_empty() {
                            match NativeSettings::from_json_bytes(inspection.original_bytes()) {
                                Ok(imported) => {
                                    settings = Some(imported);
                                    report.import = ImportState::Imported;
                                    report.activation = ActivationState::Activated;
                                    report.records = 1;
                                }
                                Err(StoreError::SecretRejected) => {
                                    report.import = ImportState::NotAttempted;
                                    report.records = 1;
                                }
                                Err(_) => report.import = ImportState::Failed,
                            }
                        } else {
                            for (index, field) in fields.into_iter().enumerate() {
                                credential_references.push(CredentialReference {
                                    credential_id: format!("{}-settings-{index:03}", run_id),
                                    credential_kind: field.to_owned(),
                                });
                            }
                            report.import = ImportState::NotAttempted;
                            report.records = 1;
                        }
                    }
                    Err(_) => report.import = ImportState::Failed,
                }
            }
            ArtifactKind::SessionUsageArchive => {
                let semantic = artifact_root.join(SEMANTIC_ARCHIVE_FILE_NAME);
                create_semantic_archive(artifact.path(), &semantic)?;
                let imported = import_session_archive(&semantic)?;
                report.records = imported.len() as u64;
                report.import = ImportState::Imported;
                report.activation = ActivationState::Activated;
                sessions.extend(imported);
            }
            _ => {}
        }
        report_items.push(report);
    }

    sync_directory(&run_root)?;
    Ok(ValidatedImport {
        run_id,
        expected_settings_revision: 0,
        settings,
        sessions,
        report_items,
        staged_artifacts,
        credential_references,
        credential_operations: Vec::new(),
    })
}

pub fn prepare_staged_credentials(
    mut import: ValidatedImport,
    native: &NativePaths,
) -> Result<StagedCredentialImport, MigrationError> {
    let settings = read_staged_document(&import, native, "settings")?;
    let credentials = read_staged_document(&import, native, "credentials")?;
    let plan = plan_import(credentials.as_deref(), settings.as_deref())
        .map_err(|_| credential_validation_error())?;

    import.credential_references = plan
        .transfers
        .iter()
        .map(|transfer| CredentialReference {
            credential_id: transfer.credential_id.clone(),
            credential_kind: transfer.kind.as_str().to_owned(),
        })
        .collect();
    import.credential_operations.clear();
    if let Some(stripped) = &plan.stripped_settings {
        import.settings = Some(
            NativeSettings::from_json_bytes(stripped).map_err(|_| credential_validation_error())?,
        );
        if let Some(report) = import
            .report_items
            .iter_mut()
            .find(|item| item.artifact_kind == "settings")
        {
            report.import = ImportState::Imported;
            report.activation = ActivationState::Activated;
            report.records = 1;
        }
    }
    if !plan.transfers.is_empty()
        && let Some(report) = import
            .report_items
            .iter_mut()
            .find(|item| item.artifact_kind == "credentials")
    {
        report.import = ImportState::Imported;
        report.activation = ActivationState::Activated;
        report.records = plan.transfers.len() as u64;
    }
    if !plan.issues.is_empty() {
        import.report_items.push(ImportReportItem {
            artifact_kind: "credential-unknown".to_owned(),
            preservation: PreserveState::Preserved,
            import: ImportState::Unsupported,
            activation: ActivationState::Inactive,
            records: plan.issues.len() as u64,
        });
    }
    write_recovery_manifest(native, &import)?;
    Ok(StagedCredentialImport {
        import,
        credentials: plan,
    })
}

pub fn recover_staged_credentials(
    native: &NativePaths,
    run_id: &str,
) -> Result<StagedCredentialImport, MigrationError> {
    if run_id.is_empty()
        || run_id.len() > 128
        || !run_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(credential_validation_error());
    }
    let run_root = native.root().join(STAGING_DIRECTORY_NAME).join(run_id);
    let bytes = read_bounded_file(
        &run_root.join(CREDENTIAL_RECOVERY_FILE_NAME),
        MAX_ARTIFACT_FILE_BYTES,
    )?;
    let manifest: RecoveryManifest =
        serde_json::from_slice(&bytes).map_err(|_| credential_validation_error())?;
    let import = manifest.into_import()?;
    if import.run_id != run_id {
        return Err(credential_validation_error());
    }
    let settings = read_staged_document(&import, native, "settings")?;
    let credentials = read_staged_document(&import, native, "credentials")?;
    let plan = plan_import(credentials.as_deref(), settings.as_deref())
        .map_err(|_| credential_validation_error())?;
    let planned_ids = plan
        .transfers
        .iter()
        .map(|transfer| (&transfer.credential_id, transfer.kind.as_str()))
        .collect::<Vec<_>>();
    let manifest_ids = import
        .credential_references
        .iter()
        .map(|reference| (&reference.credential_id, reference.credential_kind.as_str()))
        .collect::<Vec<_>>();
    if planned_ids != manifest_ids {
        return Err(credential_validation_error());
    }
    Ok(StagedCredentialImport {
        import,
        credentials: plan,
    })
}

fn read_staged_document(
    import: &ValidatedImport,
    native: &NativePaths,
    artifact_kind: &str,
) -> Result<Option<Vec<u8>>, MigrationError> {
    let Some(artifact) = import
        .staged_artifacts
        .iter()
        .find(|artifact| artifact.artifact_kind == artifact_kind)
    else {
        return Ok(None);
    };
    let run_root = native
        .root()
        .join(STAGING_DIRECTORY_NAME)
        .join(&import.run_id);
    let path = run_root.join(&artifact.staging_id);
    if !path.starts_with(&run_root) {
        return Err(credential_validation_error());
    }
    read_bounded_file(&path, MAX_ARTIFACT_FILE_BYTES).map(Some)
}

fn read_bounded_file(path: &Path, maximum: u64) -> Result<Vec<u8>, MigrationError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.len() > maximum {
        return Err(credential_validation_error());
    }
    let mut file = File::open(path)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(&mut file)
        .take(maximum + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > maximum {
        return Err(credential_validation_error());
    }
    Ok(bytes)
}

fn credential_validation_error() -> MigrationError {
    MigrationError::Validation {
        kind: ArtifactKind::Credentials,
        reason: MigrationReason::InvalidCredentials,
    }
}

#[derive(Serialize, Deserialize)]
struct RecoveryManifest {
    run_id: String,
    expected_settings_revision: u64,
    settings: Option<Vec<u8>>,
    sessions: Vec<RecoverySession>,
    report_items: Vec<RecoveryReportItem>,
    staged_artifacts: Vec<StagedArtifactManifest>,
    credential_references: Vec<CredentialReferenceManifest>,
}

#[derive(Serialize, Deserialize)]
struct RecoverySession {
    client: String,
    session: String,
    original_entry_json: Vec<u8>,
    legacy_key: String,
    source_revision: u64,
    archive_metadata_json: Option<Vec<u8>>,
}

#[derive(Serialize, Deserialize)]
struct RecoveryReportItem {
    artifact_kind: String,
    preservation: String,
    import: String,
    activation: String,
    records: u64,
}

#[derive(Serialize, Deserialize)]
struct StagedArtifactManifest {
    staging_id: String,
    artifact_kind: String,
    bytes: u64,
}

#[derive(Serialize, Deserialize)]
struct CredentialReferenceManifest {
    credential_id: String,
    credential_kind: String,
}

impl RecoveryManifest {
    fn from_import(import: &ValidatedImport) -> Self {
        Self {
            run_id: import.run_id.clone(),
            expected_settings_revision: import.expected_settings_revision,
            settings: import
                .settings
                .as_ref()
                .map(|settings| settings.json_bytes().to_vec()),
            sessions: import
                .sessions
                .iter()
                .map(|session| RecoverySession {
                    client: session.client.clone(),
                    session: session.session.clone(),
                    original_entry_json: session.original_entry_json.clone(),
                    legacy_key: session.legacy_key.clone(),
                    source_revision: session.source_revision,
                    archive_metadata_json: session.archive_metadata_json.clone(),
                })
                .collect(),
            report_items: import
                .report_items
                .iter()
                .map(|item| RecoveryReportItem {
                    artifact_kind: item.artifact_kind.clone(),
                    preservation: preservation_id(item.preservation).to_owned(),
                    import: import_id(item.import).to_owned(),
                    activation: activation_id(item.activation).to_owned(),
                    records: item.records,
                })
                .collect(),
            staged_artifacts: import
                .staged_artifacts
                .iter()
                .map(|artifact| StagedArtifactManifest {
                    staging_id: artifact.staging_id.clone(),
                    artifact_kind: artifact.artifact_kind.clone(),
                    bytes: artifact.bytes,
                })
                .collect(),
            credential_references: import
                .credential_references
                .iter()
                .map(|reference| CredentialReferenceManifest {
                    credential_id: reference.credential_id.clone(),
                    credential_kind: reference.credential_kind.clone(),
                })
                .collect(),
        }
    }

    fn into_import(self) -> Result<ValidatedImport, MigrationError> {
        Ok(ValidatedImport {
            run_id: self.run_id,
            expected_settings_revision: self.expected_settings_revision,
            settings: self
                .settings
                .map(|bytes| NativeSettings::from_json_bytes(&bytes))
                .transpose()
                .map_err(|_| credential_validation_error())?,
            sessions: self
                .sessions
                .into_iter()
                .map(|session| ImportedSession {
                    client: session.client,
                    session: session.session,
                    original_entry_json: session.original_entry_json,
                    legacy_key: session.legacy_key,
                    source_revision: session.source_revision,
                    archive_metadata_json: session.archive_metadata_json,
                })
                .collect(),
            report_items: self
                .report_items
                .into_iter()
                .map(|item| {
                    Ok(ImportReportItem {
                        artifact_kind: item.artifact_kind,
                        preservation: parse_preservation(&item.preservation)?,
                        import: parse_import(&item.import)?,
                        activation: parse_activation(&item.activation)?,
                        records: item.records,
                    })
                })
                .collect::<Result<_, MigrationError>>()?,
            staged_artifacts: self
                .staged_artifacts
                .into_iter()
                .map(|artifact| StagedArtifact {
                    staging_id: artifact.staging_id,
                    artifact_kind: artifact.artifact_kind,
                    bytes: artifact.bytes,
                })
                .collect(),
            credential_references: self
                .credential_references
                .into_iter()
                .map(|reference| CredentialReference {
                    credential_id: reference.credential_id,
                    credential_kind: reference.credential_kind,
                })
                .collect(),
            credential_operations: Vec::new(),
        })
    }
}

fn write_recovery_manifest(
    native: &NativePaths,
    import: &ValidatedImport,
) -> Result<(), MigrationError> {
    let run_root = native
        .root()
        .join(STAGING_DIRECTORY_NAME)
        .join(&import.run_id);
    let temporary = run_root.join(format!(".{CREDENTIAL_RECOVERY_FILE_NAME}.tmp"));
    let destination = run_root.join(CREDENTIAL_RECOVERY_FILE_NAME);
    let bytes = serde_json::to_vec(&RecoveryManifest::from_import(import))
        .map_err(|_| credential_validation_error())?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    file.write_all(&bytes)?;
    file.flush()?;
    file.sync_all()?;
    fs::rename(&temporary, &destination)?;
    sync_directory(&run_root)?;
    Ok(())
}

fn preservation_id(value: PreserveState) -> &'static str {
    match value {
        PreserveState::Preserved => "preserved",
        PreserveState::NotPresent => "not_present",
        PreserveState::Failed => "failed",
    }
}

fn import_id(value: ImportState) -> &'static str {
    match value {
        ImportState::Imported => "imported",
        ImportState::Unsupported => "unsupported",
        ImportState::Excluded => "excluded",
        ImportState::NotAttempted => "not_attempted",
        ImportState::Failed => "failed",
    }
}

fn activation_id(value: ActivationState) -> &'static str {
    match value {
        ActivationState::Activated => "active",
        ActivationState::Inactive => "inactive",
        ActivationState::Failed => "failed",
        ActivationState::NotApplicable => "not_applicable",
    }
}

fn parse_preservation(value: &str) -> Result<PreserveState, MigrationError> {
    match value {
        "preserved" => Ok(PreserveState::Preserved),
        "not_present" => Ok(PreserveState::NotPresent),
        "failed" => Ok(PreserveState::Failed),
        _ => Err(credential_validation_error()),
    }
}

fn parse_import(value: &str) -> Result<ImportState, MigrationError> {
    match value {
        "imported" => Ok(ImportState::Imported),
        "unsupported" => Ok(ImportState::Unsupported),
        "excluded" => Ok(ImportState::Excluded),
        "not_attempted" => Ok(ImportState::NotAttempted),
        "failed" => Ok(ImportState::Failed),
        _ => Err(credential_validation_error()),
    }
}

fn parse_activation(value: &str) -> Result<ActivationState, MigrationError> {
    match value {
        "active" => Ok(ActivationState::Activated),
        "inactive" => Ok(ActivationState::Inactive),
        "failed" => Ok(ActivationState::Failed),
        "not_applicable" => Ok(ActivationState::NotApplicable),
        _ => Err(credential_validation_error()),
    }
}

fn failed_preservation(kind: &str) -> ImportReportItem {
    ImportReportItem {
        artifact_kind: kind.to_owned(),
        preservation: PreserveState::Failed,
        import: ImportState::NotAttempted,
        activation: ActivationState::Inactive,
        records: 0,
    }
}

fn ensure_artifact_below_legacy_root(
    legacy: &LegacyInstallation,
    artifact: &DiscoveredArtifact,
) -> Result<(), MigrationError> {
    let root = if artifact.path().starts_with(legacy.user_data()) {
        legacy.user_data()
    } else {
        legacy.shared_data()
    };
    let relative = artifact
        .path()
        .strip_prefix(root)
        .map_err(|_| MigrationError::Validation {
            kind: artifact.kind,
            reason: MigrationReason::InvalidRoot,
        })?;
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::RootDir))
    {
        return Err(MigrationError::Validation {
            kind: artifact.kind,
            reason: MigrationReason::InvalidRoot,
        });
    }
    let canonical_root = fs::canonicalize(root).map_err(|_| MigrationError::Validation {
        kind: artifact.kind,
        reason: MigrationReason::InvalidRoot,
    })?;
    let canonical_artifact =
        fs::canonicalize(artifact.path()).map_err(|_| MigrationError::Validation {
            kind: artifact.kind,
            reason: MigrationReason::InvalidRoot,
        })?;
    if !canonical_artifact.starts_with(canonical_root) {
        return Err(MigrationError::Validation {
            kind: artifact.kind,
            reason: MigrationReason::InvalidRoot,
        });
    }
    Ok(())
}

#[derive(Default)]
struct CopyStats {
    entries: usize,
    bytes: u64,
}

fn copy_verified_directory(
    source: &Path,
    destination: &Path,
    expected_entries: usize,
    expected_bytes: u64,
) -> Result<(), MigrationReason> {
    fs::create_dir(destination).map_err(|_| MigrationReason::SourceChanged)?;
    make_private_directory(destination).map_err(|_| MigrationReason::SourceChanged)?;
    let mut stats = CopyStats {
        entries: 1,
        bytes: 0,
    };
    copy_directory_children(
        source,
        destination,
        0,
        expected_entries,
        expected_bytes,
        &mut stats,
    )?;
    if stats.entries != expected_entries || stats.bytes != expected_bytes {
        return Err(MigrationReason::SourceChanged);
    }
    compare_directory_children(source, destination, 0)
}

fn copy_directory_children(
    source: &Path,
    destination: &Path,
    depth: usize,
    expected_entries: usize,
    expected_bytes: u64,
    stats: &mut CopyStats,
) -> Result<(), MigrationReason> {
    if depth > crate::discovery::MAX_ARTIFACT_DEPTH {
        return Err(MigrationReason::NotRegular);
    }
    let mut children = fs::read_dir(source)
        .map_err(|_| MigrationReason::SourceChanged)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| MigrationReason::SourceChanged)?;
    children.sort_by_key(std::fs::DirEntry::file_name);
    for child in children {
        stats.entries = stats
            .entries
            .checked_add(1)
            .ok_or(MigrationReason::SourceChanged)?;
        if stats.entries > expected_entries {
            return Err(MigrationReason::SourceChanged);
        }
        let source_path = child.path();
        let metadata =
            fs::symlink_metadata(&source_path).map_err(|_| MigrationReason::SourceChanged)?;
        let destination_path = destination.join(child.file_name());
        if metadata.file_type().is_file() {
            stats.bytes = stats
                .bytes
                .checked_add(metadata.len())
                .ok_or(MigrationReason::SourceChanged)?;
            if stats.bytes > expected_bytes {
                return Err(MigrationReason::SourceChanged);
            }
            copy_verified_file(&source_path, &destination_path)?;
        } else if metadata.file_type().is_dir() {
            fs::create_dir(&destination_path).map_err(|_| MigrationReason::CopyMismatch)?;
            make_private_directory(&destination_path).map_err(|_| MigrationReason::CopyMismatch)?;
            copy_directory_children(
                &source_path,
                &destination_path,
                depth + 1,
                expected_entries,
                expected_bytes,
                stats,
            )?;
        } else {
            return Err(MigrationReason::NotRegular);
        }
    }
    Ok(())
}

fn compare_directory_children(
    source: &Path,
    destination: &Path,
    depth: usize,
) -> Result<(), MigrationReason> {
    if depth > crate::discovery::MAX_ARTIFACT_DEPTH {
        return Err(MigrationReason::NotRegular);
    }
    let mut source_children = fs::read_dir(source)
        .map_err(|_| MigrationReason::SourceChanged)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| MigrationReason::SourceChanged)?;
    let mut destination_children = fs::read_dir(destination)
        .map_err(|_| MigrationReason::CopyMismatch)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| MigrationReason::CopyMismatch)?;
    source_children.sort_by_key(std::fs::DirEntry::file_name);
    destination_children.sort_by_key(std::fs::DirEntry::file_name);
    if source_children.len() != destination_children.len() {
        return Err(MigrationReason::SourceChanged);
    }
    for (source_child, destination_child) in source_children.iter().zip(destination_children) {
        if source_child.file_name() != destination_child.file_name() {
            return Err(MigrationReason::SourceChanged);
        }
        let source_metadata = fs::symlink_metadata(source_child.path())
            .map_err(|_| MigrationReason::SourceChanged)?;
        let destination_metadata = fs::symlink_metadata(destination_child.path())
            .map_err(|_| MigrationReason::CopyMismatch)?;
        if source_metadata.file_type().is_file() && destination_metadata.file_type().is_file() {
            compare_files(&source_child.path(), &destination_child.path())?;
        } else if source_metadata.file_type().is_dir() && destination_metadata.file_type().is_dir()
        {
            compare_directory_children(&source_child.path(), &destination_child.path(), depth + 1)?;
        } else {
            return Err(MigrationReason::NotRegular);
        }
    }
    Ok(())
}

fn copy_verified_file(source: &Path, destination: &Path) -> Result<(), MigrationReason> {
    let before = file_stamp(source)?;
    if before.bytes > MAX_ARTIFACT_FILE_BYTES {
        return Err(MigrationReason::SourceChanged);
    }
    let mut source_file = File::open(source).map_err(|_| MigrationReason::SourceChanged)?;
    let opened = stamp_from_metadata(
        &source_file
            .metadata()
            .map_err(|_| MigrationReason::SourceChanged)?,
    );
    if opened != before {
        return Err(MigrationReason::SourceChanged);
    }
    let mut destination_file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)
        .map_err(|_| MigrationReason::CopyMismatch)?;
    make_private_file(&destination_file).map_err(|_| MigrationReason::CopyMismatch)?;
    let mut buffer = [0_u8; COPY_BUFFER_BYTES];
    let mut copied = 0_u64;
    loop {
        let read = source_file
            .read(&mut buffer)
            .map_err(|_| MigrationReason::SourceChanged)?;
        if read == 0 {
            break;
        }
        copied = copied
            .checked_add(read as u64)
            .ok_or(MigrationReason::SourceChanged)?;
        if copied > before.bytes || copied > MAX_ARTIFACT_FILE_BYTES {
            return Err(MigrationReason::SourceChanged);
        }
        destination_file
            .write_all(&buffer[..read])
            .map_err(|_| MigrationReason::CopyMismatch)?;
    }
    destination_file
        .flush()
        .map_err(|_| MigrationReason::CopyMismatch)?;
    destination_file
        .sync_all()
        .map_err(|_| MigrationReason::CopyMismatch)?;
    if copied != before.bytes || file_stamp(source)? != before {
        return Err(MigrationReason::SourceChanged);
    }
    compare_files(source, destination)
}

fn compare_files(source: &Path, destination: &Path) -> Result<(), MigrationReason> {
    let mut source = File::open(source).map_err(|_| MigrationReason::SourceChanged)?;
    let mut destination = File::open(destination).map_err(|_| MigrationReason::CopyMismatch)?;
    let mut left = [0_u8; COPY_BUFFER_BYTES];
    let mut right = [0_u8; COPY_BUFFER_BYTES];
    loop {
        let left_count = source
            .read(&mut left)
            .map_err(|_| MigrationReason::SourceChanged)?;
        let right_count = destination
            .read(&mut right)
            .map_err(|_| MigrationReason::CopyMismatch)?;
        if left_count != right_count || left[..left_count] != right[..right_count] {
            return Err(MigrationReason::CopyMismatch);
        }
        if left_count == 0 {
            return Ok(());
        }
    }
}

fn file_stamp(path: &Path) -> Result<FileStamp, MigrationReason> {
    let metadata = fs::symlink_metadata(path).map_err(|_| MigrationReason::SourceChanged)?;
    if !metadata.file_type().is_file() {
        return Err(MigrationReason::NotRegular);
    }
    Ok(stamp_from_metadata(&metadata))
}

fn stamp_from_metadata(metadata: &fs::Metadata) -> FileStamp {
    #[cfg(unix)]
    let identity = {
        use std::os::unix::fs::MetadataExt;
        FileIdentity::Unix {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    };
    #[cfg(not(unix))]
    let identity = FileIdentity::Portable {
        created: metadata.created().ok(),
    };
    FileStamp {
        bytes: metadata.len(),
        modified: metadata.modified().ok(),
        identity,
    }
}

fn create_semantic_archive(source: &Path, destination: &Path) -> Result<(), MigrationError> {
    let source = Connection::open_with_flags(
        source,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| MigrationError::Validation {
        kind: ArtifactKind::SessionUsageArchive,
        reason: MigrationReason::SqliteBackupFailed,
    })?;
    source
        .pragma_update(None, "query_only", true)
        .map_err(|_| MigrationError::Validation {
            kind: ArtifactKind::SessionUsageArchive,
            reason: MigrationReason::SqliteBackupFailed,
        })?;
    let destination_file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)?;
    make_private_file(&destination_file)?;
    drop(destination_file);
    let mut semantic = Connection::open_with_flags(
        destination,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| MigrationError::Validation {
        kind: ArtifactKind::SessionUsageArchive,
        reason: MigrationReason::SqliteBackupFailed,
    })?;
    {
        let backup =
            Backup::new(&source, &mut semantic).map_err(|_| MigrationError::Validation {
                kind: ArtifactKind::SessionUsageArchive,
                reason: MigrationReason::SqliteBackupFailed,
            })?;
        backup
            .run_to_completion(128, Duration::from_millis(1), None)
            .map_err(|_| MigrationError::Validation {
                kind: ArtifactKind::SessionUsageArchive,
                reason: MigrationReason::SqliteBackupFailed,
            })?;
    }
    drop(semantic);
    let check = Connection::open_with_flags(
        destination,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| MigrationError::Validation {
        kind: ArtifactKind::SessionUsageArchive,
        reason: MigrationReason::SqliteBackupFailed,
    })?;
    check
        .pragma_update(None, "query_only", true)
        .map_err(|_| MigrationError::Validation {
            kind: ArtifactKind::SessionUsageArchive,
            reason: MigrationReason::SqliteBackupFailed,
        })?;
    let result: String = check
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .map_err(|_| MigrationError::Validation {
            kind: ArtifactKind::SessionUsageArchive,
            reason: MigrationReason::SqliteBackupFailed,
        })?;
    if result != "ok" {
        return Err(MigrationError::Validation {
            kind: ArtifactKind::SessionUsageArchive,
            reason: MigrationReason::SqliteBackupFailed,
        });
    }
    // FlushFileBuffers requires write access on Windows; a read-only handle
    // fails with PermissionDenied.
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(destination)?
        .sync_all()?;
    Ok(())
}

fn import_session_archive(path: &Path) -> Result<Vec<ImportedSession>, MigrationError> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| archive_error(MigrationReason::ArchiveSchema))?;
    connection
        .pragma_update(None, "query_only", true)
        .map_err(|_| archive_error(MigrationReason::ArchiveSchema))?;
    let quick_check: String = connection
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .map_err(|_| archive_error(MigrationReason::ArchiveSchema))?;
    if quick_check != "ok" {
        return Err(archive_error(MigrationReason::ArchiveSchema));
    }

    let schema_version = archive_metadata(&connection, "schema-version")?
        .ok_or_else(|| archive_error(MigrationReason::ArchiveSchema))?;
    if schema_version != "1" {
        return Err(archive_error(MigrationReason::UnsupportedArchiveVersion));
    }
    if archive_metadata(&connection, "legacy-migrated")?.as_deref() != Some("1") {
        return Err(archive_error(MigrationReason::ArchiveSchema));
    }
    let archive_revision = archive_metadata(&connection, "revision")?
        .ok_or_else(|| archive_error(MigrationReason::ArchiveRevisionMismatch))?
        .parse::<u64>()
        .map_err(|_| archive_error(MigrationReason::ArchiveRevisionMismatch))?;

    let expected_rows: i64 = connection
        .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
        .map_err(|_| archive_error(MigrationReason::ArchiveSchema))?;
    let expected_rows: usize = expected_rows
        .try_into()
        .map_err(|_| archive_error(MigrationReason::ArchiveCapacity))?;
    if expected_rows > MAX_ARCHIVE_ROWS {
        return Err(archive_error(MigrationReason::ArchiveCapacity));
    }

    let mut metadata = serde_json::Map::new();
    metadata.insert("revision".to_owned(), Value::from(archive_revision));
    for (legacy, native) in [("pruned-day", "prunedDay"), ("pruned-month", "prunedMonth")] {
        if let Some(value) = archive_metadata(&connection, legacy)? {
            metadata.insert(native.to_owned(), Value::String(value));
        }
    }
    let archive_metadata_json = serde_json::to_vec(&Value::Object(metadata))
        .map_err(|_| archive_error(MigrationReason::MalformedArchiveRow))?;

    let mut statement = connection
        .prepare(
            "SELECT session_key, entry_json, revision
             FROM sessions ORDER BY session_key LIMIT ?1",
        )
        .map_err(|_| archive_error(MigrationReason::ArchiveSchema))?;
    let mut rows = statement
        .query([i64::try_from(MAX_ARCHIVE_ROWS + 1)
            .map_err(|_| archive_error(MigrationReason::ArchiveCapacity))?])
        .map_err(|_| archive_error(MigrationReason::ArchiveSchema))?;
    let mut imported = Vec::with_capacity(expected_rows);
    let mut identities = BTreeSet::new();
    while let Some(row) = rows
        .next()
        .map_err(|_| archive_error(MigrationReason::MalformedArchiveRow))?
    {
        if imported.len() == MAX_ARCHIVE_ROWS {
            return Err(archive_error(MigrationReason::ArchiveCapacity));
        }
        let legacy_key: String = row
            .get(0)
            .map_err(|_| archive_error(MigrationReason::MalformedArchiveRow))?;
        let entry_value = row
            .get_ref(1)
            .map_err(|_| archive_error(MigrationReason::MalformedArchiveRow))?;
        let original_entry_json = entry_value
            .as_bytes()
            .map_err(|_| archive_error(MigrationReason::MalformedArchiveRow))?
            .to_vec();
        let source_revision: i64 = row
            .get(2)
            .map_err(|_| archive_error(MigrationReason::MalformedArchiveRow))?;
        let source_revision: u64 = source_revision
            .try_into()
            .map_err(|_| archive_error(MigrationReason::ArchiveRevisionMismatch))?;
        if source_revision > archive_revision {
            return Err(archive_error(MigrationReason::ArchiveRevisionMismatch));
        }
        if original_entry_json.len() > MAX_ARCHIVE_ENTRY_BYTES {
            return Err(archive_error(MigrationReason::ArchiveCapacity));
        }
        let entry: Value = serde_json::from_slice(&original_entry_json)
            .map_err(|_| archive_error(MigrationReason::MalformedArchiveRow))?;
        let fields = entry
            .as_object()
            .ok_or_else(|| archive_error(MigrationReason::MalformedArchiveRow))?;
        let client = fields
            .get("client")
            .and_then(Value::as_str)
            .filter(|value| valid_identity(value, 64))
            .ok_or_else(|| archive_error(MigrationReason::UnresolvedArchiveIdentity))?;
        let session = fields
            .get("sessionId")
            .and_then(Value::as_str)
            .filter(|value| valid_identity(value, 512))
            .ok_or_else(|| archive_error(MigrationReason::UnresolvedArchiveIdentity))?;
        if !valid_legacy_key(&legacy_key) {
            return Err(archive_error(MigrationReason::MalformedArchiveRow));
        }
        if !identities.insert((client.to_owned(), session.to_owned())) {
            return Err(archive_error(MigrationReason::ConflictingArchiveIdentity));
        }
        imported.push(ImportedSession {
            client: client.to_owned(),
            session: session.to_owned(),
            original_entry_json,
            legacy_key,
            source_revision,
            archive_metadata_json: Some(archive_metadata_json.clone()),
        });
    }
    if imported.len() != expected_rows {
        return Err(archive_error(MigrationReason::ArchiveRevisionMismatch));
    }
    Ok(imported)
}

fn archive_metadata(connection: &Connection, key: &str) -> Result<Option<String>, MigrationError> {
    connection
        .query_row("SELECT value FROM metadata WHERE key = ?1", [key], |row| {
            row.get(0)
        })
        .optional()
        .map_err(|_| archive_error(MigrationReason::ArchiveSchema))
}

fn archive_error(reason: MigrationReason) -> MigrationError {
    MigrationError::Validation {
        kind: ArtifactKind::SessionUsageArchive,
        reason,
    }
}

fn valid_identity(value: &str, maximum: usize) -> bool {
    !value.is_empty()
        && value.len() <= maximum
        && !value
            .chars()
            .any(|character| character.is_control() || matches!(character, '/' | '\\' | ':' | '*'))
}

fn valid_legacy_key(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 1_024
        && !value.chars().any(char::is_control)
        && !is_absolute_path_text(value)
}

fn is_absolute_path_text(value: &str) -> bool {
    value.starts_with('/')
        || value.starts_with("\\\\")
        || (value.len() >= 3
            && value.as_bytes()[0].is_ascii_alphabetic()
            && value.as_bytes()[1] == b':'
            && matches!(value.as_bytes()[2], b'/' | b'\\'))
}

fn is_semantically_supported(kind: ArtifactKind) -> bool {
    matches!(
        kind,
        ArtifactKind::Settings | ArtifactKind::SessionUsageArchive
    )
}

fn artifact_kind_id(kind: ArtifactKind) -> &'static str {
    match kind {
        ArtifactKind::Settings => "settings",
        ArtifactKind::Credentials => "credentials",
        ArtifactKind::LimitsHistory => "limits-history",
        ArtifactKind::HubDevices => "hub-devices",
        ArtifactKind::IcloudRevisions => "icloud-revisions",
        ArtifactKind::SessionUsageArchive => "session-usage-archive",
        ArtifactKind::SessionUsageArchiveWal => "session-usage-archive-wal",
        ArtifactKind::SessionUsageArchiveShm => "session-usage-archive-shm",
        ArtifactKind::SessionUsageArchiveJson => "session-usage-archive-json",
        ArtifactKind::DailyHistoryArchive => "daily-history-archive",
        ArtifactKind::CodexLocalUsage => "codex-local-usage",
        ArtifactKind::CodexLocalUsageWal => "codex-local-usage-wal",
        ArtifactKind::CodexLocalUsageShm => "codex-local-usage-shm",
        ArtifactKind::PluginManager => "plugin-manager",
        ArtifactKind::PluginPackage => "plugin-package",
        ArtifactKind::PluginSettings => "plugin-settings",
        ArtifactKind::PluginData => "plugin-data",
        ArtifactKind::PluginCredentials => "plugin-credentials",
    }
}

fn artifact_directory_name(kind: ArtifactKind, ordinal: usize) -> String {
    let bundle = match kind {
        ArtifactKind::SessionUsageArchive
        | ArtifactKind::SessionUsageArchiveWal
        | ArtifactKind::SessionUsageArchiveShm => "session-usage-archive-bundle",
        ArtifactKind::CodexLocalUsage
        | ArtifactKind::CodexLocalUsageWal
        | ArtifactKind::CodexLocalUsageShm => "codex-local-usage-bundle",
        _ => artifact_kind_id(kind),
    };
    format!("{bundle}-{ordinal:03}")
}

fn artifact_file_name(kind: ArtifactKind) -> &'static str {
    match kind {
        ArtifactKind::SessionUsageArchive => "database.sqlite",
        ArtifactKind::SessionUsageArchiveWal => "database.sqlite-wal",
        ArtifactKind::SessionUsageArchiveShm => "database.sqlite-shm",
        ArtifactKind::CodexLocalUsage => "database.sqlite",
        ArtifactKind::CodexLocalUsageWal => "database.sqlite-wal",
        ArtifactKind::CodexLocalUsageShm => "database.sqlite-shm",
        _ => "source",
    }
}

fn next_run_id() -> Result<String, MigrationError> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| MigrationError::Io(io::ErrorKind::InvalidData))?
        .as_nanos();
    let sequence = NEXT_RUN.fetch_add(1, Ordering::Relaxed);
    Ok(format!("migration-{nanos:020}-{sequence:010}"))
}

fn create_private_directory_all(path: &Path) -> Result<(), MigrationError> {
    fs::create_dir_all(path)?;
    make_private_directory(path)?;
    Ok(())
}

#[cfg(unix)]
fn make_private_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn make_private_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn make_private_file(file: &File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn make_private_file(_file: &File) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}
