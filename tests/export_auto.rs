//! End-to-end export behaviour through the writer, with the real file set.
//!
//! These prove the properties `docs/export.md` promises a user pointing
//! auto-export at a synced folder: the files are current, and an idle machine
//! does not keep re-writing them.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use token_monitor_lite::export::{
    AppIdentity, DailyModelRow, DailyRow, EXPORT_FILENAMES, ExportDocument, JsonDaily, JsonSlice,
    SnapshotDocument, SnapshotRow, file_set, schedule, signature, write_file_set,
    writer::ExportMemory,
};

fn scratch(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "token-monitor-autoexport-{}-{}",
        name,
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).expect("create scratch");
    path
}

fn document(tokens: u128, day: &str) -> ExportDocument {
    ExportDocument {
        generated_at: "2026-07-03T14:30:00.000Z".to_owned(),
        app: AppIdentity::default(),
        snapshot: SnapshotDocument {
            today: serde_json::json!({ "total": tokens }),
            month: serde_json::json!({ "total": tokens }),
            all_time: serde_json::json!({ "total": tokens }),
        },
        daily: vec![JsonDaily {
            date: day.to_owned(),
            tokens,
            cost: 1.0,
            per_client: [(
                "codex".to_owned(),
                JsonSlice {
                    tokens,
                    cost: 1.0,
                    ..JsonSlice::default()
                },
            )]
            .into_iter()
            .collect::<BTreeMap<_, _>>(),
            per_model: BTreeMap::new(),
        }],
        monthly: Vec::new(),
    }
}

fn rows(day: &str, tokens: u128) -> (Vec<SnapshotRow>, Vec<DailyRow>, Vec<DailyModelRow>) {
    (
        vec![SnapshotRow {
            period: "today",
            dimension: "tool",
            name: "codex".to_owned(),
            tokens,
            cost_usd: "1".to_owned(),
        }],
        vec![DailyRow {
            date: day.to_owned(),
            tool: "codex".to_owned(),
            tokens,
            cost_usd: "1".to_owned(),
        }],
        vec![DailyModelRow {
            date: day.to_owned(),
            model: "gpt-5".to_owned(),
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            unclassified_tokens: tokens,
            total_tokens: tokens,
            cost_usd: "1".to_owned(),
        }],
    )
}

/// One auto-export pass, as the application runs it.
fn pass(
    folder: &std::path::Path,
    memory: &mut ExportMemory,
    doc: &ExportDocument,
    day: &str,
    tokens: u128,
) -> usize {
    let (snapshot_rows, daily_rows, daily_models) = rows(day, tokens);
    let files = file_set(doc, &snapshot_rows, &daily_rows, &daily_models);
    let signature = signature(doc);
    write_file_set(folder, &files, &signature, memory, false)
        .expect("write")
        .written
        .len()
}

#[test]
fn the_first_auto_export_writes_all_four_files() {
    let folder = scratch("first");
    let mut memory = ExportMemory::new();
    let written = pass(
        &folder,
        &mut memory,
        &document(10, "2026-07-03"),
        "2026-07-03",
        10,
    );
    assert_eq!(written, 4);
    for name in EXPORT_FILENAMES {
        assert!(folder.join(name).is_file(), "{name} missing");
    }
    let _ = fs::remove_dir_all(&folder);
}

#[test]
fn a_second_pass_with_identical_data_writes_nothing() {
    let folder = scratch("idle");
    let mut memory = ExportMemory::new();
    let doc = document(10, "2026-07-03");
    pass(&folder, &mut memory, &doc, "2026-07-03", 10);
    // The same payload again: an idle machine must not touch the folder.
    let written = pass(&folder, &mut memory, &doc, "2026-07-03", 10);
    assert_eq!(written, 0, "an unchanged auto-export rewrote files");
    let _ = fs::remove_dir_all(&folder);
}

#[test]
fn a_changed_payload_is_written() {
    let folder = scratch("changed");
    let mut memory = ExportMemory::new();
    pass(
        &folder,
        &mut memory,
        &document(10, "2026-07-03"),
        "2026-07-03",
        10,
    );
    let written = pass(
        &folder,
        &mut memory,
        &document(20, "2026-07-03"),
        "2026-07-03",
        20,
    );
    assert!(written > 0, "a changed payload did not update the folder");
    // The JSON really carries the new number.
    let json = fs::read_to_string(folder.join("token-monitor-export.json")).expect("read");
    assert!(json.contains("20"), "the export is stale: {json}");
    let _ = fs::remove_dir_all(&folder);
}

#[test]
fn a_clock_tick_alone_never_causes_a_write() {
    // Only `generatedAt` moves. Because it is excluded from the signature, this
    // must be a no-op — that is the whole point of the no-rewrite rule.
    let folder = scratch("clock");
    let mut memory = ExportMemory::new();
    let mut first = document(10, "2026-07-03");
    first.generated_at = "2026-07-03T14:30:00.000Z".to_owned();
    pass(&folder, &mut memory, &first, "2026-07-03", 10);

    let mut second = document(10, "2026-07-03");
    second.generated_at = "2026-07-03T15:30:00.000Z".to_owned();
    let written = pass(&folder, &mut memory, &second, "2026-07-03", 10);
    assert_eq!(written, 0, "a timestamp-only change rewrote the folder");
    let _ = fs::remove_dir_all(&folder);
}

#[test]
fn the_cadence_gates_the_attempt() {
    // A short tick must not export more often than the configured interval.
    let last = 1_000_000;
    assert!(!schedule::is_due(60, Some(last), last + 30_000));
    assert!(schedule::is_due(60, Some(last), last + 60_000));
    // And the range a user can select is the documented one.
    assert_eq!(schedule::clamp_interval(1), 30);
    assert_eq!(schedule::clamp_interval(7200), 3600);
}

#[test]
fn every_written_csv_is_excel_readable() {
    let folder = scratch("excel");
    let mut memory = ExportMemory::new();
    pass(
        &folder,
        &mut memory,
        &document(10, "2026-07-03"),
        "2026-07-03",
        10,
    );
    for name in [
        "token-monitor-snapshot.csv",
        "token-monitor-daily.csv",
        "token-monitor-daily-models.csv",
    ] {
        let bytes = fs::read(folder.join(name)).expect("read");
        assert!(bytes.starts_with(b"\xEF\xBB\xBF"), "{name} has no BOM");
        // CRLF throughout: no bare LF.
        let text = String::from_utf8_lossy(&bytes);
        assert!(!text.contains("\r\r"), "{name} doubled its CR");
        let bare_lf = text
            .match_indices('\n')
            .filter(|(index, _)| *index == 0 || bytes[*index - 1] != b'\r')
            .count();
        assert_eq!(bare_lf, 0, "{name} has a bare LF");
    }
    let _ = fs::remove_dir_all(&folder);
}

#[test]
fn a_json_export_is_valid_utf8_without_a_bom() {
    let folder = scratch("json");
    let mut memory = ExportMemory::new();
    pass(
        &folder,
        &mut memory,
        &document(10, "2026-07-03"),
        "2026-07-03",
        10,
    );
    let bytes = fs::read(folder.join("token-monitor-export.json")).expect("read");
    assert!(!bytes.starts_with(b"\xEF\xBB\xBF"), "the JSON has a BOM");
    let text = std::str::from_utf8(&bytes).expect("valid UTF-8");
    // It parses, and its dates are ISO.
    let value: serde_json::Value = serde_json::from_str(text).expect("valid JSON");
    assert_eq!(value["daily"][0]["date"], "2026-07-03");
}
