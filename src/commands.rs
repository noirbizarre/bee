//! What `bee auth` does, apart from parsing its arguments.
//!
//! Everything a person sees is written here, to writers handed in, so it can be
//! tested. The library never prints: sign-in URLs and prompts go to stderr and
//! only `status` writes to stdout, which carries protocol traffic when Bee is
//! launched by a client and is never touched on that path.

use std::io::{self, BufRead, IsTerminal, Write};
use std::time::Duration;

use bee::auth::{self, CredentialStore, FileStore, Status, StoreState};
use bee::config::{Config, Environment};
use bee::error::{Error, Result};
use bee::http::{Http, UreqHttp};
use bee::login;
use bee::provider::{Profile, Providers};

use crate::cli::{AuthCommand, LoginArgs};

pub mod models;

/// How long a request to the provider may take.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Everything `bee auth` touches, handed in so tests can stand in for it.
pub struct Context<'a> {
    pub env: &'a Environment,
    pub providers: &'a Providers,
    pub store: &'a dyn CredentialStore,
    pub http: &'a dyn Http,
    pub login_http: &'a dyn bee::http::LoginHttp,
    /// Optional injected discovery; authentication success does not depend on it.
    pub discovery: Option<&'a bee::models::Service>,
    /// How long to wait for the browser.
    pub timeout: Duration,
    /// Try to open a URL in a browser. Best effort.
    pub open: &'a dyn Fn(&str),
    /// Whether a person is typing, so a prompt is worth showing.
    pub interactive: bool,
    pub out: &'a mut dyn Write,
    pub err: &'a mut dyn Write,
    pub input: &'a mut dyn BufRead,
}

/// Run `bee auth` against the real process: its environment, configuration,
/// credential file, network and terminal.
pub fn run_auth(command: AuthCommand) -> Result<()> {
    let env = Environment::from_process();
    let project_dir = std::env::current_dir().map_err(|source| Error::Io {
        path: ".".to_owned(),
        source,
    })?;
    let config = Config::load(&env, &project_dir)?;
    let configured = Providers::from_config(&config)?;
    let onboarding = match &command {
        AuthCommand::Login(LoginArgs {
            profile: Some(name),
            ..
        }) if configured.get(name).is_none() => {
            Some(bee::config::persist::prepare(&env, &project_dir, name)?)
        }
        _ => None,
    };
    let providers = onboarding
        .as_ref()
        .map_or(&configured, |plan| plan.providers());
    let discovery =
        bee::models::Service::production(bee::models::Settings::from_config(&config)?, &env);
    let store = FileStore::from_environment(&env)?;
    let http = UreqHttp::new(HTTP_TIMEOUT);
    let stdin = io::stdin();
    let interactive = stdin.is_terminal();
    let mut input = stdin.lock();
    let mut out = io::stdout().lock();
    let mut err = io::stderr().lock();
    execute(
        command,
        &mut Context {
            env: &env,
            providers,
            store: &store,
            http: &http,
            login_http: &bee::http::AsyncLoginHttp,
            discovery: Some(&discovery),
            timeout: login::DEFAULT_TIMEOUT,
            open: &open_browser,
            interactive,
            out: &mut out,
            err: &mut err,
            input: &mut input,
        },
    )?;
    if let Some(plan) = onboarding {
        plan.commit(&env, &project_dir)?;
    }
    Ok(())
}

/// Run one `bee auth` command.
pub fn execute(command: AuthCommand, context: &mut Context<'_>) -> Result<()> {
    match command {
        AuthCommand::Login(args) => {
            login(&args, context)?;
            let profile = chosen(context.providers, args.profile.as_deref())?;
            if profile.discover_models()
                && let Some(discovery) = context.discovery
            {
                discovery.invalidate_endpoint(profile.name());
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|source| Error::Io {
                        path: "<async runtime>".into(),
                        source,
                    })?;
                match runtime.block_on(discovery.list_endpoint(profile, context.env)) {
                    Ok(listing) => writeln!(
                        context.err,
                        "Discovered {} models ({}); listing does not verify inference access.",
                        listing.models.len(),
                        listing.scope
                    )
                    .map_err(write_error)?,
                    Err(error) => writeln!(context.err, "Signed in; model discovery: {error}")
                        .map_err(write_error)?,
                }
            }
            Ok(())
        }
        AuthCommand::Logout { profile } => logout(profile.as_deref(), context),
        AuthCommand::Status { profile } => status(profile.as_deref(), context),
    }
}

/// The profile named, or the default one.
fn chosen<'a>(providers: &'a Providers, name: Option<&str>) -> Result<&'a Profile> {
    match name.or_else(|| providers.default_name()) {
        Some(name) => auth::profile(providers, name),
        // Nothing named and nothing configured: report it as an unknown
        // profile, which lists what is defined (nothing) and so says what to do.
        None => auth::profile(providers, "<default>"),
    }
}

fn write_error(source: io::Error) -> Error {
    Error::Io {
        path: "<terminal>".to_owned(),
        source,
    }
}

fn login(args: &LoginArgs, context: &mut Context<'_>) -> Result<()> {
    let profile = chosen(context.providers, args.profile.as_deref())?;
    // Before opening a browser: signing in to something that cannot be stored
    // would waste the user's time and then throw the key away.
    auth::require_store(profile)?;
    let name = profile.name();

    if profile
        .login_spec()
        .is_some_and(bee::preset::LoginSpec::is_copilot)
        && !args.with_token
    {
        if args.paste_code {
            return Err(Error::Copilot {
                reason: "device login displays a code to enter in GitHub; omit --paste-code",
            });
        }
        let store = context
            .store
            .file_store()
            .ok_or(Error::AuthStoreUnsupported)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| Error::Copilot {
                reason: "cannot start sign-in runtime; retry login",
            })?;
        return runtime.block_on(async {
            tokio::time::timeout(context.timeout, async {
                let login = auth::copilot::SignIn::prepare(store, name, context.login_http).await?;
                writeln!(context.err, "Continue with GitHub Copilot for `{name}`:\n\n  {}\n\nEnter code: {}\nWaiting for device authorization...", login.url, login.user_code).map_err(write_error)?;
                if !args.no_browser { (context.open)(&login.url); }
                let credential = login.complete(context.login_http).await?;
                login.commit(store, name, &credential).await?;
                writeln!(context.err, "Signed in to GitHub Copilot for `{name}`.").map_err(write_error)
            }).await.map_err(|_| Error::Copilot { reason: "device authorization timed out; retry login" })?
        });
    }

    if profile
        .login_spec()
        .is_some_and(bee::preset::LoginSpec::is_chatgpt)
    {
        if args.with_token || args.paste_code {
            return Err(Error::ChatGpt {
                reason: "ChatGPT requires browser authorization with a loopback callback",
            });
        }
        let store = context
            .store
            .file_store()
            .ok_or(Error::AuthStoreUnsupported)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| Error::ChatGpt {
                reason: "cannot start sign-in runtime",
            })?;
        return runtime.block_on(async {
            tokio::time::timeout(context.timeout, async {
                let login = auth::chatgpt::SignIn::prepare(store, name).await?;
                writeln!(context.err, "Continue with ChatGPT for `{name}`:\n\n  {}\n\nWaiting for browser authorization...", login.url).map_err(write_error)?;
                if !args.no_browser { (context.open)(&login.url); }
                let credential = login.complete(context.login_http).await?;
                login.commit(store, name, &credential).await?;
                if credential.chatgpt_plan_enabled() == Some(false) {
                    return writeln!(context.err, "Signed in to ChatGPT for `{name}`, but ChatGPT plan use is disabled. Repeat `bee auth login {name}` to grant permission before inference.").map_err(write_error);
                }
                writeln!(context.err, "Signed in to ChatGPT for `{name}`. Eligible inference uses your ChatGPT plan. Manage usage: https://chatgpt.com/settings/usage").map_err(write_error)
            }).await.map_err(|_| Error::ChatGpt { reason: "browser sign-in timed out; retry login" })?
        });
    }

    if args.with_token {
        if context.interactive {
            writeln!(
                context.err,
                "Paste the API token for `{name}`, then press Enter:"
            )
            .map_err(write_error)?;
        }
        let mut line = String::new();
        context
            .input
            .read_line(&mut line)
            .map_err(|source| Error::Io {
                path: "<stdin>".to_owned(),
                source,
            })?;
        auth::store_secret(profile, &line, context.store)?;
        writeln!(context.err, "Stored the API token for `{name}`.").map_err(write_error)?;
        return Ok(());
    }

    // The flow is data (ADR-009): whatever `credential.login` names, not a
    // provider this code knows.
    let Some(flow) = profile.login_spec() else {
        return Err(Error::AuthNoLoginFlow {
            profile: name.to_owned(),
        });
    };

    let secret = if args.paste_code {
        let err = &mut *context.err;
        let input = &mut *context.input;
        login::sign_in_headless(
            flow,
            context.http,
            &mut |url| {
                let _ = writeln!(
                    err,
                    "Open this URL in any browser and sign in:\n\n  {url}\n\n\
                     Then paste the code it shows here and press Enter:"
                );
            },
            &mut || {
                let mut line = String::new();
                input.read_line(&mut line)?;
                Ok(line)
            },
        )?
    } else {
        let err = &mut *context.err;
        let open = context.open;
        let browser = !args.no_browser;
        login::sign_in(flow, context.http, context.timeout, &mut |url| {
            let _ = writeln!(
                err,
                "Sign in to `{name}` in your browser. If it does not open, visit:\n\n  {url}\n\n\
                     Waiting for the sign-in to finish (Ctrl-C to cancel)..."
            );
            if browser {
                open(url);
            }
        })?
    };
    // Stored only now, after every check passed: a failed sign-in must leave
    // the previous credential exactly as it was.
    auth::store_secret(profile, secret.expose(), context.store)?;
    writeln!(
        context.err,
        "Signed in. Stored the credential for `{name}`."
    )
    .map_err(write_error)
}

fn logout(name: Option<&str>, context: &mut Context<'_>) -> Result<()> {
    let profile = chosen(context.providers, name)?;
    if profile
        .login_spec()
        .is_some_and(bee::preset::LoginSpec::is_chatgpt)
    {
        let store = context
            .store
            .file_store()
            .ok_or(Error::AuthStoreUnsupported)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| Error::ChatGpt {
                reason: "cannot start logout runtime",
            })?;
        let confirmed = runtime.block_on(auth::chatgpt::logout(
            store,
            profile.name(),
            context.login_http,
        ))?;
        return writeln!(context.err, "Signed out of ChatGPT for `{}`. {}", profile.name(), if confirmed { "Renewable session revoked; registration retained." } else { "Remote revocation was not confirmed; disconnect Bee in ChatGPT settings: https://chatgpt.com/settings/usage" }).map_err(write_error);
    }
    let message = if auth::forget(profile, context.store)? {
        format!(
            "Removed the stored credential for `{}`. It stays valid with the provider until you revoke it there.",
            profile.name()
        )
    } else {
        format!("No credential was stored for `{}`.", profile.name())
    };
    writeln!(context.err, "{message}").map_err(write_error)?;
    if profile.is_copilot() {
        writeln!(context.err, "Review or revoke GitHub authorization at https://github.com/settings/connections/applications/{}", auth::copilot::CLIENT_ID).map_err(write_error)?;
    }
    Ok(())
}

fn status(name: Option<&str>, context: &mut Context<'_>) -> Result<()> {
    if let Some(name) = name {
        auth::profile(context.providers, name)?;
    }
    let statuses = auth::status(
        context.providers,
        context.env,
        context.store,
        auth::unix_now(),
    )?;
    let statuses: Vec<_> = statuses
        .into_iter()
        .filter(|status| name.is_none_or(|name| status.profile == name))
        .collect();
    if statuses.is_empty() {
        return writeln!(context.out, "No provider profiles are configured.").map_err(write_error);
    }
    let width = statuses
        .iter()
        .map(|status| status.profile.len())
        .max()
        .unwrap_or_default();
    for entry in statuses {
        let description = match &entry.status {
            Status::Unauthenticated => "no credential (unauthenticated endpoint)".to_owned(),
            Status::Env {
                variable,
                set: true,
            } => format!("environment variable `{variable}` is set"),
            Status::Env {
                variable,
                set: false,
            } => {
                format!("environment variable `{variable}` is NOT set")
            }
            Status::Store { state, login } => {
                let state = match state {
                    StoreState::LoggedIn => "stored credential present",
                    StoreState::NotLoggedIn => "no stored credential; run `bee auth login`",
                    StoreState::Expired => "stored credential expired; run `bee auth login`",
                    StoreState::PlanDisabled => {
                        "signed in; ChatGPT plan use disabled; repeat `bee auth login` to enable"
                    }
                    StoreState::Refreshable => {
                        "ChatGPT access token expired; will renew on next request"
                    }
                    _ => "stored credential in an unknown state",
                };
                let how = match login {
                    Some(flow) => format!("browser sign-in: {flow}"),
                    None => "token only".to_owned(),
                };
                format!("{state} ({how})")
            }
            _ => "unknown".to_owned(),
        };
        let default = if entry.default { " (default)" } else { "" };
        writeln!(
            context.out,
            "{:<width$}  {description}{default}",
            entry.profile
        )
        .map_err(write_error)?;
    }
    Ok(())
}

/// Ask the desktop to open `url`. Failure is fine: the URL is always printed.
fn open_browser(url: &str) {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    // Detached and silent: a browser's own chatter would land in the terminal
    // the user is reading the prompt in.
    let _ = std::process::Command::new(program)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

// Share the fake with integration tests so command tests need no Unix file permissions.
#[cfg(test)]
#[path = "../tests/support/credential_store.rs"]
mod test_store;

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::io::{Read, Write};
    use std::net::TcpStream;

    use bee::auth::{Audience, CredentialStore};
    use bee::config::project_config_path;
    use bee::http::{HttpResponse, Unreachable};

    use super::test_store::MemoryStore;
    use super::*;

    const KEY: &str = "sk-or-test-key";

    #[test]
    fn copilot_cli_onboards_with_device_instructions_and_never_prints_the_token() {
        struct Device;
        impl bee::http::LoginHttp for Device {
            fn post_json<'a>(
                &'a self,
                url: &'a str,
                _: String,
            ) -> futures::future::BoxFuture<'a, std::result::Result<HttpResponse, Unreachable>>
            {
                let value = if url.ends_with("/device/code") {
                    serde_json::json!({"verification_uri":"https://github.com/login/device","user_code":"ABCD-EFGH","device_code":"fixture-device-secret","interval":1,"expires_in":900})
                } else {
                    assert_eq!(url, "https://github.com/login/oauth/access_token");
                    serde_json::json!({"access_token":"fixture-github-secret","token_type":"bearer"})
                };
                Box::pin(async move {
                    Ok(HttpResponse {
                        status: 200,
                        body: value.to_string(),
                    })
                })
            }
        }
        let root = tempfile::tempdir().unwrap();
        let env = Environment::new(None, Some(root.path().into()), None);
        let plan = bee::config::persist::prepare(&env, root.path(), "github").unwrap();
        let store = FileStore::from_environment(&env).unwrap();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut input: &[u8] = b"";
        {
            let mut context = Context {
                env: &env,
                providers: plan.providers(),
                store: &store,
                http: &KeyGiver,
                login_http: &Device,
                timeout: Duration::from_secs(5),
                open: &|_| panic!("--no-browser must not open the desktop"),
                interactive: false,
                discovery: None,
                out: &mut out,
                err: &mut err,
                input: &mut input,
            };
            execute(AuthCommand::Login(args(false, false, true)), &mut context).unwrap();
            assert!(out.is_empty());
        }
        plan.commit(&env, root.path()).unwrap();
        let configured = Providers::from_config(&Config::load(&env, root.path()).unwrap()).unwrap();
        assert!(configured.get("github").unwrap().is_copilot());
        assert_eq!(
            store
                .get(Audience::Provider("github"))
                .unwrap()
                .unwrap()
                .secret()
                .expose(),
            "fixture-github-secret"
        );
        {
            let mut context = Context {
                env: &env,
                providers: &configured,
                store: &store,
                http: &KeyGiver,
                login_http: &Device,
                timeout: Duration::from_secs(5),
                open: &|_| {},
                interactive: false,
                discovery: None,
                out: &mut out,
                err: &mut err,
                input: &mut input,
            };
            execute(AuthCommand::Status { profile: None }, &mut context).unwrap();
            execute(AuthCommand::Logout { profile: None }, &mut context).unwrap();
        }
        assert!(store.get(Audience::Provider("github")).unwrap().is_none());
        let diagnostics = String::from_utf8(err).unwrap();
        assert!(diagnostics.contains("ABCD-EFGH"));
        assert!(diagnostics.contains("https://github.com/login/device"));
        assert!(diagnostics.contains(bee::auth::copilot::CLIENT_ID));
        let output = String::from_utf8(out).unwrap();
        assert!(output.contains("stored credential present"));
        for secret in ["fixture-github-secret", "fixture-device-secret"] {
            assert!(!format!("{diagnostics}{output}").contains(secret));
        }
    }

    #[cfg(unix)]
    #[test]
    fn token_login_discovers_its_connection_without_making_authentication_depend_on_discovery() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        struct Listing {
            calls: AtomicUsize,
            fail: bool,
        }
        impl bee::models::Http for Listing {
            fn get<'a>(
                &'a self,
                url: &'a str,
                headers: reqwest::header::HeaderMap,
                _: usize,
            ) -> futures::future::BoxFuture<'a, std::result::Result<Vec<u8>, bee::models::Error>>
            {
                assert_eq!(url, "https://openrouter.ai/api/v1/models/user");
                assert_eq!(headers["authorization"], format!("Bearer {KEY}"));
                self.calls.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move {
                    if self.fail {
                        Err(bee::models::Error::Transport)
                    } else {
                        Ok(br#"{"data":[{"id":"native-model"}]}"#.to_vec())
                    }
                })
            }
        }
        for fail in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let env = Environment::new(None, Some(root.path().into()), None);
            let plan = bee::config::persist::prepare(&env, root.path(), "openrouter").unwrap();
            let store = FileStore::from_environment(&env).unwrap();
            let http = Arc::new(Listing {
                calls: AtomicUsize::new(0),
                fail,
            });
            let service = bee::models::Service::new(
                bee::models::Settings::default(),
                http.clone(),
                Arc::new(bee::models::FileCache(None)),
                Arc::new(bee::models::SystemClock),
            );
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let input = format!("{KEY}\n");
            execute(
                AuthCommand::Login(LoginArgs {
                    profile: Some("openrouter".into()),
                    with_token: true,
                    paste_code: false,
                    no_browser: false,
                }),
                &mut Context {
                    env: &env,
                    providers: plan.providers(),
                    store: &store,
                    http: &KeyGiver,
                    login_http: &bee::http::AsyncLoginHttp,
                    discovery: Some(&service),
                    timeout: Duration::from_secs(1),
                    open: &|_| {},
                    interactive: false,
                    out: &mut out,
                    err: &mut err,
                    input: &mut input.as_bytes(),
                },
            )
            .unwrap();
            assert_eq!(http.calls.load(Ordering::SeqCst), 1);
            assert!(
                store
                    .get(Audience::Provider("openrouter"))
                    .unwrap()
                    .is_some()
            );
            assert!(out.is_empty());
            let message = String::from_utf8(err).unwrap();
            assert!(!message.contains(KEY));
            assert!(message.contains(if fail {
                "Signed in; model discovery:"
            } else {
                "Discovered 1 models"
            }));
        }
    }

    /// Answers the exchange with a key, whatever it is asked.
    struct KeyGiver;

    impl Http for KeyGiver {
        fn post_json(&self, _: &str, _: &str) -> std::result::Result<HttpResponse, Unreachable> {
            Ok(HttpResponse {
                status: 200,
                body: format!(r#"{{"key":"{KEY}"}}"#),
            })
        }
    }

    struct Fixture {
        providers: Providers,
        store: MemoryStore,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let project = dir.path().join("project");
            std::fs::create_dir_all(project_config_path(&project).parent().unwrap()).unwrap();
            std::fs::write(
                project_config_path(&project),
                "[providers.or]\npreset = \"openrouter\"\n",
            )
            .unwrap();
            let env = Environment::new(None, Some(dir.path().join("home")), None);
            let providers = Providers::from_config(&Config::load(&env, &project).unwrap()).unwrap();
            Self {
                providers,
                store: MemoryStore::default(),
            }
        }

        fn store(&self) -> &MemoryStore {
            &self.store
        }

        /// Run `bee auth login` and return what it wrote to stderr.
        fn login(&self, args: LoginArgs, open: &dyn Fn(&str), input: &str) -> (Result<()>, String) {
            self.login_with_http(args, open, input, &KeyGiver)
        }

        fn login_with_http(
            &self,
            args: LoginArgs,
            open: &dyn Fn(&str),
            input: &str,
            http: &dyn Http,
        ) -> (Result<()>, String) {
            let env = Environment::new(None, None, None);
            let store = self.store();
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let mut input = input.as_bytes();
            let result = execute(
                AuthCommand::Login(args),
                &mut Context {
                    env: &env,
                    providers: &self.providers,
                    store,
                    http,
                    login_http: &bee::http::AsyncLoginHttp,
                    timeout: Duration::from_secs(5),
                    open,
                    interactive: false,
                    discovery: None,
                    out: &mut out,
                    err: &mut err,
                    input: &mut input,
                },
            );
            assert!(out.is_empty(), "login must not write to stdout");
            (result, String::from_utf8(err).unwrap())
        }
    }

    fn args(with_token: bool, paste_code: bool, no_browser: bool) -> LoginArgs {
        LoginArgs {
            profile: None,
            with_token,
            paste_code,
            no_browser,
        }
    }

    #[test]
    fn the_browser_sign_in_prints_the_url_opens_it_and_stores_the_key() {
        let fixture = Fixture::new();
        let opened = RefCell::new(Vec::new());
        let open = |url: &str| {
            opened.borrow_mut().push(url.to_owned());
            // Play the browser: follow the callback with the matching state.
            let query = url.split_once('?').unwrap().1;
            let param = |name: &str| {
                query
                    .split('&')
                    .find_map(|pair| pair.strip_prefix(&format!("{name}=")))
                    .unwrap()
                    .to_owned()
            };
            let port = param("callback_url")
                .strip_prefix("http%3A%2F%2Flocalhost%3A")
                .unwrap()
                .split('%')
                .next()
                .unwrap()
                .parse::<u16>()
                .unwrap();
            let state = param("state");
            std::thread::spawn(move || {
                let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
                write!(
                    stream,
                    "GET /callback?code=c&state={state} HTTP/1.1\r\n\r\n"
                )
                .unwrap();
                let _ = stream.read_to_string(&mut String::new());
            });
        };

        let (result, err) = fixture.login(args(false, false, false), &open, "");

        result.unwrap();
        let opened = opened.into_inner();
        assert_eq!(opened.len(), 1);
        assert!(err.contains(&opened[0]), "the URL is always printed: {err}");
        assert!(!err.contains(KEY), "{err}");
        let stored = fixture
            .store()
            .get(Audience::Provider("or"))
            .unwrap()
            .unwrap();
        assert_eq!(stored.secret().expose(), KEY);
    }

    #[test]
    fn no_browser_prints_the_url_without_opening_anything() {
        let fixture = Fixture::new();
        let open = |_: &str| panic!("the browser must not be opened");

        // Nobody calls back, so the wait ends; the point is that `open` never ran.
        let env = Environment::new(None, None, None);
        let store = fixture.store();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut input: &[u8] = b"";
        let result = execute(
            AuthCommand::Login(args(false, false, true)),
            &mut Context {
                env: &env,
                providers: &fixture.providers,
                store,
                http: &KeyGiver,
                login_http: &bee::http::AsyncLoginHttp,
                timeout: Duration::from_millis(100),
                open: &open,
                interactive: false,
                discovery: None,
                out: &mut out,
                err: &mut err,
                input: &mut input,
            },
        );

        assert!(result.is_err());
        let err = String::from_utf8(err).unwrap();
        assert!(err.contains("https://openrouter.ai/auth?"), "{err}");
        assert_eq!(store.get(Audience::Provider("or")).unwrap(), None);
    }

    #[test]
    fn the_paste_code_sign_in_asks_for_the_code_and_stores_the_key() {
        let fixture = Fixture::new();

        let (result, err) = fixture.login(args(false, true, false), &|_| {}, "the-code\n");

        result.unwrap();
        assert!(err.contains("paste the code"), "{err}");
        assert!(!err.contains("callback_url"), "{err}");
        let stored = fixture
            .store()
            .get(Audience::Provider("or"))
            .unwrap()
            .unwrap();
        assert_eq!(stored.secret().expose(), KEY);
    }

    #[test]
    fn a_token_from_standard_input_is_stored_and_never_echoed() {
        let fixture = Fixture::new();

        let (result, err) = fixture.login(args(true, false, false), &|_| {}, "my-token\n");

        result.unwrap();
        assert!(!err.contains("my-token"), "{err}");
        let stored = fixture
            .store()
            .get(Audience::Provider("or"))
            .unwrap()
            .unwrap();
        assert_eq!(stored.secret().expose(), "my-token");
    }

    #[rstest::rstest]
    #[case::expired_code(403)]
    #[case::provider_failure(500)]
    fn a_failed_openrouter_reauthorization_preserves_the_previous_key(#[case] status: u16) {
        struct Refused(u16);
        impl Http for Refused {
            fn post_json(
                &self,
                _: &str,
                _: &str,
            ) -> std::result::Result<HttpResponse, Unreachable> {
                Ok(HttpResponse {
                    status: self.0,
                    body: "secret-provider-response".into(),
                })
            }
        }
        let fixture = Fixture::new();
        fixture
            .login(args(true, false, false), &|_| {}, KEY)
            .0
            .unwrap();

        let (result, output) = fixture.login_with_http(
            args(false, true, false),
            &|_| {},
            "secret-expired-code",
            &Refused(status),
        );

        let error = result.unwrap_err();
        assert!(matches!(error, bee::Error::AuthExchangeFailed { .. }));
        let diagnostic = format!("{output}{error:?}{error}");
        for secret in [KEY, "secret-expired-code", "secret-provider-response"] {
            assert!(!diagnostic.contains(secret));
        }
        assert_eq!(
            fixture
                .store()
                .get(Audience::Provider("or"))
                .unwrap()
                .unwrap()
                .secret()
                .expose(),
            KEY
        );
    }

    #[test]
    fn an_openrouter_denial_preserves_the_previous_key() {
        let fixture = Fixture::new();
        fixture
            .login(args(true, false, false), &|_| {}, KEY)
            .0
            .unwrap();
        let open = |url: &str| {
            let url = reqwest::Url::parse(url).unwrap();
            let callback = url
                .query_pairs()
                .find(|(name, _)| name == "callback_url")
                .unwrap()
                .1
                .into_owned();
            let port = reqwest::Url::parse(&callback).unwrap().port().unwrap();
            std::thread::spawn(move || {
                let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
                write!(stream, "GET /callback?error=access_denied HTTP/1.1\r\n\r\n").unwrap();
                let _ = stream.read_to_string(&mut String::new());
            });
        };

        let (result, output) = fixture.login(args(false, false, false), &open, "");

        assert!(matches!(result, Err(bee::Error::AuthDenied)));
        assert!(!output.contains(KEY));
        assert_eq!(
            fixture
                .store()
                .get(Audience::Provider("or"))
                .unwrap()
                .unwrap()
                .secret()
                .expose(),
            KEY
        );
    }

    #[cfg(unix)]
    #[rstest::rstest]
    #[case::enabled(true)]
    #[case::identity_only(false)]
    fn chatgpt_cli_login_status_and_logout_preserve_identity_without_printing_tokens(
        #[case] plan: bool,
    ) {
        use std::sync::atomic::Ordering;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.toml");
        std::fs::write(&path, "[providers.account]\npreset='chatgpt'").unwrap();
        let env = Environment::new(None, Some(root.path().into()), Some(path));
        let providers = Providers::from_config(&Config::load(&env, root.path()).unwrap()).unwrap();
        let store = FileStore::from_environment(&env).unwrap();
        let oauth = crate::test_chatgpt::OAuth::new();
        if !plan {
            *oauth.scopes.lock().unwrap() = "openid profile".into();
        }
        let open = |url: &str| oauth.browser(url);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut input: &[u8] = b"";
        {
            let mut context = Context {
                env: &env,
                providers: &providers,
                store: &store,
                http: &KeyGiver,
                login_http: &oauth,
                timeout: Duration::from_secs(10),
                open: &open,
                interactive: false,
                discovery: None,
                out: &mut out,
                err: &mut err,
                input: &mut input,
            };
            execute(AuthCommand::Login(args(false, false, false)), &mut context).unwrap();
            execute(AuthCommand::Status { profile: None }, &mut context).unwrap();
            assert_eq!(
                store
                    .get(Audience::Provider("account"))
                    .unwrap()
                    .unwrap()
                    .chatgpt_plan_enabled(),
                Some(plan)
            );
            // Both confirmed and unconfirmed revocation must remove local tokens.
            if !plan {
                oauth.revoke_status.store(503, Ordering::SeqCst);
            }
            execute(AuthCommand::Logout { profile: None }, &mut context).unwrap();
            execute(AuthCommand::Status { profile: None }, &mut context).unwrap();
        }
        let output = String::from_utf8(out).unwrap();
        let diagnostics = String::from_utf8(err).unwrap();
        assert!(output.contains(if plan {
            "stored credential present"
        } else {
            "ChatGPT plan use disabled"
        }));
        assert!(output.contains("no stored credential"));
        assert!(diagnostics.contains(if plan {
            "Eligible inference uses your ChatGPT plan"
        } else {
            "ChatGPT plan use is disabled"
        }));
        assert!(diagnostics.contains(if plan {
            "Renewable session revoked"
        } else {
            "Remote revocation was not confirmed"
        }));
        for secret in [
            crate::test_chatgpt::ACCESS,
            "fixture-refresh-secret",
            "fixture-authorization-secret",
        ] {
            assert!(!format!("{output}{diagnostics}").contains(secret));
        }
        let record = store.get(Audience::Provider("account")).unwrap().unwrap();
        assert!(record.secret().expose().is_empty());
    }
}
