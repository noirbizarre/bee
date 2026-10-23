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
    /// The ACPv2 integration does not exist yet.
    // Returned instead of `Ok(())` so a bootstrap build can never be mistaken
    // for a working agent by a client that launches it and waits for a handshake.
    #[error("bee does not implement ACPv2 yet")]
    #[diagnostic(
        code(bee::error::not_implemented),
        help(
            "this build is a project bootstrap with no protocol support; \
             use a release that implements ACPv2 once one is published"
        )
    )]
    NotImplemented,

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
