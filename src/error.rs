//! The error type, and the diagnostics it renders to.
//!
//! `thiserror` defines them, `miette` renders them. A diagnostic must carry the
//! two things the user does not already know: what specifically failed, and
//! what to do about it.

use miette::Diagnostic;
use thiserror::Error;

/// The crate's result type.
pub type Result<T> = std::result::Result<T, Error>;

/// Everything that can go wrong.
///
/// Diagnostic codes are `bee::<module>::<kind>`. A code is a public
/// identifier users grep for, so renaming one is a breaking change.
#[derive(Debug, Error, Diagnostic)]
#[non_exhaustive]
pub enum Error {
    /// The ACPv2 connection ended with an error.
    // Holds the SDK error's text, not the SDK error: this module must not
    // import the SDK, or an upgrade would leak out of `acp.rs` (ADR-001).
    #[error("the ACPv2 connection failed: {reason}")]
    #[diagnostic(
        code(bee::acp::connection),
        help(
            "the client probably disconnected abnormally or sent traffic that is \
             not ACPv2; re-run with `-v` and check the client's own log"
        )
    )]
    Connection {
        /// What the SDK reported.
        reason: String,
    },

    /// A configuration file exists but could not be read.
    #[error("failed to read the configuration file `{path}`")]
    #[diagnostic(
        code(bee::config::read),
        help(
            "check that it is a readable UTF-8 text file, not a directory; \
             fix its permissions or remove it"
        )
    )]
    ConfigRead {
        /// The configuration file.
        path: String,
        /// Why.
        #[source]
        source: std::io::Error,
    },

    /// The file named by `BEE_CONFIG` does not exist.
    #[error("`BEE_CONFIG` points at `{path}`, which does not exist")]
    #[diagnostic(
        code(bee::config::selected_missing),
        help(
            "create the file, fix the path, or unset `BEE_CONFIG` to go without an intermediate layer"
        )
    )]
    ConfigSelectedMissing {
        /// The path `BEE_CONFIG` names.
        path: String,
    },

    /// A configuration file is not valid TOML.
    // Carries the parser's message and a position, never the source line: the
    // line may hold a secret, and diagnostics end up in logs.
    #[error("invalid configuration in `{path}` at line {line}, column {column}: {reason}")]
    #[diagnostic(
        code(bee::config::parse),
        help(
            "fix the TOML syntax at that position; the offending text is not echoed in case it is a secret"
        )
    )]
    ConfigParse {
        /// The configuration file.
        path: String,
        /// 1-based line of the error.
        line: usize,
        /// 1-based column of the error.
        column: usize,
        /// What the parser reported.
        reason: String,
    },

    /// Two configuration layers give one key values of different types.
    // Names types, not values, for the same reason `ConfigParse` omits source.
    #[error(
        "configuration key `{key}` is {lower_type} in `{lower_path}` but {higher_type} in `{higher_path}`"
    )]
    #[diagnostic(
        code(bee::config::conflict),
        help("give the key the same type in both files, or remove it from one of them")
    )]
    ConfigConflict {
        /// The dotted key.
        key: String,
        /// The file that set the key first (lower precedence).
        lower_path: String,
        /// The type the lower layer gave it.
        lower_type: &'static str,
        /// The file that conflicts with it (higher precedence).
        higher_path: String,
        /// The type the higher layer gave it.
        higher_type: &'static str,
    },

    /// A provider setting has the wrong TOML type.
    // Names the key, the file and the types, never the value: the value may be
    // a secret someone put in the wrong place.
    #[error("provider setting `{key}` in `{path}` must be {expected}, not {found}")]
    #[diagnostic(
        code(bee::provider::wrong_type),
        help(
            "change `{key}` to {expected}; the offending value is not echoed in case it is a secret"
        )
    )]
    ProviderType {
        /// The dotted key.
        key: String,
        /// The file that set it.
        path: String,
        /// The type the setting needs.
        expected: &'static str,
        /// The type it has.
        found: &'static str,
    },

    /// A provider profile names a protocol Bee does not implement.
    #[error("provider setting `{key}` in `{path}` names a protocol Bee does not support")]
    #[diagnostic(
        code(bee::provider::unknown_protocol),
        help(
            "each name under `protocols` selects how Bee talks to the endpoint, not who runs it; use one of: {supported}"
        )
    )]
    ProviderUnknownProtocol {
        /// The dotted key.
        key: String,
        /// The file that set it.
        path: String,
        /// The protocols Bee supports, comma separated.
        supported: String,
    },

    /// A profile serves several protocols and does not say which to prefer.
    #[error("provider profile `{profile}` lists several protocols ({available}) but no `prefer`")]
    #[diagnostic(
        code(bee::provider::no_preference),
        help(
            "set `prefer = [\"<protocol>\", ...]` in `[providers.{profile}]`, most preferred first; \
             Bee does not guess among the protocols"
        )
    )]
    ProviderNoPreference {
        /// The profile name.
        profile: String,
        /// The protocols it lists, comma separated.
        available: String,
    },

    /// A provider table holds a key Bee does not know.
    // Rejected instead of kept: a mistyped `base_ulr` would otherwise send
    // traffic to the protocol's default endpoint without a word.
    #[error("unknown provider setting `{key}` in `{path}`")]
    #[diagnostic(
        code(bee::provider::unknown_key),
        help("check the spelling; this table accepts: {allowed}")
    )]
    ProviderUnknownKey {
        /// The dotted key.
        key: String,
        /// The file that set it.
        path: String,
        /// The keys the table accepts, comma separated.
        allowed: String,
    },

    /// A provider profile lacks a required key.
    #[error("provider profile `{profile}` has no `{key}`")]
    #[diagnostic(
        code(bee::provider::missing_key),
        help(
            "add `{key}` to `[providers.{profile}]`; it is needed to know how to reach the endpoint"
        )
    )]
    ProviderMissingKey {
        /// The profile name.
        profile: String,
        /// The missing key.
        key: &'static str,
        /// The file that defines the profile.
        path: String,
    },

    /// A provider setting has the right type but an unusable value.
    // `reason` is always fixed text from this crate, never derived from the
    // value, so it cannot repeat a secret.
    #[error("invalid provider setting `{key}` in `{path}`: {reason}")]
    #[diagnostic(
        code(bee::provider::invalid),
        help("fix the value; it is not echoed in case it is a secret")
    )]
    ProviderInvalid {
        /// The dotted key.
        key: String,
        /// The file that set it.
        path: String,
        /// What is wrong with it.
        reason: &'static str,
    },

    /// A profile names a preset that does not exist.
    // The name is not echoed: the likeliest typo is a pasted key.
    #[error("provider setting `{key}` in `{path}` names a preset that does not exist")]
    #[diagnostic(
        code(bee::provider::unknown_preset),
        help(
            "use one of: {available}; or define it under `[presets.<name>]` in your configuration"
        )
    )]
    ProviderUnknownPreset {
        /// The dotted key.
        key: String,
        /// The file that set it.
        path: String,
        /// The defined presets, comma separated.
        available: String,
    },

    /// A credential names a login flow that does not exist.
    #[error("provider setting `{key}` in `{path}` names a login flow that does not exist")]
    #[diagnostic(
        code(bee::provider::unknown_login),
        help(
            "use one of: {available}; or define it under `[login_flows.<name>]` in your global \
             configuration"
        )
    )]
    ProviderUnknownLogin {
        /// The dotted key.
        key: String,
        /// The file that set it.
        path: String,
        /// The defined login flows, comma separated.
        available: String,
    },

    /// The project file defines a login flow.
    // Names the key only: it is the URLs the file chose that are the problem.
    #[error("the project configuration `{path}` may not define the login flow setting `{key}`")]
    #[diagnostic(
        code(bee::provider::untrusted_layer),
        help(
            "a login flow decides where the authorization code and key are sent, and a project \
             file can come from a repository you only cloned; define it in your global \
             configuration or in the file `BEE_CONFIG` names"
        )
    )]
    ProviderUntrustedLayer {
        /// The dotted key.
        key: String,
        /// The project file.
        path: String,
    },

    /// `provider` names a profile that is not defined.
    #[error("the default `provider` does not name a defined profile (defined: {available})")]
    #[diagnostic(
        code(bee::provider::unknown_default),
        help(
            "set `provider` to one of the profiles under `[providers]`, or define the one you meant"
        )
    )]
    ProviderUnknownDefault {
        /// The defined profiles, comma separated.
        available: String,
    },

    /// Several profiles exist and none is the default.
    #[error("several provider profiles are defined ({available}) but no default `provider` is set")]
    #[diagnostic(
        code(bee::provider::no_default),
        help("set `provider = \"<profile>\"` at the top of the configuration to pick the default")
    )]
    ProviderNoDefault {
        /// The defined profiles, comma separated.
        available: String,
    },

    // Authentication (ADR-008). None of these carries a credential, an
    // authorization code, a verifier or a response body: they name a profile,
    // a variable, a path or a status, and say what to do next.
    /// A profile's credential variable is not set.
    #[error(
        "provider profile `{profile}` reads its credential from `{variable}`, which is not set"
    )]
    #[diagnostic(
        code(bee::auth::missing_env),
        help(
            "export `{variable}` in the environment that launches Bee, or point `credential` at \
             another source; an empty value counts as unset"
        )
    )]
    AuthMissingEnv {
        /// The profile.
        profile: String,
        /// The variable's name.
        variable: String,
    },

    /// A stored credential was asked for but none is stored.
    #[error("no credential is stored for provider profile `{profile}`")]
    #[diagnostic(
        code(bee::auth::not_logged_in),
        help(
            "run `bee auth login {profile}` to sign in, or `bee auth login {profile} --with-token` \
             to store an API token"
        )
    )]
    AuthNotLoggedIn {
        /// The profile.
        profile: String,
    },

    /// A stored credential is past its expiry.
    #[error("the stored credential for provider profile `{profile}` has expired")]
    #[diagnostic(
        code(bee::auth::expired),
        help("run `bee auth login {profile}` again; Bee does not refresh this kind of credential")
    )]
    AuthExpired {
        /// The profile.
        profile: String,
    },

    /// The credential file can be read by other users.
    #[error("the credential file `{path}` is accessible to other users (mode {mode})")]
    #[diagnostic(
        code(bee::auth::insecure_permissions),
        help(
            "run `chmod 600 {path}`, then retry; Bee refuses to read credentials that others \
             may have read"
        )
    )]
    AuthInsecurePermissions {
        /// The credential file.
        path: String,
        /// Its permission bits, in octal.
        mode: String,
    },

    /// The credential file could not be read.
    #[error("failed to read the credential file `{path}`")]
    #[diagnostic(
        code(bee::auth::store_read),
        help(
            "check that it is a readable file, not a directory; fix its permissions or remove it"
        )
    )]
    AuthStoreRead {
        /// The credential file.
        path: String,
        /// Why.
        #[source]
        source: std::io::Error,
    },

    /// The credential file is not what Bee wrote.
    // Fixed text and a position, never the offending text: the file is full of
    // secrets.
    #[error("the credential file `{path}` is not valid: {reason}")]
    #[diagnostic(
        code(bee::auth::store_parse),
        help(
            "fix or delete the file, then sign in again with `bee auth login`; its content is not \
             echoed because it holds secrets"
        )
    )]
    AuthStoreParse {
        /// The credential file.
        path: String,
        /// What is wrong with it.
        reason: String,
    },

    /// The credential file could not be written.
    #[error("failed to write the credential file `{path}`")]
    #[diagnostic(
        code(bee::auth::store_write),
        help(
            "check that the directory is writable and the disk is not full; the previous file is \
             left as it was"
        )
    )]
    AuthStoreWrite {
        /// The credential file.
        path: String,
        /// Why.
        #[source]
        source: std::io::Error,
    },

    /// There is nowhere to keep credentials.
    #[error("cannot tell where to keep credentials: neither `XDG_DATA_HOME` nor `HOME` is usable")]
    #[diagnostic(
        code(bee::auth::no_store_path),
        help(
            "set `XDG_DATA_HOME` to an absolute directory, or `HOME`; or use an `env` credential, \
             which needs no file"
        )
    )]
    AuthNoStorePath,

    /// The credential file's permissions cannot be enforced on this platform.
    #[error("storing credentials is not supported on this platform")]
    #[diagnostic(
        code(bee::auth::store_unsupported),
        help(
            "use an `env` credential instead; Bee only writes credentials where it can make the file private"
        )
    )]
    AuthStoreUnsupported,

    /// The operating system could not provide random bytes.
    #[error("the operating system's random number generator failed")]
    #[diagnostic(
        code(bee::auth::randomness),
        help("nothing was stored; retry, and check that the system is not out of entropy sources")
    )]
    AuthRandomness,

    /// The sign-in callback carried a different `state` than the request.
    #[error("the sign-in callback did not carry the expected `state`")]
    #[diagnostic(
        code(bee::auth::state_mismatch),
        help(
            "this can mean something other than your browser called back; nothing was stored. \
             Run `bee auth login` again and finish the sign-in in the same browser window"
        )
    )]
    AuthStateMismatch,

    /// The user, or the provider, refused the sign-in.
    #[error("the sign-in was not authorized")]
    #[diagnostic(
        code(bee::auth::denied),
        help(
            "run `bee auth login` again and approve the request, or store a token with `--with-token`"
        )
    )]
    AuthDenied,

    /// Nobody completed the sign-in in time.
    #[error("the sign-in was not completed within {seconds} seconds")]
    #[diagnostic(
        code(bee::auth::callback_timeout),
        help(
            "run `bee auth login` again; on a machine without a browser use `--paste-code`, \
             or store a token with `--with-token`"
        )
    )]
    AuthCallbackTimeout {
        /// How long Bee waited.
        seconds: u64,
    },

    /// The local callback server failed.
    #[error("the local sign-in callback listener failed")]
    #[diagnostic(
        code(bee::auth::callback_io),
        help("check that loopback networking works, or use `--paste-code` to avoid the listener")
    )]
    AuthCallbackIo {
        /// Why.
        #[source]
        source: std::io::Error,
    },

    /// The callback did not carry an authorization code.
    #[error("the sign-in callback carried no authorization code")]
    #[diagnostic(
        code(bee::auth::callback_invalid),
        help("run `bee auth login` again; if it repeats, use `--paste-code`")
    )]
    AuthCallbackInvalid,

    /// The provider refused to exchange the code for a key.
    // A status and fixed text: the response body could echo the code or the key.
    #[error("the `{provider}` sign-in could not exchange the authorization code{status}: {reason}")]
    #[diagnostic(code(bee::auth::exchange_failed), help("{help}"))]
    AuthExchangeFailed {
        /// The login flow's name.
        provider: String,
        /// ` (HTTP 403)`, or empty when there was no response.
        status: String,
        /// What it means.
        reason: &'static str,
        /// What to do.
        help: String,
    },

    /// `--paste-code` was asked of a flow that only redirects.
    #[error("the `{flow}` sign-in cannot show a code to paste")]
    #[diagnostic(
        code(bee::auth::no_headless_flow),
        help(
            "run `bee auth login` on a machine with a browser, or store a token with `--with-token`"
        )
    )]
    AuthNoHeadlessFlow {
        /// The login flow's name.
        flow: String,
    },

    /// The profile has no browser sign-in configured.
    #[error("provider profile `{profile}` has no browser sign-in to run")]
    #[diagnostic(
        code(bee::auth::no_login_flow),
        help(
            "add `login = \"openrouter\"` next to `store = true` in its `credential` if the \
             provider supports it, or store an API token with `bee auth login {profile} --with-token`"
        )
    )]
    AuthNoLoginFlow {
        /// The profile.
        profile: String,
    },

    /// The profile reads its credential from somewhere Bee does not write.
    #[error("provider profile `{profile}` does not use the credential store")]
    #[diagnostic(
        code(bee::auth::not_stored),
        help(
            "set the variable its `credential` names, or change `credential` to \
             `{{ store = true }}` to let Bee keep the credential"
        )
    )]
    AuthNotStored {
        /// The profile.
        profile: String,
    },

    /// A token was given but it is empty.
    #[error("the token is empty")]
    #[diagnostic(
        code(bee::auth::empty_token),
        help(
            "pipe the token to `bee auth login <profile> --with-token`, for example `echo \"$KEY\" | bee auth login <profile> --with-token`"
        )
    )]
    AuthEmptyToken,

    /// `bee auth` was given a profile that is not defined.
    #[error("no provider profile is called `{profile}` (defined: {available})")]
    #[diagnostic(
        code(bee::auth::unknown_profile),
        help("use one of the profiles under `[providers]`, or define it first")
    )]
    AuthUnknownProfile {
        /// The name that was given.
        profile: String,
        /// The defined profiles, comma separated.
        available: String,
    },

    /// Reading or writing a file failed.
    #[error("failed to access `{path}`")]
    #[diagnostic(code(bee::error::io))]
    Io {
        /// The path that could not be accessed.
        path: String,
        /// Why.
        #[source]
        source: std::io::Error,
    },
}
