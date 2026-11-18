//! Retry backoff, from `src/shared/limits/retryPolicy.js`.
//!
//! See `docs/migration/limits-spec.md` §1.6. The jitter source is injected so
//! tests are deterministic and no hidden randomness reaches a test fixture.

/// Default first-retry delay.
pub const DEFAULT_RETRY_BASE_MS: i64 = 5_000;
/// Default retry ceiling.
pub const DEFAULT_RETRY_MAX_MS: i64 = 5 * 60_000;
/// Ceiling applied to a provider-supplied `Retry-After`.
pub const MAX_RETRY_AFTER_MS: i64 = 60 * 60_000;
/// Jitter never exceeds this, however large the base delay.
const RETRY_AFTER_JITTER_MAX_MS: i64 = 5_000;

/// Parse a `Retry-After` header.
///
/// Accepts both wire forms: delta-seconds and an HTTP-date. Values are clamped
/// to `[0, MAX_RETRY_AFTER_MS]`; anything unparseable is `None`, so a broken
/// header cannot suppress retries forever.
pub fn parse_retry_after_header(value: &str, now_ms: i64) -> Option<i64> {
    let raw = value.trim();
    if raw.is_empty() {
        return None;
    }
    if !raw.is_empty()
        && raw
            .chars()
            .all(|character| character.is_ascii_digit() || character == '.')
    {
        let seconds: f64 = raw.parse().ok()?;
        return Some(clamp_delay((seconds * 1000.0) as i64));
    }
    let at = super::window::parse_instant_ms(raw)?;
    Some(clamp_delay(at - now_ms))
}

fn clamp_delay(value: i64) -> i64 {
    value.clamp(0, MAX_RETRY_AFTER_MS)
}

/// Compute the delay before the next attempt.
///
/// `retry_after_ms`, when the provider sent one, wins: honouring the server's
/// own backoff is the only way not to make a throttle worse. Otherwise the delay
/// doubles from `base_ms` to `max_ms` and is jittered over the upper half of the
/// cap, so concurrent clients do not synchronise.
///
/// `random` must return a value in `[0, 1)`; anything else is treated as `0.5`.
pub fn compute_retry_delay_ms(
    attempt: u32,
    retry_after_ms: Option<i64>,
    base_ms: i64,
    max_ms: i64,
    random: impl FnOnce() -> f64,
) -> i64 {
    let unit = |value: f64| {
        if value.is_finite() {
            value.clamp(0.0, 0.999_999_999)
        } else {
            0.5
        }
    };
    if let Some(retry_after) = retry_after_ms.filter(|value| *value > 0) {
        let bounded = retry_after.clamp(0, MAX_RETRY_AFTER_MS);
        let jitter_cap = RETRY_AFTER_JITTER_MAX_MS.min(bounded / 10);
        return bounded + (unit(random()) * jitter_cap as f64).ceil() as i64;
    }
    let base = if base_ms > 0 {
        base_ms
    } else {
        DEFAULT_RETRY_BASE_MS
    };
    let max = if max_ms >= base { max_ms } else { base };
    let exponent = attempt.saturating_sub(1).min(30);
    let cap = base
        .saturating_mul(1_i64.checked_shl(exponent).unwrap_or(i64::MAX))
        .min(max);
    let half = cap / 2;
    half + (unit(random()) * half as f64).ceil() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_jitter() -> f64 {
        0.0
    }

    #[test]
    fn delay_doubles_from_the_base_to_the_ceiling() {
        let delay = |attempt: u32| {
            compute_retry_delay_ms(
                attempt,
                None,
                DEFAULT_RETRY_BASE_MS,
                DEFAULT_RETRY_MAX_MS,
                no_jitter,
            )
        };
        assert_eq!(delay(1), 2_500);
        assert_eq!(delay(2), 5_000);
        assert_eq!(delay(3), 10_000);
        assert_eq!(delay(4), 20_000);
        // 5s * 2^6 = 320s > 300s ceiling.
        assert_eq!(delay(7), DEFAULT_RETRY_MAX_MS / 2);
        assert_eq!(delay(40), DEFAULT_RETRY_MAX_MS / 2);
    }

    #[test]
    fn jitter_stays_inside_the_upper_half_of_the_cap() {
        for unit in [0.0, 0.25, 0.5, 0.999] {
            let delay = compute_retry_delay_ms(
                1,
                None,
                DEFAULT_RETRY_BASE_MS,
                DEFAULT_RETRY_MAX_MS,
                || unit,
            );
            assert!((2_500..=5_000).contains(&delay), "unit {unit} gave {delay}");
        }
        // A broken RNG must not produce an out-of-range delay.
        let delay =
            compute_retry_delay_ms(1, None, DEFAULT_RETRY_BASE_MS, DEFAULT_RETRY_MAX_MS, || {
                f64::NAN
            });
        assert!((2_500..=5_000).contains(&delay));
    }

    #[test]
    fn a_server_retry_after_wins_over_the_backoff_curve() {
        let delay = compute_retry_delay_ms(5, Some(42_000), 5_000, 300_000, no_jitter);
        assert_eq!(delay, 42_000);
    }

    #[test]
    fn retry_after_is_bounded_and_jittered_by_at_most_ten_percent() {
        let delay = compute_retry_delay_ms(1, Some(9_000_000), 5_000, 300_000, no_jitter);
        assert_eq!(delay, MAX_RETRY_AFTER_MS);

        let jittered = compute_retry_delay_ms(1, Some(100_000), 5_000, 300_000, || 0.999);
        assert!(jittered >= 100_000);
        assert!(jittered <= 105_000, "{jittered}");
    }

    #[test]
    fn retry_after_header_accepts_seconds_and_http_dates() {
        let now = 1_794_744_000_000;
        assert_eq!(parse_retry_after_header("120", now), Some(120_000));
        assert_eq!(parse_retry_after_header("0", now), Some(0));
        assert_eq!(parse_retry_after_header("", now), None);
        assert_eq!(parse_retry_after_header("soon", now), None);
        // Absurd values are clamped rather than trusted.
        assert_eq!(
            parse_retry_after_header("999999", now),
            Some(MAX_RETRY_AFTER_MS)
        );
        let http_date = "2026-11-15T12:02:00Z";
        assert_eq!(parse_retry_after_header(http_date, now), Some(120_000));
        // A date in the past means "retry now", not a negative delay.
        assert_eq!(
            parse_retry_after_header("2026-11-15T11:00:00Z", now),
            Some(0)
        );
    }
}
