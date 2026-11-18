//! Read-only settings inspection. Captured values may include legacy secrets:
//! keep them in the backend, never send this object to a UI or sync endpoint.

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::Path;

const MAX_SETTINGS_BYTES: usize = 4 * 1024 * 1024;
const CREDENTIAL_SETTING_FIELDS: &[&str] = &[
    "alibabaCookie",
    "claudeWebCookie",
    "clineApiKey",
    "commandcodeCookie",
    "copilotApiToken",
    "deepseekApiKey",
    "devinBearerToken",
    "factoryApiKey",
    "hubHostSecret",
    "kimiApiKey",
    "kimiWebAccessToken",
    "manusSessionToken",
    "minimaxApiKey",
    "minimaxApiProfiles",
    "mistralCookie",
    "museWebCookie",
    "notionCookie",
    "ollamaApiKey",
    "ollamaCookie",
    "opencodeCookie",
    "opencodeProfiles",
    "opendesignCookie",
    "openrouterProfiles",
    "qoderCookie",
    "secret",
    "stepfunToken",
    "thirdPartyProfiles",
    "traeAccessToken",
    "traeCookie",
    "traeDeviceId",
    "typesafeCookie",
    "volcengineAccessKeyId",
    "volcengineAgentAccessKeyId",
    "volcengineAgentSecretAccessKey",
    "volcengineSecretAccessKey",
    "zaiApiKey",
    "zaiTeamApiKey",
    "zaiTeamOrganizationId",
    "zaiTeamProjectId",
    "zedCookie",
];

/// All settings keys survive roundtrip, including unknown keys and explicit
/// null. No first-run defaults or identity migrations run during inspection.
#[derive(Serialize, Deserialize)]
#[serde(try_from = "BTreeMap<String, Value>")]
pub struct Preferences {
    #[serde(flatten)]
    fields: BTreeMap<String, Value>,
}

impl TryFrom<BTreeMap<String, Value>> for Preferences {
    type Error = &'static str;

    fn try_from(fields: BTreeMap<String, Value>) -> Result<Self, Self::Error> {
        if fields
            .get("clients")
            .is_some_and(|value| !value.is_null() && !value.is_string())
        {
            return Err("clients must be a CSV string or null");
        }
        Ok(Self { fields })
    }
}

impl Preferences {
    pub fn clients(&self) -> Option<&str> {
        self.fields.get("clients").and_then(Value::as_str)
    }

    /// Read a string field by name.
    ///
    /// Read-only on purpose: writes go through [`Preferences::with_native_edits`],
    /// which clones the whole document before touching it. An API that could set
    /// one field would be an invitation to drop the others.
    pub fn get_str(&self, field: &str) -> Option<&str> {
        self.fields.get(field).and_then(Value::as_str)
    }

    /// Whether a field is present at all, even as `null`.
    ///
    /// Distinguishing "absent" from "present but unreadable" is what lets the
    /// settings surface report a default it is *using* without claiming the file
    /// agreed with it.
    pub fn has_field(&self, field: &str) -> bool {
        self.fields.contains_key(field)
    }

    pub fn has_clients(&self) -> bool {
        self.fields.contains_key("clients")
    }

    pub fn clients_is_null(&self) -> bool {
        self.fields.get("clients").is_some_and(Value::is_null)
    }

    pub fn recognized_credential_fields(&self) -> Vec<&str> {
        CREDENTIAL_SETTING_FIELDS
            .iter()
            .copied()
            .filter(|field| {
                self.fields
                    .get(*field)
                    .is_some_and(credential_value_present)
            })
            .collect()
    }

    /// Whether closing the window should hide it instead of quitting.
    ///
    /// Native-owned, like the timezone: absent or unreadable means the window
    /// close quits, which is the only behaviour that can never strand the app
    /// behind an icon that is not there.
    pub fn close_to_tray(&self) -> bool {
        self.fields
            .get("closeToTray")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    /// The folder auto-export writes to, when it is enabled.
    ///
    /// An absent or empty value means auto-export is off. The folder is a path,
    /// never a credential, so it lives in settings rather than the keyring — but
    /// it is never logged either, because a path can carry a person's name.
    pub fn export_folder(&self) -> Option<&str> {
        self.fields
            .get("exportFolder")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }

    /// The auto-export cadence in seconds, clamped to the documented range.
    ///
    /// `docs/export.md` offers 30 seconds to 60 minutes with a one-minute
    /// default. A stored value outside the range is clamped rather than rejected,
    /// so an edited settings file cannot make the app write every second.
    pub fn export_interval_seconds(&self) -> u64 {
        self.fields
            .get("exportIntervalSeconds")
            .and_then(Value::as_u64)
            .map(|seconds| {
                seconds.clamp(
                    crate::export::schedule::MIN_INTERVAL_SECONDS,
                    crate::export::schedule::MAX_INTERVAL_SECONDS,
                )
            })
            .unwrap_or(crate::export::schedule::DEFAULT_INTERVAL_SECONDS)
    }

    /// Replace only the native-owned editable fields. Every other imported
    /// value, including unknown objects and explicit nulls, is retained.
    pub fn with_native_edits(
        &self,
        enabled_clients: &[String],
        timezone: &str,
        selected_source_ids: &[String],
        close_to_tray: bool,
        export: &ExportEdits<'_>,
        appearance: &crate::settings::SettingsEdits,
    ) -> Result<Vec<u8>> {
        let mut fields = self.fields.clone();
        fields.insert(
            "clients".to_owned(),
            Value::String(enabled_clients.join(",")),
        );
        fields.insert("timezone".to_owned(), Value::String(timezone.to_owned()));
        fields.insert("closeToTray".to_owned(), Value::Bool(close_to_tray));
        fields.insert(
            "nativeSourceIds".to_owned(),
            Value::Array(
                selected_source_ids
                    .iter()
                    .cloned()
                    .map(Value::String)
                    .collect(),
            ),
        );
        // Auto-export is native-owned for the same reason the tray setting is:
        // it describes what this window does, not what the tool reports.
        fields.insert(
            "exportFolder".to_owned(),
            match export.folder {
                Some(folder) => Value::String(folder.to_owned()),
                None => Value::Null,
            },
        );
        fields.insert(
            "exportIntervalSeconds".to_owned(),
            Value::Number(crate::export::schedule::clamp_interval(export.interval_seconds).into()),
        );
        // Appearance, currency and language. Written as the same plain strings the
        // reference tool stores, so a settings file stays readable by both.
        fields.insert(
            crate::settings::THEME_FIELD.to_owned(),
            Value::String(appearance.theme.wire().to_owned()),
        );
        fields.insert(
            crate::settings::DENSITY_FIELD.to_owned(),
            Value::String(appearance.density.wire().to_owned()),
        );
        fields.insert(
            crate::settings::MOTION_FIELD.to_owned(),
            Value::String(appearance.motion.wire().to_owned()),
        );
        fields.insert(
            crate::settings::LANGUAGE_FIELD.to_owned(),
            Value::String(appearance.language.wire().to_owned()),
        );
        fields.insert(
            crate::settings::CURRENCY_FIELD.to_owned(),
            Value::String(appearance.currency.code().to_owned()),
        );
        serde_json::to_vec(&fields).context("could not encode native settings")
    }

    pub(crate) fn timezone(&self) -> Option<&str> {
        self.fields.get("timezone").and_then(Value::as_str)
    }

    pub(crate) fn native_source_ids(&self) -> Option<Vec<&str>> {
        self.fields
            .get("nativeSourceIds")?
            .as_array()
            .map(|values| values.iter().filter_map(Value::as_str).collect())
    }
}

/// The export fields a settings save writes.
///
/// A borrowed view so a save cannot accidentally move an owned path out of the
/// draft it was validating.
#[derive(Clone, Copy, Debug)]
pub struct ExportEdits<'a> {
    /// `None` turns auto-export off.
    pub folder: Option<&'a str>,
    pub interval_seconds: u64,
}

impl Default for ExportEdits<'_> {
    fn default() -> Self {
        Self {
            folder: None,
            interval_seconds: crate::export::schedule::DEFAULT_INTERVAL_SECONDS,
        }
    }
}

fn credential_value_present(value: &Value) -> bool {
    match value {
        Value::String(value) => !value.is_empty(),
        Value::Array(values) => !values.is_empty(),
        Value::Object(fields) => !fields.is_empty(),
        _ => false,
    }
}

impl fmt::Debug for Preferences {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Preferences { values: [redacted] }")
    }
}

pub struct ImportInspection {
    preferences: Preferences,
    original_bytes: Vec<u8>,
}

impl ImportInspection {
    pub fn preferences(&self) -> &Preferences {
        &self.preferences
    }

    pub fn bytes_read(&self) -> usize {
        self.original_bytes.len()
    }

    pub fn original_bytes(&self) -> &[u8] {
        &self.original_bytes
    }
}

impl fmt::Debug for ImportInspection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ImportInspection")
            .field("preferences", &self.preferences)
            .field("bytes_read", &self.original_bytes.len())
            .finish()
    }
}

/// Read only the explicitly selected file. Does not access credentials.json,
/// create a native store, change permissions or apply imported preferences.
pub fn inspect_settings(path: &Path) -> Result<ImportInspection> {
    let file = File::open(path).context("could not open selected settings file")?;
    let metadata = file
        .metadata()
        .context("could not inspect settings metadata")?;
    ensure!(metadata.is_file(), "selected settings source is not a file");
    ensure!(
        metadata.len() <= MAX_SETTINGS_BYTES as u64,
        "settings exceed 4 MiB limit"
    );
    // Enforce the bound on actual bytes too, in case the source grows after stat.
    let mut bytes = Vec::new();
    file.take(MAX_SETTINGS_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .context("could not read selected settings file")?;
    ensure!(
        bytes.len() <= MAX_SETTINGS_BYTES,
        "settings exceed 4 MiB limit"
    );
    // Keep serde diagnostics (which can echo user-controlled values) private.
    let preferences = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("invalid settings object or clients selection"))?;
    Ok(ImportInspection {
        preferences,
        original_bytes: bytes,
    })
}
