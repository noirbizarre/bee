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

    /// Show detailed diagnostics.
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
    /// Inspect the built-in provider integrations without configuration or credentials.
    #[command(subcommand)]
    Providers(ProvidersCommand),
    /// List available account models or browse public model metadata.
    #[command(subcommand)]
    Models(ModelsCommand),
    /// Manage account credentials.
    #[command(subcommand)]
    Auth(AuthCommand),
}

/// Compiled-in integrations, distinct from the public Models.dev catalogue.
#[derive(Debug, Subcommand)]
pub enum ProvidersCommand {
    /// List supported native and custom provider types.
    List,
    /// Show supported authentication and declared fields for one provider type.
    Show {
        /// Compiled provider ID, as referenced by an account's provider field.
        provider_type: String,
    },
}

/// `bee models ...`.
#[derive(Debug, Subcommand)]
pub enum ModelsCommand {
    /// Browse the public catalog's providers, independently of configured accounts.
    Providers {
        /// Print structured metadata and freshness rather than a table.
        #[arg(long)]
        json: bool,
    },
    /// List available models across active accounts, with automatic account discovery.
    List {
        /// Account in the active profile; all active accounts when omitted.
        #[arg(value_name = "ACCOUNT", conflicts_with = "catalog")]
        profile: Option<String>,
        /// Browse a Models.dev provider independently of configured accounts.
        #[arg(long, value_name = "ID", conflicts_with = "endpoint")]
        catalog: Option<String>,
        /// Refresh endpoint metadata without expanding declared account availability.
        #[arg(long)]
        endpoint: bool,
        /// Print per-field provenance and freshness as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Refresh an account's native catalogue, or the public catalogue when omitted.
    Refresh {
        /// Account in the active profile; omitted refreshes Models.dev.
        #[arg(value_name = "ACCOUNT")]
        profile: Option<String>,
        /// Print structured refresh status.
        #[arg(long)]
        json: bool,
    },
}

/// `bee auth ...`.
#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    /// Authenticate an account, creating it when its name identifies a native provider.
    ///
    /// Without flags, imports declared environment credentials or runs native browser/device login.
    /// Explicit environment login switches the account to stored authority.
    /// With `--with-token`, reads an API token from standard input.
    Login(LoginArgs),
    /// Forget the credential Bee stored for an account.
    ///
    /// API keys stay valid with the provider until revoked there. ChatGPT
    /// logout also attempts renewable-session revocation, retaining registration.
    Logout {
        /// Configured account; may be omitted only with exactly one active account.
        #[arg(value_name = "ACCOUNT")]
        profile: Option<String>,
    },
    /// Show where each active account's credential comes from and whether it is
    /// available. Never prints a credential.
    Status {
        /// Only this configured account, including disabled accounts.
        #[arg(value_name = "ACCOUNT")]
        profile: Option<String>,
    },
}

/// The arguments of `bee auth login`.
#[derive(Debug, Args)]
pub struct LoginArgs {
    /// Configured account or native provider name; omitted requires one active account.
    #[arg(value_name = "ACCOUNT")]
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
