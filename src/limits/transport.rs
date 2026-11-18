//! Outbound probe transport and the credential boundary.
//!
//! A probe is: read the credential from the keyring service, make one HTTP
//! request under a deadline, map the result to a [`LimitStatus`].
//!
//! There is no plaintext fallback and no credential file lane: the token comes
//! from [`crate::credentials::CredentialService`] or the probe does not happen.
//! Nothing here logs a credential, and [`ProbeRequest`]'s `Debug` is redacted so
//! an accidental log line cannot leak one either.
//!
//! Network behaviour is ASSUMED, not VERIFIED: see spec §5.

use std::time::Duration;

use super::status::LimitStatus;

/// What to fetch, and how the result should be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeEndpoint {
    /// Provider catalog id: `claude` or `codex`.
    pub provider: &'static str,
    pub url: &'static str,
    /// Logical credential name. The service derives the stored id from it with
    /// [`crate::credentials::stable_credential_id`]; an id is never hand-written,
    /// because the service only accepts its own derived form.
    pub credential: &'static str,
}

impl ProbeEndpoint {
    /// The credential id this endpoint reads.
    pub fn credential_id(&self) -> String {
        crate::credentials::stable_credential_id(self.credential)
    }
}

/// The two endpoints this build knows.
///
/// Recorded from the reference's provider modules (spec §2.4). The paths are
/// ASSUMED current: nothing here has been run against a live account.
pub const CLAUDE_USAGE: ProbeEndpoint = ProbeEndpoint {
    provider: "claude",
    url: "https://api.anthropic.com/api/oauth/usage",
    credential: "claude-oauth",
};

pub const CODEX_USAGE: ProbeEndpoint = ProbeEndpoint {
    provider: "codex",
    url: "https://chatgpt.com/backend-api/wham/usage",
    credential: "codex-oauth",
};

/// A transport failure that is not an HTTP status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportFailure {
    /// The deadline expired before a response arrived.
    Timeout,
    /// The connection failed, or the response was unusable.
    Unreachable,
}

/// One HTTP GET, authenticated.
///
/// `token` is borrowed for the duration of the call and must not be retained.
pub trait LimitsTransport {
    fn fetch(
        &self,
        endpoint: &ProbeEndpoint,
        token: &str,
        deadline: Duration,
    ) -> Result<ProbeResponse, TransportFailure>;
}

/// A 2xx body or a non-2xx status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeResponse {
    pub status: u16,
    pub body: String,
}

impl ProbeResponse {
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The status a non-2xx response maps to.
    ///
    /// Mirrors the reference's error mapping: a rejected credential is
    /// `unauthorized`, a throttle is `rateLimited`, and anything else is
    /// `unavailable`. It never guesses `ok`.
    pub fn failure_status(&self) -> LimitStatus {
        match self.status {
            401 | 403 => LimitStatus::Unauthorized,
            429 => LimitStatus::RateLimited,
            _ => LimitStatus::Unavailable,
        }
    }
}

/// The real transport, over the `ureq` client the updater already uses.
///
/// ASSUMED: this has never been run against a live account (spec §5).
pub struct HttpsTransport;

impl LimitsTransport for HttpsTransport {
    fn fetch(
        &self,
        endpoint: &ProbeEndpoint,
        token: &str,
        deadline: Duration,
    ) -> Result<ProbeResponse, TransportFailure> {
        // One request, one deadline, no redirect following: an API call that
        // answers with a redirect is not something to chase with a credential
        // attached.
        let mut response = shared_agent()
            .get(endpoint.url)
            .header("authorization", &format!("Bearer {token}"))
            .header("accept", "application/json")
            .config()
            .timeout_global(Some(deadline))
            .build()
            .call()
            .map_err(|error| match error {
                ureq::Error::Timeout(_) => TransportFailure::Timeout,
                _ => TransportFailure::Unreachable,
            })?;
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .with_config()
            .limit(MAX_PROBE_BODY_BYTES)
            .read_to_string()
            .map_err(|_| TransportFailure::Unreachable)?;
        Ok(ProbeResponse { status, body })
    }
}

/// A quota payload is small; anything larger is not one.
const MAX_PROBE_BODY_BYTES: u64 = 1024 * 1024;

/// No redirects, HTTPS only, and a shared agent so repeated probes reuse the
/// pool rather than opening a connection each time.
fn shared_agent() -> &'static ureq::Agent {
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .https_only(true)
            .max_redirects(0)
            .http_status_as_error(false)
            .build()
            .new_agent()
    })
}

/// A transport that replays recorded responses.
///
/// Tests use this and only this. A missing recording is `Unreachable` rather
/// than a silent pass, so a fixture that forgets a provider fails loudly.
#[derive(Debug, Default)]
pub struct RecordedTransport {
    responses: Vec<(u16, String)>,
    cursor: std::sync::Mutex<usize>,
}

impl RecordedTransport {
    pub fn new(responses: Vec<(u16, String)>) -> Self {
        Self {
            responses,
            cursor: std::sync::Mutex::new(0),
        }
    }

    /// One recording from a fixture file.
    pub fn from_json(body: &str) -> Result<Self, serde_json::Error> {
        let parsed: serde_json::Value = serde_json::from_str(body)?;
        let mut responses = Vec::new();
        if let Some(entries) = parsed.get("responses").and_then(|value| value.as_array()) {
            for entry in entries {
                let status = entry
                    .get("status")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(200) as u16;
                let body = entry
                    .get("body")
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "{}".to_owned());
                responses.push((status, body));
            }
        }
        Ok(Self::new(responses))
    }
}

impl LimitsTransport for RecordedTransport {
    fn fetch(
        &self,
        _endpoint: &ProbeEndpoint,
        _token: &str,
        _deadline: Duration,
    ) -> Result<ProbeResponse, TransportFailure> {
        let mut cursor = self
            .cursor
            .lock()
            .map_err(|_| TransportFailure::Unreachable)?;
        let recorded = self.responses.get(*cursor).cloned();
        *cursor += 1;
        let (status, body) = recorded.ok_or(TransportFailure::Unreachable)?;
        Ok(ProbeResponse { status, body })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rejected_credential_is_unauthorized() {
        assert_eq!(
            ProbeResponse {
                status: 401,
                body: String::new()
            }
            .failure_status(),
            LimitStatus::Unauthorized
        );
        assert_eq!(
            ProbeResponse {
                status: 403,
                body: String::new()
            }
            .failure_status(),
            LimitStatus::Unauthorized
        );
    }

    #[test]
    fn a_throttle_is_rate_limited_not_unavailable() {
        assert_eq!(
            ProbeResponse {
                status: 429,
                body: String::new()
            }
            .failure_status(),
            LimitStatus::RateLimited
        );
    }

    #[test]
    fn an_unknown_status_never_becomes_ok() {
        for status in [0_u16, 400, 404, 418, 500, 503] {
            let mapped = ProbeResponse {
                status,
                body: String::new(),
            }
            .failure_status();
            assert!(!mapped.is_ok(), "{status} mapped to {mapped:?}");
            assert_eq!(mapped, LimitStatus::Unavailable);
        }
    }

    #[test]
    fn a_recorded_transport_replays_in_order_and_then_fails() {
        let transport =
            RecordedTransport::new(vec![(200, "{\"a\":1}".to_owned()), (429, "{}".to_owned())]);
        let first = transport
            .fetch(&CLAUDE_USAGE, "token", Duration::from_secs(1))
            .unwrap();
        assert_eq!(first.status, 200);
        assert!(first.is_success());
        let second = transport
            .fetch(&CLAUDE_USAGE, "token", Duration::from_secs(1))
            .unwrap();
        assert_eq!(second.failure_status(), LimitStatus::RateLimited);
        assert_eq!(
            transport.fetch(&CLAUDE_USAGE, "token", Duration::from_secs(1)),
            Err(TransportFailure::Unreachable)
        );
    }

    #[test]
    fn a_recording_with_no_entries_is_not_a_pass() {
        let transport = RecordedTransport::from_json("{}").expect("parse");
        assert_eq!(
            transport.fetch(&CODEX_USAGE, "token", Duration::from_secs(1)),
            Err(TransportFailure::Unreachable)
        );
    }

    #[test]
    fn endpoints_name_their_credential_and_never_a_path() {
        for endpoint in [CLAUDE_USAGE, CODEX_USAGE] {
            assert!(
                endpoint.url.starts_with("https://"),
                "{} is not https",
                endpoint.url
            );
            assert!(
                !endpoint.credential.contains('/'),
                "credential looks like a path"
            );
            let id = endpoint.credential_id();
            assert!(id.starts_with("credential-"), "{id} is not a service id");
            assert_eq!(id.len(), 27, "{id} is not the service's id shape");
            assert_eq!(id, endpoint.credential_id(), "the id must be stable");
        }
    }
}
