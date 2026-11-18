//! Appearance, currency and language in the settings presentation.
//!
//! These check the projection: which option is marked selected, what the summary
//! lines say, and — most importantly — that an unsaved draft does not repaint the
//! window while it does move the draft's own ticks.

use std::sync::Arc;

use token_monitor_lite::accounting::TokenCounts;
use token_monitor_lite::aggregation::CanonicalSnapshot;
use token_monitor_lite::application::{
    ApplicationSnapshot, CredentialAvailability, CredentialStatus, ImportReportView, SettingsView,
    SourceAvailability, SourceChoice,
};
use token_monitor_lite::currency::Currency;
use token_monitor_lite::limits::scheduler::LimitsSnapshot;
use token_monitor_lite::runtime::{MonitorSnapshot, RuntimeMetrics, WatchStatus};
use token_monitor_lite::settings::{
    Density, Language, MotionPreference, SettingsValues, ThemeMode,
};
use token_monitor_lite::ui::UiController;

fn monitor() -> Arc<MonitorSnapshot> {
    Arc::new(MonitorSnapshot {
        generation: 1,
        runtime_generation: 1,
        runtime_initialized: true,
        settings_generation: 1,
        day_generation: 1,
        pricing_generation: 1,
        canonical: Arc::new(CanonicalSnapshot {
            generation: 1,
            timezone_name: "UTC".to_owned(),
            totals: TokenCounts::default(),
            cache_write_1h: 0,
            day_models: Arc::from([]),
            session_models: Arc::from([]),
            session_days: Arc::from([]),
            retained_periods: Arc::from([]),
            sources: Arc::from([]),
        }),
        source_health: Arc::from([]),
        watch_status: WatchStatus::Available,
        metrics: RuntimeMetrics::default(),
    })
}

fn application_snapshot(appearance: SettingsValues) -> Arc<ApplicationSnapshot> {
    Arc::new(ApplicationSnapshot {
        revision: 1,
        settings: SettingsView {
            revision: 1,
            enabled_clients: vec!["claude".to_owned()],
            timezone: "UTC".to_owned(),
            selected_source_ids: Vec::new(),
            sources: Vec::<SourceChoice>::new(),
            close_to_tray: false,
            export_folder: None,
            export_interval_seconds: 60,
            appearance,
        },
        draft: None,
        import_report: ImportReportView::default(),
        monitor: None,
        source_availability: SourceAvailability::Unavailable,
        credential_status: CredentialStatus {
            availability: CredentialAvailability::Available,
            pending: false,
        },
        notice: None,
        limits: LimitsSnapshot::default(),
    })
}

fn controller(appearance: SettingsValues) -> UiController {
    let mut controller = UiController::new();
    // A fixed clock keeps the projections deterministic.
    controller.set_clock_ms(1_794_571_200_000);
    controller.accept_snapshot(monitor());
    controller.accept_application_snapshot(application_snapshot(appearance));
    controller
}

#[test]
fn the_committed_theme_marks_exactly_one_option_selected() {
    for theme in ThemeMode::ALL {
        let values = SettingsValues {
            theme,
            ..SettingsValues::default()
        };
        let settings = controller(values).presentation().settings;
        assert_eq!(settings.theme_options.len(), 3);
        let selected = settings
            .theme_options
            .iter()
            .filter(|row| row.selected)
            .collect::<Vec<_>>();
        assert_eq!(selected.len(), 1, "theme {theme:?} selected {selected:?}");
        assert_eq!(selected[0].value, theme.wire());
    }
}

#[test]
fn every_choice_group_offers_the_documented_values() {
    let settings = controller(SettingsValues::default())
        .presentation()
        .settings;
    assert_eq!(
        settings
            .theme_options
            .iter()
            .map(|row| row.value.as_str())
            .collect::<Vec<_>>(),
        ["system", "light", "dark"]
    );
    assert_eq!(
        settings
            .density_options
            .iter()
            .map(|row| row.value.as_str())
            .collect::<Vec<_>>(),
        ["comfortable", "compact"]
    );
    assert_eq!(
        settings
            .motion_options
            .iter()
            .map(|row| row.value.as_str())
            .collect::<Vec<_>>(),
        ["system", "on", "off"]
    );
    assert_eq!(
        settings
            .currency_options
            .iter()
            .map(|row| row.value.as_str())
            .collect::<Vec<_>>(),
        ["USD", "TWD", "HKD", "CNY"]
    );
    assert_eq!(
        settings
            .language_options
            .iter()
            .map(|row| row.value.as_str())
            .collect::<Vec<_>>(),
        ["auto", "en", "zh-TW", "zh-CN", "ko", "ja", "pt-BR"]
    );
}

#[test]
fn the_currency_summary_says_accounting_stays_in_usd() {
    // The one line a user reads before changing this must not imply totals move.
    let values = SettingsValues {
        currency: Currency::Twd,
        ..SettingsValues::default()
    };
    let settings = controller(values).presentation().settings;
    assert!(settings.currency_summary.contains("TWD"));
    assert!(
        settings
            .currency_summary
            .to_ascii_lowercase()
            .contains("usd"),
        "the summary must name the accounting currency: {}",
        settings.currency_summary
    );
}

#[test]
fn an_unreadable_setting_is_surfaced_not_swallowed() {
    let values = SettingsValues {
        unreadable: vec!["themeMode", "currency"],
        ..SettingsValues::default()
    };
    let settings = controller(values).presentation().settings;
    assert!(settings.settings_warnings.contains("themeMode"));
    assert!(settings.settings_warnings.contains("currency"));
}

#[test]
fn a_clean_document_has_no_warning_line() {
    let settings = controller(SettingsValues::default())
        .presentation()
        .settings;
    assert!(settings.settings_warnings.is_empty());
}

#[test]
fn editing_the_draft_moves_the_tick_without_repainting() {
    let mut controller = controller(SettingsValues::default());
    let before = controller.presentation();
    assert_eq!(before.appearance.theme, ThemeMode::System);
    assert_eq!(
        before.appearance.dark,
        token_monitor_lite::system_palette::detect().dark
    );

    controller.set_theme("light");
    controller.set_currency("TWD");
    let after = controller.presentation();
    // Still the committed appearance: nothing repaints before Save.
    assert_eq!(after.appearance.theme, ThemeMode::System);
    assert_eq!(after.appearance.currency, Currency::Usd);
    // But the draft's own ticks moved.
    let light = after
        .settings
        .theme_options
        .iter()
        .find(|row| row.value == "light")
        .expect("light option");
    assert!(light.selected, "the draft tick did not move");
    let twd = after
        .settings
        .currency_options
        .iter()
        .find(|row| row.value == "TWD")
        .expect("TWD option");
    assert!(twd.selected);
}

#[test]
fn an_unrecognised_choice_is_ignored_rather_than_defaulted() {
    // A stray or stale value must not silently reset the stored choice.
    let mut controller = controller(SettingsValues::default());
    controller.set_theme("midnight");
    controller.set_currency("EUR");
    controller.set_language("fr");
    let settings = controller.presentation().settings;
    let system = settings
        .theme_options
        .iter()
        .find(|row| row.value == "system")
        .expect("system option");
    assert!(system.selected, "an unknown theme reset the choice");
    let usd = settings
        .currency_options
        .iter()
        .find(|row| row.value == "USD")
        .expect("USD option");
    assert!(usd.selected);
}

#[test]
fn the_language_names_stay_readable_in_every_locale() {
    let settings = controller(SettingsValues::default())
        .presentation()
        .settings;
    let labels = settings
        .language_options
        .iter()
        .map(|row| row.label.as_str())
        .collect::<Vec<_>>();
    for expected in [
        "繁體中文",
        "简体中文",
        "한국어",
        "日本語",
        "Português (Brasil)",
    ] {
        assert!(labels.contains(&expected), "missing {expected}");
    }
}

#[test]
fn the_appearance_summary_reflects_the_draft() {
    let mut controller = controller(SettingsValues::default());
    controller.set_theme("dark");
    controller.set_density("compact");
    let settings = controller.presentation().settings;
    assert!(settings.appearance_summary.contains("Dark"));
    assert!(settings.appearance_summary.contains("Compact"));
}

#[test]
fn every_committed_appearance_value_reaches_the_projection() {
    // One pass over all five fields, so a field wired to the wrong accessor shows
    // up as a mismatch rather than passing by default.
    let values = SettingsValues {
        theme: ThemeMode::Light,
        density: Density::Compact,
        motion: MotionPreference::Off,
        language: Language::Ko,
        currency: Currency::Hkd,
        unreadable: Vec::new(),
    };
    let presentation = controller(values).presentation();
    assert_eq!(presentation.appearance.theme, ThemeMode::Light);
    assert!(!presentation.appearance.dark, "light mode must not be dark");
    assert_eq!(presentation.appearance.density, Density::Compact);
    assert!(
        !presentation.appearance.reduced_motion,
        "Off must not reduce"
    );
    assert_eq!(presentation.appearance.language, Language::Ko);
    assert_eq!(presentation.appearance.currency, Currency::Hkd);
}
