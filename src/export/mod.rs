//! Data export: the four files, their exact serialization, and the writer.
//!
//! Ports `src/shared/exporter.js`. `docs/migration/export-spec.md` states the
//! format, the privacy boundary, and which claims are VERIFIED versus ASSUMED.
//!
//! The module is pure with respect to the filesystem except in [`writer`]: every
//! rendering function takes data and returns text, so a golden-file test can
//! compare bytes without touching a disk.

pub mod builder;
pub mod csv;
pub mod json;
pub mod rows;
pub mod schedule;
pub mod writer;

pub use json::{AppIdentity, ExportDocument, JsonDaily, JsonMonthly, JsonSlice, SnapshotDocument};
pub use rows::{DailyModelRow, DailyRow, ModelDay, PeriodTotalsRow, SnapshotEntry, SnapshotRow};
pub use writer::{ExportOutcome, write_file_set};

/// The complete set of generated filenames, in write order.
///
/// This list is the source of truth the writer uses to remove orphans, so a stale
/// `daily.csv` left behind after history empties is deleted rather than kept
/// forever. It must therefore exist independently of what is written this run.
pub const EXPORT_FILENAMES: [&str; 4] = [
    "token-monitor-export.json",
    "token-monitor-snapshot.csv",
    "token-monitor-daily.csv",
    "token-monitor-daily-models.csv",
];

/// One file to write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportFile {
    pub name: &'static str,
    pub contents: String,
}

/// The four files for one export, minus the conditional ones.
///
/// `daily.csv` and `daily-models.csv` are written only when they have at least one
/// row — the reference's own rule, not an optimisation: a daily file with a header
/// and no rows would claim a tracked history that does not exist.
pub fn file_set(
    document: &ExportDocument,
    snapshot_rows: &[SnapshotRow],
    daily_rows: &[DailyRow],
    daily_model_rows: &[DailyModelRow],
) -> Vec<ExportFile> {
    let mut files = vec![
        ExportFile {
            name: EXPORT_FILENAMES[0],
            contents: json::render(document),
        },
        ExportFile {
            name: EXPORT_FILENAMES[1],
            contents: csv::render(
                &rows::SNAPSHOT_COLUMNS,
                snapshot_rows.iter().map(|row| {
                    vec![
                        row.period.to_owned(),
                        row.dimension.to_owned(),
                        row.name.clone(),
                        csv::format_count(row.tokens),
                        row.cost_usd.clone(),
                    ]
                }),
            ),
        },
    ];
    if !daily_rows.is_empty() {
        files.push(ExportFile {
            name: EXPORT_FILENAMES[2],
            contents: csv::render(
                &rows::DAILY_COLUMNS,
                daily_rows.iter().map(|row| {
                    vec![
                        row.date.clone(),
                        row.tool.clone(),
                        csv::format_count(row.tokens),
                        row.cost_usd.clone(),
                    ]
                }),
            ),
        });
    }
    if !daily_model_rows.is_empty() {
        files.push(ExportFile {
            name: EXPORT_FILENAMES[3],
            contents: csv::render(
                &rows::DAILY_MODEL_COLUMNS,
                daily_model_rows.iter().map(|row| {
                    vec![
                        row.date.clone(),
                        row.model.clone(),
                        csv::format_count(row.input_tokens),
                        csv::format_count(row.output_tokens),
                        csv::format_count(row.cache_read_tokens),
                        csv::format_count(row.cache_write_tokens),
                        csv::format_count(row.unclassified_tokens),
                        csv::format_count(row.total_tokens),
                        row.cost_usd.clone(),
                    ]
                }),
            ),
        });
    }
    files
}

/// The signature of what a file set means, for the no-rewrite comparison.
pub fn signature(document: &ExportDocument) -> String {
    json::signature(document)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::json::{JsonDaily, JsonSlice, SnapshotDocument};
    use serde_json::json;

    fn document(tokens: u128) -> ExportDocument {
        ExportDocument {
            generated_at: "2026-07-03T14:30:00.000Z".to_owned(),
            app: AppIdentity::default(),
            snapshot: SnapshotDocument {
                today: json!({ "total": tokens }),
                month: json!({ "total": tokens }),
                all_time: json!({ "total": tokens }),
            },
            daily: vec![JsonDaily {
                date: "2026-07-03".to_owned(),
                tokens: 12,
                cost: 2.0,
                per_client: [(
                    "codex".to_owned(),
                    JsonSlice {
                        tokens: 12,
                        cost: 2.0,
                        ..JsonSlice::default()
                    },
                )]
                .into_iter()
                .collect(),
                per_model: Default::default(),
            }],
            monthly: Vec::new(),
        }
    }

    fn snapshot_row() -> SnapshotRow {
        SnapshotRow {
            period: "today",
            dimension: "tool",
            name: "codex".to_owned(),
            tokens: 20,
            cost_usd: "2".to_owned(),
        }
    }

    #[test]
    fn the_filename_list_is_the_reference_list_in_order() {
        assert_eq!(
            EXPORT_FILENAMES,
            [
                "token-monitor-export.json",
                "token-monitor-snapshot.csv",
                "token-monitor-daily.csv",
                "token-monitor-daily-models.csv"
            ]
        );
    }

    #[test]
    fn only_the_two_unconditional_files_are_written_without_history() {
        let files = file_set(&document(5), &[snapshot_row()], &[], &[]);
        let names = files.iter().map(|file| file.name).collect::<Vec<_>>();
        assert_eq!(
            names,
            vec!["token-monitor-export.json", "token-monitor-snapshot.csv"]
        );
    }

    #[test]
    fn the_daily_files_appear_when_they_have_rows() {
        let daily = vec![DailyRow {
            date: "2026-07-03".to_owned(),
            tool: "codex".to_owned(),
            tokens: 12,
            cost_usd: "2".to_owned(),
        }];
        let daily_models = vec![DailyModelRow {
            date: "2026-07-03".to_owned(),
            model: "gpt-5".to_owned(),
            input_tokens: 2,
            output_tokens: 2,
            cache_read_tokens: 1,
            cache_write_tokens: 0,
            unclassified_tokens: 0,
            total_tokens: 5,
            cost_usd: "1".to_owned(),
        }];
        let files = file_set(&document(5), &[snapshot_row()], &daily, &daily_models);
        let names = files.iter().map(|file| file.name).collect::<Vec<_>>();
        assert_eq!(names, EXPORT_FILENAMES.to_vec());
    }

    #[test]
    fn only_the_conditional_files_that_have_rows_are_written() {
        // Daily rows but no model rows: three files, and no empty model file.
        let daily = vec![DailyRow {
            date: "2026-07-03".to_owned(),
            tool: "codex".to_owned(),
            tokens: 12,
            cost_usd: "2".to_owned(),
        }];
        let files = file_set(&document(5), &[snapshot_row()], &daily, &[]);
        let names = files.iter().map(|file| file.name).collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                "token-monitor-export.json",
                "token-monitor-snapshot.csv",
                "token-monitor-daily.csv"
            ]
        );
    }

    #[test]
    fn every_csv_starts_with_a_bom_and_ends_with_crlf() {
        let daily = vec![DailyRow {
            date: "2026-07-03".to_owned(),
            tool: "codex".to_owned(),
            tokens: 12,
            cost_usd: "2".to_owned(),
        }];
        let files = file_set(&document(5), &[snapshot_row()], &daily, &[]);
        for file in files.iter().filter(|file| file.name.ends_with(".csv")) {
            assert!(
                file.contents.starts_with(csv::BOM),
                "{} has no BOM",
                file.name
            );
            assert!(
                file.contents.ends_with("\r\n"),
                "{} does not end with CRLF",
                file.name
            );
            assert!(
                !file.contents.contains("\n\n"),
                "{} has a blank line",
                file.name
            );
        }
    }

    #[test]
    fn the_json_file_has_no_bom() {
        let files = file_set(&document(5), &[snapshot_row()], &[], &[]);
        let json = files
            .iter()
            .find(|file| file.name == "token-monitor-export.json")
            .expect("json");
        assert!(!json.contents.starts_with(csv::BOM));
    }

    #[test]
    fn the_signature_matches_the_document_signature() {
        let doc = document(5);
        assert_eq!(signature(&doc), json::signature(&doc));
    }
}
