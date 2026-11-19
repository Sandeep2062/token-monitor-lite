//! Translation-catalogue tests.
//!
//! Slint bundles the `.po` files at compile time, but the catalogue is generated,
//! so the *generated* files need checking: a locale missing a message, or an
//! English table that drifted off the source strings, would ship silently.
//!
//! The generator reads the upstream reference at
//! `token-monitor/src/electron/renderer/i18n.js`; these tests read its output.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const LOCALES: [&str; 6] = ["en", "zh-TW", "zh-CN", "ko", "ja", "pt-BR"];

fn catalogue_path(locale: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("ui/i18n")
        .join(locale)
        .join("LC_MESSAGES/token-monitor-lite.po")
}

/// Parse the `msgid`/`msgstr` pairs out of a `.po` file.
///
/// Deliberately tiny: only the two directives the generator emits, with the
/// header entry skipped. A full gettext parser is not needed to prove parity.
fn parse_po(text: &str) -> BTreeMap<String, String> {
    let mut pairs = BTreeMap::new();
    let mut pending: Option<String> = None;
    let mut in_header = false;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("msgid ") {
            let value = unquote(rest);
            in_header = value.is_empty();
            pending = Some(value);
        } else if let Some(rest) = line.strip_prefix("msgstr ") {
            let value = unquote(rest);
            match pending.take() {
                Some(msgid) if !in_header => {
                    pairs.insert(msgid, value);
                }
                Some(_) | None => {}
            }
        }
    }
    pairs
}

fn unquote(value: &str) -> String {
    let trimmed = value.trim();
    let inner = trimmed
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(trimmed);
    inner
        .replace("\\n", "\n")
        .replace("\\t", "\t")
        .replace("\\\"", "\"")
        .replace("\\\\", "\\")
}

fn load(locale: &str) -> BTreeMap<String, String> {
    let path = catalogue_path(locale);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    parse_po(&text)
}

#[test]
fn every_locale_has_a_catalogue() {
    for locale in LOCALES {
        let path = catalogue_path(locale);
        assert!(path.is_file(), "missing catalogue for {locale}");
    }
}

#[test]
fn every_locale_carries_the_same_message_set() {
    let english = load("en");
    assert!(!english.is_empty(), "the English table is empty");
    for locale in LOCALES {
        let table = load(locale);
        let missing = english
            .keys()
            .filter(|key| !table.contains_key(*key))
            .collect::<Vec<_>>();
        assert!(missing.is_empty(), "{locale} is missing {missing:?}");
        let extra = table
            .keys()
            .filter(|key| !english.contains_key(*key))
            .collect::<Vec<_>>();
        assert!(extra.is_empty(), "{locale} has stray keys {extra:?}");
    }
}

#[test]
fn the_english_table_is_the_source_language() {
    // A "translation" of English into English would hide a generator bug, so the
    // English table must be exactly its own msgids.
    for (msgid, msgstr) in load("en") {
        assert_eq!(msgid, msgstr, "English drifted from its source string");
    }
}

#[test]
fn the_translated_locales_actually_translate() {
    // At least the section headings must differ from English, or the bundle would
    // be a no-op that still passes the parity check above.
    let english = load("en");
    for locale in ["zh-TW", "zh-CN", "ko", "ja", "pt-BR"] {
        let table = load(locale);
        let differing = table
            .iter()
            .filter(|(msgid, msgstr)| english.get(*msgid) != Some(*msgstr))
            .count();
        assert!(
            differing >= 15,
            "{locale} translated only {differing} of {} messages",
            table.len()
        );
    }
}

#[test]
fn the_section_strings_the_ui_uses_are_present() {
    // These are the `@tr()` source strings in `ui/app.slint`. A rename in Slint
    // without regenerating would leave English text in a Japanese window.
    let required = [
        "Appearance",
        "Theme",
        "Light",
        "Dark",
        "System",
        "Density",
        "Comfortable",
        "Compact",
        "Reduce Motion",
        "On",
        "Off",
        "Currency",
        "USD - US Dollar",
        "TWD - New Taiwan Dollar",
        "HKD - Hong Kong Dollar",
        "CNY - Chinese Yuan",
        "Language",
        "Interface language",
        "Auto (system)",
        "English",
    ];
    for locale in LOCALES {
        let table = load(locale);
        for key in required {
            assert!(
                table.contains_key(key),
                "{locale} is missing the source string {key:?}"
            );
        }
    }
}

#[test]
fn the_language_choices_are_named_in_their_own_language() {
    // An interface language menu is unreadable if the options are translated:
    // someone looking for 日本語 must see 日本語 whatever the current language is.
    for locale in LOCALES {
        let table = load(locale);
        assert_eq!(table["繁體中文"], "繁體中文");
        assert_eq!(table["简体中文"], "简体中文");
        assert_eq!(table["한국어"], "한국어");
        assert_eq!(table["日本語"], "日本語");
    }
}

#[test]
fn the_currency_source_strings_match_the_reference() {
    // `settings.currency.*` in the upstream catalogue.
    let english = load("en");
    assert_eq!(english["USD - US Dollar"], "USD - US Dollar");
    assert_eq!(
        english["TWD - New Taiwan Dollar"],
        "TWD - New Taiwan Dollar"
    );
    assert_eq!(english["HKD - Hong Kong Dollar"], "HKD - Hong Kong Dollar");
    assert_eq!(english["CNY - Chinese Yuan"], "CNY - Chinese Yuan");
}

#[test]
fn no_message_is_an_untranslated_placeholder() {
    // A blank msgstr would render an empty label rather than falling back.
    for locale in LOCALES {
        for (msgid, msgstr) in load(locale) {
            assert!(!msgstr.trim().is_empty(), "{locale}: {msgid:?} is blank");
        }
    }
}

#[test]
fn the_generator_is_committed_so_the_bundle_is_reproducible() {
    // The `.po` files are generated, so without the generator they would be
    // unmaintainable artifacts.
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let generator = root.join("scripts/generate-settings-i18n.py");
    assert!(generator.is_file(), "the i18n generator is missing");
}

#[test]
fn the_bundle_does_not_depend_on_the_uncommitted_reference_tree() {
    // `token-monitor/` is gitignored reference material: it exists on a
    // developer's machine and never in a CI checkout. Anything the build needs
    // must be committed, so the generated catalogs are what the build reads — the
    // generator only needs the reference when it is *run* to regenerate them.
    //
    // This test exists because the assertion it replaces checked for the
    // reference tree and therefore failed on every clean CI run.
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for locale in LOCALES {
        let path = catalogue_path(locale);
        assert!(
            path.is_file(),
            "{} must be committed: the build reads it, and the reference tree is not in CI",
            path.display()
        );
    }
}
