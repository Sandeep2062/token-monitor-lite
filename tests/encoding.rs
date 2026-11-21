//! Repo-wide text-encoding guard.
//!
//! Two failures motivated this, both real and both silent:
//!
//! 1. A PowerShell 5.1 script with a literal em dash and no BOM cannot be
//!    *parsed* — 5.1 decodes a BOM-less `.ps1` as the ANSI codepage, so a
//!    multi-byte character becomes mojibake before the shell even tries to run
//!    it. Verified on this host: 5.1 reports `Unexpected token`, while
//!    PowerShell 7.7.0 reads the same file correctly.
//! 2. Text written through that path ended up committed as mojibake: an em dash
//!    read as cp1252 and re-encoded as UTF-8 turns one character into four
//!    bytes. Twelve such sequences reached `docs/migration/inventory.md` and one
//!    reached a commit message before anyone noticed, because viewing them
//!    through a terminal re-encodes the output and can mask the damage.
//!
//! Checks run on the committed tree, not the working copy, so a stray ignored
//! file cannot fail the gate. No dependency: walking the tree is a few dozen
//! lines and `walkdir` would be a new cost for no gain.

use std::fs;
use std::path::{Path, PathBuf};

/// Directories that never belong to the source tree.
const SKIPPED: &[&str] = &[
    "target",
    ".git",
    "node_modules",
    "dist",
    "token-monitor",
    "__pycache__",
    ".commandcode",
];

/// Extensions treated as text. Anything else is skipped rather than guessed at.
const TEXT_EXTENSIONS: &[&str] = &[
    "rs",
    "toml",
    "lock",
    "md",
    "json",
    "yml",
    "yaml",
    "slint",
    "po",
    "pot",
    "csv",
    "txt",
    "ps1",
    "psm1",
    "psd1",
    "py",
    "cjs",
    "js",
    "sh",
    "xml",
    "plist",
    "iss",
    "desktop",
    "svg",
    "service",
    "gitignore",
    "gitattributes",
];

/// Byte sequences that only appear when UTF-8 text has been read as cp1252 and
/// written back out. Each is high-confidence: legitimate text in this repository's
/// locales does not contain them. Portuguese uses a-tilde directly, never the
/// two-character cp1252 misread of it.
const MOJIBAKE: &[(&[u8], &str)] = &[
    (
        b"\xc3\xa2\xe2\x82\xac",
        "misread em/en dash (a-hat e-acute euro)",
    ),
    (b"\xc3\xa2\xc2\x80", "misread em/en dash (a-hat control)"),
    (b"\xc3\x83\xc2\xa9", "misread e-acute"),
    (b"\xc3\x83\xc2\xa0", "misread a-grave"),
    (b"\xc3\x83\xc2\xa3", "misread a-tilde"),
    (b"\xc3\xa2\xc5\x93", "misread quote"),
];

fn is_text(path: &Path) -> bool {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some(ext) => TEXT_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()),
        // Extensionless files are judged by name.
        None => matches!(
            path.file_name().and_then(|name| name.to_str()),
            Some(".gitignore" | ".gitattributes" | "LICENSE" | "COMMIT_EDITMSG")
        ),
    }
}

fn walk(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if SKIPPED.contains(&name.as_ref()) {
            continue;
        }
        if path.is_dir() {
            walk(&path, out);
        } else if is_text(&path) {
            out.push(path);
        }
    }
}

fn repo_files() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    walk(root, &mut files);
    files
}

/// Which mojibake signature, if any, this file contains.
fn mojibake_hits(bytes: &[u8]) -> Vec<&'static str> {
    MOJIBAKE
        .iter()
        .filter(|(needle, _)| bytes.windows(needle.len()).any(|window| window == *needle))
        .map(|(_, label)| *label)
        .collect()
}

#[test]
fn no_tracked_text_file_contains_mojibake() {
    let mut offenders = Vec::new();
    for path in repo_files() {
        let Ok(bytes) = fs::read(&path) else {
            continue;
        };
        // Binary content inside a text extension is not our business.
        if bytes.contains(&0) {
            continue;
        }
        let hits = mojibake_hits(&bytes);
        if !hits.is_empty() {
            offenders.push(format!(
                "{}: {}",
                path.strip_prefix(env!("CARGO_MANIFEST_DIR"))
                    .unwrap_or(&path)
                    .display(),
                hits.join(", ")
            ));
        }
    }

    assert!(
        offenders.is_empty(),
        "mojibake found in {} file(s). This is UTF-8 text that was read as cp1252 \
         and re-written as UTF-8 — the usual cause is a PowerShell 5.1 script \
         without a BOM. Repair with `encode('cp1252').decode('utf-8')`:\n  {}",
        offenders.len(),
        offenders.join("\n  ")
    );
}

#[test]
fn powershell_scripts_are_ascii_or_carry_a_bom() {
    // PowerShell 5.1 parses a BOM-less script as ANSI, so non-ASCII source in one
    // is a parse error at best and mojibake at worst. Either keep the script
    // ASCII, or save it with a UTF-8 BOM so 5.1 decodes it correctly.
    let mut offenders = Vec::new();
    for path in repo_files() {
        let extension = path
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !matches!(extension.as_str(), "ps1" | "psm1" | "psd1") {
            continue;
        }
        let Ok(bytes) = fs::read(&path) else {
            continue;
        };
        let non_ascii = bytes.iter().any(|byte| *byte > 0x7F);
        let has_bom = bytes.starts_with(b"\xef\xbb\xbf");
        if non_ascii && !has_bom {
            offenders.push(
                path.strip_prefix(env!("CARGO_MANIFEST_DIR"))
                    .unwrap_or(&path)
                    .display()
                    .to_string(),
            );
        }
    }

    assert!(
        offenders.is_empty(),
        "PowerShell script(s) with non-ASCII text and no BOM, which PowerShell 5.1 \
         cannot parse. Add a UTF-8 BOM or make the file ASCII:\n  {}",
        offenders.join("\n  ")
    );
}

#[test]
fn the_guard_actually_detects_damage() {
    // A guard nobody has seen fail is a guard nobody can trust.
    let damaged = b"records 1\xc3\xa2\xe2\x82\xac\xe2\x80\x9c4 only";
    assert!(!mojibake_hits(damaged).is_empty());

    let healthy = "records 1\u{2013}4 only".as_bytes();
    assert!(mojibake_hits(healthy).is_empty());

    // Portuguese contains a literal a-tilde but never the cp1252 misread of it.
    let portuguese = "N\u{00c3}O e n\u{00e3}o".as_bytes();
    assert!(mojibake_hits(portuguese).is_empty());
}
