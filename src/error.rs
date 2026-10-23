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
