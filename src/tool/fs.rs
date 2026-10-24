//! Shared filesystem policies; the execution base deliberately is not a sandbox.

use std::io;
use std::path::PathBuf;

use super::{Code, Context, Failure};

pub(super) fn path(context: &Context, value: &str) -> Result<PathBuf, Failure> {
    if value.is_empty() || value.contains('\0') {
        return Err(Failure::new(
            Code::InvalidArguments,
            "Use a nonempty path without NUL bytes.",
        ));
    }
    Ok(context.cwd().join(value))
}

pub(super) fn io(error: io::Error) -> Failure {
    // OS messages can quote arbitrary paths; keep model guidance bounded and diagnostics redacted.
    let message = match error.kind() {
        io::ErrorKind::NotFound => "Path not found; check the path and its symlink targets.",
        io::ErrorKind::PermissionDenied => {
            "Path is inaccessible; choose a readable path or correct its permissions."
        }
        _ => "Filesystem operation failed; check the path, permissions and symlink targets.",
    };
    Failure::new(Code::Execution, message)
}

pub(super) fn limit() -> Failure {
    Failure::new(
        Code::OutputLimit,
        "An entry cannot fit the output budget; increase the budget or request smaller content.",
    )
}
