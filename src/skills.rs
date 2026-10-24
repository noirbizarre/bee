//! Standard Agent Skills discovery and content-redacted validation.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use unicode_normalization::UnicodeNormalization;
use yaml_serde::Value;

use crate::instructions::{Diagnostic, read_source};

/// Additional roots; normal configuration layering replaces the entire array.
#[derive(Clone, Default)]
pub struct Settings {
    roots: Vec<PathBuf>,
    origin: String,
}

impl Settings {
    /// Validate only the skills-owned table and resolve paths at their leaf origin.
    pub fn from_config(config: &crate::config::Config, launch: &Path) -> crate::Result<Self> {
        let origin = config
            .origin("skills.extra_roots")
            .or_else(|| config.origin("skills"));
        let failure = |key: &str, reason| crate::Error::SkillsSetting {
            path: config
                .origin(key)
                .map_or_else(|| "<configuration>".into(), |p| p.display().to_string()),
            reason,
        };
        let Some(value) = config.get("skills") else {
            return Ok(Self::default());
        };
        let table = value
            .as_table()
            .ok_or_else(|| failure("skills", "must be a table"))?;
        if let Some(key) = table.keys().find(|key| *key != "extra_roots") {
            // Consult the offending leaf's provenance without echoing its arbitrary key.
            return Err(failure(
                &format!("skills.{key}"),
                "contains an unknown setting; use extra_roots",
            ));
        }
        let Some(value) = table.get("extra_roots") else {
            return Ok(Self::default());
        };
        let values = value.as_array().ok_or_else(|| {
            failure(
                "skills.extra_roots",
                "extra_roots must be an array of paths",
            )
        })?;
        if values.len() > 64 {
            return Err(failure(
                "skills.extra_roots",
                "extra_roots exceeds 64 locations",
            ));
        }
        let source = origin.map(|p| launch.join(p));
        let base = source.as_deref().and_then(Path::parent).unwrap_or(launch);
        let roots = values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| base.join(s))
                    .ok_or_else(|| {
                        failure(
                            "skills.extra_roots",
                            "extra_roots entries must be nonempty strings",
                        )
                    })
            })
            .collect::<crate::Result<_>>()?;
        Ok(Self {
            roots,
            origin: source.map_or_else(|| "<configuration>".into(), |p| p.display().to_string()),
        })
    }
}

/// A validated skill's metadata and canonical source identity.
#[derive(Clone)]
pub struct Skill {
    /// Normalized name used for collision handling.
    pub name: String,
    /// When to use the skill, disclosed without its body.
    pub description: String,
    /// Canonical SKILL.md path; resources are relative to its parent.
    pub path: PathBuf,
    directory_name: String,
    // Activation errors need the same redacted locator as initial discovery.
    diagnostic_source: Option<(PathBuf, usize)>,
}

impl std::fmt::Debug for Skill {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Skill([redacted])")
    }
}

/// Parse real YAML frontmatter and enforce the standard's metadata constraints.
pub fn validate(text: &str, directory: &str, path: &Path) -> Result<Skill, Diagnostic> {
    let failure = |reason| {
        Diagnostic::new(
            "bee::skills::metadata",
            path,
            reason,
            "fix SKILL.md YAML frontmatter to match the Agent Skills specification",
        )
    };
    let text = text.trim_start_matches('\u{feff}');
    let mut lines = text.split_inclusive('\n');
    if lines.next().map(str::trim_end) != Some("---") {
        return Err(failure("missing YAML frontmatter"));
    }
    let mut yaml = String::new();
    let mut closed = false;
    for line in lines {
        if line.trim_end() == "---" {
            closed = true;
            break;
        }
        if yaml.len() + line.len() > 32 * 1024 {
            return Err(failure("frontmatter exceeds 32 KiB"));
        }
        yaml.push_str(line);
    }
    if !closed {
        return Err(failure("unterminated YAML frontmatter"));
    }
    // Parser error messages can quote arbitrary scalars or keys: retain positions only.
    let value: Value = yaml_serde::from_str(&yaml).map_err(|error| {
        let mut diagnostic = failure("invalid YAML or duplicate mapping key");
        diagnostic.position = error.location().map(|p| (p.line() + 1, p.column()));
        diagnostic
    })?;
    let table = value
        .as_mapping()
        .ok_or_else(|| failure("frontmatter must be a mapping"))?;
    let allowed = [
        "name",
        "description",
        "license",
        "compatibility",
        "metadata",
        "allowed-tools",
    ];
    if table
        .keys()
        .any(|key| !key.as_str().is_some_and(|key| allowed.contains(&key)))
    {
        return Err(failure(
            "unknown frontmatter field; use standard fields or metadata",
        ));
    }
    let string = |key: &str| table.get(Value::String(key.into())).and_then(Value::as_str);
    let name = string("name").ok_or_else(|| failure("name must be a string"))?;
    // Match skills-ref's international names rather than imposing function-name ASCII rules.
    let name: String = name.trim().nfkc().collect();
    if name.is_empty()
        || name.chars().count() > 64
        || name.to_lowercase() != name
        || name.starts_with('-')
        || name.ends_with('-')
        || name.contains("--")
        || !name.chars().all(|c| c.is_alphanumeric() || c == '-')
        || name != directory.nfkc().collect::<String>()
    {
        return Err(failure(
            "name violates naming rules or does not match the skill directory",
        ));
    }
    let description = string("description")
        .filter(|s| !s.trim().is_empty() && s.chars().count() <= 1024)
        .ok_or_else(|| {
            failure("description must be a nonblank string of at most 1024 characters")
        })?;
    for key in ["license", "compatibility", "allowed-tools"] {
        if table.contains_key(Value::String(key.into())) && string(key).is_none() {
            return Err(failure(
                "optional license, compatibility and allowed-tools fields must be strings",
            ));
        }
    }
    if string("compatibility").is_some_and(|s| s.trim().is_empty() || s.chars().count() > 500) {
        return Err(failure("compatibility must contain 1–500 characters"));
    }
    if let Some(metadata) = table.get(Value::String("metadata".into()))
        && !metadata.as_mapping().is_some_and(|map| {
            map.iter()
                .all(|(k, v)| k.as_str().is_some() && v.as_str().is_some())
        })
    {
        return Err(failure("metadata must map string keys to string values"));
    }
    Ok(Skill {
        name,
        description: description.into(),
        path: path.into(),
        directory_name: directory.into(),
        diagnostic_source: None,
    })
}

impl Skill {
    pub(crate) fn snapshot(&self) -> Result<String, Diagnostic> {
        self.read_snapshot().map_err(|mut diagnostic| {
            if let Some((source, index)) = &self.diagnostic_source {
                diagnostic.path.clone_from(source);
                diagnostic.root_index = Some(*index);
            }
            diagnostic
        })
    }

    fn read_snapshot(&self) -> Result<String, Diagnostic> {
        let text = read_source(&self.path)?.ok_or_else(|| {
            Diagnostic::new(
                "bee::skills::read",
                &self.path,
                "skill no longer exists",
                "restore the skill or rediscover the catalog",
            )
        })?;
        let current = validate(&text, &self.directory_name, &self.path)?;
        if current.name != self.name {
            return Err(Diagnostic::new(
                "bee::skills::metadata",
                &self.path,
                "skill identity changed",
                "restore its name or start a new turn",
            ));
        }
        Ok(text)
    }
}

pub(crate) async fn discover(
    project: &Path,
    home: Option<&Path>,
    settings: &Settings,
) -> (Vec<Skill>, Vec<Diagnostic>) {
    let mut roots = vec![(project.join(".agents/skills"), None)];
    roots.extend(
        settings
            .roots
            .iter()
            .enumerate()
            .map(|(i, p)| (p.clone(), Some(i))),
    );
    if let Some(home) = home {
        roots.push((home.join(".agents/skills"), None));
    }
    let mut skills = BTreeMap::new();
    let mut identities = BTreeSet::new();
    let mut diagnostics = Vec::new();
    let mut count = 0;
    for (root, configured) in roots {
        let root_failure = |reason| {
            let mut diagnostic = Diagnostic::new(
                "bee::skills::discovery",
                if configured.is_some() {
                    Path::new(&settings.origin)
                } else {
                    &root
                },
                reason,
                "check the skill search directory and its permissions (configured entries use skills.extra_roots)",
            );
            diagnostic.root_index = configured;
            diagnostic
        };
        let entries = match std::fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && configured.is_none() => continue,
            Err(_) => {
                diagnostics.push(root_failure("cannot enumerate skill root"));
                continue;
            }
        };
        let mut paths = Vec::new();
        for entry in entries {
            tokio::task::yield_now().await;
            count += 1;
            if count > 4096 {
                diagnostics.push(root_failure("discovery exceeds 4096 candidates"));
                return (Vec::new(), diagnostics);
            }
            match entry {
                Ok(e) => paths.push(e.path()),
                Err(_) => diagnostics.push(root_failure("cannot inspect directory entry")),
            }
        }
        paths.sort();
        for directory in paths {
            tokio::task::yield_now().await;
            let path = directory.join("SKILL.md");
            if !path.exists() {
                continue;
            }
            let candidate: Result<Option<Skill>, Diagnostic> = (|| {
                let canonical = path
                    .canonicalize()
                    .map_err(|_| root_failure("cannot resolve skill source"))?;
                if identities.contains(&canonical) {
                    return Ok(None);
                }
                let text = read_source(&canonical)?
                    .ok_or_else(|| root_failure("skill disappeared during discovery"))?;
                let name = directory.file_name().and_then(|s| s.to_str()).unwrap_or("");
                let mut skill = validate(&text, name, &canonical)?;
                if let Some(index) = configured {
                    skill.diagnostic_source = Some((settings.origin.clone().into(), index));
                }
                identities.insert(canonical);
                Ok(Some(skill))
            })();
            match candidate {
                Ok(Some(skill)) => {
                    skills.entry(skill.name.clone()).or_insert(skill);
                }
                Ok(None) => {}
                Err(mut diagnostic) => {
                    if let Some(index) = configured {
                        diagnostic.path = settings.origin.clone().into();
                        diagnostic.root_index = Some(index);
                    }
                    diagnostics.push(diagnostic);
                }
            }
        }
    }
    (skills.into_values().collect(), diagnostics)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    fn skill(root: &Path, name: &str, description: &str) {
        write(
            &root.join(name).join("SKILL.md"),
            &format!("---\nname: {name}\ndescription: {description}\n---\nBODY"),
        );
    }

    #[tokio::test]
    async fn project_then_ordered_configured_then_user_skills_win_and_invalid_candidates_allow_fallback()
     {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let home = root.path().join("home");
        let first = root.path().join("first");
        let second = root.path().join("second");
        skill(&project.join(".agents/skills"), "shared", "project");
        skill(&first, "shared", "configured");
        skill(&first, "extra", "first");
        skill(&second, "extra", "second");
        skill(&home.join(".agents/skills"), "shared", "user");
        skill(&home.join(".agents/skills"), "personal", "user");
        skill(&home.join(".agents/skills"), "fallback", "valid");
        write(
            &project.join(".agents/skills/fallback/SKILL.md"),
            "---\nname: fallback\ndescription: 123\n---\n",
        );
        let settings = Settings {
            roots: vec![first, second],
            ..Settings::default()
        };
        let (skills, diagnostics) = discover(&project, Some(&home), &settings).await;
        assert_eq!(
            skills
                .iter()
                .map(|s| (s.name.as_str(), s.description.as_str()))
                .collect::<Vec<_>>(),
            [
                ("extra", "first"),
                ("fallback", "valid"),
                ("personal", "user"),
                ("shared", "project")
            ]
        );
        assert_eq!(diagnostics.len(), 1);
    }

    #[tokio::test]
    async fn configured_arrays_replace_and_relative_roots_use_the_winning_source_directory() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let project = root.path().join("project");
        write(
            &home.join(".config/bee/config.toml"),
            "[skills]\nextra_roots=['missing-global']\n",
        );
        write(
            &project.join(".config/bee.toml"),
            "[skills]\nextra_roots=['../shared']\n",
        );
        skill(&project.join("shared"), "extra", "found");
        let environment = crate::config::Environment::new(None, Some(home), None);
        let config = crate::config::Config::load(&environment, &project).unwrap();
        let settings = Settings::from_config(&config, root.path()).unwrap();
        assert_eq!(settings.roots.len(), 1);
        let (skills, warnings) = discover(&project, None, &settings).await;
        assert!(warnings.is_empty());
        assert_eq!(skills[0].name, "extra");
    }

    #[tokio::test]
    async fn configured_candidate_and_activation_diagnostics_never_echo_root_values() {
        let root = tempfile::tempdir().unwrap();
        let extra = root.path().join("secret-sentinel");
        skill(&extra, "valid", "works");
        write(
            &extra.join("invalid/SKILL.md"),
            "---\nname: invalid\ndescription: 123\n---\n",
        );
        let settings = Settings {
            roots: vec![extra.clone()],
            origin: "settings.toml".into(),
        };
        let (skills, warnings) = discover(&root.path().join("project"), None, &settings).await;
        assert_eq!(skills.len(), 1);
        assert_eq!(warnings.len(), 1);
        write(
            &extra.join("valid/SKILL.md"),
            "---\nname: valid\ndescription: 123\n---\n",
        );
        let error = skills[0].snapshot().unwrap_err();
        for diagnostic in [&warnings[0], &error] {
            let rendered = format!(
                "{diagnostic} {diagnostic:?} {:?}",
                miette::Report::new(diagnostic.clone())
            );
            assert!(!rendered.contains("secret-sentinel"));
            assert!(rendered.contains("settings.toml"));
            assert!(rendered.contains("skills.extra_roots[0]"));
        }
    }

    #[test]
    fn unknown_settings_blame_their_own_layer_instead_of_an_inherited_root_array() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let project = root.path().join("project");
        write(
            &home.join(".config/bee/config.toml"),
            "[skills]\nextra_roots=[]\n",
        );
        let path = project.join(".config/bee.toml");
        write(&path, "[skills]\nextra_root=[]\n");
        let config = crate::config::Config::load(
            &crate::config::Environment::new(None, Some(home), None),
            &project,
        )
        .unwrap();
        let error = Settings::from_config(&config, root.path()).err().unwrap();
        match error {
            crate::Error::SkillsSetting { path: source, .. } => {
                assert_eq!(Path::new(&source), path)
            }
            _ => panic!("expected a typed skills setting diagnostic"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn linked_skill_roots_are_deduplicated_by_canonical_identity() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let real = root.path().join("real");
        skill(&real, "shared", "one");
        std::fs::create_dir_all(project.join(".agents/skills")).unwrap();
        std::os::unix::fs::symlink(real.join("shared"), project.join(".agents/skills/shared"))
            .unwrap();
        let settings = Settings {
            roots: vec![real],
            ..Settings::default()
        };
        let (skills, warnings) = discover(&project, None, &settings).await;
        assert!(warnings.is_empty());
        assert_eq!(skills.len(), 1);
    }
}
