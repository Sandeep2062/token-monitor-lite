//! Limits provider status.
//!
//! The vocabulary is the reference's `VALID_STATUSES` in
//! `src/shared/limits/core.js`, plus `timeout`, which
//! `providerDeadline`/`ProbeTimeoutError` emits even though that list omits it.
//! See `docs/migration/limits-spec.md` §1.1.

use std::fmt;

/// A provider's outcome for one attempt.
///
/// `rate_limited` is a provider-wide throttle; `source_rate_limited` is one
/// lane of it. They are kept apart because the reference keeps them apart, and
/// they imply different operator actions.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "camelCase")]
pub enum LimitStatus {
    /// The provider is switched off in settings. Not a failure.
    Disabled,
    /// No credential or account is configured. Not a failure.
    NotConfigured,
    /// The credential was rejected.
    Unauthorized,
    /// The provider answered that the account is over its limit.
    RateLimited,
    /// One lane of the provider is over its limit.
    SourceRateLimited,
    /// The probe exceeded its deadline.
    Timeout,
    /// The provider could not be reached or answered unusably.
    Unavailable,
    /// The attempt failed for another reason.
    Error,
    /// The provider answered with usable quota.
    Ok,
}

impl LimitStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::NotConfigured => "notConfigured",
            Self::Unauthorized => "unauthorized",
            Self::RateLimited => "rateLimited",
            Self::SourceRateLimited => "sourceRateLimited",
            Self::Timeout => "timeout",
            Self::Unavailable => "unavailable",
            Self::Error => "error",
            Self::Ok => "ok",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value.trim() {
            "disabled" => Self::Disabled,
            "notConfigured" => Self::NotConfigured,
            "unauthorized" => Self::Unauthorized,
            "rateLimited" => Self::RateLimited,
            "sourceRateLimited" => Self::SourceRateLimited,
            "timeout" => Self::Timeout,
            "unavailable" => Self::Unavailable,
            "error" => Self::Error,
            "ok" => Self::Ok,
            _ => return None,
        })
    }

    /// Whether a failed attempt may keep the last good snapshot.
    ///
    /// Mirrors `TRANSIENT_STATUSES` in `runtime.js`: these leave cached quota
    /// alone, and every other non-`ok` status clears it. A missing credential or
    /// a rejected one is a real state change, not an outage.
    pub fn is_transient(self) -> bool {
        matches!(
            self,
            Self::Timeout
                | Self::RateLimited
                | Self::SourceRateLimited
                | Self::Unavailable
                | Self::Error
        )
    }

    /// Whether retrying could plausibly help.
    ///
    /// Mirrors `RETRYABLE_LIMIT_STATUSES` in `retryPolicy.js`, which is the same
    /// set as [`Self::is_transient`] — kept as a separate question because a
    /// caller may want to retry a transient failure without retaining its cache,
    /// or the reverse.
    pub fn is_retryable(self) -> bool {
        self.is_transient()
    }

    pub fn is_ok(self) -> bool {
        self == Self::Ok
    }

    /// Whether the account needs a person: sign in, re-authenticate, or open the
    /// provider's console.
    pub fn needs_attention(self) -> bool {
        matches!(self, Self::NotConfigured | Self::Unauthorized)
    }
}

impl fmt::Display for LimitStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_status_round_trips_through_its_wire_spelling() {
        for status in [
            LimitStatus::Disabled,
            LimitStatus::NotConfigured,
            LimitStatus::Unauthorized,
            LimitStatus::RateLimited,
            LimitStatus::SourceRateLimited,
            LimitStatus::Timeout,
            LimitStatus::Unavailable,
            LimitStatus::Error,
            LimitStatus::Ok,
        ] {
            assert_eq!(LimitStatus::parse(status.as_str()), Some(status));
        }
        assert_eq!(LimitStatus::parse("throttled"), None);
        assert_eq!(LimitStatus::parse(""), None);
    }

    #[test]
    fn transient_and_retryable_are_the_reference_sets() {
        for status in [
            LimitStatus::Timeout,
            LimitStatus::RateLimited,
            LimitStatus::SourceRateLimited,
            LimitStatus::Unavailable,
            LimitStatus::Error,
        ] {
            assert!(status.is_transient(), "{status}");
            assert!(status.is_retryable(), "{status}");
        }
        for status in [
            LimitStatus::NotConfigured,
            LimitStatus::Unauthorized,
            LimitStatus::Disabled,
            LimitStatus::Ok,
        ] {
            assert!(!status.is_transient(), "{status}");
            assert!(!status.is_retryable(), "{status}");
        }
    }

    #[test]
    fn only_credential_problems_ask_for_a_person() {
        assert!(LimitStatus::NotConfigured.needs_attention());
        assert!(LimitStatus::Unauthorized.needs_attention());
        assert!(!LimitStatus::Unavailable.needs_attention());
        assert!(!LimitStatus::RateLimited.needs_attention());
        assert!(!LimitStatus::Ok.needs_attention());
    }

    #[test]
    fn ordering_places_ok_last_and_disabled_first() {
        assert!(LimitStatus::Disabled < LimitStatus::Ok);
        assert!(LimitStatus::Timeout < LimitStatus::Ok);
        assert!(LimitStatus::Ok.is_ok());
    }
}
