//! Bounded, side-effect-free project MCP discovery and normalization.

use super::Error;
use crate::config::Environment;
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    io::Read,
    path::{Path, PathBuf},
};

/// A frozen effective launch or connection definition. Debug never prints values.
#[derive(Clone, PartialEq, Eq)]
pub enum Transport {
    /// Executed directly, never through a shell.
    Stdio {
        /// Resolved executable path.
        command: PathBuf,
        /// Literal argv, without shell expansion.
        args: Vec<String>,
        /// Child working directory.
        cwd: PathBuf,
        /// Complete child environment, including the minimal baseline.
        env: BTreeMap<String, String>,
    },
    /// Streamable HTTP; credentials are private headers.
    Http {
        /// Validated endpoint; never shown in diagnostics.
        url: String,
        /// Private explicit headers.
        headers: BTreeMap<String, String>,
    },
}

impl std::fmt::Debug for Transport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Transport([redacted])")
    }
}

/// Stable project identity, independently sourced from its executable settings.
#[derive(Clone)]
pub struct Definition {
    /// Explicit key in the servers envelope.
    pub id: String,
    /// Canonical project boundary.
    pub root: PathBuf,
    /// Source used for actionable diagnostics.
    pub source: PathBuf,
    /// Private effective settings.
    pub transport: Transport,
}

impl std::fmt::Debug for Definition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Definition([redacted])")
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Setting {
    Literal(String),
    Reference(Reference),
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reference {
    env: String,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    servers: BTreeMap<String, Raw>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    command: Option<Vec<String>>,
    cwd: Option<PathBuf>,
    #[serde(default)]
    env: BTreeMap<String, Setting>,
    url: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, Setting>,
}

// Claude compatibility keeps its published executable/args fields at the edge;
// canonical definitions use a single argv and infer transport without a tag.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClaudeRaw {
    #[serde(rename = "type")]
    kind: Option<String>,
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    cwd: Option<PathBuf>,
    #[serde(default)]
    env: BTreeMap<String, Setting>,
    url: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, Setting>,
}

impl ClaudeRaw {
    fn into_raw(self, source: &Path) -> Result<Raw, Error> {
        let inferred = match (self.command.is_some(), self.url.is_some()) {
            (true, false) => "stdio",
            (false, true) if self.args.is_empty() => "http",
            _ => {
                return Err(error(
                    source,
                    "invalid Claude transport; specify only command or url, with compatible fields",
                ));
            }
        };
        if self.kind.as_deref().is_some_and(|kind| kind != inferred) {
            return Err(error(
                source,
                "invalid Claude transport type; match stdio to command or http to url",
            ));
        }
        Ok(Raw {
            command: self
                .command
                .map(|command| std::iter::once(command).chain(self.args).collect()),
            cwd: self.cwd,
            env: self.env,
            url: self.url,
            headers: self.headers,
        })
    }
}

fn error(path: &Path, reason: &'static str) -> Error {
    Error {
        path: path.into(),
        reason,
    }
}

/// The explicit minimal environment passed to project and client MCP children.
pub(crate) fn baseline(environment: &Environment) -> BTreeMap<String, String> {
    [
        "PATH",
        "HOME",
        "USERPROFILE",
        "TMPDIR",
        "TMP",
        "TEMP",
        "SystemRoot",
        "WINDIR",
        "COMSPEC",
        "PATHEXT",
    ]
    .into_iter()
    .filter_map(|k| environment.variable(k).map(|v| (k.into(), v)))
    .collect()
}

fn settings(
    values: BTreeMap<String, Setting>,
    environment: &Environment,
    path: &Path,
) -> Result<BTreeMap<String, String>, Error> {
    values.into_iter().map(|(key, value)| {
        let value = match value {
            Setting::Literal(value) => value,
            Setting::Reference(reference) => environment.variable(&reference.env)
                .ok_or_else(|| error(path, "an environment reference is unset or empty; set the referenced variable"))?,
        };
        if key.is_empty() || key.contains(['=', '\0']) || value.contains('\0') {
            return Err(error(path, "invalid environment/header entry; use nonempty names and values without NUL"));
        }
        Ok((key, value))
    }).collect()
}

/// Validate HTTP authority and headers without contacting the endpoint.
pub(crate) fn validate_http(
    url: &str,
    headers: &BTreeMap<String, String>,
) -> Result<(), &'static str> {
    let url = reqwest::Url::parse(url).map_err(|_| "invalid MCP URL; use an absolute HTTPS URL")?;
    let loopback = url.host_str().is_some_and(|h| {
        h == "localhost"
            || h.trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    if !(url.scheme() == "https" || url.scheme() == "http" && loopback)
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err("MCP requires HTTPS (or loopback HTTP), without URL credentials or fragments");
    }
    for (name, value) in headers {
        let name = http::HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| "invalid MCP header name; use HTTP header syntax")?;
        http::HeaderValue::from_str(value)
            .map_err(|_| "invalid MCP header value; remove control characters")?;
        if [
            "host",
            "accept",
            "content-type",
            "content-length",
            "connection",
            "transfer-encoding",
            "mcp-session-id",
            "mcp-protocol-version",
            "last-event-id",
        ]
        .contains(&name.as_str())
        {
            return Err("reserved MCP header; remove protocol and routing header overrides");
        }
    }
    Ok(())
}

pub(crate) fn executable(
    command: &str,
    cwd: &Path,
    environment: &Environment,
) -> Result<PathBuf, &'static str> {
    if command.is_empty() || command.contains('\0') {
        return Err("invalid executable; specify an executable path or PATH name");
    }
    let command = Path::new(command);
    let path = if command.is_absolute() || command.components().count() > 1 {
        cwd.join(command)
    } else {
        environment
            .variable("PATH")
            .and_then(|path| {
                std::env::split_paths(&path)
                    .flat_map(|p| {
                        let path = cwd.join(p).join(command);
                        let mut candidates = vec![path.clone()];
                        if cfg!(windows) && command.extension().is_none() {
                            for ext in environment
                                .variable("PATHEXT")
                                .unwrap_or_else(|| ".COM;.EXE".into())
                                .split(';')
                            {
                                candidates.push(path.with_extension(ext.trim_start_matches('.')));
                            }
                        }
                        candidates
                    })
                    .find(|p| is_executable(p))
            })
            .ok_or("MCP executable was not found; install it or configure its absolute path")?
    };
    if !is_executable(&path) {
        return Err("MCP executable is not an executable file; fix its path or permissions");
    }
    path.canonicalize()
        .map_err(|_| "MCP executable is inaccessible; install it or correct its path")
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn normalize(
    raw: Raw,
    root: &Path,
    environment: &Environment,
    source: &Path,
) -> Result<Transport, Error> {
    match (raw.command, raw.url) {
        (Some(argv), None) if raw.headers.is_empty() => {
            let (executable_name, args) = argv.split_first().ok_or_else(|| {
                error(
                    source,
                    "empty stdio command; put the executable first, followed by its arguments",
                )
            })?;
            let cwd = root
                .join(raw.cwd.unwrap_or_default())
                .canonicalize()
                .map_err(|_| error(source, "MCP cwd is inaccessible; use an existing directory"))?;
            if !cwd.is_dir() || args.iter().any(|a| a.contains('\0')) {
                return Err(error(
                    source,
                    "invalid MCP cwd or arguments; use a directory and NUL-free arguments",
                ));
            }
            let command = executable(executable_name, root, environment)
                .map_err(|reason| error(source, reason))?;
            let mut env = baseline(environment);
            env.extend(settings(raw.env, environment, source)?);
            Ok(Transport::Stdio {
                command,
                args: args.to_vec(),
                cwd,
                env,
            })
        }
        (None, Some(url)) if raw.cwd.is_none() && raw.env.is_empty() => {
            let headers = settings(raw.headers, environment, source)?;
            validate_http(&url, &headers).map_err(|reason| error(source, reason))?;
            let mut normalized = BTreeMap::new();
            for (name, value) in headers {
                if normalized
                    .insert(name.to_ascii_lowercase(), value)
                    .is_some()
                {
                    return Err(error(
                        source,
                        "duplicate HTTP header; declare each case-insensitive name once",
                    ));
                }
            }
            Ok(Transport::Http {
                url,
                headers: normalized,
            })
        }
        _ => Err(error(
            source,
            "invalid MCP definition; specify exactly one command array or url, with only compatible fields",
        )),
    }
}

// Serde's ordinary JSON Value silently overwrites duplicate keys. Reject them at
// every depth before normalizing credentials or selecting server identifiers.
struct Unique(Value);
impl<'de> Deserialize<'de> for Unique {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Unique;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("unique JSON values")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(self, mut m: M) -> Result<Unique, M::Error> {
                let mut values = serde_json::Map::new();
                while let Some((k, v)) = m.next_entry::<String, Unique>()? {
                    if values.insert(k, v.0).is_some() {
                        return Err(serde::de::Error::custom("duplicate key"));
                    }
                }
                Ok(Unique(Value::Object(values)))
            }
            fn visit_seq<M: serde::de::SeqAccess<'de>>(self, mut m: M) -> Result<Unique, M::Error> {
                let mut values = Vec::new();
                while let Some(v) = m.next_element::<Unique>()? {
                    values.push(v.0);
                }
                Ok(Unique(Value::Array(values)))
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Unique, E> {
                Ok(Unique(Value::String(v.into())))
            }
            fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Unique, E> {
                Ok(Unique(Value::Bool(v)))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Unique, E> {
                Ok(Unique(v.into()))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Unique, E> {
                Ok(Unique(v.into()))
            }
            fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Unique, E> {
                Ok(Unique(serde_json::json!(v)))
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Unique, E> {
                Ok(Unique(Value::Null))
            }
        }
        d.deserialize_any(Visitor)
    }
}

fn read(path: &Path, root: &Path) -> Result<String, Error> {
    let canonical = path
        .canonicalize()
        .map_err(|_| error(path, "cannot resolve MCP source; check access and symlinks"))?;
    if !canonical.starts_with(root) {
        return Err(error(
            path,
            "MCP source escapes the project root; move it inside the project",
        ));
    }
    let metadata = std::fs::metadata(&canonical)
        .map_err(|_| error(path, "cannot inspect MCP source; check file access"))?;
    if !metadata.is_file() || metadata.len() > super::MAX_BYTES as u64 {
        return Err(error(
            path,
            "MCP source must be a regular file of at most 1 MiB",
        ));
    }
    let mut content = String::new();
    std::fs::File::open(canonical)
        .and_then(|file| {
            file.take(super::MAX_BYTES as u64 + 1)
                .read_to_string(&mut content)
        })
        .map_err(|_| {
            error(
                path,
                "cannot read MCP source as UTF-8; check access and encoding",
            )
        })?;
    if content.len() > super::MAX_BYTES {
        return Err(error(
            path,
            "MCP source exceeds 1 MiB; simplify definitions",
        ));
    }
    Ok(content)
}

fn present(path: &Path) -> Result<bool, Error> {
    // Broken symlinks are present invalid configuration, never fallback authority.
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(error(path, "cannot inspect MCP source; check access")),
    }
}

/// Discover definitions without spawning a process or opening a network socket.
///
/// # Errors
/// Returns only source locators and value-free repair guidance.
pub fn discover(cwd: &Path, environment: &Environment) -> Result<Vec<Definition>, Error> {
    let root = super::project_root(cwd).map_err(|_| {
        error(
            cwd,
            "cannot resolve project root; use an accessible session directory",
        )
    })?;
    let canonical = root.join(".agents/mcp.toml");
    // One canonical source makes fallback independent of unrelated resource files.
    let claude = !present(&canonical)?;
    let path = if claude {
        root.join(".mcp.json")
    } else {
        canonical
    };
    if !present(&path)? {
        return Ok(Vec::new());
    }
    let mut definitions = Vec::new();
    let content = read(&path, &root)?;
    let servers = if claude {
        let Unique(mut value) = serde_json::from_str(&content).map_err(|_| {
            error(
                &path,
                "invalid Claude JSON or duplicate keys; use a unique mcpServers object",
            )
        })?;
        if value
            .as_object()
            .is_none_or(|v| v.len() != 1 || !v.contains_key("mcpServers"))
        {
            return Err(error(
                &path,
                "Claude fallback requires only a mcpServers envelope",
            ));
        }
        let servers =
            serde_json::from_value::<BTreeMap<String, ClaudeRaw>>(value["mcpServers"].take())
                .map_err(|_| {
                    error(
                        &path,
                        "invalid Claude server fields; use supported stdio or HTTP settings",
                    )
                })?;
        servers
            .into_iter()
            .map(|(id, raw)| raw.into_raw(&path).map(|raw| (id, raw)))
            .collect::<Result<BTreeMap<_, _>, _>>()?
    } else {
        toml::from_str::<File>(&content)
                .map_err(|_| {
                    error(
                        &path,
                         "invalid or duplicate MCP TOML definition; use unique servers tables with command arrays or URLs, without type or args fields",
                    )
                })?
                .servers
    };
    for (id, raw) in servers {
        if id.is_empty()
            || id.len() > 128
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        {
            return Err(error(
                &path,
                "invalid server identifier; use 1-128 ASCII letters, digits, dots, underscores or hyphens",
            ));
        }
        let transport = normalize(raw, &root, environment, &path)?;
        definitions.push(Definition {
            id,
            root: root.clone(),
            source: path.clone(),
            transport,
        });
        if definitions.len() > 32 {
            return Err(error(&path, "too many MCP servers; configure at most 32"));
        }
    }
    Ok(definitions)
}
