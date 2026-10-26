//! Deterministic, service-free scenarios for ACP client integration tests.

/// A built-in turn repeated for every accepted prompt in a scenario process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scenario {
    /// Two fixed assistant chunks followed by successful completion.
    Success,
    /// An itemized plan and a completed replacement.
    Plan,
    /// Client authorization for a harmless synthetic operation.
    Permission,
    /// Negotiated, bounded form input.
    Input,
    /// Readiness followed by waiting for client cancellation or close.
    Cancel,
    /// Partial output followed by a controlled turn failure.
    Error,
}

impl Scenario {
    /// All supported launch names paired with their scenarios.
    pub const NAMED: [(&str, Self); 6] = [
        ("success", Self::Success),
        ("plan", Self::Plan),
        ("permission", Self::Permission),
        ("input", Self::Input),
        ("cancel", Self::Cancel),
        ("error", Self::Error),
    ];
}
