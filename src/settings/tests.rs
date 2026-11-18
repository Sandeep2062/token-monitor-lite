//! Settings value tests: normalization, fallbacks, and byte-exact retention.

use super::*;
use crate::config::Preferences;

fn preferences(json: &str) -> Preferences {
    serde_json::from_str(json).expect("preferences")
}

#[test]
fn an_empty_document_yields_the_documented_defaults() {
    let values = SettingsValues::read(&preferences("{}"));
    assert_eq!(values.theme, ThemeMode::System);
    assert_eq!(values.density, Density::Comfortable);
    assert_eq!(values.motion, MotionPreference::System);
    assert_eq!(values.language, Language::En);
    assert_eq!(values.currency, crate::currency::Currency::Usd);
    assert!(values.unreadable.is_empty());
}

#[test]
fn the_defaults_match_the_reference_tool() {
    // `main.js` seeds `language: 'auto'` and `reduceMotion: 'system'`.
    assert_eq!(MotionPreference::default().wire(), "system");
    assert_eq!(Language::Auto.wire(), "auto");
    // Currency defaults to USD there too.
    assert_eq!(crate::currency::Currency::default().code(), "USD");
}

#[test]
fn each_field_reads_its_stored_value() {
    let values = SettingsValues::read(&preferences(
        r#"{"themeMode":"dark","density":"compact","reduceMotion":"off",
            "language":"ja","currency":"TWD"}"#,
    ));
    assert_eq!(values.theme, ThemeMode::Dark);
    assert_eq!(values.density, Density::Compact);
    assert_eq!(values.motion, MotionPreference::Off);
    assert_eq!(values.language, Language::Ja);
    assert_eq!(values.currency, crate::currency::Currency::Twd);
    assert!(values.unreadable.is_empty());
}

#[test]
fn an_unreadable_value_falls_back_and_is_reported() {
    // A settings file is editable by hand, so a bad enum must not break startup —
    // but it must not silently look accepted either.
    let values = SettingsValues::read(&preferences(
        r#"{"themeMode":"midnight","density":7,"reduceMotion":"maybe",
            "language":"fr","currency":"EUR"}"#,
    ));
    assert_eq!(values.theme, ThemeMode::System);
    assert_eq!(values.density, Density::Comfortable);
    assert_eq!(values.motion, MotionPreference::System);
    assert_eq!(values.language, Language::En);
    assert_eq!(values.currency, crate::currency::Currency::Usd);
    assert_eq!(
        values.unreadable,
        vec![
            THEME_FIELD,
            DENSITY_FIELD,
            MOTION_FIELD,
            LANGUAGE_FIELD,
            CURRENCY_FIELD
        ]
    );
}

#[test]
fn an_explicit_null_is_reported_as_unreadable_not_as_absent() {
    let values = SettingsValues::read(&preferences(r#"{"themeMode":null}"#));
    assert_eq!(values.theme, ThemeMode::System);
    assert_eq!(values.unreadable, vec![THEME_FIELD]);
}

#[test]
fn an_absent_field_is_not_reported_as_unreadable() {
    let values = SettingsValues::read(&preferences(r#"{"clients":"claude"}"#));
    assert!(values.unreadable.is_empty());
}

#[test]
fn theme_modes_map_to_the_expected_darkness() {
    assert!(ThemeMode::Dark.is_dark(false));
    assert!(!ThemeMode::Light.is_dark(true));
    assert!(ThemeMode::System.is_dark(true));
    assert!(!ThemeMode::System.is_dark(false));
}

#[test]
fn motion_resolution_mirrors_the_reference() {
    // `shouldReduceMotion`: on => true, off => false, system => the OS answer.
    assert!(MotionPreference::On.should_reduce(false));
    assert!(!MotionPreference::Off.should_reduce(true));
    assert!(MotionPreference::System.should_reduce(true));
    assert!(!MotionPreference::System.should_reduce(false));
}

#[test]
fn every_wire_value_round_trips_through_parse() {
    for mode in ThemeMode::ALL {
        assert_eq!(ThemeMode::parse(mode.wire()), Some(mode));
    }
    for density in Density::ALL {
        assert_eq!(Density::parse(density.wire()), Some(density));
    }
    for motion in MotionPreference::ALL {
        assert_eq!(MotionPreference::parse(motion.wire()), Some(motion));
    }
    for language in Language::ALL {
        assert_eq!(Language::parse(language.wire()), Some(language));
    }
    for currency in crate::currency::Currency::ALL {
        assert_eq!(
            crate::currency::Currency::parse(currency.code()),
            Some(currency)
        );
    }
}

#[test]
fn the_six_languages_are_the_upstream_catalogue() {
    let codes = Language::ALL.map(Language::catalogue);
    assert_eq!(
        codes,
        ["en", "en", "zh-TW", "zh-CN", "ko", "ja", "pt-BR"],
        "the catalogue directories must match the generated .po layout"
    );
}

#[test]
fn auto_resolves_against_the_system_locale() {
    assert_eq!(Language::Auto.resolve("ja-JP"), Language::Ja);
    assert_eq!(Language::Auto.resolve("pt_BR"), Language::PtBr);
    assert_eq!(Language::Auto.resolve("en-US"), Language::En);
    // An explicit choice is never overridden by the system.
    assert_eq!(Language::Ko.resolve("ja-JP"), Language::Ko);
    // An unknown locale falls back to English rather than a random catalogue.
    assert_eq!(Language::Auto.resolve("xx-YY"), Language::En);
}

#[test]
fn chinese_resolution_honours_the_script_subtag() {
    // Region alone is not enough: `zh-Hans-HK` is simplified in Hong Kong.
    assert_eq!(resolve_locale("zh-Hant-TW"), Some(Language::ZhTw));
    assert_eq!(resolve_locale("zh-Hans-CN"), Some(Language::ZhCn));
    assert_eq!(resolve_locale("zh-Hans-HK"), Some(Language::ZhCn));
    assert_eq!(resolve_locale("zh-Hant-SG"), Some(Language::ZhTw));
    assert_eq!(resolve_locale("zh-TW"), Some(Language::ZhTw));
    assert_eq!(resolve_locale("zh-CN"), Some(Language::ZhCn));
    assert_eq!(resolve_locale("zh"), Some(Language::ZhCn));
}

#[test]
fn density_changes_the_row_metrics() {
    let (compact_row, compact_gap) = Density::Compact.metrics();
    let (comfortable_row, comfortable_gap) = Density::Comfortable.metrics();
    assert!(compact_row < comfortable_row);
    assert!(compact_gap < comfortable_gap);
}
