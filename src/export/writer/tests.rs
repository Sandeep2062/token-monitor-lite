//! Writer tests, against a real temporary directory.
//!
//! These are the only export tests that touch a filesystem. They prove the
//! no-rewrite rule by comparing modification times, which is the property a sync
//! client actually observes.

use std::fs;
use std::path::PathBuf;

use super::*;
use crate::export::csv;

fn scratch(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "token-monitor-export-{}-{}",
        name,
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).expect("create scratch");
    path
}

fn file(name: &'static str, contents: &str) -> ExportFile {
    ExportFile {
        name,
        contents: contents.to_owned(),
    }
}

fn daily_file() -> ExportFile {
    file(
        "token-monitor-daily.csv",
        &csv::render(
            &["date", "tool", "tokens", "cost_usd"],
            vec![vec!["2026-07-03", "codex", "5", "1"]],
        ),
    )
}

#[test]
fn a_first_write_creates_every_file() {
    let folder = scratch("first");
    let files = vec![
        file("token-monitor-export.json", "{}\n"),
        file("token-monitor-snapshot.csv", "h\r\n"),
        daily_file(),
    ];
    let mut memory = ExportMemory::new();
    let outcome = write_file_set(&folder, &files, "sig-1", &mut memory, false).expect("write");
    assert_eq!(outcome.written.len(), 3);
    assert!(outcome.changed());
    for name in [
        "token-monitor-export.json",
        "token-monitor-snapshot.csv",
        "token-monitor-daily.csv",
    ] {
        assert!(folder.join(name).is_file(), "{name} missing");
    }
    assert_eq!(memory.folder(), Some(folder.as_path()));
    assert_eq!(memory.signature(), Some("sig-1"));
    let _ = fs::remove_dir_all(&folder);
}

#[test]
fn an_unchanged_payload_rewrites_nothing() {
    let folder = scratch("unchanged");
    let files = vec![file("token-monitor-export.json", "{}\n")];
    let mut memory = ExportMemory::new();
    write_file_set(&folder, &files, "sig-1", &mut memory, false).expect("first");

    let target = folder.join("token-monitor-export.json");
    let before = fs::metadata(&target).expect("metadata").modified().ok();

    // A second auto-export with the same payload must not touch the folder.
    let outcome = write_file_set(&folder, &files, "sig-1", &mut memory, false).expect("second");
    assert!(
        outcome.written.is_empty(),
        "an unchanged export rewrote a file"
    );
    assert!(outcome.unchanged.contains(&"token-monitor-export.json"));
    assert!(!outcome.changed());

    let after = fs::metadata(&target).expect("metadata").modified().ok();
    assert_eq!(before, after, "the file's mtime moved");
    let _ = fs::remove_dir_all(&folder);
}

#[test]
fn a_changed_payload_rewrites_only_the_files_that_changed() {
    let folder = scratch("partial");
    let mut memory = ExportMemory::new();
    write_file_set(
        &folder,
        &[file("token-monitor-export.json", "{}\n")],
        "sig-1",
        &mut memory,
        false,
    )
    .expect("first");

    let outcome = write_file_set(
        &folder,
        &[
            file("token-monitor-export.json", "{\"a\":1}\n"),
            file("token-monitor-snapshot.csv", "period\r\n"),
        ],
        "sig-2",
        &mut memory,
        false,
    )
    .expect("second");
    // The JSON changed, the CSV is new.
    assert_eq!(outcome.written.len(), 2);
    assert!(outcome.unchanged.is_empty());
    let _ = fs::remove_dir_all(&folder);
}

#[test]
fn identical_bytes_are_not_rewritten_even_under_force() {
    // A manual export to the same folder must not bump mtimes for files whose
    // contents already match.
    let folder = scratch("force");
    let files = vec![file("token-monitor-export.json", "{}\n")];
    let mut memory = ExportMemory::new();
    write_file_set(&folder, &files, "sig-1", &mut memory, false).expect("first");

    let target = folder.join("token-monitor-export.json");
    let before = fs::metadata(&target).expect("metadata").modified().ok();
    let outcome = write_file_set(&folder, &files, "sig-1", &mut memory, true).expect("forced");
    assert!(outcome.written.is_empty(), "force rewrote identical bytes");
    let after = fs::metadata(&target).expect("metadata").modified().ok();
    assert_eq!(before, after);
    let _ = fs::remove_dir_all(&folder);
}

#[test]
fn a_force_export_to_a_new_folder_writes_everything() {
    // The signature matched but the folder is different: the files must appear.
    let first = scratch("folder-a");
    let second = scratch("folder-b");
    let files = vec![file("token-monitor-export.json", "{}\n")];
    let mut memory = ExportMemory::new();
    write_file_set(&first, &files, "sig-1", &mut memory, false).expect("first");

    let outcome = write_file_set(&second, &files, "sig-1", &mut memory, true).expect("second");
    assert_eq!(outcome.written.len(), 1);
    assert!(second.join("token-monitor-export.json").is_file());
    let _ = fs::remove_dir_all(&first);
    let _ = fs::remove_dir_all(&second);
}

#[test]
fn a_stale_daily_file_is_removed_when_history_empties() {
    let folder = scratch("orphan");
    let mut memory = ExportMemory::new();
    write_file_set(
        &folder,
        &[file("token-monitor-export.json", "{}\n"), daily_file()],
        "sig-1",
        &mut memory,
        false,
    )
    .expect("first");
    assert!(folder.join("token-monitor-daily.csv").is_file());

    // The second run has no daily rows, so the file must not survive.
    let outcome = write_file_set(
        &folder,
        &[file("token-monitor-export.json", "{}\n")],
        "sig-2",
        &mut memory,
        false,
    )
    .expect("second");
    assert!(
        !folder.join("token-monitor-daily.csv").exists(),
        "a stale daily.csv survived"
    );
    assert_eq!(outcome.removed, vec!["token-monitor-daily.csv"]);
    let _ = fs::remove_dir_all(&folder);
}

#[test]
fn a_missing_folder_is_an_error_not_a_silent_success() {
    let folder = std::env::temp_dir().join("token-monitor-export-does-not-exist-xyz");
    let _ = fs::remove_dir_all(&folder);
    let mut memory = ExportMemory::new();
    let error = write_file_set(
        &folder,
        &[file("token-monitor-export.json", "{}\n")],
        "sig-1",
        &mut memory,
        false,
    )
    .expect_err("should fail");
    assert!(matches!(error, ExportError::Folder));
}

#[test]
fn the_written_bytes_are_exactly_the_rendered_bytes() {
    // The BOM and CRLF must survive the write; a text-mode write on Windows would
    // turn the CRLF into CRCRLF.
    let folder = scratch("bytes");
    let contents = csv::render(&["date", "tool"], vec![vec!["2026-07-03", "codex"]]);
    let mut memory = ExportMemory::new();
    write_file_set(
        &folder,
        &[file("token-monitor-snapshot.csv", &contents)],
        "sig-1",
        &mut memory,
        false,
    )
    .expect("write");
    let written = fs::read(folder.join("token-monitor-snapshot.csv")).expect("read");
    assert_eq!(written, contents.as_bytes());
    assert!(written.starts_with(b"\xEF\xBB\xBF"), "the BOM was lost");
    assert!(written.ends_with(b"\r\n"));
    let _ = fs::remove_dir_all(&folder);
}

#[test]
fn no_temporary_file_is_left_behind() {
    let folder = scratch("temp");
    let mut memory = ExportMemory::new();
    write_file_set(
        &folder,
        &[file("token-monitor-export.json", "{}\n")],
        "sig-1",
        &mut memory,
        false,
    )
    .expect("write");
    let leftovers = fs::read_dir(&folder)
        .expect("read dir")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
        .count();
    assert_eq!(leftovers, 0, "an atomic write left its temporary file");
    let _ = fs::remove_dir_all(&folder);
}
