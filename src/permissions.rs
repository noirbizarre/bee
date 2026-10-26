//! Ordered native-tool routing; remembered ACP approvals belong to the client.

use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

/// Increasing restriction order permits project policy to tighten trusted policy.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    /// Execute without contacting the client.
    Allow,
    /// Request an ACP decision for this invocation.
    #[default]
    Ask,
    /// Refuse execution without contacting the client.
    Deny,
}

/// Implementation-owned resource semantics, independent of presentation metadata.
pub enum Resource {
    /// A requested path, normalized relative to the session directory.
    Path(PathBuf),
    /// The complete shell command, without parsing or decomposition.
    Command(String),
    /// A literal query, glob, or custom tool resource.
    Value(String),
}

/// One concrete check; operations can require several checks atomically.
pub struct Check {
    /// Permission action (for example `read`, `edit`, or `shell`).
    pub action: String,
    /// Requested operation resource.
    pub resource: Resource,
    /// Trusted implementation default when no configured rule matches.
    pub default: Effect,
}

impl Check {
    /// Describe a path operation without inspecting the filesystem.
    pub fn path(action: &str, path: impl Into<PathBuf>, default: Effect) -> Self {
        Self {
            action: action.into(),
            resource: Resource::Path(path.into()),
            default,
        }
    }

    /// Describe a query or custom resource.
    pub fn value(action: &str, value: impl Into<String>, default: Effect) -> Self {
        Self {
            action: action.into(),
            resource: Resource::Value(value.into()),
            default,
        }
    }

    /// Describe a shell command, retaining the exact text for the ACP subject.
    pub fn command(command: impl Into<String>) -> Self {
        Self {
            action: "shell".into(),
            resource: Resource::Command(command.into()),
            default: Effect::Ask,
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Rule {
    action: String,
    resource: String,
    effect: Effect,
}

/// Immutable launch-time routing, with separately evaluated project restrictions.
#[derive(Clone, Default)]
pub struct Settings {
    trusted: Vec<Rule>,
    project: Vec<Rule>,
    home: Option<PathBuf>,
}

// Rule text can contain credentials or private commands, like any configuration value.
impl std::fmt::Debug for Settings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Settings([redacted])")
    }
}

impl Settings {
    /// Parse ordered rule arrays without letting project configuration erase trusted rules.
    ///
    /// # Errors
    /// Returns a value-free diagnostic for malformed permission rules.
    pub fn from_config(
        config: &crate::config::Config,
        environment: &crate::config::Environment,
    ) -> crate::Result<Self> {
        let mut settings = Self {
            home: environment.home_dir().map(Path::to_path_buf),
            ..Self::default()
        };
        for (path, project, value) in config.permission_layers() {
            let rules: Vec<Rule> = value.clone().try_into().map_err(|_| crate::Error::PermissionSetting {
                path: path.display().to_string(),
                reason: "must be an array of rules with string action/resource and allow, ask, or deny effect",
            })?;
            // Empty actions are almost certainly a typo; an empty resource can be intentional.
            if rules.iter().any(|rule| rule.action.is_empty()) {
                return Err(crate::Error::PermissionSetting {
                    path: path.display().to_string(),
                    reason: "requires a nonempty action in every rule",
                });
            }
            if project {
                settings.project.extend(rules);
            } else {
                settings.trusted.extend(rules);
            }
        }
        Ok(settings)
    }

    /// Resolve every resource before execution; deny dominates ask, which dominates allow.
    pub fn evaluate(&self, checks: &[Check], cwd: &Path) -> Effect {
        checks
            .iter()
            .map(|check| {
                let resource = match &check.resource {
                    Resource::Path(path) => path_resource(path, cwd, false),
                    Resource::Command(command) | Resource::Value(command) => command.clone(),
                };
                let resolve = |rules: &[Rule], fallback| {
                    rules
                        .iter()
                        .rfind(|rule| {
                            if !wildcard(&rule.action, &check.action) {
                                return false;
                            }
                            let mut pattern = rule.resource.clone();
                            let mut value = resource.clone();
                            if let Resource::Path(path) = &check.resource {
                                pattern = expand_home(&pattern, self.home.as_deref());
                                // Only path resources use platform path normalization; commands stay raw.
                                if cfg!(windows) {
                                    pattern = pattern.replace('\\', "/");
                                }
                                // Keep absolute/home scopes absolute: stripping cwd from `/cwd/*`
                                // would turn a scoped allow into `*`, which also matches external paths.
                                let absolute = Path::new(&pattern).is_absolute();
                                value = path_resource(path, cwd, absolute);
                                pattern = path_resource(Path::new(&pattern), cwd, absolute);
                            }
                            wildcard(&pattern, &value)
                                || matches!(check.resource, Resource::Command(_))
                                    && pattern
                                        .strip_suffix(" *")
                                        .is_some_and(|bare| wildcard(bare, &value))
                        })
                        .map_or(fallback, |rule| rule.effect)
                };
                resolve(&self.trusted, check.default).max(resolve(&self.project, Effect::Allow))
            })
            .max()
            .unwrap_or(Effect::Ask)
    }
}

fn expand_home(pattern: &str, home: Option<&Path>) -> String {
    let Some(home) = home else {
        return pattern.into();
    };
    for prefix in ["~", "$HOME"] {
        if pattern == prefix {
            return home.to_string_lossy().into_owned();
        }
        if let Some(suffix) = pattern.strip_prefix(&format!("{prefix}/")) {
            return home.join(suffix).to_string_lossy().into_owned();
        }
    }
    pattern.into()
}

fn lexical(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            other => result.push(other.as_os_str()),
        }
    }
    result
}

fn path_resource(path: &Path, cwd: &Path, keep_absolute: bool) -> String {
    // Matching requested paths avoids filesystem races and works for absent destinations.
    let mut cwd = lexical(cwd);
    let mut absolute = lexical(&cwd.join(path));
    if cfg!(windows) {
        // Prefix stripping must share matching's case rules, including absolute user patterns.
        cwd = PathBuf::from(cwd.to_string_lossy().to_lowercase());
        absolute = PathBuf::from(absolute.to_string_lossy().to_lowercase());
    }
    let path = if keep_absolute {
        &absolute
    } else {
        absolute.strip_prefix(&cwd).unwrap_or(&absolute)
    };
    let value = if path.as_os_str().is_empty() {
        ".".into()
    } else {
        path.to_string_lossy().into_owned()
    };
    if cfg!(windows) {
        value.replace('\\', "/")
    } else {
        value
    }
}

fn wildcard(pattern: &str, value: &str) -> bool {
    // Backtrack only to the latest star; query text is literal rather than a regex program.
    let pattern: Vec<_> = pattern.chars().collect();
    let value: Vec<_> = value.chars().collect();
    let (mut p, mut v, mut star) = (0, 0, None);
    while v < value.len() {
        // A resource can itself contain `*`; pattern stars must still record their wildcard state.
        if p < pattern.len() && pattern[p] == '*' {
            star = Some((p, v));
            p += 1;
        } else if p < pattern.len() && (pattern[p] == '?' || pattern[p] == value[v]) {
            p += 1;
            v += 1;
        } else if let Some((position, consumed)) = star {
            star = Some((position, consumed + 1));
            p = position + 1;
            v = consumed + 1;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}
