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

pub mod error;

pub use error::{Error, Result};

/// Run the agent.
///
/// # Errors
///
/// Always returns [`Error::NotImplemented`] until the ACPv2 integration lands.
pub fn run() -> Result<()> {
    Err(Error::NotImplemented)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_before_acpv2_exists_reports_it_instead_of_pretending_to_succeed() {
        assert!(matches!(run(), Err(Error::NotImplemented)));
    }
}
