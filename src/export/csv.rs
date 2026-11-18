//! CSV serialization: UTF-8 BOM, CRLF, RFC 4180.
//!
//! Ports `toCsv` and `csvEscape` from `src/shared/exporter.js`. See
//! `docs/migration/export-spec.md` §2.

/// The byte-order mark Excel needs to read UTF-8 correctly.
pub const BOM: &str = "\u{feff}";
/// RFC 4180 line ending.
pub const CRLF: &str = "\r\n";

/// Quote a field only when it needs it.
///
/// A field containing a quote, comma, CR or LF is quoted and its quotes doubled;
/// everything else is emitted bare. This matches the reference exactly, so a
/// plain value never grows quotes it does not need.
pub fn escape(value: &str) -> String {
    if value.contains(['"', ',', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}

/// Coerce a number for a CSV cell.
///
/// A non-finite value becomes `0`, never `NaN`: a spreadsheet reading `NaN` in a
/// `tokens` column is worse than a zero. Mirrors `num()`.
pub fn coerce(value: f64) -> f64 {
    if value.is_finite() { value } else { 0.0 }
}

/// Render a whole CSV document.
///
/// `rows` supplies the cell text for each column, already formatted. A trailing
/// CRLF after the last row is part of the format — some tools treat a missing one
/// as a truncated file.
pub fn render<'a, I, C>(columns: &[&str], rows: I) -> String
where
    I: IntoIterator<Item = Vec<C>>,
    C: AsRef<str> + 'a,
{
    let mut out = String::from(BOM);
    out.push_str(
        &columns
            .iter()
            .map(|column| escape(column))
            .collect::<Vec<_>>()
            .join(","),
    );
    out.push_str(CRLF);
    for row in rows {
        let line = row
            .iter()
            .map(|cell| escape(cell.as_ref()))
            .collect::<Vec<_>>()
            .join(",");
        out.push_str(&line);
        out.push_str(CRLF);
    }
    out
}

/// Format a token count for a CSV cell. Integral, never scientific notation.
pub fn format_count(value: u128) -> String {
    value.to_string()
}

/// Format a cost as a plain decimal in USD.
///
/// `nano_usd` is the smallest unit the pricing layer produces. Ten decimal places
/// keeps a fraction of a cent visible without ever printing `1e-7`, which a
/// spreadsheet would import as text.
pub fn format_usd(nano_usd: i128) -> String {
    if nano_usd == 0 {
        return "0".to_owned();
    }
    let negative = nano_usd < 0;
    let magnitude = nano_usd.unsigned_abs();
    let whole = magnitude / 1_000_000_000;
    let fraction = magnitude % 1_000_000_000;
    let mut text = if fraction == 0 {
        whole.to_string()
    } else {
        // Trim trailing zeroes so 1/2 reads as 0.5, not 0.5000000000.
        let digits = format!("{fraction:09}");
        let trimmed = digits.trim_end_matches('0');
        format!("{whole}.{trimmed}")
    };
    if negative {
        text.insert(0, '-');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_document_starts_with_a_bom_and_uses_crlf() {
        let csv = render(&["a", "b"], vec![vec!["1", "2"]]);
        assert!(csv.starts_with(BOM), "missing BOM");
        assert_eq!(csv, format!("{BOM}a,b\r\n1,2\r\n"));
        assert!(csv.ends_with("\r\n"), "the last row needs a trailing CRLF");
    }

    #[test]
    fn plain_values_are_not_quoted() {
        assert_eq!(escape("codex"), "codex");
        assert_eq!(escape("2026-07-03"), "2026-07-03");
        assert_eq!(escape(""), "");
    }

    #[test]
    fn rfc4180_reserved_characters_are_quoted() {
        assert_eq!(escape("a,b"), "\"a,b\"");
        assert_eq!(escape("a\"b"), "\"a\"\"b\"");
        assert_eq!(escape("a\nb"), "\"a\nb\"");
        assert_eq!(escape("a\r\nb"), "\"a\r\nb\"");
    }

    #[test]
    fn a_quoted_field_doubles_its_quotes_and_keeps_its_commas() {
        assert_eq!(escape("\"x\",y"), "\"\"\"x\"\",y\"");
    }

    #[test]
    fn a_non_finite_number_becomes_zero_not_nan() {
        assert_eq!(coerce(f64::NAN), 0.0);
        assert_eq!(coerce(f64::INFINITY), 0.0);
        assert_eq!(coerce(-1.5), -1.5);
    }

    #[test]
    fn counts_are_integral_and_never_scientific() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(1_234_567_890_123), "1234567890123");
    }

    #[test]
    fn usd_is_a_plain_decimal_without_exponents() {
        assert_eq!(format_usd(0), "0");
        assert_eq!(format_usd(2_000_000_000), "2");
        assert_eq!(format_usd(500_000_000), "0.5");
        assert_eq!(format_usd(2_000_000), "0.002");
        // A fraction of a cent stays visible rather than rounding to 0.
        assert_eq!(format_usd(1), "0.000000001");
        assert_eq!(format_usd(-1_500_000_000), "-1.5");
    }

    #[test]
    fn a_cost_never_contains_an_exponent_marker() {
        for value in [1_i128, 999, 1_000_000_000, 12_345_678_901_234] {
            let text = format_usd(value);
            assert!(!text.contains('e') && !text.contains('E'), "{text}");
        }
    }
}
