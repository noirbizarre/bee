//! OpenRouter's browser sign-in, free of the ACPv2 SDK.
//!
//! OpenRouter documents an authorization-code flow with PKCE (RFC 7636) whose
//! result is not an OAuth access token but an *API key*, used as a bearer
//! token for inference. This module is that flow and nothing more general:
//! providers differ in what they return, how long it lives and whether it
//! refreshes, and a universal OAuth client would have to erase those
//! differences. Only the primitives ([`Pkce`], the callback listener, percent
//! encoding) are shared; a second provider gets its own adapter (ADR-008).
//!
//! Nothing here prints. The caller is handed the authorization URL and decides
//! how to show it.
//!
//! The authorization code, the PKCE verifier and the key it buys are secrets:
//! they are wrapped in [`Secret`], and no diagnostic carries them or the
//! provider's response body.

use std::io::{self, ErrorKind, Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::auth::Secret;
use crate::error::{Error, Result};
use crate::http::Http;

/// How long to wait for the browser to call back by default.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

/// The most of a callback request Bee reads. A browser's request line is a few
/// hundred bytes; anything bigger is not one, and reading it all would let any
/// local process make Bee buffer without bound.
const MAX_REQUEST: usize = 8 * 1024;

/// A name for the key, which OpenRouter shows the user to confirm.
const KEY_LABEL: &str = "Bee";

/// The name diagnostics use for this provider.
const PROVIDER: &str = "OpenRouter";

/// `n` random bytes from the operating system, base64url encoded.
fn random_token(bytes: usize) -> Result<String> {
    let mut buffer = vec![0_u8; bytes];
    getrandom::fill(&mut buffer).map_err(|_| Error::AuthRandomness)?;
    Ok(URL_SAFE_NO_PAD.encode(buffer))
}

/// A PKCE verifier and the challenge derived from it (RFC 7636, method `S256`).
///
/// The challenge goes in the browser's URL; the verifier never leaves Bee
/// until the code is exchanged, which is what makes a stolen code useless.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pkce {
    verifier: Secret,
    challenge: String,
}

impl Pkce {
    /// A fresh pair: 32 random bytes make a 43-character verifier, the
    /// shortest RFC 7636 allows and 256 bits of entropy.
    ///
    /// # Errors
    ///
    /// [`Error::AuthRandomness`] if the system has no random bytes to give.
    pub fn generate() -> Result<Self> {
        Ok(Self::from_verifier(&random_token(32)?))
    }

    /// The pair for a given verifier. Public so the derivation can be checked
    /// against the RFC's own example.
    #[must_use]
    pub fn from_verifier(verifier: &str) -> Self {
        let digest = Sha256::digest(verifier.as_bytes());
        Self {
            verifier: Secret::new(verifier),
            challenge: URL_SAFE_NO_PAD.encode(digest),
        }
    }

    /// What the authorization URL carries.
    #[must_use]
    pub fn challenge(&self) -> &str {
        &self.challenge
    }

    /// What the exchange proves possession of.
    #[must_use]
    pub fn verifier(&self) -> &Secret {
        &self.verifier
    }
}

/// An authorization code, single use and short lived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Code(Secret);

impl Code {
    /// Wrap a code the user pasted or the callback carried.
    ///
    /// # Errors
    ///
    /// [`Error::AuthCallbackInvalid`] if it is blank.
    pub fn new(code: &str) -> Result<Self> {
        let code = code.trim();
        if code.is_empty() {
            Err(Error::AuthCallbackInvalid)
        } else {
            Ok(Self(Secret::new(code)))
        }
    }
}

/// OpenRouter's endpoints.
///
/// Parameters, not constants, so tests point them at a fake. They are not
/// configurable by the user: an endpoint that receives the authorization code
/// and returns a key is exactly what a redirect would target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenRouter {
    auth_url: String,
    exchange_url: String,
}

impl Default for OpenRouter {
    fn default() -> Self {
        Self {
            auth_url: "https://openrouter.ai/auth".to_owned(),
            exchange_url: "https://openrouter.ai/api/v1/auth/keys".to_owned(),
        }
    }
}

impl OpenRouter {
    /// OpenRouter's real endpoints.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Other endpoints, for tests.
    #[must_use]
    pub fn with_endpoints(auth_url: impl Into<String>, exchange_url: impl Into<String>) -> Self {
        Self {
            auth_url: auth_url.into(),
            exchange_url: exchange_url.into(),
        }
    }

    /// The URL to send the user to.
    ///
    /// `callback_url` is where the browser returns with the code, `state` is
    /// echoed back on it. Without a callback OpenRouter shows the code on
    /// screen for the user to paste (the headless flow), which is why a
    /// challenge is always sent: a displayed code is useless without the
    /// verifier.
    #[must_use]
    pub fn authorization_url(
        &self,
        pkce: &Pkce,
        state: Option<&str>,
        callback_url: Option<&str>,
    ) -> String {
        let mut url = format!("{}?", self.auth_url);
        if let Some(callback) = callback_url {
            url.push_str(&format!("callback_url={}&", percent_encode(callback)));
        }
        url.push_str(&format!(
            "code_challenge={}&code_challenge_method=S256&key_label={KEY_LABEL}",
            percent_encode(pkce.challenge())
        ));
        if let Some(state) = state {
            url.push_str(&format!("&state={}", percent_encode(state)));
        }
        url
    }

    /// Exchange `code` for the API key it stands for.
    ///
    /// # Errors
    ///
    /// [`Error::AuthExchangeFailed`], with advice per status. The response body
    /// is never part of it.
    pub fn exchange(&self, http: &dyn Http, code: &Code, pkce: &Pkce) -> Result<Secret> {
        let body = json!({
            "code": code.0.expose(),
            "code_verifier": pkce.verifier().expose(),
            "code_challenge_method": "S256",
        })
        .to_string();
        let failed =
            |status: String, reason: &'static str, help: &'static str| Error::AuthExchangeFailed {
                provider: PROVIDER,
                status,
                reason,
                help,
            };
        let response = http.post_json(&self.exchange_url, &body).map_err(|_| {
            failed(
                String::new(),
                "the request did not reach the server",
                "check your network connection and try `bee auth login` again",
            )
        })?;
        let status = format!(" (HTTP {})", response.status);
        match response.status {
            200 => serde_json::from_str::<Value>(&response.body)
                .ok()
                .and_then(|value| value.get("key")?.as_str().map(str::to_owned))
                .filter(|key| !key.is_empty())
                .map(Secret::new)
                .ok_or_else(|| {
                    failed(
                        status,
                        "the response did not contain a key",
                        "try `bee auth login` again; if it repeats, store a key with `--with-token`",
                    )
                }),
            400 => Err(failed(
                status,
                "the code challenge method was not accepted",
                "this is probably a Bee bug; report it, and store a key with `--with-token` meanwhile",
            )),
            403 => Err(failed(
                status,
                "the code or its verifier is not valid, or the code has expired (codes last 10 minutes)",
                "run `bee auth login` again and finish the sign-in promptly, signed in to OpenRouter in that browser",
            )),
            405 => Err(failed(
                status,
                "the request was not an HTTPS POST",
                "a proxy may be rewriting the request; store a key with `--with-token` instead",
            )),
            _ => Err(failed(
                status,
                "the response was not the one expected",
                "try again in a moment; if it repeats, store a key with `--with-token`",
            )),
        }
    }
}

/// A listener on the loopback interface that waits for the browser to return.
///
/// Bound to `127.0.0.1` on a port the OS picks, so nothing off this machine
/// can reach it and no fixed port can be squatted. OpenRouter accepts any
/// localhost port for exactly this use.
#[derive(Debug)]
pub struct CallbackListener {
    listener: TcpListener,
    port: u16,
}

impl CallbackListener {
    /// Bind a free loopback port.
    ///
    /// # Errors
    ///
    /// [`Error::AuthCallbackIo`] if the OS refuses.
    pub fn bind() -> Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .map_err(|source| Error::AuthCallbackIo { source })?;
        let port = listener
            .local_addr()
            .map_err(|source| Error::AuthCallbackIo { source })?
            .port();
        Ok(Self { listener, port })
    }

    /// The URL to register as the callback.
    ///
    /// `localhost`, not the address: it is the name OpenRouter documents for
    /// local callbacks.
    #[must_use]
    pub fn url(&self) -> String {
        format!("http://localhost:{}/callback", self.port)
    }

    /// The port, so a test can play the browser.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Wait for the browser to return, up to `timeout`.
    ///
    /// Requests for anything but `/callback` (a browser asks for a favicon)
    /// are answered and ignored. The first real callback ends the wait.
    ///
    /// # Errors
    ///
    /// [`Error::AuthStateMismatch`] if `state` is not the one sent,
    /// [`Error::AuthDenied`] if the callback reports an error,
    /// [`Error::AuthCallbackInvalid`] if it carries no code,
    /// [`Error::AuthCallbackTimeout`] if nobody came, or
    /// [`Error::AuthCallbackIo`].
    pub fn wait(&self, expected_state: &str, timeout: Duration) -> Result<Code> {
        let io_error = |source| Error::AuthCallbackIo { source };
        // Polled instead of blocking in `accept`, which has no timeout: a user
        // who closes the tab must not leave Bee hanging forever.
        self.listener.set_nonblocking(true).map_err(io_error)?;
        let deadline = Instant::now() + timeout;
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => match handle(stream, expected_state) {
                    Outcome::Ignored => {}
                    Outcome::Code(code) => return Ok(code),
                    Outcome::Denied => return Err(Error::AuthDenied),
                    Outcome::StateMismatch => return Err(Error::AuthStateMismatch),
                    Outcome::Invalid => return Err(Error::AuthCallbackInvalid),
                },
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Err(Error::AuthCallbackTimeout {
                            seconds: timeout.as_secs(),
                        });
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(source) => return Err(io_error(source)),
            }
        }
    }
}

/// What one callback request amounted to.
enum Outcome {
    /// Not the callback; keep waiting.
    Ignored,
    Code(Code),
    Denied,
    StateMismatch,
    Invalid,
}

/// Answer one connection and say what it meant.
fn handle(mut stream: TcpStream, expected_state: &str) -> Outcome {
    // The accepted socket may inherit the listener's non-blocking mode, and a
    // client that connects and says nothing must not stall the wait for good.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));

    let Some(line) = read_request_line(&mut stream) else {
        respond(&mut stream, "400 Bad Request", "Bad request.");
        return Outcome::Ignored;
    };
    let Some((path, query)) = parse_request_line(&line) else {
        respond(&mut stream, "404 Not Found", "Not found.");
        return Outcome::Ignored;
    };
    if path != "/callback" {
        respond(&mut stream, "404 Not Found", "Not found.");
        return Outcome::Ignored;
    }
    let param = |name: &str| {
        query
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    };

    // The pages are fixed text: echoing a parameter back would let any page
    // that can reach localhost put its own content in front of the user.
    if param("error").is_some() {
        respond(
            &mut stream,
            "200 OK",
            "Sign-in was not authorized. You can close this tab and return to the terminal.",
        );
        return Outcome::Denied;
    }
    if param("state") != Some(expected_state) {
        respond(
            &mut stream,
            "400 Bad Request",
            "This sign-in response was not expected. Return to the terminal.",
        );
        return Outcome::StateMismatch;
    }
    match param("code").map(Code::new) {
        Some(Ok(code)) => {
            respond(
                &mut stream,
                "200 OK",
                "Signed in to Bee. You can close this tab and return to the terminal.",
            );
            Outcome::Code(code)
        }
        _ => {
            respond(
                &mut stream,
                "400 Bad Request",
                "The sign-in response carried no code. Return to the terminal.",
            );
            Outcome::Invalid
        }
    }
}

/// The first line of the request, reading at most [`MAX_REQUEST`] bytes.
fn read_request_line(stream: &mut TcpStream) -> Option<String> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 1024];
    while !buffer.contains(&b'\n') {
        if buffer.len() >= MAX_REQUEST {
            return None;
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return None,
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
        }
    }
    let end = buffer.iter().position(|byte| *byte == b'\n')?;
    String::from_utf8(buffer[..end].to_vec())
        .ok()
        .map(|line| line.trim_end().to_owned())
}

/// Split `GET /path?a=b HTTP/1.1` into the path and the decoded parameters.
fn parse_request_line(line: &str) -> Option<(String, Vec<(String, String)>)> {
    let mut parts = line.split(' ');
    // Only GET: a callback is a redirect, which a browser follows with GET.
    if parts.next()? != "GET" {
        return None;
    }
    let target = parts.next()?;
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let query = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(key), percent_decode(value))
        })
        .collect();
    Some((path.to_owned(), query))
}

/// Write a minimal response and close.
fn respond(stream: &mut TcpStream, status: &str, message: &str) {
    let body = format!("<!doctype html><meta charset=\"utf-8\"><title>Bee</title><p>{message}</p>");
    // Best effort: the browser having gone away changes nothing for the flow.
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.flush();
}

/// Run the browser flow: listen, hand `present` the URL, wait, exchange.
///
/// # Errors
///
/// Whatever [`CallbackListener::wait`] and [`OpenRouter::exchange`] report.
pub fn sign_in(
    flow: &OpenRouter,
    http: &dyn Http,
    timeout: Duration,
    present: &mut dyn FnMut(&str),
) -> Result<Secret> {
    let listener = CallbackListener::bind()?;
    let pkce = Pkce::generate()?;
    let state = random_token(16)?;
    present(&flow.authorization_url(&pkce, Some(&state), Some(&listener.url())));
    let code = listener.wait(&state, timeout)?;
    flow.exchange(http, &code, &pkce)
}

/// Run the flow for a machine the browser cannot call back: OpenRouter shows
/// the code and `read_code` collects what the user pastes.
///
/// # Errors
///
/// [`Error::AuthCallbackIo`] if `read_code` fails, [`Error::AuthCallbackInvalid`]
/// if it gives nothing, or what [`OpenRouter::exchange`] reports.
pub fn sign_in_headless(
    flow: &OpenRouter,
    http: &dyn Http,
    present: &mut dyn FnMut(&str),
    read_code: &mut dyn FnMut() -> io::Result<String>,
) -> Result<Secret> {
    let pkce = Pkce::generate()?;
    // No `state`: OpenRouter does not return one without a redirect, and the
    // user pasting the code is the confirmation.
    present(&flow.authorization_url(&pkce, None, None));
    let code = Code::new(&read_code().map_err(|source| Error::AuthCallbackIo { source })?)?;
    flow.exchange(http, &code, &pkce)
}

/// Percent-encode everything but RFC 3986's unreserved characters.
fn percent_encode(text: &str) -> String {
    let mut encoded = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// Decode `%XX` escapes and `+`; anything malformed is kept as it is.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => decoded.push(b' '),
            b'%' => {
                // `get` is `None` past the end or off a character boundary,
                // so a truncated or multi-byte escape falls through as text.
                let hex = text
                    .get(index + 1..index + 3)
                    .and_then(|hex| u8::from_str_radix(hex, 16).ok());
                if let Some(byte) = hex {
                    decoded.push(byte);
                    index += 2;
                } else {
                    decoded.push(b'%');
                }
            }
            byte => decoded.push(byte),
        }
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_encoding_leaves_only_unreserved_characters() {
        assert_eq!(
            percent_encode("http://localhost:1/callback?x=a b"),
            "http%3A%2F%2Flocalhost%3A1%2Fcallback%3Fx%3Da%20b"
        );
        assert_eq!(percent_encode("A-z_0.9~"), "A-z_0.9~");
    }

    #[test]
    fn percent_decoding_inverts_encoding_and_tolerates_malformed_escapes() {
        let text = "http://localhost:1/callback?x=a b&é";
        assert_eq!(percent_decode(&percent_encode(text)), text);
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz%4"), "%zz%4");
    }

    #[test]
    fn a_request_line_is_split_into_path_and_decoded_parameters() {
        let (path, query) =
            parse_request_line("GET /callback?code=a%2Fb&state=s&flag HTTP/1.1").unwrap();
        assert_eq!(path, "/callback");
        assert_eq!(
            query,
            vec![
                ("code".to_owned(), "a/b".to_owned()),
                ("state".to_owned(), "s".to_owned()),
                ("flag".to_owned(), String::new()),
            ]
        );
    }

    #[test]
    fn only_get_requests_can_be_a_callback() {
        assert!(parse_request_line("POST /callback?code=x HTTP/1.1").is_none());
        assert!(parse_request_line("").is_none());
    }
}
