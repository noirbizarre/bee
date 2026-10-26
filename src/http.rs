//! The one HTTP seam, free of the ACPv2 SDK.
//!
//! A trait so the sign-in flow is tested against a fake and never the network,
//! and so the client behind it can change without touching the flow. CLI sign-in
//! has a blocking seam; ACP sign-in and inference use cancellable asynchronous I/O.

use std::fmt;
use std::time::Duration;

use futures::StreamExt;
use serde_json::Value;

use crate::llm::Error;

/// Bounded, redirect-free GET transport for optional model discovery.
pub struct DiscoveryHttp;

impl crate::models::Http for DiscoveryHttp {
    fn get<'a>(
        &'a self,
        url: &'a str,
        headers: reqwest::header::HeaderMap,
        limit: usize,
    ) -> futures::future::BoxFuture<'a, Result<Vec<u8>, crate::models::Error>> {
        Box::pin(async move {
            use crate::models::Error;
            let client = reqwest::Client::builder()
                // Listing headers can be credentials even on a same-origin redirect.
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                .build()
                .map_err(|_| Error::Transport)?;
            let response = client
                .get(url)
                .headers(headers)
                .timeout(Duration::from_secs(15))
                .send()
                .await
                .map_err(|_| Error::Transport)?;
            if !response.status().is_success() {
                if matches!(response.status().as_u16(), 401 | 403) {
                    return Err(Error::Unauthorized);
                }
                if response.status().is_server_error() || response.status().as_u16() == 429 {
                    return Err(Error::Transport);
                }
                return Err(Error::Rejected);
            }
            if response
                .content_length()
                .is_some_and(|size| size > limit as u64)
            {
                return Err(Error::Limit);
            }
            let mut stream = response.bytes_stream();
            let mut bytes = Vec::new();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|_| Error::Transport)?;
                if bytes.len().saturating_add(chunk.len()) > limit {
                    return Err(Error::Limit);
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(bytes)
        })
    }
}

/// Maximum size of one wire event or assembled tool argument.
pub(crate) const MAX_EVENT: usize = 1024 * 1024;
const MAX_RESPONSE: usize = 16 * MAX_EVENT;
type EventSink<'a> = dyn FnMut(Option<&str>, &str) -> Result<(), Error> + Send + 'a;

/// Async inference transport. No debug implementation can expose its requests.
#[derive(Clone)]
pub(crate) struct InferenceHttp {
    client: reqwest::Client,
    #[cfg(test)]
    test_route: Option<String>,
}

impl InferenceHttp {
    pub(crate) fn new() -> Result<Self, Error> {
        reqwest::Client::builder()
            // Even a same-origin redirect can change where a prompt is stored.
            .redirect(reqwest::redirect::Policy::none())
            // Even protocol-level retries must not duplicate billable/tool work.
            .retry(reqwest::retry::never())
            .build()
            .map(|client| Self {
                client,
                #[cfg(test)]
                test_route: None,
            })
            .map_err(|_| Error::Transport)
    }

    #[cfg(test)]
    pub(crate) fn test_routed(route: String) -> Self {
        let mut transport = Self::new().expect("test client");
        transport.test_route = Some(route);
        transport
    }

    pub(crate) async fn post(
        &self,
        url: &str,
        headers: reqwest::header::HeaderMap,
        body: Value,
        timeout: Duration,
        protocol: crate::provider::Protocol,
        event: &mut EventSink<'_>,
    ) -> Result<(), Error> {
        #[cfg(test)]
        let routed;
        #[cfg(test)]
        let url = if let Some(route) = &self.test_route {
            // Preserve official endpoint identity for capability checks while exercising
            // the real adapter against a credential-free local test server.
            let suffix = match url {
                "https://api.openai.com/v1/responses" => "/responses",
                "https://api.openai.com/v1/chat/completions" => "/chat/completions",
                "https://api.anthropic.com/v1/messages" => "/v1/messages",
                _ => panic!("test route requires a known official endpoint"),
            };
            routed = format!("{route}{suffix}");
            routed.as_str()
        } else {
            url
        };
        let response = self
            .client
            .post(url)
            .headers(headers)
            .json(&body)
            .timeout(timeout)
            .send()
            .await
            .map_err(transport_error)?;
        match response.status().as_u16() {
            200..=299 => {}
            status => {
                // Error bodies can contain credentials or prompts; inspect a bounded
                // prefix privately and retain the status when classification is unknown.
                let mut stream = response.bytes_stream();
                let mut bytes = Vec::new();
                while let Some(chunk) = stream.next().await {
                    let chunk = chunk.map_err(transport_error)?;
                    if bytes.len().saturating_add(chunk.len()) > 64 * 1024 {
                        return Err(Error::Http(status));
                    }
                    bytes.extend_from_slice(&chunk);
                }
                if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
                    let error = crate::llm::provider_error(protocol, &value["error"]);
                    if error != Error::Provider {
                        return Err(error);
                    }
                }
                return Err(match status {
                    401 | 403 => Error::Authentication,
                    429 => Error::RateLimit,
                    _ => Error::Http(status),
                });
            }
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

/// Async sign-in transport, injectable without blocking an ACP dispatch loop.
pub trait LoginHttp: Send + Sync {
    /// OAuth public clients exchange form bodies; default keeps existing fakes compatible.
    fn post_form<'a>(
        &'a self,
        _url: &'a str,
        _body: String,
    ) -> futures::future::BoxFuture<'a, Result<HttpResponse, Unreachable>> {
        Box::pin(async { Err(Unreachable) })
    }
    /// Trusted OIDC discovery and signing keys, without credentials or redirects.
    fn get<'a>(
        &'a self,
        _url: &'a str,
    ) -> futures::future::BoxFuture<'a, Result<HttpResponse, Unreachable>> {
        Box::pin(async { Err(Unreachable) })
    }
    /// Exchange a credential-bearing JSON body without retaining it in diagnostics.
    fn post_json<'a>(
        &'a self,
        url: &'a str,
        body: String,
    ) -> futures::future::BoxFuture<'a, Result<HttpResponse, Unreachable>>;
}

/// HTTPS-only production sign-in transport.
pub struct AsyncLoginHttp;

impl LoginHttp for AsyncLoginHttp {
    fn post_form<'a>(
        &'a self,
        url: &'a str,
        body: String,
    ) -> futures::future::BoxFuture<'a, Result<HttpResponse, Unreachable>> {
        Box::pin(async move { post_login_form(&login_client(true)?, url, body).await })
    }
    fn get<'a>(
        &'a self,
        url: &'a str,
    ) -> futures::future::BoxFuture<'a, Result<HttpResponse, Unreachable>> {
        Box::pin(async move { login_response(login_client(true)?.get(url)).await })
    }
    fn post_json<'a>(
        &'a self,
        url: &'a str,
        body: String,
    ) -> futures::future::BoxFuture<'a, Result<HttpResponse, Unreachable>> {
        Box::pin(login_exchange(url, body))
    }
}

/// Owned credential exchange; dropping the future abandons network work.
pub(crate) async fn login_exchange(url: &str, body: String) -> Result<HttpResponse, Unreachable> {
    post_login(&login_client(true)?, url, body).await
}

fn login_client(https_only: bool) -> Result<reqwest::Client, Unreachable> {
    // Only local transport tests disable HTTPS; the production entry point always enables it.
    reqwest::Client::builder()
        .https_only(https_only)
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|_| Unreachable)
}

async fn post_login(
    client: &reqwest::Client,
    url: &str,
    body: String,
) -> Result<HttpResponse, Unreachable> {
    login_response(
        client
            .post(url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .body(body),
    )
    .await
}

async fn post_login_form(
    client: &reqwest::Client,
    url: &str,
    body: String,
) -> Result<HttpResponse, Unreachable> {
    login_response(
        client
            .post(url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body),
    )
    .await
}

async fn login_response(request: reqwest::RequestBuilder) -> Result<HttpResponse, Unreachable> {
    let response = request.send().await.map_err(|_| Unreachable)?;
    let status = response.status().as_u16();
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| Unreachable)?;
        if bytes.len().saturating_add(chunk.len()) > MAX_EVENT {
            return Err(Unreachable);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(HttpResponse {
        status,
        body: String::from_utf8(bytes).map_err(|_| Unreachable)?,
    })
}

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
            // A redirect must not forward the authorization code and verifier.
            .max_redirects(0)
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
            // Match the async login bound instead of the larger default body limit.
            .with_config()
            .limit(MAX_EVENT as u64)
            .read_to_string()
            .map_err(|_| Unreachable)?;
        Ok(HttpResponse { status, body })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Return the socket after writing a response, so tests can observe client cleanup.
    async fn login_server(
        response: Vec<u8>,
        keep_open: bool,
    ) -> (String, tokio::task::JoinHandle<tokio::net::TcpStream>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/token", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut headers = Vec::new();
            while !headers.ends_with(b"\r\n\r\n") {
                assert!(headers.len() < 8192);
                headers.push(stream.read_u8().await.unwrap());
            }
            // All requests use this fixed non-secret body; consume it before checking EOF.
            let mut body = [0; 2];
            stream.read_exact(&mut body).await.unwrap();
            assert_eq!(&body, b"{}");
            // Oversize-response tests intentionally close before the server finishes writing.
            let _ = stream.write_all(&response).await;
            if !keep_open {
                let _ = stream.shutdown().await;
            }
            stream
        });
        (url, server)
    }

    #[tokio::test]
    async fn production_login_transport_rejects_plain_http_before_connecting() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/token", listener.local_addr().unwrap());
        assert_eq!(
            AsyncLoginHttp.post_json(&url, "{}".into()).await,
            Err(Unreachable)
        );
        assert_eq!(
            AsyncLoginHttp.post_form(&url, "code=secret".into()).await,
            Err(Unreachable)
        );
        assert_eq!(
            LoginHttp::get(&AsyncLoginHttp, &url).await,
            Err(Unreachable)
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(20), listener.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn oauth_forms_and_oidc_gets_preserve_wire_methods_and_do_not_follow_redirects() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for method in ["form", "get"] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/oauth", listener.local_addr().unwrap());
            let body = "code=a%2Bb%26c&client_id=issued-client";
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut head = Vec::new();
                while !head.ends_with(b"\r\n\r\n") {
                    head.push(socket.read_u8().await.unwrap());
                }
                let head = String::from_utf8(head).unwrap();
                let length = head
                    .lines()
                    .find_map(|line| {
                        line.split_once(':')
                            .filter(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                            .map(|(_, v)| v.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                let mut received = vec![0; length];
                socket.read_exact(&mut received).await.unwrap();
                if method == "form" {
                    assert!(head.starts_with("POST /oauth "));
                    assert!(head.contains("content-type: application/x-www-form-urlencoded"));
                    assert_eq!(received, body.as_bytes());
                } else {
                    assert!(head.starts_with("GET /oauth "));
                    assert!(received.is_empty());
                }
                socket.write_all(b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/must-not-follow\r\nContent-Length: 0\r\n\r\n").await.unwrap();
            });
            let client = login_client(false).unwrap();
            let response = if method == "form" {
                post_login_form(&client, &url, body.into()).await
            } else {
                login_response(client.get(&url)).await
            };
            assert_eq!(response.unwrap().status, 302);
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn credential_exchange_redirects_are_returned_without_following_them() {
        let response = b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/must-not-follow\r\nContent-Length: 0\r\n\r\n".to_vec();
        let (url, server) = login_server(response, false).await;
        let client = login_client(false).unwrap();
        let (result, socket) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(post_login(&client, &url, "{}".into()), server)
        })
        .await
        .unwrap();
        drop(socket.unwrap());
        assert_eq!(result.unwrap().status, 302);
    }

    #[rstest::rstest]
    #[case::exact_limit(MAX_EVENT, false)]
    #[case::oversized(MAX_EVENT + 1, false)]
    #[case::truncated(10, true)]
    #[tokio::test]
    async fn credential_response_reads_enforce_size_and_complete_framing(
        #[case] size: usize,
        #[case] truncated: bool,
    ) {
        let mut response =
            format!("HTTP/1.1 200 OK\r\nContent-Length: {size}\r\n\r\n").into_bytes();
        response.extend(std::iter::repeat_n(
            b'x',
            if truncated { size - 1 } else { size },
        ));
        let (url, server) = login_server(response, false).await;
        let client = login_client(false).unwrap();
        let (result, socket) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(post_login(&client, &url, "{}".into()), server)
        })
        .await
        .unwrap();
        drop(socket.unwrap());
        if truncated || size > MAX_EVENT {
            assert_eq!(result, Err(Unreachable));
        } else {
            assert_eq!(result.unwrap().body.len(), size);
        }
    }

    #[rstest::rstest]
    #[case::drop(false)]
    #[case::deadline(true)]
    #[tokio::test]
    async fn abandoning_a_credential_response_closes_its_pending_body_read(#[case] deadline: bool) {
        use tokio::io::AsyncReadExt;
        let (url, server) = login_server(
            b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n".to_vec(),
            true,
        )
        .await;
        let client = login_client(false).unwrap();
        let mut operation = Box::pin(post_login(&client, &url, "{}".into()));
        // Wait for response headers with the request actually being polled, not a timing sleep.
        let mut socket = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! {
                socket = server => socket.unwrap(),
                result = &mut operation => panic!("body must remain pending: {result:?}"),
            }
        })
        .await
        .unwrap();
        if deadline {
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(31)).await;
            tokio::time::resume();
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(5), operation)
                    .await
                    .unwrap(),
                Err(Unreachable)
            );
        } else {
            drop(operation);
        }
        let mut byte = [0];
        let closed = tokio::time::timeout(Duration::from_secs(5), socket.read(&mut byte))
            .await
            .unwrap();
        // Abandoning unread HTTP data can close with FIN or reset depending on
        // the OS and whether headers were consumed before the future was dropped.
        assert!(
            matches!(closed, Ok(0))
                || closed.is_err_and(|error| matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
                ))
        );
    }

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
