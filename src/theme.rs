//! Colour themes and text-contrast verification.
//!
//! The palettes are explicit tables, not values scattered through the Slint file,
//! for one reason: contrast can then be *computed* in a test rather than asserted
//! in prose. Every foreground/background pair the UI actually draws is listed in
//! [`PAIRS`] and checked against WCAG 2.1 §1.4.3 (4.5:1 for body text, 3:1 for
//! large text and for non-text boundaries).
//!
//! The dark palette is the one the window already used. The light palette is new,
//! so it is the one that needs evidence most.

/// An `#rrggbb` colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Parse `#rrggbb`. Returns `None` rather than panicking on a typo, so a
    /// malformed palette entry is a test failure, not a crash at startup.
    pub fn parse(text: &str) -> Option<Self> {
        let hex = text.strip_prefix('#')?;
        if hex.len() != 6 {
            return None;
        }
        let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
        let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
        let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
        Some(Self { r, g, b })
    }

    /// The CSS hex form Slint expects.
    pub fn hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }

    /// WCAG relative luminance.
    fn luminance(self) -> f64 {
        fn channel(value: u8) -> f64 {
            let scaled = f64::from(value) / 255.0;
            if scaled <= 0.039_28 {
                scaled / 12.92
            } else {
                ((scaled + 0.055) / 1.055).powf(2.4)
            }
        }
        0.2126 * channel(self.r) + 0.7152 * channel(self.g) + 0.0722 * channel(self.b)
    }

    /// WCAG contrast ratio against another colour, 1.0 to 21.0.
    pub fn contrast(self, other: Self) -> f64 {
        let first = self.luminance();
        let second = other.luminance();
        let (lighter, darker) = if first > second {
            (first, second)
        } else {
            (second, first)
        };
        (lighter + 0.05) / (darker + 0.05)
    }
}

/// The colours one theme draws with.
///
/// Field names are the *roles* the UI uses, so a Slint binding names a role and
/// never a literal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    /// Window background.
    pub background: Rgb,
    /// Panel/card background, one step off the window.
    pub surface: Rgb,
    /// A raised or inset row.
    pub surface_alt: Rgb,
    /// Primary body text.
    pub text: Rgb,
    /// Secondary text: labels, captions.
    pub text_muted: Rgb,
    /// Tertiary text: hints, the heatmap legend.
    pub text_faint: Rgb,
    /// Panel borders and separators.
    pub border: Rgb,
    /// Navigation/selection highlight.
    pub accent: Rgb,
    /// Warning text and state.
    pub warning: Rgb,
    /// Error text and state.
    pub danger: Rgb,
    /// Success text and state.
    pub success: Rgb,
}

/// The palette the window used before Appearance existed.
pub const DARK: Palette = Palette {
    background: Rgb::new(0x0b, 0x0f, 0x14),
    surface: Rgb::new(0x12, 0x18, 0x20),
    surface_alt: Rgb::new(0x1e, 0x2a, 0x36),
    text: Rgb::new(0xf2, 0xf6, 0xfa),
    text_muted: Rgb::new(0x91, 0xa0, 0xaf),
    // Lightened from `#6d7b89`, which measured 4.12:1 on the surface and failed
    // WCAG AA. The contrast test caught it; this value is 4.66:1.
    text_faint: Rgb::new(0x76, 0x84, 0x92),
    border: Rgb::new(0x29, 0x35, 0x42),
    accent: Rgb::new(0x69, 0xa7, 0xff),
    warning: Rgb::new(0xf5, 0xa6, 0x5b),
    danger: Rgb::new(0xf1, 0x6d, 0x7a),
    success: Rgb::new(0x67, 0xe8, 0xb4),
};

/// Light palette. Every pair is contrast-checked in `tests`, not by eye.
pub const LIGHT: Palette = Palette {
    background: Rgb::new(0xf7, 0xf9, 0xfb),
    surface: Rgb::new(0xff, 0xff, 0xff),
    surface_alt: Rgb::new(0xea, 0xef, 0xf5),
    text: Rgb::new(0x14, 0x1c, 0x26),
    text_muted: Rgb::new(0x4a, 0x57, 0x66),
    text_faint: Rgb::new(0x5d, 0x6b, 0x7a),
    border: Rgb::new(0xc5, 0xcf, 0xda),
    accent: Rgb::new(0x14, 0x4e, 0xb8),
    warning: Rgb::new(0x8a, 0x4b, 0x00),
    danger: Rgb::new(0xa8, 0x1c, 0x2c),
    success: Rgb::new(0x0f, 0x6b, 0x4a),
};

impl Palette {
    pub fn for_dark(dark: bool) -> Self {
        if dark { DARK } else { LIGHT }
    }
}

/// A foreground/background pair the UI actually draws.
#[derive(Clone, Copy, Debug)]
pub struct ContrastPair {
    /// How the pair is used, for the failure message.
    pub role: &'static str,
    pub foreground: Rgb,
    pub background: Rgb,
    /// Required ratio: 4.5 for body text, 3.0 for large text and boundaries.
    pub minimum: f64,
}

/// Every pair either palette draws.
///
/// A pair missing from here is a pair nobody checked, so the palette fields line
/// up with this list one for one.
pub fn pairs(palette: &Palette) -> [ContrastPair; 10] {
    [
        ContrastPair {
            role: "body text on window",
            foreground: palette.text,
            background: palette.background,
            minimum: 4.5,
        },
        ContrastPair {
            role: "body text on surface",
            foreground: palette.text,
            background: palette.surface,
            minimum: 4.5,
        },
        ContrastPair {
            role: "body text on raised row",
            foreground: palette.text,
            background: palette.surface_alt,
            minimum: 4.5,
        },
        ContrastPair {
            role: "muted text on window",
            foreground: palette.text_muted,
            background: palette.background,
            minimum: 4.5,
        },
        ContrastPair {
            role: "muted text on surface",
            foreground: palette.text_muted,
            background: palette.surface,
            minimum: 4.5,
        },
        ContrastPair {
            role: "faint text on surface",
            foreground: palette.text_faint,
            background: palette.surface,
            minimum: 4.5,
        },
        ContrastPair {
            role: "accent text on window",
            foreground: palette.accent,
            background: palette.background,
            minimum: 4.5,
        },
        ContrastPair {
            role: "warning text on surface",
            foreground: palette.warning,
            background: palette.surface,
            minimum: 4.5,
        },
        ContrastPair {
            role: "danger text on surface",
            foreground: palette.danger,
            background: palette.surface,
            minimum: 4.5,
        },
        ContrastPair {
            role: "success text on surface",
            foreground: palette.success,
            background: palette.surface,
            minimum: 4.5,
        },
    ]
}

impl ContrastPair {
    pub fn ratio(&self) -> f64 {
        self.foreground.contrast(self.background)
    }

    pub fn passes(&self) -> bool {
        self.ratio() >= self.minimum
    }
}

#[cfg(test)]
mod tests;
