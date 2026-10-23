//! Argument types only.
//!
//! No behaviour lives here: parsing is one concern and doing the work is
//! another, and keeping them apart is what lets the library be used without
//! the CLI.

use clap::{Args, Parser, Subcommand};

/// A minimal, headless Rust agent runtime implementing ACPv2.
// Without a subcommand a client launched `bee` and it speaks ACPv2 on
// stdin/stdout. The only subcommand is `auth`, which a person runs by hand.
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
    /// Increase verbosity. Repeat for more.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// What to do instead of serving ACPv2.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// What a person can ask Bee to do besides being launched by a client.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Manage the credentials of provider profiles.
    #[command(subcommand)]
    Auth(AuthCommand),
}

/// `bee auth ...`.
#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    /// Store a credential for a provider profile that uses `credential = { store = true }`.
    ///
    /// Without flags, signs in with the browser if the profile has a `login`
    /// configured. With `--with-token`, reads an API token from standard input.
    Login(LoginArgs),
    /// Forget the credential Bee stored for a profile.
    ///
    /// Only Bee's copy is removed; the key stays valid with the provider until
    /// you revoke it there.
    Logout {
        /// The profile; the default profile if omitted.
        profile: Option<String>,
    },
    /// Show where each profile's credential comes from and whether it is
    /// available. Never prints a credential.
    Status {
        /// Only this profile.
        profile: Option<String>,
    },
}

/// The arguments of `bee auth login`.
#[derive(Debug, Args)]
pub struct LoginArgs {
    /// The profile; the default profile if omitted.
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
}
