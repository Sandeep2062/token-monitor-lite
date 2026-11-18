//! Appearance settings through the revisioned editor: draft, validate, conflict.
//!
//! These use the real `SettingsEditor` over a real store, because the revision CAS
//! is the property under test and a fake editor would not prove it.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use token_monitor_lite::application::{SettingsEditor, SettingsField, SettingsReason};
use token_monitor_lite::currency::Currency;
use token_monitor_lite::discovery::{HostPaths, HostPlatform, discover};
use token_monitor_lite::settings::{
    Density, Language, MotionPreference, SettingsValues, ThemeMode,
};
use token_monitor_lite::storage::{NativeSettings, StoreHandle};

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Tree(PathBuf);

impl Tree {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "token-monitor-appearance-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn paths(&self) -> HostPaths {
        HostPaths {
            platform: HostPlatform::Linux,
            home: self.0.clone(),
            app_data: None,
            local_app_data: None,
            xdg_config_home: None,
            xdg_data_home: None,
            xdg_state_home: None,
            legacy_shared_override: Some(self.0.join("legacy-shared")),
            native_data_override: Some(self.0.join("native-private")),
            enabled_clients: BTreeSet::from(["claude".to_owned()]),
            env_paths: BTreeMap::new(),
        }
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Build an editor whose stored document is `json`.
fn editor(tree: &Tree, json: &str) -> (SettingsEditor, StoreHandle, u64) {
    let discovery = discover(&tree.paths()).unwrap();
    let (store, _) = StoreHandle::open(discovery.native.clone()).unwrap();
    let stored = store
        .save_settings(0, NativeSettings::from_json_bytes(json.as_bytes()).unwrap())
        .unwrap();
    let editor = SettingsEditor::new(&discovery, &stored).unwrap();
    let revision = editor.view().revision;
    (editor, store, revision)
}

#[test]
fn the_editor_reads_the_appearance_fields() {
    let tree = Tree::new();
    let (editor, store, _) = editor(
        &tree,
        r#"{"clients":"claude","timezone":"UTC","themeMode":"light","density":"compact",
            "reduceMotion":"on","language":"ko","currency":"HKD"}"#,
    );
    let appearance = &editor.view().appearance;
    assert_eq!(appearance.theme, ThemeMode::Light);
    assert_eq!(appearance.density, Density::Compact);
    assert_eq!(appearance.motion, MotionPreference::On);
    assert_eq!(appearance.language, Language::Ko);
    assert_eq!(appearance.currency, Currency::Hkd);
    assert!(appearance.unreadable.is_empty());
    store.shutdown().unwrap();
}

#[test]
fn an_unreadable_appearance_value_is_reported_and_defaulted() {
    let tree = Tree::new();
    let (editor, store, _) = editor(&tree, r#"{"themeMode":"midnight","currency":"EUR"}"#);
    let appearance = &editor.view().appearance;
    assert_eq!(appearance.theme, ThemeMode::System);
    assert_eq!(appearance.currency, Currency::Usd);
    assert_eq!(appearance.unreadable, vec!["themeMode", "currency"]);
    store.shutdown().unwrap();
}

#[test]
fn a_draft_copies_the_committed_appearance_and_editing_it_does_not_leak_back() {
    let tree = Tree::new();
    let (editor, store, _) = editor(&tree, r#"{"themeMode":"dark"}"#);
    let mut draft = editor.open_draft();
    assert_eq!(draft.appearance.theme, ThemeMode::Dark);
    draft.appearance.theme = ThemeMode::Light;
    assert_eq!(
        editor.view().appearance.theme,
        ThemeMode::Dark,
        "editing the draft changed the committed view"
    );
    store.shutdown().unwrap();
}

#[test]
fn a_stale_revision_is_refused_rather_than_overwriting() {
    let tree = Tree::new();
    let (editor, store, revision) = editor(&tree, "{}");
    let mut draft = editor.open_draft();
    draft.base_revision = revision.saturating_sub(1);
    draft.appearance.theme = ThemeMode::Light;
    let error = editor.validate(&draft).expect_err("stale must fail");
    assert!(
        error.has(SettingsField::Revision, SettingsReason::Stale),
        "a stale revision must be reported as stale"
    );
    store.shutdown().unwrap();
}

#[test]
fn the_current_revision_saves_the_appearance_and_keeps_unknown_fields() {
    let tree = Tree::new();
    let (editor, store, _) = editor(&tree, r#"{"clients":"claude","future":{"value":null}}"#);
    let mut draft = editor.open_draft();
    draft.appearance = SettingsValues {
        theme: ThemeMode::Light,
        density: Density::Compact,
        motion: MotionPreference::Off,
        language: Language::PtBr,
        currency: Currency::Cny,
        unreadable: Vec::new(),
    };
    let settings = editor.validate(&draft).expect("valid");
    let value: serde_json::Value = serde_json::from_slice(settings.json_bytes()).unwrap();
    assert_eq!(value["themeMode"], "light");
    assert_eq!(value["density"], "compact");
    assert_eq!(value["reduceMotion"], "off");
    assert_eq!(value["language"], "pt-BR");
    assert_eq!(value["currency"], "CNY");
    // The field this app does not own is still there, null and all.
    assert_eq!(value["future"], serde_json::json!({"value": null}));
    assert_eq!(value["clients"], "claude");
    store.shutdown().unwrap();
}

#[test]
fn saving_appearance_alone_does_not_touch_the_timezone() {
    let tree = Tree::new();
    let (editor, store, _) = editor(&tree, r#"{"clients":"claude","timezone":"Europe/Berlin"}"#);
    let mut draft = editor.open_draft();
    draft.appearance.theme = ThemeMode::Light;
    let settings = editor.validate(&draft).expect("valid");
    let value: serde_json::Value = serde_json::from_slice(settings.json_bytes()).unwrap();
    assert_eq!(value["timezone"], "Europe/Berlin");
    store.shutdown().unwrap();
}

#[test]
fn a_cancel_leaves_the_committed_document_untouched() {
    // Cancel is the absence of a save: re-reading the view must show the stored
    // values, and the store must still hold the original bytes.
    let tree = Tree::new();
    let (editor, store, _) = editor(&tree, r#"{"themeMode":"dark"}"#);
    let mut draft = editor.open_draft();
    draft.appearance.theme = ThemeMode::Light;
    draft.appearance.currency = Currency::Twd;
    // No validate() call: the draft is dropped, which is what Cancel does.
    drop(draft);
    assert_eq!(editor.view().appearance.theme, ThemeMode::Dark);
    assert_eq!(editor.view().appearance.currency, Currency::Usd);
    store.shutdown().unwrap();
}

#[test]
fn repeated_reads_are_stable() {
    let tree = Tree::new();
    let (editor, store, _) = editor(&tree, r#"{"themeMode":"system"}"#);
    let first = editor.view().appearance.clone();
    for _ in 0..5 {
        assert_eq!(editor.view().appearance, first);
    }
    store.shutdown().unwrap();
}
