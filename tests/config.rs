use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use token_monitor_lite::config::{Preferences, inspect_settings};

static NEXT_FILE: AtomicUsize = AtomicUsize::new(0);

struct SettingsFile(PathBuf);

impl SettingsFile {
    fn new(bytes: &[u8]) -> Self {
        let path = std::env::temp_dir().join(format!(
            "token-monitor-config-{}-{}.json",
            std::process::id(),
            NEXT_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        std::io::Write::write_all(&mut file, bytes).unwrap();
        Self(path)
    }
}

impl Drop for SettingsFile {
    fn drop(&mut self) {
        fs::remove_file(&self.0).unwrap();
    }
}

#[test]
fn inspection_preserves_unknown_keys_original_bytes_and_file_metadata() {
    let bytes = br#"{"clients":"claude,codex","futureOption":{"enabled":true},"plugins":{"example":{"token":"private-test-value"}}}"#;
    let file = SettingsFile::new(bytes);
    let before = fs::metadata(&file.0).unwrap();
    let inspection = inspect_settings(&file.0).unwrap();
    assert_eq!(inspection.preferences().clients(), Some("claude,codex"));
    assert_eq!(inspection.bytes_read(), bytes.len());
    let roundtrip = serde_json::to_value(inspection.preferences()).unwrap();
    assert_eq!(
        roundtrip,
        serde_json::from_slice::<serde_json::Value>(bytes).unwrap()
    );
    assert_eq!(fs::read(&file.0).unwrap(), bytes);
    let after = fs::metadata(&file.0).unwrap();
    assert_eq!(before.modified().unwrap(), after.modified().unwrap());
    assert_eq!(
        before.permissions().readonly(),
        after.permissions().readonly()
    );
    assert!(!format!("{inspection:?}").contains("private-test-value"));
}

#[test]
fn empty_missing_null_and_non_catalog_selections_roundtrip_without_defaults() {
    for (input, expected) in [
        (r#"{}"#, None),
        (r#"{"clients":null}"#, None),
        (r#"{"clients":""}"#, Some("")),
        (
            r#"{"clients":"future-reader,Claude,kilocode"}"#,
            Some("future-reader,Claude,kilocode"),
        ),
    ] {
        let prefs: Preferences = serde_json::from_str(input).unwrap();
        assert_eq!(prefs.clients(), expected);
        assert_eq!(
            serde_json::to_value(&prefs).unwrap(),
            serde_json::from_str::<serde_json::Value>(input).unwrap()
        );
    }
}

#[test]
fn malformed_or_non_object_settings_never_become_defaults() {
    for input in [
        b"{".as_slice(),
        b"null",
        b"[]",
        b"42",
        br#"{"clients":false}"#,
        br#"{"clients":["claude"]}"#,
        b"\xff",
    ] {
        let file = SettingsFile::new(input);
        assert!(inspect_settings(&file.0).is_err());
        assert_eq!(fs::read(&file.0).unwrap(), input);
    }
}

#[test]
fn selected_missing_file_and_directory_return_errors_without_creating_files() {
    let file = SettingsFile::new(b"{}");
    let missing = file.0.with_extension("missing");
    assert!(inspect_settings(&missing).is_err());
    assert!(!missing.exists());
    assert!(inspect_settings(&std::env::temp_dir()).is_err());
}

#[test]
fn size_limit_accepts_exact_boundary_and_rejects_one_more_byte() {
    let mut bytes = b"{}".to_vec();
    bytes.resize(4 * 1024 * 1024, b' ');
    let file = SettingsFile::new(&bytes);
    assert_eq!(inspect_settings(&file.0).unwrap().bytes_read(), bytes.len());
    bytes.push(b' ');
    fs::write(&file.0, &bytes).unwrap();
    assert!(inspect_settings(&file.0).is_err());
    assert_eq!(fs::metadata(&file.0).unwrap().len(), bytes.len() as u64);
}

#[test]
fn diagnostics_do_not_echo_legacy_credentials() {
    let file = SettingsFile::new(br#"{"clients":123,"secret":"sensitive-test-marker"}"#);
    let message = format!("{:#}", inspect_settings(&file.0).unwrap_err());
    assert!(!message.contains("sensitive-test-marker"));
    let prefs: Preferences = serde_json::from_str(r#"{"secret":"sensitive-test-marker"}"#).unwrap();
    assert!(!format!("{prefs:?}").contains("sensitive-test-marker"));
}

#[test]
fn clients_presence_and_recognized_credentials_are_inspected_without_values() {
    let missing = SettingsFile::new(br#"{"future":null}"#);
    let null = SettingsFile::new(br#"{"clients":null}"#);
    let empty = SettingsFile::new(br#"{"clients":""}"#);
    assert!(
        !inspect_settings(&missing.0)
            .unwrap()
            .preferences()
            .has_clients()
    );
    assert!(
        inspect_settings(&null.0)
            .unwrap()
            .preferences()
            .clients_is_null()
    );
    assert_eq!(
        inspect_settings(&empty.0).unwrap().preferences().clients(),
        Some("")
    );

    let credential = SettingsFile::new(
        br#"{"clients":"claude","claudeWebCookie":"CONFIG_SECRET_SENTINEL","future":null}"#,
    );
    let inspection = inspect_settings(&credential.0).unwrap();
    assert_eq!(
        inspection.preferences().recognized_credential_fields(),
        vec!["claudeWebCookie"]
    );
    assert_eq!(
        inspection.original_bytes(),
        fs::read(&credential.0).unwrap()
    );
    assert!(!format!("{inspection:?}").contains("CONFIG_SECRET_SENTINEL"));
}

#[test]
fn supported_native_edits_preserve_every_unedited_json_value() {
    let preferences: Preferences = serde_json::from_str(
        r#"{"clients":"claude","future":{"enabled":true,"value":null},"pluginState":[1,2]}"#,
    )
    .unwrap();
    let merged = preferences
        .with_native_edits(
            &["claude".to_owned(), "codex".to_owned()],
            "America/New_York",
            &["source-opaque-a".to_owned()],
        )
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&merged).unwrap();

    assert_eq!(value["clients"], "claude,codex");
    assert_eq!(value["timezone"], "America/New_York");
    assert_eq!(
        value["nativeSourceIds"],
        serde_json::json!(["source-opaque-a"])
    );
    assert_eq!(
        value["future"],
        serde_json::json!({"enabled":true,"value":null})
    );
    assert_eq!(value["pluginState"], serde_json::json!([1, 2]));
}
