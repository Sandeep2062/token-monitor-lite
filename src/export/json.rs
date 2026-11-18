//! The JSON export document and the change signature.
//!
//! Ports `renderExportJson`, `stableStringify` and `exportSignature` from
//! `src/shared/exporter.js`. See `docs/migration/export-spec.md` §3.4 and §5.

use serde::Serialize;
use serde_json::Value;

use crate::analytics::DailyUsage;

/// The app identity the reference writes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AppIdentity {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

impl Default for AppIdentity {
    fn default() -> Self {
        Self {
            name: "token-monitor".to_owned(),
            version: None,
        }
    }
}

/// One day in the JSON series.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JsonDaily {
    pub date: String,
    pub tokens: u128,
    /// USD, as a plain number.
    pub cost: f64,
    pub per_client: std::collections::BTreeMap<String, JsonSlice>,
    pub per_model: std::collections::BTreeMap<String, JsonSlice>,
}

/// A per-client or per-model slice of a day or month.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JsonSlice {
    pub tokens: u128,
    pub cost: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unclassified_tokens: Option<u128>,
}

/// One month in the JSON series.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JsonMonthly {
    pub month: String,
    pub tokens: u128,
    pub cost: f64,
    pub per_client: std::collections::BTreeMap<String, JsonSlice>,
    pub per_model: std::collections::BTreeMap<String, JsonSlice>,
}

/// The whole document.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportDocument {
    pub generated_at: String,
    pub app: AppIdentity,
    pub snapshot: SnapshotDocument,
    pub daily: Vec<JsonDaily>,
    pub monthly: Vec<JsonMonthly>,
}

/// The three periods.
///
/// These are `serde_json::Value` rather than a typed struct on purpose: the
/// reference is lossless here, passing the period object through untouched. The
/// fields we are allowed to hand over are decided upstream, not by this type.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct SnapshotDocument {
    pub today: Value,
    pub month: Value,
    /// `allTime`, not `all_time`: the wire key is camelCase and an upstream
    /// export uses it, so a consumer written against either must agree.
    #[serde(rename = "allTime")]
    pub all_time: Value,
}

/// Render the document the way the reference does: 2-space indent, trailing
/// newline.
///
/// The indent and the trailing newline are both part of the file's shape, and a
/// golden-file test compares them byte for byte.
pub fn render(document: &ExportDocument) -> String {
    let mut text = serde_json::to_string_pretty(document).unwrap_or_else(|_| "{}".to_owned());
    text.push('\n');
    text
}

/// A deterministic rendering of a value: object keys sorted at every depth.
///
/// Equality of two documents must not depend on map insertion order, or a
/// signature would flap and rewrite an unchanged folder — exactly the bug the
/// signature exists to prevent.
pub fn stable_stringify(value: &Value) -> String {
    match value {
        Value::Array(items) => {
            let rendered = items
                .iter()
                .map(stable_stringify)
                .collect::<Vec<_>>()
                .join(",");
            format!("[{rendered}]")
        }
        Value::Object(fields) => {
            let mut keys = fields.keys().collect::<Vec<_>>();
            keys.sort();
            let rendered = keys
                .into_iter()
                .map(|key| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(key).unwrap_or_default(),
                        stable_stringify(&fields[key])
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{rendered}}}")
        }
        Value::Null => "null".to_owned(),
        other => other.to_string(),
    }
}

/// The change signature: everything meaningful, nothing volatile.
///
/// `generated_at` and the app version are excluded, so a clock tick alone can
/// never cause a write. This is the value compared against the last written one
/// to skip an unchanged export.
pub fn signature(document: &ExportDocument) -> String {
    let meaningful = serde_json::json!({
        "snapshot": {
            "today": document.snapshot.today,
            "month": document.snapshot.month,
            "allTime": document.snapshot.all_time,
        },
        "daily": document.daily,
        "monthly": document.monthly,
    });
    stable_stringify(&meaningful)
}

/// A USD amount as a JSON number.
///
/// `nano_usd` is converted through a decimal string rather than an `f64` division
/// so a value like 0.1 does not drift by one ulp into `0.09999999999999999` in a
/// file a person may read.
pub fn usd(nano_usd: i128) -> f64 {
    super::csv::format_usd(nano_usd).parse().unwrap_or(0.0)
}

/// One day rendered for the JSON series.
pub fn daily_entry(
    day: &DailyUsage,
    per_client: std::collections::BTreeMap<String, JsonSlice>,
    per_model: std::collections::BTreeMap<String, JsonSlice>,
    cost_nano: i128,
) -> JsonDaily {
    JsonDaily {
        date: super::rows::date_text(day),
        tokens: day.total,
        cost: usd(cost_nano),
        per_client,
        per_model,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn document(tokens: u128) -> ExportDocument {
        ExportDocument {
            generated_at: "2026-07-03T14:30:00.000Z".to_owned(),
            app: AppIdentity::default(),
            snapshot: SnapshotDocument {
                today: json!({"total": tokens}),
                month: json!({"total": tokens}),
                all_time: json!({"total": tokens}),
            },
            daily: Vec::new(),
            monthly: Vec::new(),
        }
    }

    #[test]
    fn the_document_is_two_space_indented_with_a_trailing_newline() {
        let text = render(&document(5));
        assert!(text.ends_with('\n'), "the file needs a trailing newline");
        assert!(!text.ends_with("\n\n"));
        assert!(text.contains("\n  \"generatedAt\""), "not 2-space indented");
        // serde uses \n; the reference's JSON.stringify does too.
        assert!(!text.contains('\r'));
    }

    #[test]
    fn the_document_carries_no_identity_field() {
        let text = render(&document(5));
        for forbidden in [
            "deviceId",
            "device_id",
            "hostname",
            "accountEmail",
            "account_email",
            "planLabel",
            "limits",
        ] {
            assert!(!text.contains(forbidden), "export leaked {forbidden}");
        }
    }

    #[test]
    fn stable_stringify_sorts_keys_at_every_depth() {
        let left = json!({"b": 1, "a": {"y": 2, "x": 3}});
        let right = json!({"a": {"x": 3, "y": 2}, "b": 1});
        assert_eq!(stable_stringify(&left), stable_stringify(&right));
        assert_eq!(stable_stringify(&left), r#"{"a":{"x":3,"y":2},"b":1}"#);
    }

    #[test]
    fn stable_stringify_handles_arrays_and_nulls() {
        assert_eq!(stable_stringify(&json!([3, 1, 2])), "[3,1,2]");
        assert_eq!(stable_stringify(&json!(null)), "null");
        assert_eq!(stable_stringify(&json!("x")), "\"x\"");
    }

    #[test]
    fn the_signature_ignores_generated_at_and_version() {
        let mut first = document(5);
        let mut second = document(5);
        second.generated_at = "2030-01-01T00:00:00.000Z".to_owned();
        second.app.version = Some("9.9.9".to_owned());
        first.app.version = Some("0.0.1".to_owned());
        assert_eq!(
            signature(&first),
            signature(&second),
            "a clock tick must not change the signature"
        );
    }

    #[test]
    fn the_signature_changes_when_the_payload_changes() {
        assert_ne!(signature(&document(5)), signature(&document(6)));
    }

    #[test]
    fn the_signature_is_stable_regardless_of_map_order() {
        let mut left = document(5);
        let mut right = document(5);
        left.daily.push(JsonDaily {
            date: "2026-07-03".to_owned(),
            tokens: 1,
            cost: 0.5,
            per_client: [(
                "codex".to_owned(),
                JsonSlice {
                    tokens: 1,
                    cost: 0.5,
                    ..JsonSlice::default()
                },
            )]
            .into_iter()
            .collect(),
            per_model: Default::default(),
        });
        right.daily.push(JsonDaily {
            date: "2026-07-03".to_owned(),
            tokens: 1,
            cost: 0.5,
            per_client: [(
                "codex".to_owned(),
                JsonSlice {
                    tokens: 1,
                    cost: 0.5,
                    ..JsonSlice::default()
                },
            )]
            .into_iter()
            .collect(),
            per_model: Default::default(),
        });
        assert_eq!(signature(&left), signature(&right));
    }

    #[test]
    fn usd_renders_a_clean_decimal() {
        assert_eq!(usd(0), 0.0);
        assert_eq!(usd(2_000_000_000), 2.0);
        assert_eq!(usd(100_000_000), 0.1);
    }
}
