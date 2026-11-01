//! SDK-free, opt-in delegation policy and inherited authority.

use crate::{Error, Result, config};

/// Cooperative depth ceiling passed to every delegated process.
pub const REMAINING_DEPTH: &str = "BEE_DELEGATION_REMAINING_DEPTH";

/// Validated immutable policy; configuration never creates child processes.
#[derive(Clone, Copy)]
pub struct Settings {
    pub(crate) enabled: bool,
    pub(crate) max_children: usize,
    pub(crate) max_concurrent: usize,
    pub(crate) depth: usize,
    pub(crate) timeout: std::time::Duration,
}

impl std::fmt::Debug for Settings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DelegationSettings([redacted])")
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            max_children: 4,
            max_concurrent: 2,
            depth: 1,
            timeout: std::time::Duration::from_secs(600),
        }
    }
}

impl Settings {
    /// Parse ordinary merged configuration, including disabled policies.
    ///
    /// # Errors
    /// Rejects unknown fields, types and unbounded resource limits.
    pub fn from_config(config: &config::Config) -> Result<Self> {
        let mut settings = Self::default();
        let Some(value) = config.get("delegation") else {
            return Ok(settings);
        };
        let fail = |key, reason| Error::DelegationSetting {
            key,
            path: config
                .origin(key)
                .map_or_else(|| "<configuration>".into(), |p| p.display().to_string()),
            reason,
        };
        // Hard ceilings also bound batch input, pending owners and timer arithmetic.
        for (key, value) in value
            .as_table()
            .ok_or_else(|| fail("delegation", "must be a table"))?
        {
            if key == "enabled" {
                settings.enabled = value
                    .as_bool()
                    .ok_or_else(|| fail("delegation.enabled", "must be a boolean"))?;
                continue;
            }
            let (known, minimum, maximum, reason) = match key.as_str() {
                "max_children" => (
                    "delegation.max_children",
                    1,
                    64,
                    "must be an integer from 1 to 64",
                ),
                "max_concurrent" => (
                    "delegation.max_concurrent",
                    1,
                    64,
                    "must be an integer from 1 to 64",
                ),
                "max_depth" => (
                    "delegation.max_depth",
                    0,
                    64,
                    "must be an integer from 0 to 64",
                ),
                "timeout_seconds" => (
                    "delegation.timeout_seconds",
                    1,
                    86400,
                    "must be an integer from 1 to 86400",
                ),
                _ => {
                    return Err(fail(
                        "delegation",
                        "contains an unknown field; use enabled, max_children, max_concurrent, max_depth or timeout_seconds",
                    ));
                }
            };
            let number = value
                .as_integer()
                .and_then(|v| usize::try_from(v).ok())
                .filter(|number| (minimum..=maximum).contains(number))
                .ok_or_else(|| fail(known, reason))?;
            match key.as_str() {
                "max_children" => settings.max_children = number,
                "max_concurrent" => settings.max_concurrent = number,
                "max_depth" => settings.depth = number,
                "timeout_seconds" => {
                    settings.timeout = std::time::Duration::from_secs(number as u64)
                }
                _ => unreachable!("validated setting name"),
            }
        }
        Ok(settings)
    }

    /// Clamp authority for an embedded child without allowing configuration to restore it.
    #[must_use]
    pub fn with_remaining_depth(mut self, remaining: usize) -> Self {
        self.depth = self.depth.min(remaining);
        self
    }

    /// Apply the inherited process ceiling after all file layering.
    ///
    /// # Errors
    /// Present but invalid authority fails closed, including empty/non-Unicode values.
    pub fn with_environment(self, environment: &config::Environment) -> Result<Self> {
        let Some(value) = environment.raw_variable(REMAINING_DEPTH) else {
            return Ok(self);
        };
        let depth = value
            .to_str()
            .filter(|s| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit()))
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| Error::DelegationSetting {
                key: REMAINING_DEPTH,
                path: "<environment>".into(),
                reason: "has an invalid inherited depth ceiling",
            })?;
        Ok(self.with_remaining_depth(depth))
    }

    pub(crate) fn available(self) -> bool {
        self.enabled && self.depth > 0
    }
}
