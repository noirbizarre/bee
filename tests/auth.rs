//! Provider credentials, the credential store and the OpenRouter sign-in,
//! in-process.
//!
//! Nothing here touches the network or the developer's own files: the
//! environment is described by value, stores are in memory or a temporary
//! directory, HTTP is a fake and "the browser" is a thread talking to the
//! loopback listener.

use std::cell::RefCell;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::thread;
use std::time::Duration;

use bee::Error;
use bee::auth::{
    self, Audience, Credential, CredentialStore, FileStore, ProfileStatus, Secret, Status,
    StoreState,
};
use bee::config::{Config, Environment, project_config_path};
use bee::http::{Http, HttpResponse, Unreachable};
use bee::login::{self, CallbackListener, Code, Pkce};
use bee::preset::{Catalog, LoginSpec};
use bee::provider::{Profile, Protocol, Providers};
use pretty_assertions::assert_eq;
use rstest::rstest;
use tempfile::TempDir;

#[path = "support/credential_store.rs"]
mod test_store;
use test_store::MemoryStore;

/// A secret that must never appear in any diagnostic or debug output.
const SECRET: &str = "sk-super-secret-token";

/// A profile that keeps its credential in the store, with a browser sign-in.
const STORED: &str = r#"
provider = "or"

[providers.or]
protocols = { openai-chat = { base_url = "https://openrouter.ai/api/v1" } }
credential = { store = true, login = "openrouter" }

[providers.token]
protocols = { anthropic-messages = {} }
credential = { store = true }

[providers.env]
protocols = { openai-chat = {} }
credential = { env = "BEE_TEST_KEY" }

[providers.local]
protocols = { openai-chat = { base_url = "http://localhost:11434/v1" } }
"#;

struct Sandbox {
    root: TempDir,
}

impl Sandbox {
    fn new() -> Self {
        Self {
            root: TempDir::new().unwrap(),
        }
    }

    fn env(&self) -> Environment {
        Environment::new(None, Some(self.root.path().join("home")), None)
    }

    fn store_path(&self) -> std::path::PathBuf {
        self.root.path().join("data/bee/credentials.json")
    }

    fn store(&self) -> FileStore {
        FileStore::new(self.store_path())
    }

    fn providers(&self, text: &str) -> Providers {
        let project = self.root.path().join("project");
        fs::create_dir_all(project_config_path(&project).parent().unwrap()).unwrap();
        fs::write(project_config_path(&project), text).unwrap();
        Providers::from_config(&Config::load(&self.env(), &project).unwrap()).unwrap()
    }
}

/// The shipped OpenRouter sign-in, which is data (ADR-009).
fn openrouter() -> LoginSpec {
    Catalog::builtin()
        .login_spec("openrouter")
        .unwrap()
        .unwrap()
}

fn rendered(error: Error) -> String {
    format!("{:?}", miette::Report::new(error))
}

fn profile<'a>(providers: &'a Providers, name: &str) -> &'a Profile {
    providers.get(name).unwrap()
}

/// What the fake answers to the next POST, and what it was sent.
struct FakeHttp {
    answer: RefCell<Option<std::result::Result<HttpResponse, Unreachable>>>,
    requests: RefCell<Vec<(String, serde_json::Value)>>,
}

impl FakeHttp {
    fn answering(status: u16, body: &str) -> Self {
        Self {
            answer: RefCell::new(Some(Ok(HttpResponse {
                status,
                body: body.to_owned(),
            }))),
            requests: RefCell::default(),
        }
    }

    fn unreachable() -> Self {
        Self {
            answer: RefCell::new(Some(Err(Unreachable))),
            requests: RefCell::default(),
        }
    }

    fn only_request(&self) -> (String, serde_json::Value) {
        let requests = self.requests.borrow();
        assert_eq!(requests.len(), 1, "exactly one request was expected");
        requests[0].clone()
    }
}

impl Http for FakeHttp {
    fn post_json(&self, url: &str, body: &str) -> std::result::Result<HttpResponse, Unreachable> {
        self.requests
            .borrow_mut()
            .push((url.to_owned(), serde_json::from_str(body).unwrap()));
        self.answer
            .borrow_mut()
            .take()
            .expect("one answer per test")
    }
}

/// Play the browser: one GET to the listener, returning the whole response.
fn browse(port: u16, target: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(stream, "GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    response
}

// ---- PKCE ------------------------------------------------------------------

#[test]
fn the_pkce_challenge_matches_the_rfc_7636_example() {
    // RFC 7636, Appendix B: the one published vector for the S256 method.
    let pkce = Pkce::from_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");

    assert_eq!(
        pkce.challenge(),
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
}

#[test]
fn a_generated_verifier_is_long_url_safe_and_never_repeats() {
    let first = Pkce::generate().unwrap();
    let second = Pkce::generate().unwrap();

    let verifier = first.verifier().expose();
    assert_eq!(verifier.len(), 43);
    assert!(
        verifier
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "the verifier must be base64url without padding"
    );
    assert_ne!(first.verifier(), second.verifier());
}

#[test]
fn the_authorization_url_carries_the_challenge_state_and_an_encoded_callback() {
    let pkce = Pkce::from_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");

    let url = openrouter().authorization_url(
        &pkce,
        Some("st4te"),
        Some("http://localhost:4242/callback"),
    );

    assert!(url.starts_with("https://openrouter.ai/auth?"), "{url}");
    assert!(
        url.contains("callback_url=http%3A%2F%2Flocalhost%3A4242%2Fcallback"),
        "{url}"
    );
    assert!(
        url.contains("code_challenge=E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"),
        "{url}"
    );
    assert!(url.contains("code_challenge_method=S256"), "{url}");
    assert!(url.contains("state=st4te"), "{url}");
    assert!(
        !url.contains("dBjftJeZ4CVP"),
        "the verifier must stay out of the URL"
    );
}

#[test]
fn the_headless_authorization_url_has_a_challenge_but_no_callback() {
    let pkce = Pkce::generate().unwrap();

    let url = openrouter().authorization_url(&pkce, None, None);

    assert!(!url.contains("callback_url"), "{url}");
    assert!(url.contains("code_challenge_method=S256"), "{url}");
    assert!(!url.contains("state="), "{url}");
}

// ---- callback listener -----------------------------------------------------

#[test]
fn a_callback_with_the_right_state_yields_its_code() {
    let listener = CallbackListener::bind().unwrap();
    let port = listener.port();
    let browser = thread::spawn(move || browse(port, "/callback?code=abc%2F123&state=expected"));

    let code = listener.wait("expected", Duration::from_secs(5)).unwrap();

    assert_eq!(code, Code::new("abc/123").unwrap());
    let page = browser.join().unwrap();
    assert!(page.starts_with("HTTP/1.1 200"), "{page}");
    assert!(!page.contains("abc"), "the page must never echo the code");
}

#[test]
fn stray_requests_are_answered_and_do_not_end_the_wait() {
    let listener = CallbackListener::bind().unwrap();
    let port = listener.port();
    let browser = thread::spawn(move || {
        let favicon = browse(port, "/favicon.ico");
        // Far more than any callback is, and no line break: Bee must not buffer
        // it, and must keep waiting afterwards.
        if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) {
            let _ = stream.write_all(&vec![b'a'; 64 * 1024]);
        }
        browse(port, "/callback?code=real&state=s");
        favicon
    });

    let code = listener.wait("s", Duration::from_secs(10)).unwrap();

    assert_eq!(code, Code::new("real").unwrap());
    assert!(browser.join().unwrap().starts_with("HTTP/1.1 404"));
}

#[test]
fn a_callback_with_another_state_is_refused() {
    let listener = CallbackListener::bind().unwrap();
    let port = listener.port();
    let browser = thread::spawn(move || browse(port, "/callback?code=stolen&state=forged"));

    let error = listener
        .wait("expected", Duration::from_secs(5))
        .unwrap_err();

    assert!(matches!(error, Error::AuthStateMismatch), "{error:?}");
    let page = browser.join().unwrap();
    assert!(
        !page.contains("forged") && !page.contains("stolen"),
        "{page}"
    );
}

#[test]
fn a_callback_without_a_state_is_refused() {
    let listener = CallbackListener::bind().unwrap();
    let port = listener.port();
    let browser = thread::spawn(move || browse(port, "/callback?code=x"));

    let error = listener
        .wait("expected", Duration::from_secs(5))
        .unwrap_err();

    browser.join().unwrap();
    assert!(matches!(error, Error::AuthStateMismatch), "{error:?}");
}

#[test]
fn a_callback_reporting_an_error_is_a_denial() {
    let listener = CallbackListener::bind().unwrap();
    let port = listener.port();
    let browser =
        thread::spawn(move || browse(port, "/callback?state=expected&error=access_denied"));

    let error = listener
        .wait("expected", Duration::from_secs(5))
        .unwrap_err();

    browser.join().unwrap();
    assert!(matches!(error, Error::AuthDenied), "{error:?}");
}

#[test]
fn a_denial_without_the_expected_state_is_not_trusted() {
    let listener = CallbackListener::bind().unwrap();
    let port = listener.port();
    let browser = thread::spawn(move || browse(port, "/callback?state=wrong&error=access_denied"));
    let error = listener
        .wait("expected", Duration::from_secs(5))
        .unwrap_err();
    browser.join().unwrap();
    assert!(matches!(error, Error::AuthStateMismatch));
}

#[test]
fn a_callback_without_a_code_is_invalid() {
    let listener = CallbackListener::bind().unwrap();
    let port = listener.port();
    let browser = thread::spawn(move || browse(port, "/callback?state=expected"));

    let error = listener
        .wait("expected", Duration::from_secs(5))
        .unwrap_err();

    browser.join().unwrap();
    assert!(matches!(error, Error::AuthCallbackInvalid), "{error:?}");
}

#[test]
fn waiting_for_a_browser_that_never_comes_times_out() {
    let listener = CallbackListener::bind().unwrap();

    let error = listener
        .wait("expected", Duration::from_millis(150))
        .unwrap_err();

    assert!(
        matches!(error, Error::AuthCallbackTimeout { .. }),
        "{error:?}"
    );
    assert!(rendered(error).contains("bee::auth::callback_timeout"));
}

// ---- exchange ---------------------------------------------------------------

#[test]
fn a_code_is_exchanged_for_a_key_proving_possession_of_the_verifier() {
    let http = FakeHttp::answering(200, &format!(r#"{{"key":"{SECRET}"}}"#));
    let pkce = Pkce::from_verifier("verifier-123");

    let key = openrouter()
        .exchange(&http, &Code::new("code-456").unwrap(), &pkce)
        .unwrap();

    assert_eq!(key.expose(), SECRET);
    let (url, body) = http.only_request();
    assert_eq!(url, "https://openrouter.ai/api/v1/auth/keys");
    assert_eq!(
        body,
        serde_json::json!({
            "code": "code-456",
            "code_verifier": "verifier-123",
            "code_challenge_method": "S256",
        })
    );
}

#[rstest]
#[case::bad_method(400, "bee::auth::exchange_failed", "challenge method")]
#[case::expired_or_wrong(403, "bee::auth::exchange_failed", "expired")]
#[case::wrong_verb(405, "bee::auth::exchange_failed", "HTTPS POST")]
#[case::server_error(500, "bee::auth::exchange_failed", "not the one expected")]
fn a_refused_exchange_is_explained_without_the_response_body(
    #[case] status: u16,
    #[case] code: &str,
    #[case] mentions: &str,
) {
    // The body is where a provider might echo the code or the key.
    let http = FakeHttp::answering(status, &format!(r#"{{"error":"{SECRET}"}}"#));

    let error = openrouter()
        .exchange(&http, &Code::new("c").unwrap(), &Pkce::generate().unwrap())
        .unwrap_err();

    // The message, unwrapped: the rendered report breaks lines mid-phrase.
    let message = error.to_string();
    assert!(message.contains(mentions), "{message}");
    assert!(message.contains(&format!("HTTP {status}")), "{message}");
    let text = rendered(error);
    assert!(text.contains(code), "{text}");
    assert!(!text.contains(SECRET), "{text}");
}

#[test]
fn an_unreachable_provider_is_reported_as_a_network_problem() {
    let error = openrouter()
        .exchange(
            &FakeHttp::unreachable(),
            &Code::new("c").unwrap(),
            &Pkce::generate().unwrap(),
        )
        .unwrap_err();

    assert!(error.to_string().contains("did not reach the server"));
}

#[test]
fn a_success_without_a_key_is_not_taken_for_one() {
    let http = FakeHttp::answering(200, r#"{"key":""}"#);

    let error = openrouter()
        .exchange(&http, &Code::new("c").unwrap(), &Pkce::generate().unwrap())
        .unwrap_err();

    assert!(error.to_string().contains("did not contain a credential"));
}

/// A one-shot HTTP server on loopback: answers `status` and `body`, and returns
/// the request it got.
fn serve_once(status: &str, body: &str) -> (u16, thread::JoinHandle<String>) {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = Vec::new();
        let mut chunk = [0_u8; 1024];
        // Headers, then as much body as `Content-Length` promises.
        loop {
            let read = stream.read(&mut chunk).unwrap();
            request.extend_from_slice(&chunk[..read]);
            let text = String::from_utf8_lossy(&request).into_owned();
            if let Some((head, rest)) = text.split_once("\r\n\r\n") {
                let length = head
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .map(str::to_owned)
                    })
                    .map_or(0, |value| value.trim().parse::<usize>().unwrap());
                if rest.len() >= length {
                    break;
                }
            }
            assert!(read > 0, "the client closed before sending its body");
        }
        stream.write_all(response.as_bytes()).unwrap();
        String::from_utf8(request).unwrap()
    });
    (port, handle)
}

#[test]
fn the_real_http_client_posts_json_and_hands_back_error_statuses_as_answers() {
    let (port, server) = serve_once("403 Forbidden", r#"{"error":"nope"}"#);
    let client = bee::http::UreqHttp::allowing_plain_http(Duration::from_secs(5));

    let response = client
        .post_json(
            &format!("http://127.0.0.1:{port}/api/v1/auth/keys"),
            r#"{"code":"c"}"#,
        )
        .unwrap();

    assert_eq!(response.status, 403);
    assert_eq!(response.body, r#"{"error":"nope"}"#);
    let request = server.join().unwrap();
    assert!(
        request.starts_with("POST /api/v1/auth/keys HTTP/1.1"),
        "{request}"
    );
    assert!(
        request
            .to_ascii_lowercase()
            .contains("content-type: application/json"),
        "{request}"
    );
    assert!(request.ends_with(r#"{"code":"c"}"#), "{request}");
}

#[test]
fn the_real_http_client_refuses_plain_http_unless_told_otherwise() {
    // The listener is never answered: the client must give up before connecting.
    let (port, _server) = serve_once("200 OK", "{}");
    let client = bee::http::UreqHttp::new(Duration::from_secs(2));

    let result = client.post_json(&format!("http://127.0.0.1:{port}/"), "{}");

    assert_eq!(result.unwrap_err(), Unreachable);
}

// ---- whole flows ------------------------------------------------------------

/// The value of `name` in `url`'s query, decoded as far as these tests need.
fn query_param(url: &str, name: &str) -> String {
    let query = url.split_once('?').unwrap().1;
    let value = query
        .split('&')
        .find_map(|pair| pair.strip_prefix(&format!("{name}=")))
        .unwrap_or_else(|| panic!("no `{name}` in {url}"));
    value.replace("%3A", ":").replace("%2F", "/")
}

#[test]
fn the_browser_flow_returns_the_key_after_the_callback() {
    let http = FakeHttp::answering(200, &format!(r#"{{"key":"{SECRET}"}}"#));
    let mut url = String::new();
    let mut browser = None;

    let key = login::sign_in(
        &openrouter(),
        &http,
        Duration::from_secs(5),
        &mut |presented| {
            presented.clone_into(&mut url);
            // The listener is already bound, so the browser may go right away.
            let callback = query_param(presented, "callback_url");
            let state = query_param(presented, "state");
            browser = Some(thread::spawn(move || {
                let port: u16 = callback
                    .strip_prefix("http://localhost:")
                    .unwrap()
                    .split('/')
                    .next()
                    .unwrap()
                    .parse()
                    .unwrap();
                browse(port, &format!("/callback?code=granted&state={state}"));
            }));
        },
    )
    .unwrap();

    browser.unwrap().join().unwrap();
    assert_eq!(key.expose(), SECRET);
    let (_, body) = http.only_request();
    assert_eq!(body["code"], "granted");
    // The verifier sent to the provider is the one the URL's challenge commits to.
    let verifier = body["code_verifier"].as_str().unwrap();
    assert_eq!(
        Pkce::from_verifier(verifier).challenge(),
        query_param(&url, "code_challenge")
    );
}

#[test]
fn a_failed_browser_flow_never_reaches_the_exchange() {
    let http = FakeHttp::answering(200, r#"{"key":"unused"}"#);

    let error = login::sign_in(
        &openrouter(),
        &http,
        Duration::from_millis(100),
        &mut |_| {},
    )
    .unwrap_err();

    assert!(
        matches!(error, Error::AuthCallbackTimeout { .. }),
        "{error:?}"
    );
    assert!(http.requests.borrow().is_empty(), "no code, so no exchange");
}

#[test]
fn the_headless_flow_exchanges_the_code_the_user_pastes() {
    let http = FakeHttp::answering(200, &format!(r#"{{"key":"{SECRET}"}}"#));
    let mut url = String::new();

    let key = login::sign_in_headless(
        &openrouter(),
        &http,
        &mut |presented| presented.clone_into(&mut url),
        &mut || Ok("  pasted-code \n".to_owned()),
    )
    .unwrap();

    assert_eq!(key.expose(), SECRET);
    assert!(!url.contains("callback_url"), "{url}");
    let (_, body) = http.only_request();
    assert_eq!(body["code"], "pasted-code");
}

#[test]
fn pasting_nothing_in_the_headless_flow_is_refused() {
    let http = FakeHttp::unreachable();

    let error = login::sign_in_headless(&openrouter(), &http, &mut |_| {}, &mut || {
        Ok("\n".to_owned())
    })
    .unwrap_err();

    assert!(matches!(error, Error::AuthCallbackInvalid), "{error:?}");
    assert!(http.requests.borrow().is_empty());
}

// ---- credential sources -------------------------------------------------------

#[test]
fn a_profile_without_a_credential_is_unauthenticated() {
    let sandbox = Sandbox::new();
    let providers = sandbox.providers(STORED);

    let credential = auth::resolve(
        profile(&providers, "local"),
        &sandbox.env(),
        &sandbox.store(),
        0,
    )
    .unwrap();

    assert_eq!(credential, None);
}

#[test]
fn an_environment_credential_is_read_from_the_described_environment() {
    let sandbox = Sandbox::new();
    let providers = sandbox.providers(STORED);
    let env = sandbox.env().with_variable("BEE_TEST_KEY", SECRET);

    let credential = auth::resolve(profile(&providers, "env"), &env, &sandbox.store(), 0)
        .unwrap()
        .unwrap();

    assert_eq!(credential.secret().expose(), SECRET);
}

#[rstest]
#[case::unset(None)]
#[case::empty(Some(""))]
fn an_unset_or_empty_variable_is_a_missing_credential(#[case] value: Option<&str>) {
    let sandbox = Sandbox::new();
    let providers = sandbox.providers(STORED);
    let env = match value {
        Some(value) => sandbox.env().with_variable("BEE_TEST_KEY", value),
        None => sandbox.env(),
    };

    let error = auth::resolve(profile(&providers, "env"), &env, &sandbox.store(), 0).unwrap_err();

    let text = rendered(error);
    assert!(text.contains("bee::auth::missing_env"), "{text}");
    assert!(text.contains("BEE_TEST_KEY"), "{text}");
}

#[test]
fn a_stored_credential_is_found_after_a_login() {
    let sandbox = Sandbox::new();
    let providers = sandbox.providers(STORED);
    let store = MemoryStore::default();
    auth::store_secret(
        profile(&providers, "token"),
        &format!("  {SECRET}\n"),
        &store,
    )
    .unwrap();

    let credential = auth::resolve(profile(&providers, "token"), &sandbox.env(), &store, 0)
        .unwrap()
        .unwrap();

    assert_eq!(
        credential.secret().expose(),
        SECRET,
        "whitespace is trimmed"
    );
}

#[test]
fn a_stored_credential_that_is_absent_points_at_the_login_command() {
    let sandbox = Sandbox::new();
    let providers = sandbox.providers(STORED);

    let error = auth::resolve(
        profile(&providers, "token"),
        &sandbox.env(),
        &MemoryStore::default(),
        0,
    )
    .unwrap_err();

    let text = rendered(error);
    assert!(text.contains("bee::auth::not_logged_in"), "{text}");
    assert!(text.contains("bee auth login token"), "{text}");
}

#[test]
fn an_expired_credential_is_not_used() {
    let sandbox = Sandbox::new();
    let providers = sandbox.providers(STORED);
    let store = MemoryStore::default();
    store
        .put(
            Audience::Provider("or"),
            &Credential::new(Secret::new(SECRET)).expiring_at(1_000),
        )
        .unwrap();
    let resolve = |now| auth::resolve(profile(&providers, "or"), &sandbox.env(), &store, now);

    assert!(resolve(999).unwrap().is_some(), "still valid just before");
    let text = rendered(resolve(1_000).unwrap_err());
    assert!(text.contains("bee::auth::expired"), "{text}");
    assert!(text.contains("bee auth login or"), "{text}");
    assert!(!text.contains(SECRET), "{text}");
}

#[rstest]
#[case::openai(Protocol::OpenAiChat, "Authorization", format!("Bearer {SECRET}"))]
#[case::responses(Protocol::OpenAiResponses, "Authorization", format!("Bearer {SECRET}"))]
#[case::anthropic(Protocol::AnthropicMessages, "x-api-key", SECRET.to_owned())]
fn the_protocol_decides_how_the_secret_is_presented(
    #[case] protocol: Protocol,
    #[case] name: &str,
    #[case] value: String,
) {
    let (header, secret) = auth::header_for(protocol, &Secret::new(SECRET));

    assert_eq!(header, name);
    assert_eq!(secret.expose(), value);
}

#[test]
fn only_stored_profiles_can_be_logged_in_or_out() {
    let sandbox = Sandbox::new();
    let providers = sandbox.providers(STORED);
    let store = sandbox.store();

    for name in ["env", "local"] {
        let error = auth::store_secret(profile(&providers, name), SECRET, &store).unwrap_err();
        assert!(rendered(error).contains("bee::auth::not_stored"), "{name}");
        let error = auth::forget(profile(&providers, name), &store).unwrap_err();
        assert!(rendered(error).contains("bee::auth::not_stored"), "{name}");
    }
    assert!(!sandbox.store_path().exists());
}

#[test]
fn a_blank_token_is_refused_and_stores_nothing() {
    let sandbox = Sandbox::new();
    let providers = sandbox.providers(STORED);

    let error =
        auth::store_secret(profile(&providers, "token"), " \n", &sandbox.store()).unwrap_err();

    assert!(rendered(error).contains("bee::auth::empty_token"));
    assert!(!sandbox.store_path().exists());
}

#[test]
fn status_reports_where_each_credential_stands_without_revealing_any() {
    let sandbox = Sandbox::new();
    let providers = sandbox.providers(STORED);
    let store = MemoryStore::default();
    auth::store_secret(profile(&providers, "token"), SECRET, &store).unwrap();
    let env = sandbox.env().with_variable("BEE_TEST_KEY", SECRET);

    let statuses = auth::status(&providers, &env, &store, 0).unwrap();

    let of =
        |name: &str| -> &ProfileStatus { statuses.iter().find(|s| s.profile == name).unwrap() };
    assert_eq!(of("local").status, Status::Unauthenticated);
    assert_eq!(
        of("env").status,
        Status::Env {
            variable: "BEE_TEST_KEY".to_owned(),
            set: true
        }
    );
    assert_eq!(
        of("token").status,
        Status::Store {
            state: StoreState::LoggedIn,
            login: None
        }
    );
    assert_eq!(
        of("or").status,
        Status::Store {
            state: StoreState::NotLoggedIn,
            login: Some("openrouter".to_owned())
        }
    );
    assert!(of("or").default);
    assert!(!format!("{statuses:?}").contains(SECRET));
}

// ---- the store ------------------------------------------------------------------

#[cfg(not(unix))]
#[test]
fn an_unsupported_file_store_refuses_every_operation_without_touching_the_file() {
    let sandbox = Sandbox::new();
    let path = sandbox.store_path();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, SECRET).unwrap();
    let store = sandbox.store();
    let audience = Audience::Provider("token");

    for error in [
        store.get(audience).unwrap_err(),
        store
            .put(audience, &Credential::new(Secret::new(SECRET)))
            .unwrap_err(),
        store.remove(audience).unwrap_err(),
    ] {
        assert!(matches!(error, Error::AuthStoreUnsupported));
        let text = rendered(error);
        assert!(!text.contains(SECRET), "{text}");
    }
    assert_eq!(fs::read_to_string(path).unwrap(), SECRET);
}

#[cfg(unix)]
mod file_store {
    use std::os::unix::fs::PermissionsExt;

    use pretty_assertions::assert_eq;

    use super::*;

    fn mode(path: &std::path::Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn the_credential_file_is_private_and_so_is_its_directory() {
        let sandbox = Sandbox::new();

        sandbox
            .store()
            .put(
                Audience::Provider("or"),
                &Credential::new(Secret::new(SECRET)),
            )
            .unwrap();

        assert_eq!(mode(&sandbox.store_path()), 0o600);
        assert_eq!(mode(sandbox.store_path().parent().unwrap()), 0o700);
    }

    #[test]
    fn replacing_a_credential_keeps_the_file_private_and_leaves_no_temporary_file() {
        let sandbox = Sandbox::new();
        let store = sandbox.store();
        store
            .put(
                Audience::Provider("or"),
                &Credential::new(Secret::new("old")),
            )
            .unwrap();

        store
            .put(
                Audience::Provider("or"),
                &Credential::new(Secret::new("new")),
            )
            .unwrap();

        assert_eq!(mode(&sandbox.store_path()), 0o600);
        let credential = store.get(Audience::Provider("or")).unwrap().unwrap();
        assert_eq!(credential.secret().expose(), "new");
        let entries: Vec<_> = fs::read_dir(sandbox.store_path().parent().unwrap())
            .unwrap()
            .collect();
        assert_eq!(entries.len(), 1, "only the credential file remains");
    }

    #[test]
    fn removing_the_last_credential_deletes_the_file() {
        let sandbox = Sandbox::new();
        let store = sandbox.store();
        store
            .put(Audience::Provider("a"), &Credential::new(Secret::new("1")))
            .unwrap();
        store
            .put(Audience::Provider("b"), &Credential::new(Secret::new("2")))
            .unwrap();

        assert!(store.remove(Audience::Provider("a")).unwrap());
        assert!(sandbox.store_path().exists());
        assert!(store.get(Audience::Provider("b")).unwrap().is_some());
        assert!(store.remove(Audience::Provider("b")).unwrap());
        assert!(!sandbox.store_path().exists());
        assert!(
            !store.remove(Audience::Provider("b")).unwrap(),
            "idempotent"
        );
    }

    #[rstest]
    #[case::group_readable(0o640)]
    #[case::world_readable(0o644)]
    #[case::group_writable(0o620)]
    fn a_credential_file_others_can_access_is_refused(#[case] permissions: u32) {
        let sandbox = Sandbox::new();
        let store = sandbox.store();
        store
            .put(
                Audience::Provider("or"),
                &Credential::new(Secret::new(SECRET)),
            )
            .unwrap();
        fs::set_permissions(
            sandbox.store_path(),
            fs::Permissions::from_mode(permissions),
        )
        .unwrap();

        let error = store.get(Audience::Provider("or")).unwrap_err();

        let text = rendered(error);
        assert!(text.contains("bee::auth::insecure_permissions"), "{text}");
        assert!(text.contains("chmod 600"), "{text}");
        assert!(!text.contains(SECRET), "{text}");
        // Refusing to write is as important: replacing would bless the file.
        assert!(
            store
                .put(Audience::Provider("x"), &Credential::new(Secret::new("y")))
                .is_err()
        );
    }

    #[rstest]
    #[case::not_json("this is not json")]
    #[case::wrong_top_level("[]")]
    #[case::wrong_version(r#"{"version": 2}"#)]
    #[case::no_version(r#"{"providers": {}}"#)]
    #[case::unknown_key(r#"{"version": 1, "extra": 1}"#)]
    #[case::entry_not_an_object(r#"{"version": 1, "providers": {"or": "sk-super-secret-token"}}"#)]
    #[case::secret_of_the_wrong_type(
        r#"{"version": 1, "providers": {"or": {"secret": 42, "note": "sk-super-secret-token"}}}"#
    )]
    #[case::empty_secret(r#"{"version": 1, "providers": {"or": {"secret": ""}}}"#)]
    #[case::bad_expiry(
        r#"{"version": 1, "providers": {"or": {"secret": "sk-super-secret-token", "expires_at": "soon"}}}"#
    )]
    #[case::truncated(r#"{"version": 1, "providers": {"or": {"secret": "sk-super-secret-to"#)]
    fn a_corrupt_credential_file_is_reported_without_quoting_it(#[case] content: &str) {
        let sandbox = Sandbox::new();
        fs::create_dir_all(sandbox.store_path().parent().unwrap()).unwrap();
        fs::write(sandbox.store_path(), content).unwrap();
        fs::set_permissions(sandbox.store_path(), fs::Permissions::from_mode(0o600)).unwrap();

        let error = sandbox.store().get(Audience::Provider("or")).unwrap_err();

        let text = rendered(error);
        assert!(text.contains("bee::auth::store_parse"), "{text}");
        assert!(!text.contains(SECRET), "{text}");
        assert!(!text.contains("sk-super"), "{text}");
    }

    #[test]
    fn a_missing_credential_file_is_an_empty_store() {
        let sandbox = Sandbox::new();

        assert_eq!(sandbox.store().get(Audience::Provider("or")).unwrap(), None);
        assert!(!sandbox.store_path().exists(), "reading creates nothing");
    }

    #[test]
    fn a_failed_write_leaves_the_previous_file_intact() {
        let sandbox = Sandbox::new();
        let store = sandbox.store();
        store
            .put(
                Audience::Provider("or"),
                &Credential::new(Secret::new("kept")),
            )
            .unwrap();
        // A read-only directory: the temporary file cannot be created in it.
        let dir = sandbox.store_path().parent().unwrap().to_owned();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();

        let result = store.put(
            Audience::Provider("or"),
            &Credential::new(Secret::new("lost")),
        );

        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        // Root ignores directory modes, so only assert when the write failed.
        if let Err(error) = result {
            assert!(rendered(error).contains("bee::auth::store_write"));
            let credential = store.get(Audience::Provider("or")).unwrap().unwrap();
            assert_eq!(credential.secret().expose(), "kept");
        }
    }

    #[test]
    fn a_provider_credential_is_never_returned_for_an_mcp_server_of_the_same_name() {
        let sandbox = Sandbox::new();
        let store = sandbox.store();
        store
            .put(
                Audience::Provider("shared"),
                &Credential::new(Secret::new("llm-key")),
            )
            .unwrap();
        store
            .put(
                Audience::McpServer("shared"),
                &Credential::new(Secret::new("mcp-token")),
            )
            .unwrap();

        let provider = store.get(Audience::Provider("shared")).unwrap().unwrap();
        let mcp = store.get(Audience::McpServer("shared")).unwrap().unwrap();

        assert_eq!(provider.secret().expose(), "llm-key");
        assert_eq!(mcp.secret().expose(), "mcp-token");
        store.remove(Audience::McpServer("shared")).unwrap();
        assert!(store.get(Audience::Provider("shared")).unwrap().is_some());
        assert_eq!(store.get(Audience::McpServer("shared")).unwrap(), None);
    }

    #[test]
    fn an_mcp_credential_alone_does_not_log_a_provider_in() {
        let sandbox = Sandbox::new();
        let providers = sandbox.providers(STORED);
        let store = sandbox.store();
        store
            .put(
                Audience::McpServer("token"),
                &Credential::new(Secret::new(SECRET)),
            )
            .unwrap();

        let error =
            auth::resolve(profile(&providers, "token"), &sandbox.env(), &store, 0).unwrap_err();

        assert!(rendered(error).contains("bee::auth::not_logged_in"));
    }

    #[test]
    fn an_expiry_survives_a_round_trip_through_the_file() {
        let sandbox = Sandbox::new();
        let store = sandbox.store();

        store
            .put(
                Audience::Provider("or"),
                &Credential::new(Secret::new(SECRET)).expiring_at(1_234),
            )
            .unwrap();

        let credential = store.get(Audience::Provider("or")).unwrap().unwrap();
        assert_eq!(credential.expires_at(), Some(1_234));
    }
}

// ---- redaction --------------------------------------------------------------------

#[test]
fn no_debug_output_carries_a_secret() {
    let sandbox = Sandbox::new();
    let providers = sandbox.providers(STORED);
    let env = sandbox.env().with_variable("BEE_TEST_KEY", SECRET);
    let store = sandbox.store();
    let credential = Credential::new(Secret::new(SECRET));
    let pkce = Pkce::from_verifier(SECRET);
    let code = Code::new(SECRET).unwrap();
    let response = HttpResponse {
        status: 200,
        body: SECRET.to_owned(),
    };
    let resolved = auth::resolve(profile(&providers, "env"), &env, &store, 0).unwrap();

    let outputs = [
        format!("{:?}", Secret::new(SECRET)),
        format!("{credential:?}"),
        format!("{resolved:?}"),
        format!("{pkce:?}"),
        format!("{code:?}"),
        format!("{response:?}"),
        format!("{env:?}"),
        format!("{providers:?}"),
        format!("{store:?}"),
    ];
    for output in outputs {
        assert!(!output.contains(SECRET), "{output}");
    }
    // The names stay visible: that is what makes the output useful.
    assert!(format!("{env:?}").contains("BEE_TEST_KEY"));
}

#[test]
fn a_secret_has_no_display_form() {
    // A compile-time property pinned as a runtime check on the debug text: the
    // only way to print a secret is `expose`, which names the intent.
    assert_eq!(format!("{:?}", Secret::new(SECRET)), "Secret(<redacted>)");
}

#[rstest]
#[case::missing_env(Error::AuthMissingEnv { profile: "p".into(), variable: "V".into() })]
#[case::not_logged_in(Error::AuthNotLoggedIn { profile: "p".into() })]
#[case::expired(Error::AuthExpired { profile: "p".into() })]
#[case::state(Error::AuthStateMismatch)]
#[case::denied(Error::AuthDenied)]
#[case::invalid_callback(Error::AuthCallbackInvalid)]
#[case::empty(Error::AuthEmptyToken)]
#[case::no_flow(Error::AuthNoLoginFlow { profile: "p".into() })]
#[case::not_stored(Error::AuthNotStored { profile: "p".into() })]
#[case::no_path(Error::AuthNoStorePath)]
fn every_authentication_diagnostic_has_a_code_and_something_to_do(#[case] error: Error) {
    let text = rendered(error);

    assert!(text.contains("bee::auth::"), "{text}");
    assert!(text.contains("help:"), "{text}");
}

#[test]
fn a_store_path_needs_a_data_or_home_directory() {
    let error = FileStore::from_environment(&Environment::new(None, None, None)).unwrap_err();

    assert!(rendered(error).contains("bee::auth::no_store_path"));
}
