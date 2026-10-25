//! Scoped project instructions and transactional, progressively loaded skills.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};

/// Maximum bytes in one instruction or activated skill source.
pub const SOURCE_LIMIT: usize = 256 * 1024;
/// Maximum bytes in the composed non-evictable instruction payload.
pub const TOTAL_LIMIT: usize = 1024 * 1024;

/// A content-free discovery failure suitable for diagnostics and ACP notices.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    /// Stable diagnostic identifier.
    pub code: &'static str,
    /// Source path, never a source excerpt.
    pub path: PathBuf,
    /// Safe static classification.
    pub reason: &'static str,
    /// Repair advice.
    pub help: &'static str,
    /// Optional one-based line and column.
    pub position: Option<(usize, usize)>,
    /// Configured root index when the actual path is a protected configuration value.
    pub root_index: Option<usize>,
}

impl std::fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.path.display())?;
        if let Some(index) = self.root_index {
            write!(f, " (skills.extra_roots[{index}])")?;
        }
        write!(f, ": {}; {}", self.reason, self.help)
    }
}

impl std::error::Error for Diagnostic {}

impl miette::Diagnostic for Diagnostic {
    fn code<'a>(&'a self) -> Option<Box<dyn std::fmt::Display + 'a>> {
        Some(Box::new(self.code))
    }

    fn help<'a>(&'a self) -> Option<Box<dyn std::fmt::Display + 'a>> {
        Some(Box::new(self.help))
    }
}

impl Diagnostic {
    pub(crate) fn new(
        code: &'static str,
        path: &Path,
        reason: &'static str,
        help: &'static str,
    ) -> Self {
        Self {
            code,
            path: path.into(),
            reason,
            help,
            position: None,
            root_index: None,
        }
    }
}

pub(crate) fn read_source(path: &Path) -> Result<Option<String>, Diagnostic> {
    let failure = || {
        Diagnostic::new(
            "bee::instructions::read",
            path,
            "expected a readable regular UTF-8 file of at most 256 KiB without NUL bytes",
            "fix permissions, encoding or source size before continuing",
        )
    };
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(failure()),
    };
    // Reject special files before opening; a FIFO must not stall the agent loop.
    if !metadata.is_file() || metadata.len() > SOURCE_LIMIT as u64 {
        return Err(failure());
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .map_err(|_| failure())?
        .take((SOURCE_LIMIT + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| failure())?;
    if bytes.len() > SOURCE_LIMIT || bytes.contains(&0) {
        return Err(failure());
    }
    String::from_utf8(bytes).map(Some).map_err(|_| failure())
}

/// Immutable, explicitly injected discovery inputs for an ACP connection.
#[derive(Clone, Default)]
pub struct Sources {
    home: Option<PathBuf>,
    skills: crate::skills::Settings,
}

impl Sources {
    /// Discover defaults with an explicit home, without reading process environment.
    pub fn new(home: Option<PathBuf>) -> Self {
        Self {
            home,
            ..Self::default()
        }
    }

    /// Resolve additional roots from the existing launch-time layered config.
    pub fn from_config(
        config: &crate::config::Config,
        environment: &crate::config::Environment,
        launch: &Path,
    ) -> crate::Result<Self> {
        Ok(Self {
            home: environment.home_dir().map(|p| launch.join(p)),
            skills: crate::skills::Settings::from_config(config, launch)?,
        })
    }

    pub(crate) async fn start(&self, cwd: &Path, previous: State) -> Result<Turn, Diagnostic> {
        let cwd = cwd.canonicalize().map_err(|_| {
            Diagnostic::new(
                "bee::instructions::root",
                cwd,
                "cannot resolve session directory",
                "use an existing accessible session directory",
            )
        })?;
        let root = cwd
            .ancestors()
            .find(|p| p.join(".git").exists())
            .unwrap_or(&cwd)
            .to_path_buf();
        let (catalog, mut diagnostics) =
            crate::skills::discover(&root, self.home.as_deref(), &self.skills).await;
        // Repeated aliases or inaccessible roots should not exhaust the notice budget.
        let mut seen = std::collections::BTreeSet::new();
        diagnostics
            .retain(|d| seen.insert((d.code, d.path.clone(), d.reason, d.position, d.root_index)));
        let user = self
            .home
            .as_deref()
            .map(|home| read_source(&home.join(".agents/AGENTS.md")))
            .transpose()?
            .flatten();
        let mut turn = Turn {
            root,
            cwd: cwd.clone(),
            user,
            catalog,
            state: previous,
            diagnostics,
            observed: BTreeSet::new(),
        };
        // Scoped files are refreshed each turn; skill snapshots remain intentionally stable.
        turn.state.scopes.clear();
        turn.scope(&cwd).await?;
        turn.render()?;
        Ok(turn)
    }
}

/// Committed semantic guidance, independent of evictable conversation messages.
#[derive(Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct State {
    scopes: BTreeMap<PathBuf, String>,
    active: BTreeMap<PathBuf, String>,
}

/// Journal only changed guidance; unchanged AGENTS/skill payloads must not be
/// duplicated on disk after every successful turn.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct Changes {
    scopes: BTreeMap<PathBuf, Option<String>>,
    active: BTreeMap<PathBuf, Option<String>>,
}

impl State {
    pub(crate) fn changes_from(&self, previous: &Self) -> Changes {
        fn changed(
            current: &BTreeMap<PathBuf, String>,
            previous: &BTreeMap<PathBuf, String>,
        ) -> BTreeMap<PathBuf, Option<String>> {
            current
                .keys()
                .chain(previous.keys())
                .filter(|path| current.get(*path) != previous.get(*path))
                .map(|path| (path.clone(), current.get(path).cloned()))
                .collect()
        }
        Changes {
            scopes: changed(&self.scopes, &previous.scopes),
            active: changed(&self.active, &previous.active),
        }
    }

    pub(crate) fn apply(&mut self, changes: Changes) {
        for (target, changes) in [
            (&mut self.scopes, changes.scopes),
            (&mut self.active, changes.active),
        ] {
            for (path, content) in changes {
                if let Some(content) = content {
                    target.insert(path, content);
                } else {
                    target.remove(&path);
                }
            }
        }
    }
}

impl std::fmt::Debug for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("InstructionState([redacted])")
    }
}

pub(crate) struct Turn {
    root: PathBuf,
    cwd: PathBuf,
    user: Option<String>,
    catalog: Vec<crate::skills::Skill>,
    pub(crate) state: State,
    pub(crate) diagnostics: Vec<Diagnostic>,
    // Remember paths even when no file exists yet: a tool can create their AGENTS.md.
    observed: BTreeSet<PathBuf>,
}

impl Turn {
    pub(crate) async fn refresh(&mut self) -> Result<bool, Diagnostic> {
        let directories: Vec<_> = self.observed.iter().cloned().collect();
        let mut changed = false;
        for directory in directories {
            changed |= self.scope(&directory).await?;
        }
        Ok(changed)
    }

    pub(crate) fn render(&self) -> Result<String, Diagnostic> {
        let mut text = String::from(
            "You are Bee, a coding agent. Follow explicit user requests and applicable project instructions. File-based instructions supplement this system guidance. Closest AGENTS.md wins conflicts within its subtree; unrelated sibling rules do not apply. Before using bash or custom tools on new paths, read their applicable AGENTS.md files. Use a skill when its description matches the task: read its SKILL.md first, then load supporting files only as needed. Skill resource paths are relative to the skill directory, not cwd.\n",
        );
        if let Some(user) = &self.user {
            text.push_str(&format!("\n## Additional user instructions\n{user}\n"));
        }
        for (directory, body) in &self.state.scopes {
            text.push_str(&format!("\n## Scoped instructions: {}\nApplies ONLY to this directory and its descendants; deeper files win conflicts.\n{body}\n", directory.display()));
        }
        text.push_str("\n## Available skills (metadata only)\n");
        for skill in &self.catalog {
            // JSON quoting keeps arbitrary descriptions from breaking the catalog structure.
            text.push_str(&format!("{}\n", serde_json::json!({"name":skill.name,"description":skill.description,"location":skill.path})));
        }
        for (path, body) in &self.state.active {
            text.push_str(&format!(
                "\n## Activated skill: {}\nResolve resources relative to {}.\n{body}\n",
                path.display(),
                path.parent().unwrap_or(&self.cwd).display()
            ));
        }
        if text.len() > TOTAL_LIMIT {
            return Err(Diagnostic::new(
                "bee::instructions::limit",
                &self.root,
                "persistent guidance exceeds 1 MiB",
                "reduce instructions/skills or start a new session with fewer activations",
            ));
        }
        Ok(text)
    }

    pub(crate) async fn scope(&mut self, directory: &Path) -> Result<bool, Diagnostic> {
        let Some(directory) = resolve(directory) else {
            return Ok(false);
        };
        if !directory.starts_with(&self.root) {
            return Ok(false);
        }
        self.observed.insert(directory.clone());
        let mut chain: Vec<_> = directory
            .ancestors()
            .take_while(|p| p.starts_with(&self.root))
            .collect();
        chain.reverse();
        let mut changed = false;
        for directory in chain {
            tokio::task::yield_now().await;
            let path = directory.join("AGENTS.md");
            if let Ok(identity) = path.canonicalize()
                && !identity.starts_with(&self.root)
            {
                return Err(Diagnostic::new(
                    "bee::instructions::scope",
                    &path,
                    "instruction symlink leaves the project",
                    "keep the AGENTS.md source within the project boundary",
                ));
            }
            match read_source(&path)? {
                Some(body) => {
                    if self.state.scopes.get(directory) != Some(&body) {
                        self.state.scopes.insert(directory.into(), body);
                        changed = true;
                    }
                }
                None => {
                    changed |= self.state.scopes.remove(directory).is_some();
                }
            }
        }
        self.render()?;
        Ok(changed)
    }

    pub(crate) async fn preflight(
        &mut self,
        target: &crate::tool::Target,
    ) -> Result<bool, Diagnostic> {
        let path = self.cwd.join(&target.path);
        let path = resolve(&path).unwrap_or(path);
        let directory = if target.directory {
            path
        } else {
            path.parent().unwrap_or(&self.cwd).to_path_buf()
        };
        self.scope(&directory).await
    }

    pub(crate) fn skill(&self, target: &crate::tool::Target) -> Option<crate::skills::Skill> {
        if !target.read {
            return None;
        }
        let path = self.cwd.join(&target.path).canonicalize().ok()?;
        self.catalog
            .iter()
            .find(|skill| skill.path == path)
            .cloned()
    }

    pub(crate) fn activate(
        &mut self,
        skill: &crate::skills::Skill,
        body: String,
    ) -> Result<bool, Diagnostic> {
        let changed = self.state.active.get(&skill.path) != Some(&body);
        self.state.active.insert(skill.path.clone(), body);
        self.render()?;
        Ok(changed)
    }
}

// Canonicalize existing ancestors before appending missing components; lexical
// `..` normalization before resolving symlinks would describe the wrong target.
fn resolve(path: &Path) -> Option<PathBuf> {
    if let Ok(path) = path.canonicalize() {
        return Some(path);
    }
    let parent = resolve(path.parent()?)?;
    Some(parent.join(path.file_name()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, path: &str, body: &str) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    #[tokio::test]
    async fn root_and_user_instructions_are_additive_and_nested_rules_remain_scoped() {
        let root = tempfile::tempdir().unwrap();
        write(root.path(), "project/.git", "gitdir: unused");
        write(root.path(), "project/AGENTS.md", "ROOT-RULE");
        write(root.path(), "home/.agents/AGENTS.md", "USER-RULE");
        write(root.path(), "project/a/AGENTS.md", "A-RULE");
        write(root.path(), "project/b/AGENTS.md", "B-RULE");
        let sources = Sources::new(Some(root.path().join("home")));
        let mut turn = sources
            .start(&root.path().join("project/a"), State::default())
            .await
            .unwrap();
        let rendered = turn.render().unwrap();
        assert!(rendered.contains("You are Bee"));
        assert!(rendered.contains("USER-RULE"));
        assert!(rendered.find("ROOT-RULE").unwrap() < rendered.find("A-RULE").unwrap());
        assert!(!rendered.contains("B-RULE"));
        let target = crate::tool::Target {
            path: "../b/new.txt".into(),
            directory: false,
            read: false,
        };
        assert!(turn.preflight(&target).await.unwrap());
        assert!(!turn.preflight(&target).await.unwrap());
        let rendered = turn.render().unwrap();
        assert!(rendered.contains("B-RULE"));
        assert_eq!(rendered.matches("Applies ONLY").count(), 3);
    }

    #[tokio::test]
    async fn external_paths_do_not_import_unrelated_rules_and_non_git_projects_use_cwd() {
        let root = tempfile::tempdir().unwrap();
        write(root.path(), "AGENTS.md", "ANCESTOR-RULE");
        write(root.path(), "project/AGENTS.md", "PROJECT-RULE");
        write(root.path(), "external/AGENTS.md", "EXTERNAL-RULE");
        let mut turn = Sources::default()
            .start(&root.path().join("project"), State::default())
            .await
            .unwrap();
        let target = crate::tool::Target {
            path: root.path().join("external/file"),
            directory: false,
            read: true,
        };
        assert!(!turn.preflight(&target).await.unwrap());
        let rendered = turn.render().unwrap();
        assert!(!rendered.contains("ANCESTOR-RULE"));
        assert!(!rendered.contains("EXTERNAL-RULE"));
    }

    #[tokio::test]
    async fn skill_activation_retains_a_complete_snapshot_separately_from_history() {
        let root = tempfile::tempdir().unwrap();
        write(
            root.path(),
            ".agents/skills/demo/SKILL.md",
            "---\nname: demo\ndescription: metadata\n---\nPRIVATE-BODY\n",
        );
        write(
            root.path(),
            ".agents/skills/demo/references/doc.md",
            "support",
        );
        let sources = Sources::default();
        let mut turn = sources.start(root.path(), State::default()).await.unwrap();
        assert!(!turn.render().unwrap().contains("PRIVATE-BODY"));
        let skill = turn.catalog[0].clone();
        turn.activate(&skill, skill.snapshot().unwrap()).unwrap();
        let next = sources.start(root.path(), turn.state).await.unwrap();
        assert!(next.render().unwrap().contains("PRIVATE-BODY"));
        assert!(!format!("{:?}", next.state).contains("PRIVATE-BODY"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinked_targets_use_effective_scope_and_external_instruction_links_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        write(root.path(), "project/real/AGENTS.md", "REAL-RULE");
        write(root.path(), "external/AGENTS.md", "EXTERNAL-RULE");
        std::os::unix::fs::symlink(
            root.path().join("project/real"),
            root.path().join("project/alias"),
        )
        .unwrap();
        let mut turn = Sources::default()
            .start(&root.path().join("project"), State::default())
            .await
            .unwrap();
        let target = crate::tool::Target {
            path: "alias/new.txt".into(),
            directory: false,
            read: false,
        };
        assert!(turn.preflight(&target).await.unwrap());
        assert!(turn.render().unwrap().contains("REAL-RULE"));
        std::fs::remove_file(root.path().join("project/real/AGENTS.md")).unwrap();
        std::os::unix::fs::symlink(
            root.path().join("external/AGENTS.md"),
            root.path().join("project/real/AGENTS.md"),
        )
        .unwrap();
        assert_eq!(
            turn.preflight(&target).await.unwrap_err().code,
            "bee::instructions::scope"
        );
    }

    #[tokio::test]
    async fn oversized_applicable_instructions_fail_instead_of_being_silently_truncated() {
        let root = tempfile::tempdir().unwrap();
        write(root.path(), "AGENTS.md", &"x".repeat(SOURCE_LIMIT + 1));
        assert!(
            Sources::default()
                .start(root.path(), State::default())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn aggregate_activated_guidance_cannot_silently_overflow_its_persistent_budget() {
        let root = tempfile::tempdir().unwrap();
        for name in ["first", "second", "third", "fourth"] {
            write(
                root.path(),
                &format!(".agents/skills/{name}/SKILL.md"),
                &format!(
                    "---\nname: {name}\ndescription: bounded metadata\n---\n{}",
                    "x".repeat(SOURCE_LIMIT - 100)
                ),
            );
        }
        let mut turn = Sources::default()
            .start(root.path(), State::default())
            .await
            .unwrap();
        let catalog = turn.catalog.clone();
        for skill in &catalog[..3] {
            turn.activate(skill, skill.snapshot().unwrap()).unwrap();
        }
        let error = turn
            .activate(&catalog[3], catalog[3].snapshot().unwrap())
            .unwrap_err();
        assert_eq!(error.code, "bee::instructions::limit");
        assert!(error.help.contains("reduce instructions"));
    }
}
