//! Argument types only.
//!
//! No behaviour lives here: parsing is one concern and doing the work is
//! another, and keeping them apart is what lets the library be used without
//! the CLI.

use clap::Parser;

/// A minimal, headless Rust agent runtime implementing ACPv2.
// No subcommands: a client launches `bee` and speaks ACPv2 on stdin/stdout.
// More launch options wait for the configuration work that needs them.
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
