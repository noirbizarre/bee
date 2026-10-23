//! The one HTTP seam, free of the ACPv2 SDK.
//!
//! A trait so the sign-in flow is tested against a fake and never the network,
//! and so the client behind it can change without touching the flow. The real
//! client is blocking: the library has no async runtime (ADR-003).

use std::fmt;
use std::time::Duration;

/// A response, reduced to what the callers use.
#[derive(Clone, PartialEq, Eq)]
pub struct HttpResponse {
    /// The HTTP status.
    pub status: u16,
    /// The body, which may hold a credential.
    pub body: String,
}

// Written by hand: a token endpoint's body *is* the credential.
impl fmt::Debug for HttpResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpResponse")
            .field("status", &self.status)
            .field("body_len", &self.body.len())
            .finish()
    }
}

/// The request never produced a response: no route, no TLS, a timeout.
///
/// Carries nothing: the transport's own message names URLs and can name more.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unreachable;

/// Sends the few requests the sign-in flow needs.
pub trait Http {
    /// `POST` `body` as JSON to `url`.
    ///
    /// A 4xx or 5xx is a response, not an error: its status is what the
    /// caller explains to the user.
    ///
    /// # Errors
    ///
    /// [`Unreachable`] if no response came back.
    fn post_json(&self, url: &str, body: &str) -> Result<HttpResponse, Unreachable>;
}

/// The real client.
#[derive(Debug, Clone)]
pub struct UreqHttp {
    agent: ureq::Agent,
}

impl UreqHttp {
    /// A client that only speaks HTTPS and gives up after `timeout`.
    ///
    /// HTTPS only: what it carries is an authorization code and the key it
    /// buys, and neither may travel in the clear.
    #[must_use]
    pub fn new(timeout: Duration) -> Self {
        Self::build(timeout, true)
    }

    /// Like [`UreqHttp::new`] but also allowing plain `http://`, for a server
    /// on the loopback interface in a test. Never for a real provider.
    #[must_use]
    pub fn allowing_plain_http(timeout: Duration) -> Self {
        Self::build(timeout, false)
    }

    fn build(timeout: Duration, https_only: bool) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            // Statuses are answers here, not failures: the flow turns each one
            // into advice.
            .http_status_as_error(false)
            .https_only(https_only)
            .build();
        Self {
            agent: config.into(),
        }
    }
}

impl Http for UreqHttp {
    fn post_json(&self, url: &str, body: &str) -> Result<HttpResponse, Unreachable> {
        let mut response = self
            .agent
            .post(url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .send(body)
            .map_err(|_| Unreachable)?;
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .read_to_string()
            .map_err(|_| Unreachable)?;
        Ok(HttpResponse { status, body })
    }
}
