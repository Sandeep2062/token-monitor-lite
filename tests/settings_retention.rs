//! Settings retention: a save must not disturb what it does not own.
//!
//! The settings file is shared — with the reference tool, with other clients'
//! settings, and with a person who edits it by hand. Anything this app does not
//! understand has to come back out unchanged, or a save silently destroys another
//! tool's configuration.
//!
//! "Byte-exact" here means *semantically* identical: same keys, same values, same
//! nesting, including explicit nulls and unknown objects. The bytes are
//! re-serialized, so key order inside an object and insignificant whitespace may
//! differ — that is inherent to writing JSON through a map, and no JSON consumer
//! can observe it. A test below pins that distinction so the claim is not
//! over-stated.

use std::collections::BTreeMap;

use token_monitor_lite::config::{ExportEdits, Preferences};
use token_monitor_lite::currency::Currency;
use token_monitor_lite::settings::{
    Density, Language, MotionPreference, SettingsEdits, SettingsValues, ThemeMode,
};

fn preferences(json: &str) -> Preferences {
    serde_json::from_str(json).expect("preferences")
}

fn edits(theme: ThemeMode, density: Density, motion: MotionPreference) -> SettingsEdits {
    SettingsEdits {
        theme,
        density,
        motion,
        language: Language::Ja,
        currency: Currency::Twd,
    }
}

fn save(input: &str, appearance: SettingsEdits) -> serde_json::Value {
    let merged = preferences(input)
        .with_native_edits(&[], "UTC", &[], false, &ExportEdits::default(), &appearance)
        .expect("encode");
    serde_json::from_slice(&merged).expect("decode")
}

#[test]
fn an_unknown_top_level_object_survives_a_save() {
    // A future option, or another tool's data.
    let value = save(
        r#"{"futureOption":{"enabled":true,"nested":{"deep":[1,2,3]}}}"#,
        SettingsEdits::default(),
    );
    assert_eq!(
        value["futureOption"],
        serde_json::json!({"enabled": true, "nested": {"deep": [1, 2, 3]}})
    );
}

#[test]
fn an_unknown_array_survives_a_save() {
    let value = save(
        r#"{"someList":["a","b",{"c":null}],"_meta":"keep me"}"#,
        SettingsEdits::default(),
    );
    assert_eq!(
        value["someList"],
        serde_json::json!(["a", "b", {"c": null}])
    );
    assert_eq!(value["_meta"], "keep me");
}

#[test]
fn an_explicit_null_survives_a_save() {
    // Explicit null and absent are different: a null may mean "cleared on
    // purpose" to whatever wrote it, so it must not be dropped.
    let value = save(r#"{"deliberatelyNull":null}"#, SettingsEdits::default());
    assert!(
        value.get("deliberatelyNull").is_some(),
        "an explicit null was dropped"
    );
    assert!(value["deliberatelyNull"].is_null());
}

#[test]
fn unknown_values_of_every_json_type_survive() {
    let input = r#"{
        "aString": "text",
        "aNumber": 42,
        "aFloat": 1.5,
        "aBool": true,
        "aNull": null,
        "anArray": [],
        "anObject": {}
    }"#;
    let value = save(input, SettingsEdits::default());
    let original: serde_json::Value = serde_json::from_str(input).expect("json");
    for key in [
        "aString", "aNumber", "aFloat", "aBool", "aNull", "anArray", "anObject",
    ] {
        assert_eq!(value[key], original[key], "type drifted for {key}");
    }
}

#[test]
fn the_whole_document_round_trips_except_the_fields_this_app_owns() {
    let input = r##"{
        "clients": "claude,codex",
        "timezone": "Europe/Berlin",
        "closeToTray": true,
        "futureOption": {"enabled": true},
        "plugins": {"example": {"token": "private-test-value"}},
        "themeColors": {"bg": "#ffffff", "fg": "#000000"},
        "vendorColors": {"openai": "#10a37f"}
    }"##;
    let original: serde_json::Value = serde_json::from_str(input).expect("json");
    let value = save(input, SettingsEdits::default());

    // Native-owned fields are the ones a save is allowed to touch.
    let owned = [
        "clients",
        "timezone",
        "closeToTray",
        "nativeSourceIds",
        "exportFolder",
        "exportIntervalSeconds",
        "themeMode",
        "density",
        "reduceMotion",
        "language",
        "currency",
    ];
    for (key, original_value) in original.as_object().expect("object") {
        if owned.contains(&key.as_str()) {
            continue;
        }
        assert_eq!(
            value.get(key),
            Some(original_value),
            "{key} did not survive a save"
        );
    }
}

#[test]
fn a_reference_owned_colour_set_is_left_alone() {
    // The reference records custom colours in `themeColors`. This app records a
    // mode instead. Choosing a mode must not delete the reference's colours.
    let value = save(
        r##"{"themeColors":{"bg":"#112233","accent":"#abcdef"}}"##,
        edits(ThemeMode::Light, Density::Compact, MotionPreference::On),
    );
    assert_eq!(
        value["themeColors"],
        serde_json::json!({"bg": "#112233", "accent": "#abcdef"})
    );
    assert_eq!(value["themeMode"], "light");
}

#[test]
fn a_save_writes_the_appearance_fields_as_plain_strings() {
    let value = save(
        "{}",
        edits(ThemeMode::Dark, Density::Compact, MotionPreference::Off),
    );
    assert_eq!(value["themeMode"], "dark");
    assert_eq!(value["density"], "compact");
    assert_eq!(value["reduceMotion"], "off");
    assert_eq!(value["language"], "ja");
    assert_eq!(value["currency"], "TWD");
}

#[test]
fn a_save_is_idempotent() {
    // Saving twice must not drift: a second pass over its own output is stable.
    let input = r#"{"futureOption":{"a":1},"clients":"claude"}"#;
    let first = save(
        input,
        edits(ThemeMode::Light, Density::Compact, MotionPreference::On),
    );
    let encoded = serde_json::to_string(&first).expect("encode");
    let second = save(
        &encoded,
        edits(ThemeMode::Light, Density::Compact, MotionPreference::On),
    );
    assert_eq!(first, second);
}

#[test]
fn an_unreadable_appearance_value_is_replaced_only_when_saved() {
    // Reading must not mutate, and saving an explicit choice is how a bad value is
    // repaired.
    let input = r#"{"themeMode":"midnight","futureOption":true}"#;
    let values = SettingsValues::read(&preferences(input));
    assert_eq!(values.theme, ThemeMode::System);
    assert_eq!(values.unreadable, vec!["themeMode"]);

    let value = save(
        input,
        edits(
            ThemeMode::Dark,
            Density::Comfortable,
            MotionPreference::System,
        ),
    );
    assert_eq!(value["themeMode"], "dark");
    assert_eq!(value["futureOption"], true);
}

#[test]
fn the_reference_field_names_are_the_ones_written() {
    // These are the keys `main.js` seeds, so a settings file stays interchangeable.
    let value = save("{}", SettingsEdits::default());
    for key in [
        "themeMode",
        "density",
        "reduceMotion",
        "language",
        "currency",
    ] {
        assert!(value.get(key).is_some(), "missing {key}");
    }
    assert_eq!(value["reduceMotion"], "system");
    assert_eq!(value["currency"], "USD");
}

#[test]
fn key_order_is_not_part_of_the_guarantee_but_key_set_is() {
    // Stated explicitly so the retention claim stays honest: the document is
    // re-encoded, so order can change. The *set* of keys and their values is what
    // is preserved.
    let input = r#"{"zebra":1,"alpha":2,"middle":{"y":1,"x":2}}"#;
    let value = save(input, SettingsEdits::default());
    let keys = value
        .as_object()
        .expect("object")
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted, "keys should be emitted in sorted order");
    // And the nested object kept both entries.
    assert_eq!(value["middle"], serde_json::json!({"y": 1, "x": 2}));
}

#[test]
fn a_document_that_is_not_an_object_is_refused_rather_than_flattened() {
    // `Preferences` deserializes through a map; an array must not silently become
    // an empty settings document.
    assert!(serde_json::from_str::<Preferences>("[]").is_err());
    assert!(serde_json::from_str::<Preferences>("\"text\"").is_err());
}

#[test]
fn the_reader_never_returns_a_borrowed_secret_field() {
    // The appearance reads are unrelated to credentials, but the same document
    // holds them; make sure the new accessors stay string-only and cannot be used
    // to walk into an object.
    let preferences = preferences(r#"{"apiKey":{"nested":"value"}}"#);
    assert_eq!(preferences.get_str("apiKey"), None);
    assert!(preferences.has_field("apiKey"));
    // And no appearance field reads an object as a value.
    let values = SettingsValues::read(&preferences);
    assert_eq!(values.unreadable, Vec::<&str>::new());
}

#[test]
fn retention_holds_for_a_document_with_many_unknown_keys() {
    // A stress version of the same guarantee: the map must not lose entries as it
    // grows.
    let mut input = BTreeMap::new();
    for index in 0..200 {
        input.insert(format!("unknown{index}"), serde_json::json!({"n": index}));
    }
    let encoded = serde_json::to_string(&input).expect("encode");
    let value = save(&encoded, SettingsEdits::default());
    for index in 0..200 {
        assert_eq!(
            value[format!("unknown{index}")],
            serde_json::json!({"n": index}),
            "unknown{index} was lost"
        );
    }
}
