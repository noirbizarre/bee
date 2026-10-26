//! The bee binary.

#![allow(clippy::result_large_err)]

use std::process::ExitCode;

use clap::{CommandFactory, Parser};
use miette::MietteHandlerOpts;

mod cli;
mod commands;

#[cfg(all(test, unix))]
#[path = "../tests/support/chatgpt.rs"]
mod test_chatgpt;

use cli::{Cli, Command};

fn main() -> ExitCode {
    let args = Cli::parse();
    // Clap's blanket args/subcommand conflict also rejects global verbosity;
    // validate only the serving selector to preserve existing command syntax.
    if args.scenario.is_some() && args.command.is_some() {
        Cli::command()
            .error(
                clap::error::ErrorKind::ArgumentConflict,
                "--scenario cannot be combined with a subcommand",
            )
            .exit();
    }
    let verbose = args.verbose > 0 || std::env::var_os("RUST_BACKTRACE").is_some();
    install_miette_hook(verbose);

    // Without a subcommand a client launched Bee: serve ACPv2. Human-facing
    // commands print only on their separate execution paths.
    let result = match args.command {
        None => match args.scenario {
            Some(scenario) => bee::run_scenario(scenario),
            None => bee::run(),
        },
        Some(Command::Auth(command)) => commands::run_auth(command),
        Some(Command::Models(command)) => commands::models::run(command),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // Rendered here rather than returned as `miette::Result`, so the
            // exit code is chosen explicitly instead of inherited from
            // whatever the report wrapper decides.
            eprintln!("{:?}", miette::Report::new(error));
            ExitCode::FAILURE
        }
    }
}

/// Install miette's diagnostic handler.
///
/// `--verbose` (or `RUST_BACKTRACE`, so a bug report already carries it)
/// prints the full cause chain; otherwise only the primary diagnostic
/// shows, since a source error the user did not ask for is noise more
/// often than it is the answer. Colour is left to miette's own
/// auto-detection so `NO_COLOR`/`TERM=dumb` still produce plain output.
fn install_miette_hook(verbose: bool) {
    let _ = miette::set_hook(Box::new(move |_| {
        let opts = MietteHandlerOpts::new();
        let opts = if verbose {
            opts.with_cause_chain()
        } else {
            opts.without_cause_chain()
        };
        Box::new(opts.build())
    }));
}
