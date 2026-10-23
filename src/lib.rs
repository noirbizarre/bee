//! A minimal, headless Rust agent runtime implementing ACPv2.
//!
//! The engine lives here; `src/main.rs` is a thin CLI over it. That split is
//! what lets the integration tests assert against the library for behaviour
//! and against the binary for output.

// miette's `Diagnostic` payloads carry source text and spans, which puts most
// error variants past clippy's 128-byte `Result` threshold. The lint is right
// about the cost and wrong about the trade: a large error that says what to do
// beats a small one that does not.
#![allow(clippy::result_large_err)]
#![warn(missing_docs)]

pub mod acp;
pub mod auth;
pub mod config;
pub mod error;
pub mod http;
pub mod login;
pub mod provider;
pub mod session;

pub use error::{Error, Result};

/// Run the agent over stdin and stdout until the client disconnects.
///
/// Stdout is reserved for protocol traffic; nothing here may print to it.
///
/// # Errors
///
/// Returns a configuration error if a configuration file is unreadable,
/// malformed or conflicts with another layer, a provider error if a provider
/// profile is invalid, [`Error::Io`] if the working
/// directory cannot be determined, and [`Error::Connection`] if the connection
/// ends with an error.
pub fn run() -> Result<()> {
    // Loaded before the first byte of protocol traffic so a bad file fails the
    // launch with a diagnostic on stderr, where the user sees it, rather than
    // surfacing mid-session. Project configuration is read from the launch
    // directory only; per-session loading waits for a consumer (ADR-006).
    let project_dir = std::env::current_dir().map_err(|source| Error::Io {
        path: ".".to_owned(),
        source,
    })?;
    let config = config::Config::load(&config::Environment::from_process(), &project_dir)?;
    // Validated here for the same reason: a typo in a base URL must stop the
    // launch, not surface as a connection error on the first prompt (ADR-007).
    let providers = provider::Providers::from_config(&config)?;

    // The one place an executor is chosen. The SDK does its stdio on its own
    // thread pool, so a plain executor suffices and the library stays free of
    // an async runtime (ADR-003).
    futures::executor::block_on(acp::serve_stdio(providers))
}
