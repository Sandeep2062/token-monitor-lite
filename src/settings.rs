//! Native settings values: theme, density, motion, currency, language.
//!
//! These are *reads and validated writes* over the existing settings document,
//! never a translation of it. The retention rule is the important part: a field
//! this app does not know about must survive a save byte for byte, because the
//! same file is shared with the reference tool and with other clients' settings.
//!
//! Values are validated on read as well as write. A settings file is a plain JSON
//! file a person can edit, so an unreadable enum must fall back to its documented
//! default rather than break the window.

use serde::{Deserialize, Serialize};

use crate::config::Preferences;

pub const THEME_FIELD: &str = "themeMode";
pub const DENSITY_FIELD: &str = "density";
pub const MOTION_FIELD: &str = "reduceMotion";
pub const CURRENCY_FIELD: &str = "currency";
pub const LANGUAGE_FIELD: &str = "language";
pub const TEXT_SIZE_FIELD: &str = "textSize";

/// Colour scheme preference.
///
/// `System` follows the desktop; the other two pin it. The reference tool records
/// a custom colour set instead (`themeColors`), which is a different feature and
/// is left untouched here.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ThemeMode {
    #[default]
    System,
    Light,
    Dark,
}

impl ThemeMode {
    pub const ALL: [Self; 3] = [Self::System, Self::Light, Self::Dark];

    /// The stored string. `system` on an absent or unreadable value, so a
    /// settings file with `"theme": "midnight"` still opens.
    pub fn wire(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "system" => Some(Self::System),
            "light" => Some(Self::Light),
            "dark" => Some(Self::Dark),
            _ => None,
        }
    }

    /// Whether this mode resolves to a dark palette, given the desktop's answer.
    pub fn is_dark(self, system_dark: bool) -> bool {
        match self {
            Self::System => system_dark,
            Self::Light => false,
            Self::Dark => true,
        }
    }
}

/// Row density. Two steps, matching the upstream `textSize` vocabulary's shape of
/// a small closed set rather than a free number.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Density {
    #[default]
    Comfortable,
    Compact,
}

impl Density {
    pub const ALL: [Self; 2] = [Self::Comfortable, Self::Compact];

    pub fn wire(self) -> &'static str {
        match self {
            Self::Comfortable => "comfortable",
            Self::Compact => "compact",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "comfortable" => Some(Self::Comfortable),
            "compact" => Some(Self::Compact),
            _ => None,
        }
    }

    /// Row height in logical pixels, and the gap between rows.
    pub fn metrics(self) -> (i32, i32) {
        match self {
            Self::Comfortable => (26, 4),
            Self::Compact => (20, 2),
        }
    }
}

/// Reduced-motion preference.
///
/// The three values are the upstream `motionPreference.js` ones, and `System` is
/// the default there too: an absent field must not force animation off.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum MotionPreference {
    #[default]
    System,
    On,
    Off,
}

impl MotionPreference {
    pub const ALL: [Self; 3] = [Self::System, Self::On, Self::Off];

    pub fn wire(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::On => "on",
            Self::Off => "off",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "system" => Some(Self::System),
            "on" => Some(Self::On),
            "off" => Some(Self::Off),
            _ => None,
        }
    }

    /// Resolve against the desktop's own preference. Mirrors
    /// `shouldReduceMotion(value, systemReduced)`.
    pub fn should_reduce(self, system_reduced: bool) -> bool {
        match self {
            Self::On => true,
            Self::Off => false,
            Self::System => system_reduced,
        }
    }
}

/// Interface language.
///
/// `Auto` follows the OS locale. The six explicit languages are exactly the
/// upstream catalogue's, so a stored value that the catalogue cannot serve is
/// rejected rather than silently left untranslated.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub enum Language {
    Auto,
    #[default]
    En,
    ZhTw,
    ZhCn,
    Ko,
    Ja,
    PtBr,
}

impl Language {
    pub const ALL: [Self; 7] = [
        Self::Auto,
        Self::En,
        Self::ZhTw,
        Self::ZhCn,
        Self::Ko,
        Self::Ja,
        Self::PtBr,
    ];

    /// The stored value. `auto` matches the upstream default.
    pub fn wire(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::En => "en",
            Self::ZhTw => "zh-TW",
            Self::ZhCn => "zh-CN",
            Self::Ko => "ko",
            Self::Ja => "ja",
            Self::PtBr => "pt-BR",
        }
    }

    /// The catalogue directory for this language, or the one an `Auto` value
    /// resolved to.
    pub fn catalogue(self) -> &'static str {
        match self {
            // `en` is the source language: Slint treats it as "no bundle".
            Self::Auto | Self::En => "en",
            Self::ZhTw => "zh-TW",
            Self::ZhCn => "zh-CN",
            Self::Ko => "ko",
            Self::Ja => "ja",
            Self::PtBr => "pt-BR",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "auto" => Some(Self::Auto),
            "en" => Some(Self::En),
            "zh-TW" => Some(Self::ZhTw),
            "zh-CN" => Some(Self::ZhCn),
            "ko" => Some(Self::Ko),
            "ja" => Some(Self::Ja),
            "pt-BR" => Some(Self::PtBr),
            _ => None,
        }
    }

    /// Resolve `Auto` against an OS locale tag such as `zh-Hant-TW`.
    pub fn resolve(self, system_locale: &str) -> Self {
        if self != Self::Auto {
            return self;
        }
        resolve_locale(system_locale).unwrap_or(Self::En)
    }
}

/// Map an OS locale tag to a catalogue language.
///
/// Script subtags are honoured for Chinese, because `zh-TW` and `zh-CN` are
/// different catalogues and guessing from the region alone is wrong for
/// `zh-Hans-HK` and `zh-Hant-SG`.
pub fn resolve_locale(tag: &str) -> Option<Language> {
    let normalised = tag.trim().replace('_', "-");
    let lower = normalised.to_ascii_lowercase();
    let mut parts = lower.split('-');
    let language = parts.next()?;
    let rest = parts.collect::<Vec<_>>();

    match language {
        "en" => Some(Language::En),
        "ja" => Some(Language::Ja),
        "ko" => Some(Language::Ko),
        "pt" => Some(Language::PtBr),
        "zh" => {
            let has = |value: &str| rest.contains(&value);
            // Script wins over region: `zh-Hans-HK` is simplified Chinese written
            // in Hong Kong, and checking the region first would serve it the
            // traditional catalogue. Only when no script is stated does the region
            // decide, which is the right fallback for a bare `zh-TW`. Everything
            // else — `zh`, `zh-CN`, `zh-SG` — is simplified.
            if has("hans") {
                Some(Language::ZhCn)
            } else if has("hant") || has("tw") || has("hk") || has("mo") {
                Some(Language::ZhTw)
            } else {
                Some(Language::ZhCn)
            }
        }
        _ => None,
    }
}

/// The values a save writes.
///
/// Derivable because every field's own default is the documented one — the
/// per-enum defaults above are the single source of truth, so this struct cannot
/// drift from them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SettingsEdits {
    pub theme: ThemeMode,
    pub density: Density,
    pub motion: MotionPreference,
    pub language: Language,
    pub currency: crate::currency::Currency,
}

/// The values a settings document currently holds, with unreadable fields
/// reported as their defaults and `known` recording that they were unreadable.
///
/// This shape lets the UI say "using the default" without pretending the file
/// agreed, which matters for a file a person can edit.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SettingsValues {
    pub theme: ThemeMode,
    pub density: Density,
    pub motion: MotionPreference,
    pub language: Language,
    pub currency: crate::currency::Currency,
    /// Fields present but unreadable, in field-name form.
    pub unreadable: Vec<&'static str>,
}

impl SettingsValues {
    /// Read the native fields out of a preferences document.
    pub fn read(preferences: &Preferences) -> Self {
        let mut values = SettingsValues::default();
        let mut unreadable = Vec::new();

        match preferences.get_str(THEME_FIELD) {
            Some(raw) => match ThemeMode::parse(raw) {
                Some(mode) => values.theme = mode,
                None => unreadable.push(THEME_FIELD),
            },
            None if preferences.has_field(THEME_FIELD) => unreadable.push(THEME_FIELD),
            None => {}
        }
        match preferences.get_str(DENSITY_FIELD) {
            Some(raw) => match Density::parse(raw) {
                Some(density) => values.density = density,
                None => unreadable.push(DENSITY_FIELD),
            },
            None if preferences.has_field(DENSITY_FIELD) => unreadable.push(DENSITY_FIELD),
            None => {}
        }
        match preferences.get_str(MOTION_FIELD) {
            Some(raw) => match MotionPreference::parse(raw) {
                Some(motion) => values.motion = motion,
                None => unreadable.push(MOTION_FIELD),
            },
            None if preferences.has_field(MOTION_FIELD) => unreadable.push(MOTION_FIELD),
            None => {}
        }
        match preferences.get_str(LANGUAGE_FIELD) {
            Some(raw) => match Language::parse(raw) {
                Some(language) => values.language = language,
                None => unreadable.push(LANGUAGE_FIELD),
            },
            None if preferences.has_field(LANGUAGE_FIELD) => unreadable.push(LANGUAGE_FIELD),
            None => {}
        }
        match preferences.get_str(CURRENCY_FIELD) {
            Some(raw) => match crate::currency::Currency::parse(raw) {
                Some(currency) => values.currency = currency,
                None => unreadable.push(CURRENCY_FIELD),
            },
            None if preferences.has_field(CURRENCY_FIELD) => unreadable.push(CURRENCY_FIELD),
            None => {}
        }

        values.unreadable = unreadable;
        values
    }
}

#[cfg(test)]
mod tests;
