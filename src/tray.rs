//! Tray badge rendering and platform capability.
//!
//! This module is deliberately free of Slint and OS calls: it turns "today's
//! totals" into a compact label and a deterministic RGBA8 bitmap, so the tray
//! image can be produced from the same presentation pass that paints the window
//! and can be tested without a display.

/// Edge length of the generated badge bitmap.
///
/// The platform scales it down for the tray; a 32 pixel source keeps two rows of
/// text legible on a 2x display while still costing one 4 KiB buffer.
pub const BADGE_SIZE: u32 = 32;

/// Bytes in one badge bitmap.
pub const BADGE_BYTES: usize = (BADGE_SIZE * BADGE_SIZE * 4) as usize;

const BACKGROUND: [u8; 4] = [0x10, 0x16, 0x1D, 0xFF];
const BORDER: [u8; 4] = [0x2B, 0x39, 0x47, 0xFF];
const TEXT: [u8; 4] = [0xF2, 0xF6, 0xFA, 0xFF];

const GLYPH_WIDTH: u32 = 3;
const GLYPH_HEIGHT: u32 = 5;
const SCALE: u32 = 2;
const ADVANCE: u32 = GLYPH_WIDTH * SCALE + 2;

/// Whether a tray icon can be shown on this platform as this build is configured.
///
/// Windows and macOS always have a native tray. On Linux the backend speaks
/// StatusNotifierItem over the session bus, so the icon can only exist when a
/// session bus is present; without one the Slint backend logs and shows nothing,
/// and hiding the window would strand the application, so the close-to-tray
/// behaviour is refused instead.
pub fn tray_capable() -> bool {
    #[cfg(target_os = "linux")]
    {
        std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some()
    }
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}

/// Compact form of a token total for the badge, at most four characters.
pub fn compact_tokens(tokens: u128) -> String {
    const UNITS: [(u128, &str); 3] = [
        (1_000_000_000_000, "T"),
        (1_000_000_000, "G"),
        (1_000_000, "M"),
    ];
    if tokens >= 1_000_000_000_000 {
        return "999T".to_owned();
    }
    for (factor, suffix) in UNITS {
        if tokens >= factor {
            let whole = tokens / factor;
            if whole < 10 {
                let tenths = (tokens % factor) / (factor / 10);
                if tenths == 0 {
                    return format!("{whole}{suffix}");
                }
                return format!("{whole}.{tenths}{suffix}");
            }
            return format!("{whole}{suffix}");
        }
    }
    if tokens >= 10_000 {
        return format!("{}k", tokens / 1_000);
    }
    if tokens >= 1_000 {
        let whole = tokens / 1_000;
        let tenths = (tokens % 1_000) / 100;
        if tenths == 0 {
            return format!("{whole}k");
        }
        return format!("{whole}.{tenths}k");
    }
    tokens.to_string()
}

/// Compact form of the presentation's cost string, at most five characters.
///
/// The presentation already decided whether a cost is known; this only shortens
/// it, and leaves an unknown cost exactly as the window shows it.
pub fn compact_cost(cost: &str) -> String {
    let trimmed = cost.trim();
    if trimmed.is_empty() || trimmed == "—" {
        return "—".to_owned();
    }
    let Some(amount) = trimmed
        .strip_prefix('$')
        .map(|value| value.replace(',', ""))
        .and_then(|value| value.parse::<f64>().ok())
    else {
        return trimmed.chars().take(5).collect();
    };
    if amount < 10.0 {
        return format!("${amount:.2}");
    }
    if amount < 1_000.0 {
        return format!("${amount:.0}");
    }
    let rounded = amount.round();
    let whole = if rounded >= 1_000_000_000.0 {
        "999".to_owned()
    } else {
        compact_tokens(rounded as u128)
            .trim_end_matches('k')
            .to_owned()
    };
    if rounded >= 1_000_000.0 {
        format!("${whole}")
    } else {
        format!("${whole}k")
    }
}

/// Render the badge for the given compact labels as RGBA8.
///
/// The bitmap is opaque: a transparent badge disappears on light tray themes.
pub fn badge_rgba(tokens: &str, cost: &str) -> Vec<u8> {
    let mut pixels = vec![0_u8; BADGE_BYTES];
    for y in 0..BADGE_SIZE {
        for x in 0..BADGE_SIZE {
            let edge = x == 0 || y == 0 || x + 1 == BADGE_SIZE || y + 1 == BADGE_SIZE;
            let color = if edge { BORDER } else { BACKGROUND };
            put(&mut pixels, x, y, color);
        }
    }
    let top = (BADGE_SIZE - GLYPH_HEIGHT * SCALE) / 2 - 4;
    draw_row(&mut pixels, tokens, top);
    draw_row(&mut pixels, cost, BADGE_SIZE / 2 + 3);
    pixels
}

fn draw_row(pixels: &mut [u8], text: &str, y: u32) {
    let glyphs = text.chars().count() as u32;
    if glyphs == 0 {
        return;
    }
    let width = glyphs * ADVANCE - 2;
    let mut x = (BADGE_SIZE.saturating_sub(width)) / 2;
    for character in text.chars() {
        draw_glyph(pixels, character, x, y);
        x += ADVANCE;
    }
}

fn draw_glyph(pixels: &mut [u8], character: char, x: u32, y: u32) {
    let rows = glyph(character);
    for (row, bits) in rows.iter().enumerate() {
        for column in 0..GLYPH_WIDTH {
            if bits & (1 << (GLYPH_WIDTH - 1 - column)) == 0 {
                continue;
            }
            for dy in 0..SCALE {
                for dx in 0..SCALE {
                    put(
                        pixels,
                        x + column * SCALE + dx,
                        y + row as u32 * SCALE + dy,
                        TEXT,
                    );
                }
            }
        }
    }
}

fn glyph(character: char) -> [u8; 5] {
    match character {
        '0' => [0b111, 0b101, 0b101, 0b101, 0b111],
        '1' => [0b010, 0b110, 0b010, 0b010, 0b111],
        '2' => [0b111, 0b001, 0b111, 0b100, 0b111],
        '3' => [0b111, 0b001, 0b111, 0b001, 0b111],
        '4' => [0b101, 0b101, 0b111, 0b001, 0b001],
        '5' => [0b111, 0b100, 0b111, 0b001, 0b111],
        '6' => [0b111, 0b100, 0b111, 0b101, 0b111],
        '7' => [0b111, 0b001, 0b001, 0b001, 0b001],
        '8' => [0b111, 0b101, 0b111, 0b101, 0b111],
        '9' => [0b111, 0b101, 0b111, 0b001, 0b111],
        '.' => [0b000, 0b000, 0b000, 0b000, 0b010],
        '$' => [0b011, 0b110, 0b011, 0b110, 0b010],
        'k' => [0b101, 0b101, 0b110, 0b101, 0b101],
        'M' => [0b101, 0b111, 0b111, 0b101, 0b101],
        'G' => [0b111, 0b100, 0b101, 0b101, 0b111],
        'T' => [0b111, 0b010, 0b010, 0b010, 0b010],
        '-' => [0b000, 0b000, 0b111, 0b000, 0b000],
        _ => [0b000, 0b000, 0b000, 0b000, 0b000],
    }
}

fn put(pixels: &mut [u8], x: u32, y: u32, color: [u8; 4]) {
    if x >= BADGE_SIZE || y >= BADGE_SIZE {
        return;
    }
    let offset = ((y * BADGE_SIZE + x) * 4) as usize;
    pixels[offset..offset + 4].copy_from_slice(&color);
}

/// Text shown when the pointer rests on the icon.
pub fn tooltip(
    tokens_text: &str,
    cost_text: &str,
    compact_tokens: &str,
    compact_cost: &str,
) -> String {
    format!(
        "Token Monitor Lite — today {tokens_text} tokens · {cost_text} ({compact_tokens} · {compact_cost})"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_labels_stay_within_four_characters() {
        assert_eq!(compact_tokens(0), "0");
        assert_eq!(compact_tokens(999), "999");
        assert_eq!(compact_tokens(1_000), "1k");
        assert_eq!(compact_tokens(1_500), "1.5k");
        assert_eq!(compact_tokens(12_345), "12k");
        assert_eq!(compact_tokens(999_999), "999k");
        assert_eq!(compact_tokens(1_000_000), "1M");
        assert_eq!(compact_tokens(1_234_567), "1.2M");
        assert_eq!(compact_tokens(12_345_678), "12M");
        assert_eq!(compact_tokens(1_234_567_890), "1.2G");
        assert_eq!(compact_tokens(u128::MAX), "999T");
        for value in [
            0,
            1,
            999,
            1_000,
            9_876,
            54_321,
            765_432,
            8_765_432,
            987_654_321,
        ] {
            assert!(compact_tokens(value).chars().count() <= 4, "{value}");
        }
    }

    #[test]
    fn cost_labels_stay_within_five_characters() {
        assert_eq!(compact_cost("—"), "—");
        assert_eq!(compact_cost(""), "—");
        assert_eq!(compact_cost("$0.00"), "$0.00");
        assert_eq!(compact_cost("$1.24"), "$1.24");
        assert_eq!(compact_cost("$12.30"), "$12");
        assert_eq!(compact_cost("$1,234.56"), "$1.2k");
        assert_eq!(compact_cost("$12,345.67"), "$12k");
        assert!(compact_cost("$1,234,567.89").chars().count() <= 5);
    }

    #[test]
    fn badge_is_opaque_and_sized() {
        let pixels = badge_rgba("12k", "$1.2");
        assert_eq!(pixels.len(), BADGE_BYTES);
        assert!(
            pixels
                .as_chunks::<4>()
                .0
                .iter()
                .all(|pixel| pixel[3] == 0xFF)
        );
        assert_eq!(BADGE_BYTES, 4096);
    }

    #[test]
    fn badge_renders_text_pixels_and_is_deterministic() {
        let with_text = badge_rgba("12k", "$1.2");
        let empty = badge_rgba("", "");
        let lit = |pixels: &[u8]| {
            pixels
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|pixel| pixel[0] == TEXT[0] && pixel[1] == TEXT[1])
                .count()
        };
        assert!(lit(&with_text) > 40, "expected glyph pixels");
        assert_eq!(lit(&empty), 0);
        assert_eq!(with_text, badge_rgba("12k", "$1.2"));
    }

    #[test]
    fn badges_never_index_out_of_bounds() {
        for label in ["", "—", "999T", "$0.42", "WWWWWWWWWW"] {
            let pixels = badge_rgba(label, label);
            assert_eq!(pixels.len(), BADGE_BYTES);
        }
    }

    #[test]
    fn tooltip_names_today_and_both_labels() {
        let text = tooltip("12,345", "$1.24", "12k", "$1.24");
        assert!(text.contains("today 12,345 tokens"), "{text}");
        assert!(text.contains("$1.24"), "{text}");
    }
}
