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
