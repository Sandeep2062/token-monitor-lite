//! Golden-file export tests.
//!
//! Each case builds the export from data shaped like the reference's examples in
//! `token-monitor/docs/export.md` and compares the produced bytes with a file in
//! `tests/fixtures/export/`.
//!
//! **What these prove and what they do not.** They prove our serializer matches
//! the documented shape byte for byte — BOM, CRLF, quoting, column order, number
//! formatting. They do **not** prove a running upstream build produces these
//! bytes: the goldens were reproduced from the documentation and the reference
//! source, not captured from an executing upstream. `docs/migration/export-spec.md`
//! §8 records that as ASSUMED.

use std::collections::BTreeMap;

use token_monitor_lite::export::{
    AppIdentity, DailyModelRow, DailyRow, EXPORT_FILENAMES, ExportDocument, JsonDaily, JsonMonthly,
    JsonSlice, ModelDay, PeriodTotalsRow, SnapshotDocument, SnapshotEntry, SnapshotRow, csv,
    file_set, rows,
};
use token_monitor_lite::pricing::CostEstimate;

fn fixture(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/export")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {name}: {error}"))
}

fn fixture_bytes(name: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/export")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|error| panic!("read {name}: {error}"))
}

fn usd(nano: i128) -> CostEstimate {
    CostEstimate {
        nano_usd: nano,
        unpriced_tokens: 0,
        provenance: None,
    }
}

/// The snapshot rows from `docs/export.md`, exactly.
fn documented_snapshot_rows() -> Vec<SnapshotRow> {
    vec![
        SnapshotRow {
            period: "today",
            dimension: "tool",
            name: "codex".to_owned(),
            tokens: 20,
            cost_usd: "2".to_owned(),
        },
        SnapshotRow {
            period: "today",
            dimension: "model",
            name: "gpt-5".to_owned(),
            tokens: 20,
            cost_usd: "2".to_owned(),
        },
        SnapshotRow {
            period: "month",
            dimension: "tool",
            name: "codex".to_owned(),
            tokens: 0,
            cost_usd: "0".to_owned(),
        },
        SnapshotRow {
            period: "allTime",
            dimension: "tool",
            name: "codex".to_owned(),
            tokens: 100,
            cost_usd: "9".to_owned(),
        },
    ]
}

/// The daily rows from `docs/export.md`, exactly.
fn documented_daily_rows() -> Vec<DailyRow> {
    vec![
        DailyRow {
            date: "2026-07-02".to_owned(),
            tool: "codex".to_owned(),
            tokens: 5,
            cost_usd: "1".to_owned(),
        },
        DailyRow {
            date: "2026-07-03".to_owned(),
            tool: "codex".to_owned(),
            tokens: 7,
            cost_usd: "1".to_owned(),
        },
        DailyRow {
            date: "2026-07-03".to_owned(),
            tool: "claude-code".to_owned(),
            tokens: 5,
            cost_usd: "1".to_owned(),
        },
    ]
}

/// The daily-model rows from `docs/export.md`, including the documented
/// unclassified remainder on the `opus` row.
fn documented_daily_model_rows() -> Vec<DailyModelRow> {
    vec![
        DailyModelRow {
            date: "2026-07-02".to_owned(),
            model: "gpt-5".to_owned(),
            input_tokens: 2,
            output_tokens: 2,
            cache_read_tokens: 1,
            cache_write_tokens: 0,
            unclassified_tokens: 0,
            total_tokens: 5,
            cost_usd: "1".to_owned(),
        },
        DailyModelRow {
            date: "2026-07-03".to_owned(),
            model: "opus".to_owned(),
            input_tokens: 0,
            output_tokens: 2,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            unclassified_tokens: 10,
            total_tokens: 12,
            cost_usd: "2".to_owned(),
        },
    ]
}

fn documented_document() -> ExportDocument {
    ExportDocument {
        generated_at: "2026-11-15T12:00:00.000Z".to_owned(),
        app: AppIdentity::default(),
        snapshot: SnapshotDocument {
            today: serde_json::json!({
                "total": 20,
                "perClient": { "codex": 20 },
                "perModel": { "gpt-5": 20 }
            }),
            month: serde_json::json!({ "total": 20 }),
            all_time: serde_json::json!({ "total": 100 }),
        },
        daily: vec![
            JsonDaily {
                date: "2026-07-02".to_owned(),
                tokens: 5,
                cost: 0.5,
                per_client: [(
                    "codex".to_owned(),
                    JsonSlice {
                        tokens: 5,
                        cost: 0.5,
                        ..JsonSlice::default()
                    },
                )]
                .into_iter()
                .collect(),
                per_model: [(
                    "gpt-5".to_owned(),
                    JsonSlice {
                        tokens: 5,
                        cost: 0.5,
                        output_tokens: Some(1),
                        cache_read_tokens: Some(1),
                        cache_write_tokens: Some(0),
                        unclassified_tokens: None,
                    },
                )]
                .into_iter()
                .collect(),
            },
            JsonDaily {
                date: "2026-07-03".to_owned(),
                tokens: 12,
                cost: 1.25,
                per_client: [
                    (
                        "claude-code".to_owned(),
                        JsonSlice {
                            tokens: 7,
                            cost: 0.75,
                            ..JsonSlice::default()
                        },
                    ),
                    (
                        "codex".to_owned(),
                        JsonSlice {
                            tokens: 5,
                            cost: 0.5,
                            ..JsonSlice::default()
                        },
                    ),
                ]
                .into_iter()
                .collect(),
                per_model: [(
                    "opus".to_owned(),
                    JsonSlice {
                        tokens: 12,
                        cost: 1.25,
                        output_tokens: Some(2),
                        cache_read_tokens: Some(0),
                        cache_write_tokens: Some(0),
                        // The documented `opus` row carries an explicit unknown
                        // remainder, which is the only way the key appears at all.
                        unclassified_tokens: Some(10),
                    },
                )]
                .into_iter()
                .collect(),
            },
        ],
        monthly: vec![JsonMonthly {
            month: "2026-07".to_owned(),
            tokens: 17,
            cost: 1.75,
            per_client: [
                (
                    "claude-code".to_owned(),
                    JsonSlice {
                        tokens: 7,
                        cost: 0.75,
                        ..JsonSlice::default()
                    },
                ),
                (
                    "codex".to_owned(),
                    JsonSlice {
                        tokens: 10,
                        cost: 1.0,
                        ..JsonSlice::default()
                    },
                ),
            ]
            .into_iter()
            .collect(),
            per_model: [
                (
                    "gpt-5".to_owned(),
                    JsonSlice {
                        tokens: 5,
                        cost: 0.5,
                        ..JsonSlice::default()
                    },
                ),
                (
                    "opus".to_owned(),
                    JsonSlice {
                        tokens: 12,
                        cost: 1.25,
                        ..JsonSlice::default()
                    },
                ),
            ]
            .into_iter()
            .collect(),
        }],
    }
}

#[test]
fn the_snapshot_csv_matches_the_golden_file() {
    let produced = csv::render(
        &rows::SNAPSHOT_COLUMNS,
        documented_snapshot_rows().iter().map(|row| {
            vec![
                row.period.to_owned(),
                row.dimension.to_owned(),
                row.name.clone(),
                csv::format_count(row.tokens),
                row.cost_usd.clone(),
            ]
        }),
    );
    assert_eq!(
        produced.as_bytes(),
        fixture_bytes("expected-token-monitor-snapshot.csv"),
        "snapshot CSV drifted from the documented shape"
    );
}

#[test]
fn the_daily_csv_matches_the_golden_file() {
    let produced = csv::render(
        &rows::DAILY_COLUMNS,
        documented_daily_rows().iter().map(|row| {
            vec![
                row.date.clone(),
                row.tool.clone(),
                csv::format_count(row.tokens),
                row.cost_usd.clone(),
            ]
        }),
    );
    assert_eq!(
        produced.as_bytes(),
        fixture_bytes("expected-token-monitor-daily.csv"),
        "daily CSV drifted from the documented shape"
    );
}

#[test]
fn the_daily_models_csv_matches_the_golden_file() {
    let produced = csv::render(
        &rows::DAILY_MODEL_COLUMNS,
        documented_daily_model_rows().iter().map(|row| {
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
    );
    assert_eq!(
        produced.as_bytes(),
        fixture_bytes("expected-token-monitor-daily-models.csv"),
        "daily-models CSV drifted from the documented shape"
    );
}

/// Compare JSON documents for equality of *content*.
///
/// Key order is deliberately not compared: `serde_json` writes object keys
/// sorted, while the reference's `JSON.stringify` writes them in insertion order.
/// Both are the same JSON document, and no consumer can tell — so the golden
/// asserts values, nesting, and shape, plus the exact key names and the 2-space
/// indent separately. Indentation *is* part of the file's shape, so it is checked
/// directly rather than through a parser.
fn assert_json_equivalent(produced: &str, golden: &str) {
    let produced_value: serde_json::Value =
        serde_json::from_str(produced).expect("produced JSON parses");
    let golden_value: serde_json::Value = serde_json::from_str(golden).expect("golden JSON parses");
    assert_eq!(
        produced_value, golden_value,
        "export JSON values drifted from the documented shape"
    );
    // Indentation: every continuation line starts with a multiple of two spaces.
    for line in produced.lines().skip(1) {
        let indent = line.len() - line.trim_start().len();
        assert_eq!(indent % 2, 0, "not 2-space indented: {line:?}");
    }
    assert!(
        produced.ends_with('\n'),
        "the file needs a trailing newline"
    );
    assert!(!produced.contains('\r'), "JSON must use LF, not CRLF");
}

#[test]
fn the_json_document_matches_the_golden_file() {
    let produced = token_monitor_lite::export::json::render(&documented_document());
    assert_json_equivalent(&produced, &fixture("expected-token-monitor-export.json"));
}

#[test]
fn the_json_wire_keys_are_the_reference_spelling() {
    // The keys, not just the values: a consumer written against an upstream
    // export must be able to read this one.
    let produced = token_monitor_lite::export::json::render(&documented_document());
    for key in [
        "\"generatedAt\"",
        "\"app\"",
        "\"snapshot\"",
        "\"today\"",
        "\"month\"",
        "\"allTime\"",
        "\"daily\"",
        "\"monthly\"",
        "\"perClient\"",
        "\"perModel\"",
        "\"outputTokens\"",
        "\"cacheReadTokens\"",
        "\"cacheWriteTokens\"",
        "\"unclassifiedTokens\"",
    ] {
        assert!(produced.contains(key), "missing wire key {key}");
    }
    // And none of the snake_case spellings leaked out.
    for wrong in ["\"all_time\"", "\"output_tokens\"", "\"per_client\""] {
        assert!(!produced.contains(wrong), "leaked snake_case key {wrong}");
    }
}

#[test]
fn the_dossiers_component_split_reproduces_the_documented_rows() {
    // The goldens above are literal. This proves the *rules* produce them from
    // input, so the golden cannot pass while the split is wrong.
    let gpt5 = ModelDay {
        tokens: 5,
        output_tokens: 2,
        cache_read_tokens: 1,
        cache_write_tokens: 0,
        unclassified_tokens: None,
        cost: usd(1_000_000_000),
    };
    let split = rows::components(true, &gpt5);
    assert_eq!(split.input, 2);
    assert_eq!(split.output, 2);
    assert_eq!(split.cache_read, 1);
    assert_eq!(split.unclassified, 0);
    assert_eq!(split.total, 5);

    // `opus`: 12 total, 2 output, no other components available, so 10 is the
    // unknown remainder and must NOT be reported as input.
    let opus = ModelDay {
        tokens: 12,
        output_tokens: 2,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        unclassified_tokens: None,
        cost: usd(2_000_000_000),
    };
    let split = rows::components(false, &opus);
    assert_eq!(split.input, 0, "the remainder was guessed as input");
    assert_eq!(split.unclassified, 10);
    assert_eq!(split.total, 12);
}

#[test]
fn the_golden_fixture_is_not_empty() {
    // A golden file that silently became empty would make every comparison pass
    // against an empty render, so pin the minimum shape.
    for name in [
        "expected-token-monitor-snapshot.csv",
        "expected-token-monitor-daily.csv",
        "expected-token-monitor-daily-models.csv",
    ] {
        let bytes = fixture_bytes(name);
        assert!(bytes.len() > 60, "{name} is suspiciously small");
        assert!(bytes.starts_with(b"\xEF\xBB\xBF"), "{name} lost its BOM");
        assert!(bytes.ends_with(b"\r\n"), "{name} lost its trailing CRLF");
    }
    let json = fixture("expected-token-monitor-export.json");
    assert!(json.contains("\"generatedAt\""));
    assert!(json.contains("\"monthly\""));
    assert!(json.ends_with('\n'));
}

#[test]
fn the_full_file_set_matches_all_four_goldens() {
    // The end-to-end path: one document plus rows produces exactly the four
    // documented files, and each one is byte-identical.
    let files = file_set(
        &documented_document(),
        &documented_snapshot_rows(),
        &documented_daily_rows(),
        &documented_daily_model_rows(),
    );
    assert_eq!(
        files.iter().map(|file| file.name).collect::<Vec<_>>(),
        EXPORT_FILENAMES.to_vec()
    );
    let expected = [
        ("expected-token-monitor-export.json", false),
        ("expected-token-monitor-snapshot.csv", true),
        ("expected-token-monitor-daily.csv", true),
        ("expected-token-monitor-daily-models.csv", true),
    ];
    for (file, (golden, is_csv)) in files.iter().zip(expected) {
        if is_csv {
            assert_eq!(
                file.contents.as_bytes(),
                fixture_bytes(golden),
                "{} drifted",
                file.name
            );
        } else {
            assert_json_equivalent(&file.contents, &fixture(golden));
        }
    }
}

#[test]
fn the_export_carries_no_identity_anywhere() {
    // The privacy promise, checked over every byte of every file.
    let files = file_set(
        &documented_document(),
        &documented_snapshot_rows(),
        &documented_daily_rows(),
        &documented_daily_model_rows(),
    );
    let forbidden = [
        "deviceId",
        "device_id",
        "hostname",
        "hostName",
        "accountEmail",
        "account_email",
        "accountKey",
        "planLabel",
        "plan_label",
        "workspace",
        "credential",
        "limits",
        "quota",
    ];
    for file in &files {
        for needle in forbidden {
            assert!(
                !file.contents.contains(needle),
                "{} contains identity field {needle}",
                file.name
            );
        }
    }
}

#[test]
fn a_quoted_field_survives_the_whole_pipeline() {
    // A model name with a comma and a quote: the golden files do not cover this,
    // so assert the escaping directly end to end.
    let document = documented_document();
    let files = file_set(
        &document,
        &[SnapshotRow {
            period: "today",
            dimension: "model",
            name: "weird,\"model\"".to_owned(),
            tokens: 1,
            cost_usd: "0".to_owned(),
        }],
        &[],
        &[],
    );
    let snapshot = files
        .iter()
        .find(|file| file.name == "token-monitor-snapshot.csv")
        .expect("snapshot");
    assert!(
        snapshot.contents.contains("\"weird,\"\"model\"\"\""),
        "the field was not RFC 4180 quoted: {:?}",
        snapshot.contents
    );
}

#[test]
fn the_daily_files_are_absent_when_there_is_no_history() {
    let files = file_set(
        &documented_document(),
        &documented_snapshot_rows(),
        &[],
        &[],
    );
    let names = files.iter().map(|file| file.name).collect::<Vec<_>>();
    assert_eq!(
        names,
        vec!["token-monitor-export.json", "token-monitor-snapshot.csv"]
    );
}

#[test]
fn the_snapshot_rows_builder_reproduces_the_golden_ordering() {
    // Build rows from period totals rather than literals, so the ordering rule
    // is exercised.
    let periods = [
        PeriodTotalsRow {
            tools: vec![SnapshotEntry {
                name: "codex".to_owned(),
                tokens: 20,
                cost: usd(2_000_000_000),
            }],
            models: vec![SnapshotEntry {
                name: "gpt-5".to_owned(),
                tokens: 20,
                cost: usd(2_000_000_000),
            }],
        },
        PeriodTotalsRow {
            tools: vec![SnapshotEntry {
                name: "codex".to_owned(),
                tokens: 0,
                cost: usd(0),
            }],
            models: Vec::new(),
        },
        PeriodTotalsRow {
            tools: vec![SnapshotEntry {
                name: "codex".to_owned(),
                tokens: 100,
                cost: usd(9_000_000_000),
            }],
            models: Vec::new(),
        },
    ];
    let built = rows::snapshot_rows(&periods);
    assert_eq!(built, documented_snapshot_rows());
}

#[test]
fn per_client_map_order_does_not_change_the_bytes() {
    // A BTreeMap makes this true by construction; the test exists so a future
    // switch to a HashMap cannot silently break the golden comparison.
    let mut first = documented_document();
    let mut second = documented_document();
    let mut reordered: BTreeMap<String, JsonSlice> = BTreeMap::new();
    reordered.insert(
        "codex".to_owned(),
        JsonSlice {
            tokens: 10,
            cost: 1.0,
            ..JsonSlice::default()
        },
    );
    reordered.insert(
        "claude-code".to_owned(),
        JsonSlice {
            tokens: 7,
            cost: 0.75,
            ..JsonSlice::default()
        },
    );
    first.monthly[0].per_client = reordered;
    second.monthly[0].per_client = BTreeMap::new();
    second.monthly[0].per_client.insert(
        "claude-code".to_owned(),
        JsonSlice {
            tokens: 7,
            cost: 0.75,
            ..JsonSlice::default()
        },
    );
    second.monthly[0].per_client.insert(
        "codex".to_owned(),
        JsonSlice {
            tokens: 10,
            cost: 1.0,
            ..JsonSlice::default()
        },
    );
    assert_eq!(
        token_monitor_lite::export::json::render(&first),
        token_monitor_lite::export::json::render(&second)
    );
}
