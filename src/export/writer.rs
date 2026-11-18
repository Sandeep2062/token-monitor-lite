//! Writing the file set to a folder, with the no-rewrite rule.
//!
//! See `docs/migration/export-spec.md` §5. The rule exists so an idle machine
//! does not re-upload unchanged files through iCloud or Obsidian Sync: a folder
//! whose contents would not change is not touched at all.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use super::{EXPORT_FILENAMES, ExportFile};

/// What a write did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExportOutcome {
    /// Files actually written.
    pub written: Vec<&'static str>,
    /// Files whose contents were already current.
    pub unchanged: Vec<&'static str>,
    /// Stale files from a previous run that were removed.
    pub removed: Vec<&'static str>,
}

impl ExportOutcome {
    /// Whether the folder changed in any way.
    pub fn changed(&self) -> bool {
        !self.written.is_empty() || !self.removed.is_empty()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error("export folder is unavailable")]
    Folder,
    #[error("export file could not be written")]
    Write,
}

/// The last thing written, so an unchanged export is a no-op.
///
/// Held in memory rather than inferred from `generated_at`, because the signature
/// deliberately excludes that field: re-reading the file could not recover the
/// signature it was written under.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExportMemory {
    signature: Option<String>,
    folder: Option<PathBuf>,
}

impl ExportMemory {
    pub fn new() -> Self {
        Self::default()
    }

    /// The folder most recently written to.
    pub fn folder(&self) -> Option<&Path> {
        self.folder.as_deref()
    }

    pub fn signature(&self) -> Option<&str> {
        self.signature.as_deref()
    }
}

/// Whether a write would be skipped: same folder, same meaningful payload.
///
/// This is the whole no-rewrite rule, as one predicate, so the caller and the
/// writer cannot disagree about it.
pub fn is_unchanged(memory: &ExportMemory, folder: &Path, signature: &str) -> bool {
    memory.folder.as_deref() == Some(folder) && memory.signature.as_deref() == Some(signature)
}

/// Write the file set, skipping anything already current.
///
/// `force` is for a manual export: a person who picks a folder and presses export
/// expects the files to be there, so the folder is written even when the payload
/// matches what was last written elsewhere. Even then, a file whose bytes already
/// match is not rewritten, so a synced folder does not see a modification.
pub fn write_file_set(
    folder: &Path,
    files: &[ExportFile],
    signature: &str,
    memory: &mut ExportMemory,
    force: bool,
) -> Result<ExportOutcome, ExportError> {
    if !force && is_unchanged(memory, folder, signature) {
        return Ok(ExportOutcome {
            unchanged: files.iter().map(|file| file.name).collect(),
            ..ExportOutcome::default()
        });
    }
    if !folder.is_dir() {
        return Err(ExportError::Folder);
    }

    let mut outcome = ExportOutcome::default();
    for file in files {
        let target = folder.join(file.name);
        // A file whose bytes already match is left alone: writing it would bump
        // its mtime and make every sync client re-upload it for nothing.
        if fs::read(&target).is_ok_and(|existing| existing == file.contents.as_bytes()) {
            outcome.unchanged.push(file.name);
            continue;
        }
        write_atomic(&target, file.contents.as_bytes())?;
        outcome.written.push(file.name);
    }

    // A file from a previous run that this run did not produce is stale — for
    // example a `daily.csv` after history emptied. Removing it keeps the folder
    // honest; leaving it would keep publishing data the app no longer has.
    for name in EXPORT_FILENAMES {
        if files.iter().any(|file| file.name == name) {
            continue;
        }
        let stale = folder.join(name);
        if fs::remove_file(&stale).is_ok() {
            outcome.removed.push(name);
        }
    }

    memory.folder = Some(folder.to_path_buf());
    memory.signature = Some(signature.to_owned());
    Ok(outcome)
}

/// Write through a temporary file and rename into place.
///
/// A half-written `token-monitor-export.json` in a synced vault is a file a
/// consumer may read mid-export, so the replacement is atomic.
fn write_atomic(target: &Path, contents: &[u8]) -> Result<(), ExportError> {
    let file_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(ExportError::Write)?;
    let temporary = target.with_file_name(format!(".{file_name}.tmp"));
    {
        let mut file = fs::File::create(&temporary).map_err(|_| ExportError::Write)?;
        file.write_all(contents).map_err(|_| ExportError::Write)?;
        file.sync_all().map_err(|_| ExportError::Write)?;
    }
    fs::rename(&temporary, target).map_err(|_| {
        let _ = fs::remove_file(&temporary);
        ExportError::Write
    })
}

#[cfg(test)]
mod tests;
