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

/// How long a request to the provider may take.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Everything `bee auth` touches, handed in so tests can stand in for it.
pub struct Context<'a> {
    pub env: &'a Environment,
    pub providers: &'a Providers,
    pub store: &'a dyn CredentialStore,
    pub http: &'a dyn Http,
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
    let providers = Providers::from_config(&Config::load(&env, &project_dir)?)?;
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
            providers: &providers,
            store: &store,
            http: &http,
            timeout: login::DEFAULT_TIMEOUT,
            open: &open_browser,
            interactive,
            out: &mut out,
            err: &mut err,
            input: &mut input,
        },
    )
}

/// Run one `bee auth` command.
pub fn execute(command: AuthCommand, context: &mut Context<'_>) -> Result<()> {
    match command {
        AuthCommand::Login(args) => login(&args, context),
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
    let message = if auth::forget(profile, context.store)? {
        format!(
            "Removed the stored credential for `{}`. It stays valid with the provider until you revoke it there.",
            profile.name()
        )
    } else {
        format!("No credential was stored for `{}`.", profile.name())
    };
    writeln!(context.err, "{message}").map_err(write_error)
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

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::io::{Read, Write};
    use std::net::TcpStream;

    use bee::auth::{Audience, CredentialStore};
    use bee::config::project_config_path;
    use bee::http::{HttpResponse, Unreachable};

    use super::*;

    const KEY: &str = "sk-or-test-key";

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
        dir: tempfile::TempDir,
        providers: Providers,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let project = dir.path().join("project");
            std::fs::create_dir_all(project_config_path(&project).parent().unwrap()).unwrap();
            std::fs::write(
                project_config_path(&project),
                "[providers.or]\nprotocols = { openai-chat = {} }\n\
                 credential = { store = true, login = \"openrouter\" }\n",
            )
            .unwrap();
            let env = Environment::new(None, Some(dir.path().join("home")), None);
            let providers = Providers::from_config(&Config::load(&env, &project).unwrap()).unwrap();
            Self { dir, providers }
        }

        fn store(&self) -> FileStore {
            FileStore::new(self.dir.path().join("credentials.json"))
        }

        /// Run `bee auth login` and return what it wrote to stderr.
        fn login(&self, args: LoginArgs, open: &dyn Fn(&str), input: &str) -> (Result<()>, String) {
            let env = Environment::new(None, None, None);
            let store = self.store();
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let mut input = input.as_bytes();
            let result = execute(
                AuthCommand::Login(args),
                &mut Context {
                    env: &env,
                    providers: &self.providers,
                    store: &store,
                    http: &KeyGiver,
                    timeout: Duration::from_secs(5),
                    open,
                    interactive: false,
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
                store: &store,
                http: &KeyGiver,
                timeout: Duration::from_millis(100),
                open: &open,
                interactive: false,
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
}
