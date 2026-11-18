//! Contrast and colour tests.
//!
//! These compute WCAG contrast ratios rather than trusting a screenshot. The
//! dark palette is the one that already shipped, so a failure here means the new
//! *light* palette is wrong — or that a dark pair was never actually accessible.

use super::*;

#[test]
fn hex_parsing_round_trips() {
    assert_eq!(Rgb::parse("#0b0f14"), Some(Rgb::new(0x0b, 0x0f, 0x14)));
    assert_eq!(
        Rgb::parse("#FFFFFF"),
        Some(Rgb::new(0xff, 0xff, 0xff)),
        "uppercase hex must parse"
    );
    assert_eq!(Rgb::new(0x12, 0x34, 0x56).hex(), "#123456");
}

#[test]
fn malformed_hex_is_rejected_rather_than_guessed() {
    for bad in ["", "#", "#fff", "#gggggg", "123456", "#1234567"] {
        assert_eq!(Rgb::parse(bad), None, "{bad:?} should not parse");
    }
}

#[test]
fn contrast_ratios_match_known_values() {
    // Black on white is the maximum, 21:1.
    let black = Rgb::new(0, 0, 0);
    let white = Rgb::new(255, 255, 255);
    assert!((black.contrast(white) - 21.0).abs() < 0.01);
    // A colour against itself is 1:1.
    assert!((white.contrast(white) - 1.0).abs() < 0.001);
    // Symmetry: the ratio does not depend on argument order.
    assert!((black.contrast(white) - white.contrast(black)).abs() < 0.001);
}

#[test]
fn contrast_is_correctly_ordered_around_mid_grey() {
    let black = Rgb::new(0, 0, 0);
    let grey = Rgb::new(0x80, 0x80, 0x80);
    let white = Rgb::new(255, 255, 255);
    assert!(black.contrast(grey) > grey.contrast(white) || grey.contrast(white) > 1.0);
    assert!(black.contrast(grey) > 5.0);
}

#[test]
fn every_dark_pair_passes_wcag_aa() {
    let failures = pairs(&DARK)
        .iter()
        .filter(|pair| !pair.passes())
        .map(|pair| format!("{}: {:.2} < {}", pair.role, pair.ratio(), pair.minimum))
        .collect::<Vec<_>>();
    assert!(failures.is_empty(), "dark contrast failures: {failures:#?}");
}

#[test]
fn every_light_pair_passes_wcag_aa() {
    let failures = pairs(&LIGHT)
        .iter()
        .filter(|pair| !pair.passes())
        .map(|pair| format!("{}: {:.2} < {}", pair.role, pair.ratio(), pair.minimum))
        .collect::<Vec<_>>();
    assert!(
        failures.is_empty(),
        "light contrast failures: {failures:#?}"
    );
}

#[test]
fn the_light_palette_is_actually_light_and_the_dark_one_dark() {
    // A palette that accidentally shipped its own inverse would still pass the
    // pair checks above, so pin the luminance direction too.
    let white = Rgb::new(255, 255, 255);
    assert!(LIGHT.background.contrast(white) < 1.2);
    assert!(DARK.background.contrast(white) > 10.0);
}

#[test]
fn the_two_palettes_differ_in_every_role() {
    // A copy-paste slip that duplicated the dark table under a light name would
    // otherwise be invisible.
    let dark = pairs(&DARK);
    let light = pairs(&LIGHT);
    for (left, right) in dark.iter().zip(light.iter()) {
        assert_eq!(left.role, right.role);
    }
    assert_ne!(DARK.background, LIGHT.background);
    assert_ne!(DARK.surface, LIGHT.surface);
    assert_ne!(DARK.text, LIGHT.text);
    assert_ne!(DARK.text_muted, LIGHT.text_muted);
    assert_ne!(DARK.border, LIGHT.border);
    assert_ne!(DARK.accent, LIGHT.accent);
}

#[test]
fn the_mode_selects_the_expected_palette() {
    use crate::settings::ThemeMode;
    assert_eq!(
        Palette::for_dark(ThemeMode::Dark.is_dark(false)),
        DARK,
        "an explicit dark mode must ignore a light desktop"
    );
    assert_eq!(
        Palette::for_dark(ThemeMode::Light.is_dark(true)),
        LIGHT,
        "an explicit light mode must ignore a dark desktop"
    );
    assert_eq!(Palette::for_dark(ThemeMode::System.is_dark(true)), DARK);
    assert_eq!(Palette::for_dark(ThemeMode::System.is_dark(false)), LIGHT);
}

#[test]
fn focus_indicators_are_visible_against_both_backgrounds() {
    // The acceptance run checks that Tab exposes a focused control; this checks
    // that the ring around it is actually perceivable, at the 3:1 non-text ratio.
    for palette in [DARK, LIGHT] {
        let ratio_surface = palette.accent.contrast(palette.surface);
        let ratio_background = palette.accent.contrast(palette.background);
        assert!(
            ratio_surface >= 3.0,
            "focus ring on surface only {ratio_surface:.2}:1"
        );
        assert!(
            ratio_background >= 3.0,
            "focus ring on background only {ratio_background:.2}:1"
        );
    }
}

#[test]
fn borders_are_perceivable_but_do_not_compete_with_text() {
    for palette in [DARK, LIGHT] {
        let ratio = palette.border.contrast(palette.surface);
        assert!(ratio >= 1.2, "border is invisible: {ratio:.2}:1");
        assert!(ratio < 4.5, "border is as loud as text: {ratio:.2}:1");
    }
}
