//! What the desktop itself asks for.
//!
//! `ThemeMode::System` and `MotionPreference::System` both need an answer from the
//! environment, and this module is the single place that produces it.
//!
//! There is no portable cross-platform API for either preference. Rather than
//! guess, the detection is deliberately conservative: a value is only returned
//! when something specific says so, and the fallback is the dark palette the
//! window has always shipped with. Getting this wrong in the "assume dark" case
//! leaves exactly the previous behaviour, which is the safe direction to be wrong
//! in.

/// The environment's resolved preference.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SystemAppearance {
    pub dark: bool,
    pub reduce_motion: bool,
}

impl Default for SystemAppearance {
    fn default() -> Self {
        Self {
            dark: true,
            reduce_motion: false,
        }
    }
}

/// Detect the desktop preference from the environment.
///
/// **ASSUMED**: no desktop's preference has been read through a native API here.
/// These are the environment conventions desktops and test harnesses actually set,
/// which is enough to make `System` behave predictably and to let a test drive it;
/// a platform-specific probe would be the upgrade path, and would need acceptance
/// evidence on each target.
pub fn detect() -> SystemAppearance {
    SystemAppearance {
        dark: detect_dark(),
        reduce_motion: detect_reduce_motion(),
    }
}

/// Parse a boolean-ish environment flag.
fn flag(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" | "dark" | "prefer-dark" => Some(true),
        "0" | "false" | "no" | "off" | "light" | "prefer-light" => Some(false),
        _ => None,
    }
}

fn detect_dark() -> bool {
    // The explicit override wins, so a user or a test can pin the answer.
    if let Ok(value) = std::env::var("TOKEN_MONITOR_DARK")
        && let Some(dark) = flag(&value)
    {
        return dark;
    }
    // GTK's scheme setting, which is what a GTK desktop exports.
    if let Ok(value) = std::env::var("GTK_THEME")
        && value.to_ascii_lowercase().contains("dark")
    {
        return true;
    }
    if let Ok(value) = std::env::var("COLORSCHEME")
        && let Some(dark) = flag(&value)
    {
        return dark;
    }
    // Nothing said so: keep the palette this window has always drawn with.
    true
}

fn detect_reduce_motion() -> bool {
    if let Ok(value) = std::env::var("TOKEN_MONITOR_REDUCE_MOTION")
        && let Some(reduce) = flag(&value)
    {
        return reduce;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fallback_is_the_palette_the_window_already_shipped() {
        // Being wrong here must not change existing behaviour.
        assert!(SystemAppearance::default().dark);
        assert!(!SystemAppearance::default().reduce_motion);
    }

    #[test]
    fn a_flag_understands_the_usual_spellings() {
        for truthy in ["1", "true", "TRUE", "yes", "on", "dark", " Prefer-Dark "] {
            assert_eq!(flag(truthy), Some(true), "{truthy:?}");
        }
        for falsy in ["0", "false", "no", "off", "light", "prefer-light"] {
            assert_eq!(flag(falsy), Some(false), "{falsy:?}");
        }
        assert_eq!(flag("maybe"), None);
        assert_eq!(flag(""), None);
    }

    #[test]
    fn detection_always_answers() {
        // Whatever the environment holds, this must not panic or return nothing.
        let appearance = detect();
        let _ = appearance.dark;
        let _ = appearance.reduce_motion;
    }
}
