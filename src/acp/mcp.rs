//! Client-owned MCP tools, reached exclusively through the ACP connection.

use std::collections::{BTreeMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use agent_client_protocol::schema::v2;
use agent_client_protocol::{Client, V2ConnectionTo};
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
}

/// This map belongs to one connection, never to a global server registry.
#[derive(Default)]
pub(super) struct Bindings {
    pub sessions: BTreeMap<String, Vec<Server>>,
    next_request: u64,
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

pub(super) fn declarations(servers: Vec<v2::McpServer>) -> Result<Vec<Server>, &'static str> {
    if servers.len() > MAX_SERVERS {
        return Err("too many MCP servers; attach at most 32 ACP servers");
    }
    let mut ids = HashSet::new();
    servers
        .into_iter()
        .map(|server| {
            let v2::McpServer::Acp(server) = server else {
                return Err("unsupported MCP transport; host the server through ACP");
            };
            let id = server.server_id.to_string();
            if id.is_empty() || id.len() > 1024 || server.name.len() > 1024 {
                return Err(
                    "invalid MCP server identity; use a nonempty ID and names up to 1024 bytes",
                );
            }
            if !ids.insert(id) {
                return Err("duplicate MCP server ID; declare each server once per session");
            }
            Ok(Server {
                id: server.server_id,
                name: server.name,
            })
        })
        .collect()
}

struct Route {
    server: v2::McpServerAcpId,
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
    connection: &V2ConnectionTo<Client>,
    bindings: &super::Mutex<Bindings>,
    server: v2::McpServerAcpId,
    method: &str,
    mut params: Value,
) -> Result<v2::MessageMcpResponse, &'static str> {
    let id = super::lock(bindings).request_id()?;
    params["_meta"] = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {},
    });
    // Never retain the outer error: its data can contain credentials or arguments.
    connection
        .send_request(
            v2::MessageMcpRequest::new(server, id, method).params(
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
        connection: &V2ConnectionTo<Client>,
        bindings: &super::Mutex<Bindings>,
        servers: &[Server],
        native: Vec<Tool>,
    ) -> Result<Self, &'static str> {
        tokio::time::timeout(
            DISCOVERY_TIMEOUT,
            Self::list(connection, bindings, servers, native),
        )
        .await
        .map_err(|_| "MCP discovery timed out; check the client's servers and retry")?
    }

    async fn list(
        connection: &V2ConnectionTo<Client>,
        bindings: &super::Mutex<Bindings>,
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
                let response = request(
                    connection,
                    bindings,
                    server.id.clone(),
                    "tools/list",
                    params,
                )
                .await?;
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
                    let alias = alias(&server.id.to_string(), name);
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
                            server: server.id.clone(),
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
            if !result.is_object()
                || result.get("isError").is_some_and(|v| !v.is_boolean())
                || result.get("content").is_some_and(|v| !v.is_array())
                || result.get("_meta").is_some_and(|v| !v.is_object())
            {
                return failure(
                    "Invalid MCP tool result; check the client's server implementation.",
                );
            }
            if result
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
    pub connection: &'a V2ConnectionTo<Client>,
    pub bindings: &'a super::Mutex<Bindings>,
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
            // Dropping this owned request cancels it; never detach or retry remote side effects.
            Ok(
                match tokio::time::timeout(
                    CALL_TIMEOUT,
                    request(
                        self.connection,
                        self.bindings,
                        route.server.clone(),
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
            !declarations(servers)
                .err()
                .unwrap()
                .contains("private-sentinel")
        );
        let servers = serde_json::from_value(
            json!([{"type":"http","name":"tools","url":"https://private-sentinel","headers":[]}]),
        )
        .unwrap();
        assert!(
            !declarations(servers)
                .err()
                .unwrap()
                .contains("private-sentinel")
        );
    }
}
