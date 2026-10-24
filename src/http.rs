//! The one HTTP seam, free of the ACPv2 SDK.
//!
//! A trait so the sign-in flow is tested against a fake and never the network,
//! and so the client behind it can change without touching the flow. The real
//! sign-in client is blocking; inference uses cancellable asynchronous I/O.

use std::fmt;
use std::time::Duration;

use futures::StreamExt;
use serde_json::Value;

use crate::llm::Error;

/// Maximum size of one wire event or assembled tool argument.
pub(crate) const MAX_EVENT: usize = 1024 * 1024;
const MAX_RESPONSE: usize = 16 * MAX_EVENT;
type EventSink<'a> = dyn FnMut(Option<&str>, &str) -> Result<(), Error> + Send + 'a;

/// Async inference transport. No debug implementation can expose its requests.
#[derive(Clone)]
pub(crate) struct InferenceHttp(reqwest::Client);

impl InferenceHttp {
    pub(crate) fn new() -> Result<Self, Error> {
        reqwest::Client::builder()
            // Even a same-origin redirect can change where a prompt is stored.
            .redirect(reqwest::redirect::Policy::none())
            // Even protocol-level retries must not duplicate billable/tool work.
            .retry(reqwest::retry::never())
            .build()
            .map(Self)
            .map_err(|_| Error::Transport)
    }

    pub(crate) async fn post(
        &self,
        url: &str,
        headers: reqwest::header::HeaderMap,
        body: Value,
        timeout: Duration,
        event: &mut EventSink<'_>,
    ) -> Result<(), Error> {
        let response = self
            .0
            .post(url)
            .headers(headers)
            .json(&body)
            .timeout(timeout)
            .send()
            .await
            .map_err(transport_error)?;
        match response.status().as_u16() {
            200..=299 => {}
            401 | 403 => return Err(Error::Authentication),
            429 => return Err(Error::RateLimit),
            status => return Err(Error::Http(status)),
        }
        let streaming = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.split(';').next() == Some("text/event-stream"));
        let mut stream = response.bytes_stream();
        let mut total = 0;
        let mut sse = Sse::default();
        let mut json = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(transport_error)?;
            total += chunk.len();
            if total > MAX_RESPONSE {
                return Err(Error::Limit);
            }
            if streaming {
                sse.push(&chunk, event)?;
            } else {
                json.extend_from_slice(&chunk);
            }
        }
        if streaming {
            // An unterminated event is truncated, not an implicit terminal event.
            sse.finish()
        } else {
            event(
                None,
                std::str::from_utf8(&json).map_err(|_| Error::Malformed)?,
            )
        }
    }
}

fn transport_error(error: reqwest::Error) -> Error {
    // reqwest's Display can contain URLs; keep only its classification.
    if error.is_timeout() {
        Error::Timeout
    } else {
        Error::Transport
    }
}

#[derive(Default)]
struct Sse {
    line: Vec<u8>,
    data: String,
    name: String,
    cr: bool,
}

impl Sse {
    fn push(&mut self, bytes: &[u8], event: &mut EventSink<'_>) -> Result<(), Error> {
        for &byte in bytes {
            if self.cr && byte == b'\n' {
                self.cr = false;
                continue;
            }
            self.cr = byte == b'\r';
            if matches!(byte, b'\n' | b'\r') {
                self.line(event)?;
            } else {
                self.line.push(byte);
                if self.line.len() > MAX_EVENT {
                    return Err(Error::Limit);
                }
            }
        }
        Ok(())
    }

    fn line(&mut self, event: &mut EventSink<'_>) -> Result<(), Error> {
        let line = std::str::from_utf8(&self.line).map_err(|_| Error::Malformed)?;
        if line.is_empty() {
            if !self.data.is_empty() {
                self.data.pop();
                event(Some(&self.name), &self.data)?;
            }
            self.data.clear();
            self.name.clear();
        } else if !line.starts_with(':') {
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "data" => {
                    self.data.push_str(value);
                    self.data.push('\n');
                }
                "event" => self.name = value.to_owned(),
                _ => {} // SSE IDs/retry do not change request-local model output.
            }
            if self.data.len() > MAX_EVENT {
                return Err(Error::Limit);
            }
        }
        self.line.clear();
        Ok(())
    }

    fn finish(&self) -> Result<(), Error> {
        if self.line.is_empty() && self.data.is_empty() {
            Ok(())
        } else {
            Err(Error::Malformed)
        }
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_framing_survives_every_possible_byte_boundary() {
        let wire = ": comment\r\nevent: message\r\ndata: héllo\r\ndata: world\r\n\r\n".as_bytes();
        for split in 0..=wire.len() {
            let mut decoder = Sse::default();
            let mut seen = Vec::new();
            let mut sink = |name: Option<&str>, data: &str| {
                seen.push((name.unwrap().to_owned(), data.to_owned()));
                Ok(())
            };
            decoder.push(&wire[..split], &mut sink).unwrap();
            decoder.push(&wire[split..], &mut sink).unwrap();
            decoder.finish().unwrap();
            assert_eq!(seen, vec![("message".into(), "héllo\nworld".into())]);
        }
    }

    #[test]
    fn sse_rejects_unbounded_and_truncated_events() {
        let mut decoder = Sse::default();
        assert_eq!(
            decoder.push(&vec![b'x'; MAX_EVENT + 1], &mut |_, _| Ok(())),
            Err(Error::Limit)
        );
        let mut decoder = Sse::default();
        decoder
            .push(b"data: incomplete\n", &mut |_, _| Ok(()))
            .unwrap();
        assert_eq!(decoder.finish(), Err(Error::Malformed));
    }
}
