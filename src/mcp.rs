//! Project MCP definitions and direct connections, independent of the ACP SDK.

pub mod config;
pub(crate) mod runtime;
mod transport;

/// Configuration diagnostics deliberately retain no source values or parser errors.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
#[error("MCP configuration at {path}: {reason}")]
#[diagnostic(
    code(bee::mcp::configuration),
    help("Fix the indicated MCP source and retry the prompt.")
)]
pub struct Error {
    /// Source locator, never its contents.
    pub path: std::path::PathBuf,
    /// Static, actionable classification.
    pub reason: &'static str,
}

pub(crate) const MAX_BYTES: usize = 1024 * 1024;

/// Keep project resources on the same boundary as instructions and Agent Skills.
pub(crate) fn project_root(cwd: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
    let cwd = cwd.canonicalize()?;
    Ok(cwd
        .ancestors()
        .find(|p| p.join(".git").exists())
        .unwrap_or(&cwd)
        .to_path_buf())
}
