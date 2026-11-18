//! Display currency.
//!
//! Ports `token-monitor/src/shared/currency.js` and its fixed rate table.
//!
//! **Accounting stays in USD.** Every amount in this application is stored and
//! summed in USD; a currency here only changes how a number is *shown*. That is
//! why [`format_from_usd`] takes nano-USD and returns text: there is no API that
//! converts an amount into another currency and hands it back as a number a
//! caller could then add to a total. The type makes the accounting boundary hard
//! to cross by accident rather than leaving it to a comment.

use crate::export::csv;

/// A currency the interface can display amounts in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Currency {
    #[default]
    Usd,
    Twd,
    Hkd,
    Cny,
}

/// The fixed conversion the reference ships.
///
/// These are the reference's own constants, not live rates: a rate that silently
/// changed between runs would make two exports of the same data disagree.
struct Rate {
    code: &'static str,
    symbol: &'static str,
    /// Units of this currency per 1 USD.
    per_usd: f64,
}

const RATES: [Rate; 4] = [
    Rate {
        code: "USD",
        symbol: "$",
        per_usd: 1.0,
    },
    Rate {
        code: "TWD",
        symbol: "NT$",
        per_usd: 31.5,
    },
    Rate {
        code: "HKD",
        symbol: "HK$",
        per_usd: 7.8,
    },
    Rate {
        code: "CNY",
        symbol: "¥",
        per_usd: 6.8,
    },
];

impl Currency {
    pub const ALL: [Self; 4] = [Self::Usd, Self::Twd, Self::Hkd, Self::Cny];

    fn rate(self) -> &'static Rate {
        match self {
            Self::Usd => &RATES[0],
            Self::Twd => &RATES[1],
            Self::Hkd => &RATES[2],
            Self::Cny => &RATES[3],
        }
    }

    pub fn code(self) -> &'static str {
        self.rate().code
    }

    pub fn symbol(self) -> &'static str {
        self.rate().symbol
    }

    /// Units of this currency per 1 USD.
    pub fn per_usd(self) -> f64 {
        self.rate().per_usd
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_uppercase().as_str() {
            "USD" => Some(Self::Usd),
            "TWD" => Some(Self::Twd),
            "HKD" => Some(Self::Hkd),
            "CNY" => Some(Self::Cny),
            _ => None,
        }
    }

    /// Convert a USD amount for display.
    ///
    /// Rounded to six decimals exactly as `convertUsd` does, so the displayed
    /// value of a given USD amount does not depend on floating-point noise.
    pub fn convert(self, usd: f64) -> f64 {
        let amount = if usd.is_finite() { usd } else { 0.0 };
        let scaled = amount * self.per_usd();
        (scaled * 1_000_000.0).round() / 1_000_000.0
    }

    /// How many decimals to show.
    ///
    /// `fractionDigitsFor` in the reference: four decimals below the boundary, two
    /// above, so a fraction of a cent stays visible on small amounts instead of
    /// collapsing to `$0.00`.
    pub fn fraction_digits(self, amount: f64) -> usize {
        let boundary = if self == Self::Usd { 10.0 } else { 1.0 };
        if amount.abs() >= boundary { 2 } else { 4 }
    }

    /// Format a USD amount in this currency. The only way out is text.
    pub fn format_from_usd(self, usd: f64) -> String {
        let amount = self.convert(usd);
        let digits = self.fraction_digits(amount);
        format!("{}{amount:.digits$}", self.symbol())
    }

    /// Format a nano-USD amount, the unit the pricing layer produces.
    pub fn format_nano(self, nano_usd: i128) -> String {
        self.format_from_usd(usd_from_nano(nano_usd))
    }
}

/// Nano-USD to a USD float, through the decimal rendering so a value like 0.1
/// does not drift into 0.09999999999999999.
pub fn usd_from_nano(nano_usd: i128) -> f64 {
    csv::format_usd(nano_usd).parse().unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rate_table_matches_the_reference() {
        assert_eq!(Currency::Usd.per_usd(), 1.0);
        assert_eq!(Currency::Twd.per_usd(), 31.5);
        assert_eq!(Currency::Hkd.per_usd(), 7.8);
        assert_eq!(Currency::Cny.per_usd(), 6.8);
        assert_eq!(Currency::Usd.symbol(), "$");
        assert_eq!(Currency::Twd.symbol(), "NT$");
        assert_eq!(Currency::Hkd.symbol(), "HK$");
        assert_eq!(Currency::Cny.symbol(), "¥");
    }

    #[test]
    fn the_four_codes_are_the_ones_the_settings_offer() {
        let codes = Currency::ALL.map(Currency::code);
        assert_eq!(codes, ["USD", "TWD", "HKD", "CNY"]);
    }

    #[test]
    fn usd_conversion_is_the_identity() {
        assert_eq!(Currency::Usd.convert(1.0), 1.0);
        assert_eq!(Currency::Usd.convert(0.0), 0.0);
        assert_eq!(Currency::Usd.convert(123.456789), 123.456789);
    }

    #[test]
    fn conversion_multiplies_by_the_reference_rate() {
        assert_eq!(Currency::Twd.convert(1.0), 31.5);
        assert_eq!(Currency::Hkd.convert(2.0), 15.6);
        assert_eq!(Currency::Cny.convert(10.0), 68.0);
    }

    #[test]
    fn a_non_finite_amount_never_becomes_nan_in_the_output() {
        // A NaN in a money label is worse than a zero.
        assert_eq!(Currency::Usd.convert(f64::NAN), 0.0);
        assert_eq!(Currency::Twd.convert(f64::INFINITY), 0.0);
    }

    #[test]
    fn fractional_cents_stay_visible_on_small_amounts() {
        // Below the boundary: four decimals.
        assert_eq!(Currency::Usd.format_from_usd(0.0012), "$0.0012");
        assert_eq!(Currency::Twd.format_from_usd(0.0001), "NT$0.0032");
        // At or above: two decimals.
        assert_eq!(Currency::Usd.format_from_usd(12.5), "$12.50");
        assert_eq!(Currency::Usd.format_from_usd(10.0), "$10.00");
        assert_eq!(Currency::Twd.format_from_usd(1.0), "NT$31.50");
    }

    #[test]
    fn the_dollar_boundary_is_ten_for_usd_and_one_otherwise() {
        assert_eq!(Currency::Usd.fraction_digits(9.99), 4);
        assert_eq!(Currency::Usd.fraction_digits(10.0), 2);
        assert_eq!(Currency::Twd.fraction_digits(0.99), 4);
        assert_eq!(Currency::Twd.fraction_digits(1.0), 2);
    }

    #[test]
    fn a_negative_amount_keeps_its_sign() {
        // Magnitude decides the decimals, exactly as `fractionDigitsFor` does: 3.5
        // is below the USD boundary of 10, so four decimals.
        assert_eq!(Currency::Usd.format_from_usd(-3.5), "$-3.5000");
        assert_eq!(Currency::Usd.format_from_usd(-12.5), "$-12.50");
    }

    #[test]
    fn parsing_is_case_insensitive_and_rejects_unknown_codes() {
        assert_eq!(Currency::parse("twd"), Some(Currency::Twd));
        assert_eq!(Currency::parse(" TWD "), Some(Currency::Twd));
        assert_eq!(Currency::parse("EUR"), None);
        assert_eq!(Currency::parse(""), None);
    }

    #[test]
    fn nano_usd_round_trips_through_the_decimal_rendering() {
        assert_eq!(usd_from_nano(0), 0.0);
        assert_eq!(usd_from_nano(1_000_000_000), 1.0);
        assert_eq!(usd_from_nano(100_000_000), 0.1);
        // 0.1 must not become 0.09999999999999999 before conversion.
        assert!((usd_from_nano(100_000_000) * 6.8 - 0.68).abs() < 1e-12);
        assert_eq!(Currency::Cny.format_nano(100_000_000), "¥0.6800");
    }

    #[test]
    fn zero_formats_with_the_boundary_digits() {
        assert_eq!(Currency::Usd.format_from_usd(0.0), "$0.0000");
        assert_eq!(Currency::Cny.format_from_usd(0.0), "¥0.0000");
    }
}
