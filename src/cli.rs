//! Argument types only.
//!
//! No behaviour lives here: parsing is one concern and doing the work is
//! another, and keeping them apart is what lets the library be used without
//! the CLI.

use clap::builder::TypedValueParser;
use clap::{Args, Parser, Subcommand};

/// A minimal, headless Rust agent runtime implementing ACPv2.
// Without a subcommand a client launched `bee` and it speaks ACPv2 on
// stdin/stdout. Subcommands are human-facing operations, never ACP traffic.
#[derive(Debug, Parser)]
#[command(
    name = "bee",
    version,
    about,
    long_about = "A minimal, headless Rust agent runtime implementing ACPv2.\n\n\
                  Started by an ACPv2 client, Bee speaks the protocol on stdin and \
                  stdout. Stdout carries protocol traffic only; diagnostics go to stderr."
)]
pub struct Cli {
    /// Serve a deterministic ACP test scenario without configuration or an LLM.
    #[arg(long, value_parser = scenario_parser())]
    pub scenario: Option<bee::scenario::Scenario>,

    /// Increase verbosity. Repeat for more.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// What to do instead of serving ACPv2.
    #[command(subcommand)]
    pub command: Option<Command>,
}

fn scenario_parser() -> impl TypedValueParser<Value = bee::scenario::Scenario> {
    clap::builder::PossibleValuesParser::new(bee::scenario::Scenario::NAMED.map(|(name, _)| name))
        .map(|name| {
            bee::scenario::Scenario::NAMED
                .into_iter()
                .find(|(candidate, _)| *candidate == name)
                .expect("the possible-values parser validated the name")
                .1
        })
}

/// What a person can ask Bee to do besides being launched by a client.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Discover model IDs and metadata without making the catalog an allowlist.
    #[command(subcommand)]
    Models(ModelsCommand),
    /// Manage the credentials of provider profiles.
    #[command(subcommand)]
    Auth(AuthCommand),
}

/// `bee models ...`.
#[derive(Debug, Subcommand)]
pub enum ModelsCommand {
    /// Browse the public catalog's providers, independently of configured connections.
    Providers {
        /// Print structured metadata and freshness rather than a table.
        #[arg(long)]
        json: bool,
    },
    /// Show model suggestions, automatically discovering supported native connections.
    List {
        /// Connection in the active profile; its default connection when omitted.
        #[arg(value_name = "CONNECTION", conflicts_with = "catalog_provider")]
        profile: Option<String>,
        /// Browse a Models.dev provider rather than a configured connection.
        #[arg(long, conflicts_with = "endpoint")]
        catalog_provider: Option<String>,
        /// Force connection discovery, including generic OpenAI-compatible endpoints.
        #[arg(long)]
        endpoint: bool,
        /// Print per-field provenance and freshness as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Refresh a connection's native catalogue, or the public catalogue when omitted.
    Refresh {
        /// Connection in the active profile; omitted refreshes Models.dev.
        #[arg(value_name = "CONNECTION")]
        profile: Option<String>,
        /// Print structured refresh status.
        #[arg(long)]
        json: bool,
    },
}

/// `bee auth ...`.
#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    /// Store a credential for a connection that uses `credential = { store = true }`.
    ///
    /// Without flags, signs in with the browser if the connection has a `login`
    /// configured. With `--with-token`, reads an API token from standard input.
    Login(LoginArgs),
    /// Forget the credential Bee stored for a connection.
    ///
    /// API keys stay valid with the provider until revoked there. ChatGPT
    /// logout also attempts renewable-session revocation, retaining registration.
    Logout {
        /// Connection in the active profile; its default connection if omitted.
        #[arg(value_name = "CONNECTION")]
        profile: Option<String>,
    },
    /// Show where each active connection's credential comes from and whether it is
    /// available. Never prints a credential.
    Status {
        /// Only this connection in the active profile.
        #[arg(value_name = "CONNECTION")]
        profile: Option<String>,
    },
}

/// The arguments of `bee auth login`.
#[derive(Debug, Args)]
pub struct LoginArgs {
    /// Connection in the active profile; its default connection if omitted.
    #[arg(value_name = "CONNECTION")]
    pub profile: Option<String>,

    /// Read an API token from standard input instead of signing in.
    ///
    /// Pipe it (`echo "$KEY" | bee auth login --with-token`) so it is neither
    /// echoed nor kept in your shell history.
    #[arg(long, conflicts_with_all = ["paste_code", "no_browser"])]
    pub with_token: bool,

    /// Sign in without a local callback: open the printed URL anywhere, then
    /// paste the code the provider shows. For SSH sessions and containers.
    #[arg(long, conflicts_with = "no_browser")]
    pub paste_code: bool,

    /// Print the sign-in URL without trying to open a browser.
    #[arg(long)]
    pub no_browser: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    // Catches the derive mistakes that would otherwise only surface as a panic
    // the first time a user runs the binary.
    #[test]
    fn the_command_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn global_verbosity_remains_available_to_human_facing_commands() {
        for args in [
            ["bee", "-v", "auth", "status"],
            ["bee", "auth", "status", "-v"],
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            assert_eq!(cli.verbose, 1);
            assert!(matches!(cli.command, Some(Command::Auth(_))));
        }
    }
}
