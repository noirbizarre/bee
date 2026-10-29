//! Composed client-hosted and explicitly authorized direct MCP tools.

use std::collections::{BTreeMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use agent_client_protocol::schema::v2;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::agent::{Executor, ToolResult};
use crate::llm::{Call, Tool};

const MAX_SERVERS: usize = 32;
const MAX_PAGES: usize = 64;
const MAX_TOOLS: usize = 256;
const MAX_CATALOG_BYTES: usize = 1024 * 1024;
const MAX_DEPTH: usize = 32;
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(30);
const CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// Only the routing fields are retained; arbitrary declaration metadata is not state.
#[derive(Clone)]
pub(super) struct Server {
    id: v2::McpServerAcpId,
    name: String,
    direct: Option<crate::mcp::config::Transport>,
}

/// This map belongs to one connection, never to a global server registry.
#[derive(Default)]
pub(super) struct Bindings {
    pub sessions: BTreeMap<String, Vec<Server>>,
    pub next_request: u64,
    pub direct: std::sync::Arc<crate::mcp::runtime::Manager>,
}

impl Bindings {
    pub fn request_id(&mut self) -> Result<String, &'static str> {
        // Refuse wraparound rather than reuse an identifier still active at a peer.
        let id = self.next_request;
        self.next_request = id
            .checked_add(1)
            .ok_or("MCP request identifiers exhausted; reconnect the client")?;
        Ok(format!("bee-mcp-{id}"))
    }
}

/// Freeze project settings and reconcile ownership before discovering any tools.
pub(super) async fn prepare(
    reply: &super::Reply,
    runtime: &super::Runtime,
    cwd: &std::path::Path,
) -> Result<Vec<Server>, super::TurnFailure> {
    let manager = super::lock(&reply.mcp).direct.clone();
    let mut servers = reply.servers.clone();
    if let Some(environment) = &runtime.project_mcp {
        let discovered = crate::mcp::config::discover(cwd, environment);
        let definitions = match discovered {
            Ok(definitions) => definitions,
            Err(error) => {
                manager.finish(&reply.id, true).await;
                return Err(super::TurnFailure::new(error.to_string()));
            }
        };
        for definition in definitions {
            let id = serde_json::to_string(&("project", definition.root, &definition.id))
                .expect("identity is JSON");
            servers.push(Server {
                id: id.into(),
                name: format!("project/{}", definition.id),
                direct: Some(definition.transport),
            });
        }
    }
    if servers.len() > MAX_SERVERS {
        return Err(super::TurnFailure::new(
            "too many combined MCP servers; configure at most 32",
        ));
    }
    let definitions = servers
        .iter()
        .filter_map(|server| {
            server
                .direct
                .clone()
                .map(|definition| (server.id.to_string(), definition))
        })
        .collect();
    manager.reconcile(&reply.id, &definitions).await;
    for server in &servers {
        let Some(definition) = &server.direct else {
            continue;
        };
        let entry = manager
            .get(&reply.id, &server.id.to_string())
            .ok_or_else(|| super::TurnFailure::new(crate::mcp::runtime::UNAVAILABLE))?;
        if entry.ready().await {
            continue;
        }
        let (action, transport, target) = match definition {
            crate::mcp::config::Transport::Stdio { command, .. } => (
                "mcp.launch",
                "stdio",
                command
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
            ),
            crate::mcp::config::Transport::Http { url, .. } => (
                "mcp.connect",
                "http",
                reqwest::Url::parse(url)
                    .map(|u| u.origin().ascii_serialization())
                    .unwrap_or_default(),
            ),
        };
        let check = crate::permissions::Check::value(
            action,
            server.name.clone(),
            crate::permissions::Effect::Ask,
        );
        let allowed = match runtime.permissions.evaluate(&[check], cwd) {
            crate::permissions::Effect::Allow => true,
            crate::permissions::Effect::Deny => false,
            crate::permissions::Effect::Ask => {
                let subject = serde_json::from_value(json!({"type":"_bee_mcp_connection", "server":server.name, "transport":transport, "target":target, "revision":entry.revision, "scope":"session"}))
                    .map_err(|_| super::TurnFailure::new("cannot construct MCP permission subject; report this failure"))?;
                reply
                    .permission(
                        subject,
                        format!("Connect MCP server {} and discover its tools", server.name),
                    )
                    .await
                    .map_err(|_| {
                        super::TurnFailure::new(
                            "MCP connection permission cancelled; retry the prompt",
                        )
                    })?
            }
        };
        if !allowed {
            return Err(super::TurnFailure::new(
                "MCP connection permission denied; approve the server or remove its definition",
            ));
        }
        if !reply.current() || manager.is_closing(&reply.id) {
            return Err(super::TurnFailure::new(
                "MCP connection cancelled; retry the prompt",
            ));
        }
        entry.connect().await.map_err(super::TurnFailure::new)?;
    }
    Ok(servers)
}

/// Replacement runs off dispatch, retaining the session mutation reservation.
pub(super) async fn replace(
    id: &str,
    servers: &[Server],
    cwd: &std::path::Path,
    project: Option<&crate::config::Environment>,
    bindings: &super::Mutex<Bindings>,
    sessions: &super::Mutex<super::Sessions>,
    tasks: &super::Tasks,
) -> Result<(), String> {
    let manager = super::lock(bindings).direct.clone();
    let mut definitions: BTreeMap<_, _> = servers
        .iter()
        .filter_map(|s| s.direct.clone().map(|t| (s.id.to_string(), t)))
        .collect();
    if let Some(environment) = project {
        match crate::mcp::config::discover(cwd, environment) {
            Ok(project) => {
                for definition in project {
                    let id = serde_json::to_string(&("project", definition.root, definition.id))
                        .expect("identity is JSON");
                    definitions.insert(id, definition.transport);
                }
            }
            Err(error) => {
                manager.closing(id);
                super::lock(sessions).cancel(id);
                let pending = super::lock(tasks).get(id).cloned();
                if let Some(pending) = pending {
                    pending.await;
                }
                manager.finish(id, true).await;
                manager.reopen(id);
                return Err(error.to_string());
            }
        }
    }
    if manager.changed(id, &definitions) {
        manager.closing(id);
        super::lock(sessions).cancel(id);
        let pending = super::lock(tasks).get(id).cloned();
        if let Some(pending) = pending {
            pending.await;
        }
        manager.reconcile(id, &definitions).await;
    }
    manager.reopen(id);
    Ok(())
}

pub(super) fn declarations(
    servers: Vec<v2::McpServer>,
    cwd: &std::path::Path,
    environment: &crate::config::Environment,
) -> Result<Vec<Server>, &'static str> {
    if servers.len() > MAX_SERVERS {
        return Err("too many MCP servers; attach at most 32 servers");
    }
    let mut ids = HashSet::new();
    servers
        .into_iter()
        .map(|server| {
            let (id, name, direct, namespace) = match server {
                v2::McpServer::Acp(server) => (server.server_id, server.name, None, "acp"),
                v2::McpServer::Stdio(server) => {
                    let command = std::path::PathBuf::from(&server.command);
                    if !command.is_absolute() || command.as_os_str().to_string_lossy().contains('\0') || server.args.iter().any(|a| a.contains('\0')) {
                        return Err("invalid MCP executable; use an absolute path and NUL-free arguments");
                    }
                    let mut env = crate::mcp::config::baseline(environment);
                    let mut keys = HashSet::new();
                    for entry in server.env {
                        if entry.name.is_empty() || entry.name.contains(['=', '\0']) || entry.value.contains('\0') || !keys.insert(entry.name.clone()) { return Err("invalid MCP environment; use unique valid names and NUL-free values"); }
                        env.insert(entry.name, entry.value);
                    }
                    let id = serde_json::to_string(&("client", "stdio", &server.name)).expect("identity is JSON");
                    (id.into(), server.name, Some(crate::mcp::config::Transport::Stdio { command, args: server.args, cwd: cwd.into(), env }), "stdio")
                }
                v2::McpServer::Http(server) => {
                    let mut headers = BTreeMap::new();
                    for entry in server.headers {
                        if headers.insert(entry.name.to_ascii_lowercase(), entry.value).is_some() { return Err("duplicate MCP header; declare each header once"); }
                    }
                    crate::mcp::config::validate_http(&server.url, &headers)?;
                    let id = serde_json::to_string(&("client", "http", &server.name)).expect("identity is JSON");
                    (id.into(), server.name, Some(crate::mcp::config::Transport::Http { url: server.url, headers }), "http")
                }
                _ => return Err("unsupported MCP transport; use ACP, stdio or Streamable HTTP"),
            };
            let text = id.to_string();
            // Generated direct identities include a namespace and JSON escaping;
            // opaque ACP identifiers retain their original 1024-byte ceiling.
            let max_id_bytes = if direct.is_some() { 2048 } else { 1024 };
            if text.is_empty() || text.len() > max_id_bytes || (direct.is_some() && name.is_empty()) || name.len() > 1024 {
                return Err(
                    "invalid MCP server identity; use a nonempty ID and names up to 1024 bytes",
                );
            }
            if !ids.insert((namespace, text)) {
                return Err("duplicate MCP server ID; declare each server once per session");
            }
            Ok(Server {
                id,
                name: format!("client/{namespace}/{name}"),
                direct,
            })
        })
        .collect()
}

struct Route {
    server: Server,
    display: String,
    name: String,
    validator: jsonschema::Validator,
}

/// Definitions and routes are built together so aliases cannot change dispatch.
pub(super) struct Catalog {
    definitions: Vec<Tool>,
    routes: BTreeMap<String, Route>,
}

fn alias(server: &str, name: &str) -> String {
    // JSON encodes the pair unambiguously; a separator alone can occur in an ID.
    let digest = Sha256::digest(serde_json::to_vec(&(server, name)).expect("string pair is JSON"));
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("mcp_{}", &hex[..56])
}

async fn request(
    reply: &super::Reply,
    server: &Server,
    method: &str,
    mut params: Value,
) -> Result<v2::MessageMcpResponse, &'static str> {
    if server.direct.is_some() {
        let manager = super::lock(&reply.mcp).direct.clone();
        let entry = manager
            .get(&reply.id, &server.id.to_string())
            .ok_or(crate::mcp::runtime::UNAVAILABLE)?;
        let envelope = entry.request(method, params).await?;
        return serde_json::from_value(envelope)
            .map_err(|_| "invalid MCP response envelope; check the server implementation");
    }
    let connection = &reply.connection;
    let bindings = &reply.mcp;
    let id = super::lock(bindings).request_id()?;
    params["_meta"] = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {},
    });
    // Never retain the outer error: its data can contain credentials or arguments.
    connection
        .send_request(
            v2::MessageMcpRequest::new(server.id.clone(), id, method).params(
                params
                    .as_object()
                    .expect("internal MCP parameters are objects")
                    .clone(),
            ),
        )
        .block_task()
        .await
        .map_err(|_| "MCP server is unavailable; check the client's server attachment")
}

/// Bound schema work before synchronous compilation; offline mode forbids I/O.
fn shallow(value: &Value, depth: usize) -> bool {
    if depth > MAX_DEPTH {
        return false;
    }
    match value {
        Value::Array(values) => values.iter().all(|v| shallow(v, depth + 1)),
        Value::Object(values) => values.values().all(|v| shallow(v, depth + 1)),
        _ => true,
    }
}

impl Catalog {
    pub async fn discover(
        reply: &super::Reply,
        servers: &[Server],
        native: Vec<Tool>,
    ) -> Result<Self, &'static str> {
        tokio::time::timeout(DISCOVERY_TIMEOUT, Self::list(reply, servers, native))
            .await
            .map_err(|_| "MCP discovery timed out; check the client's servers and retry")?
    }

    async fn list(
        reply: &super::Reply,
        servers: &[Server],
        mut definitions: Vec<Tool>,
    ) -> Result<Self, &'static str> {
        let mut routes = BTreeMap::new();
        let mut aliases: HashSet<_> = definitions.iter().map(|tool| tool.name.clone()).collect();
        let mut bytes = 0;
        for server in servers {
            let mut cursor: Option<String> = None;
            let mut cursors = HashSet::new();
            let mut names = HashSet::new();
            for page in 0..MAX_PAGES {
                let mut params = json!({});
                if let Some(cursor) = &cursor {
                    params["cursor"] = Value::String(cursor.clone());
                }
                let response = request(reply, server, "tools/list", params).await?;
                // Discovery errors fail the turn rather than advertise an incomplete catalog.
                let v2::MessageMcpResponse::Result { result, .. } = response else {
                    return Err(
                        "MCP discovery failed; check the client's server permissions and retry",
                    );
                };
                bytes += result.to_string().len();
                if bytes > MAX_CATALOG_BYTES || !shallow(&result, 0) {
                    return Err("MCP catalog exceeds limits; attach fewer or simpler tools");
                }
                let tools = result
                    .get("tools")
                    .and_then(Value::as_array)
                    .ok_or("invalid MCP tool list; check the client's server implementation")?;
                for tool in tools {
                    let name = tool
                        .get("name")
                        .and_then(Value::as_str)
                        .filter(|name| !name.is_empty() && name.len() <= 1024)
                        .ok_or("invalid MCP tool name; check the client's server implementation")?;
                    if !names.insert(name.to_owned()) || routes.len() >= MAX_TOOLS {
                        return Err(
                            "duplicate or too many MCP tools; check the client's tool catalog",
                        );
                    }
                    let description = match tool.get("description") {
                        None => "",
                        Some(Value::String(value)) => value,
                        _ => {
                            return Err(
                                "invalid MCP tool description; check the client's server implementation",
                            );
                        }
                    };
                    let schema = tool
                        .get("inputSchema")
                        .filter(|schema| {
                            schema.is_object()
                                && schema.get("type").and_then(Value::as_str) == Some("object")
                        })
                        .ok_or("invalid MCP input schema; tools must accept an object")?;
                    let validator = jsonschema::options()
                        .with_draft(jsonschema::Draft::Draft202012)
                        .offline()
                        .build(schema)
                        .map_err(
                            |_| "unsupported MCP input schema; use a valid offline object schema",
                        )?;
                    let alias = if server.direct.is_some() {
                        // Different tuple shape keeps direct identities disjoint from opaque ACP IDs.
                        let identity =
                            serde_json::to_string(&("direct", server.id.to_string(), name))
                                .expect("identity is JSON");
                        let digest = Sha256::digest(identity.as_bytes());
                        format!(
                            "mcp_{}",
                            digest
                                .iter()
                                .take(28)
                                .map(|b| format!("{b:02x}"))
                                .collect::<String>()
                        )
                    } else {
                        alias(&server.id.to_string(), name)
                    };
                    if !aliases.insert(alias.clone()) {
                        return Err(
                            "MCP tool alias collision; change the conflicting tool identity",
                        );
                    }
                    definitions.push(Tool {
                        name: alias.clone(),
                        description: format!("{} / {name}: {description}", server.name),
                        parameters: schema.clone(),
                    });
                    routes.insert(
                        alias,
                        Route {
                            server: server.clone(),
                            display: server.name.clone(),
                            name: name.into(),
                            validator,
                        },
                    );
                }
                cursor = match result.get("nextCursor") {
                    None => None,
                    Some(Value::String(value)) if !value.is_empty() && value.len() <= 4096 => {
                        Some(value.clone())
                    }
                    _ => {
                        return Err("invalid MCP cursor; check the client's server implementation");
                    }
                };
                let Some(next) = &cursor else { break };
                if !cursors.insert(next.clone()) || page + 1 == MAX_PAGES {
                    return Err(
                        "MCP pagination exceeds limits; check the client's server implementation",
                    );
                }
            }
        }
        definitions.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Self {
            definitions,
            routes,
        })
    }
}

fn failure(message: &str) -> ToolResult {
    ToolResult {
        output: json!({"error":{"code":"mcp_tool_error","message":message}}).to_string(),
        is_error: true,
    }
}

fn outcome(response: v2::MessageMcpResponse) -> ToolResult {
    let is_error = match &response {
        v2::MessageMcpResponse::Error { .. } => true,
        v2::MessageMcpResponse::Result { result, .. } => {
            if !shallow(result, 0)
                || !result.is_object()
                || result.get("isError").is_some_and(|v| !v.is_boolean())
                || result.get("content").is_some_and(|v| !v.is_array())
                || result.get("_meta").is_some_and(|v| !v.is_object())
            {
                return failure(
                    "Invalid MCP tool result; check the client's server implementation.",
                );
            }
            if result.get("task").is_some()
                || result
                    .get("resultType")
                    .is_some_and(|v| v.as_str() != Some("complete"))
            {
                return failure(
                    "Unsupported MCP outcome; interactive continuation and tasks are not supported.",
                );
            }
            result
                .get("isError")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        }
        _ => return failure("Unsupported MCP response; check the client's server implementation."),
    };
    // Keep the entire envelope: structured content, error extensions and both metadata layers.
    let output = serde_json::to_string(&response).expect("MCP response is JSON");
    if output.len() > crate::tool::MAX_OUTPUT_BYTES {
        return failure("MCP output exceeds the byte limit; request a smaller result.");
    }
    ToolResult { output, is_error }
}

pub(super) struct Composite<'a> {
    pub native: super::interactions::Native<'a>,
    pub catalog: Catalog,
}

impl Composite<'_> {
    pub fn definitions(&self) -> &[Tool] {
        &self.catalog.definitions
    }
}

impl Executor for Composite<'_> {
    fn execute_observed<'a>(
        &'a self,
        call: &'a Call,
        snapshot: Option<std::sync::Arc<str>>,
        observer: Option<&'a dyn crate::agent::Observer>,
    ) -> Pin<Box<dyn Future<Output = Result<ToolResult, crate::llm::Error>> + Send + 'a>> {
        if !self.catalog.routes.contains_key(&call.name) {
            return self.native.execute_observed(call, snapshot, observer);
        }
        Box::pin(async move {
            if let Some(observer) = observer {
                observer
                    .observe(call, crate::agent::Observation::Started)
                    .await?;
            }
            self.execute(call).await
        })
    }
    fn description(&self, call: &Call) -> crate::presentation::Description {
        if let Some(route) = self.catalog.routes.get(&call.name) {
            crate::presentation::Description {
                name: format!("{} / {}", route.display, route.name),
                kind: "other",
                locations: Vec::new(),
            }
        } else {
            self.native.description(call)
        }
    }
    fn targets(&self, call: &Call) -> Vec<crate::tool::Target> {
        if self.catalog.routes.contains_key(&call.name) {
            Vec::new()
        } else {
            self.native.targets(call)
        }
    }
    fn output_limit(&self) -> usize {
        self.native.output_limit()
    }

    fn target(&self, call: &Call) -> Option<crate::tool::Target> {
        // Remote tools retain client-owned semantics even if names resemble native tools.
        if self.catalog.routes.contains_key(&call.name) {
            None
        } else {
            self.native.target(call)
        }
    }

    fn execute_snapshot<'a>(
        &'a self,
        call: &'a Call,
        snapshot: std::sync::Arc<str>,
    ) -> Pin<Box<dyn Future<Output = Result<ToolResult, crate::llm::Error>> + Send + 'a>> {
        if self.catalog.routes.contains_key(&call.name) {
            self.execute(call)
        } else {
            self.native.execute_snapshot(call, snapshot)
        }
    }

    fn execute<'a>(
        &'a self,
        call: &'a Call,
    ) -> Pin<Box<dyn Future<Output = Result<ToolResult, crate::llm::Error>> + Send + 'a>> {
        Box::pin(async move {
            let Some(route) = self.catalog.routes.get(&call.name) else {
                return self.native.execute(call).await;
            };
            if !call.arguments.is_object() || !route.validator.is_valid(&call.arguments) {
                return Ok(failure(
                    "Arguments do not match the MCP input schema; correct the call.",
                ));
            }
            if route.server.direct.is_some() {
                let resource = format!("{}/{}", route.display, route.name);
                let check = crate::permissions::Check::value(
                    "mcp.call",
                    resource,
                    crate::permissions::Effect::Ask,
                );
                let effect = self
                    .native
                    .permissions
                    .evaluate(&[check], self.native.inner.cwd());
                let allowed = match effect {
                    crate::permissions::Effect::Allow => true,
                    crate::permissions::Effect::Deny => false,
                    crate::permissions::Effect::Ask => {
                        let mut subject = self
                            .native
                            .reply
                            .tool_permission_subject(call, &self.description(call))?;
                        let manager = super::lock(&self.native.reply.mcp).direct.clone();
                        let entry = manager
                            .get(&self.native.reply.id, &route.server.id.to_string())
                            .ok_or(crate::llm::Error::Cancelled)?;
                        if let v2::RequestPermissionSubject::ToolCall(tool) = &mut subject {
                            tool.tool_call.meta = serde_json::from_value(json!({"bee/mcp":{"server":route.server.id.to_string(),"revision":entry.revision}})).map_err(|_| crate::llm::Error::Tool)?;
                        }
                        self.native
                            .reply
                            .permission(subject, format!("Call MCP tool {}", route.display))
                            .await?
                    }
                };
                if !allowed {
                    return Ok(failure(
                        "MCP tool permission denied; change permission policy or approve the call.",
                    ));
                }
                if !self.native.reply.current() {
                    return Err(crate::llm::Error::Cancelled);
                }
            }
            // Dropping this owned request cancels it; never detach or retry remote side effects.
            Ok(
                match tokio::time::timeout(
                    CALL_TIMEOUT,
                    request(
                        self.native.reply,
                        &route.server,
                        "tools/call",
                        json!({"name":route.name,"arguments":call.arguments}),
                    ),
                )
                .await
                {
                    Ok(Ok(response)) => outcome(response),
                    Ok(Err(message)) => failure(message),
                    Err(_) => failure(
                        "MCP tool timed out; check the client's server before trying again.",
                    ),
                },
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_are_portable_and_distinguish_ambiguous_identity_pairs() {
        let first = alias("a/b", "c");
        assert_ne!(first, alias("a", "b/c"));
        assert_eq!(first, alias("a/b", "c"));
        assert!(first.len() <= 64);
        assert!(
            first
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        );
    }

    #[test]
    fn inner_errors_preserve_null_data_extensions_and_both_metadata_layers() {
        let response: v2::MessageMcpResponse = serde_json::from_value(json!({
            "error":{"code":-32000,"message":"denied","data":null,"extension":[1,2]},
            "_meta":{"carrier":true}
        }))
        .unwrap();
        let result = outcome(response);
        assert!(result.is_error);
        let value: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(value["error"]["extension"], json!([1, 2]));
        assert!(value["error"].as_object().unwrap().contains_key("data"));
        assert_eq!(value["_meta"]["carrier"], true);
        let result = outcome(v2::MessageMcpResponse::success(json!({
            "structuredContent":null,"content":[],"_meta":{"inner":true},"extension":42
        })));
        assert!(!result.is_error);
        let value: Value = serde_json::from_str(&result.output).unwrap();
        assert!(
            value["result"]
                .as_object()
                .unwrap()
                .contains_key("structuredContent")
        );
        assert_eq!(value["result"]["_meta"]["inner"], true);
        assert_eq!(value["result"]["extension"], 42);
    }

    #[test]
    fn malformed_and_noncomplete_outcomes_are_errors_without_echoing_values() {
        for value in [
            Value::Null,
            json!({"content":"secret"}),
            json!({"_meta":"secret"}),
            json!({"resultType":"secret"}),
        ] {
            let result = outcome(v2::MessageMcpResponse::success(value));
            assert!(result.is_error);
            assert!(!result.output.contains("secret"));
        }
    }

    #[test]
    fn request_ids_never_wrap_and_schema_depth_is_bounded() {
        let mut bindings = Bindings::default();
        assert_ne!(
            bindings.request_id().unwrap(),
            bindings.request_id().unwrap()
        );
        bindings.next_request = u64::MAX;
        assert!(bindings.request_id().is_err());
        let mut schema = Value::Null;
        for _ in 0..MAX_DEPTH + 1 {
            schema = json!([schema]);
        }
        assert!(!shallow(&schema, 0));
    }

    #[test]
    fn duplicate_and_unsupported_declarations_are_rejected_without_values() {
        let acp = json!({"type":"acp","serverId":"private-sentinel","name":"tools"});
        let servers = serde_json::from_value(json!([acp, acp])).unwrap();
        assert!(
            !declarations(servers, std::path::Path::new("/"), &Default::default())
                .err()
                .unwrap()
                .contains("private-sentinel")
        );
        let servers = serde_json::from_value(
            json!([{"type":"http","name":"tools","url":"http://private-sentinel","headers":[]}]),
        )
        .unwrap();
        assert!(
            !declarations(servers, std::path::Path::new("/"), &Default::default())
                .err()
                .unwrap()
                .contains("private-sentinel")
        );
    }
}
