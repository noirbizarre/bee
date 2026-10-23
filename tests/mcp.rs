//! MCP-over-ACP tool invocation, exercised in-process (ADR-002).
//!
//! A spike: the client hosts an MCP server with one tool, and a test-local agent
//! calls it through the SDK's `mcp/message` method. The agent lives here rather
//! than in `src/acp.rs` on purpose: a client only sends an ACP-transport MCP
//! server to an agent advertising the `mcp.acp` capability, which Bee's real
//! agent does not until MCP is integrated.

use agent_client_protocol::mcp_server::McpServer;
use agent_client_protocol::schema::{ProtocolVersion, v2};
use agent_client_protocol::{Agent, Client, Error, Responder, V2ConnectionTo};
use agent_client_protocol_rmcp::McpServerExt;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::oneshot;

#[derive(Deserialize, JsonSchema)]
struct EchoInput {
    message: String,
}

#[derive(Serialize, JsonSchema)]
struct EchoOutput {
    echoed: String,
}

/// The `params` of an MCP request, with the metadata the SDK demands.
///
/// Without `_meta` the SDK answers -32602: there is no MCP `initialize`
/// handshake in this binding, so the version and capabilities travel on every
/// request instead.
fn mcp_params(mut params: Value) -> serde_json::Map<String, Value> {
    params["_meta"] = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {},
    });
    params
        .as_object()
        .expect("MCP params are always an object")
        .clone()
}

/// Like [`mcp_params`], but without the metadata.
fn bare_params(params: Value) -> serde_json::Map<String, Value> {
    params
        .as_object()
        .expect("MCP params are always an object")
        .clone()
}

/// What the spike agent saw: the servers the client declared, and the answer to
/// the single `mcp/message` request it sent.
struct Outcome {
    declared: Vec<v2::McpServer>,
    response: Result<v2::MessageMcpResponse, Error>,
}

/// Run one `session/new` with a client-hosted `echo` MCP server, have the agent
/// send one `mcp/message` request, and return what happened.
async fn agent_sends(
    method: &'static str,
    params: serde_json::Map<String, Value>,
) -> Result<Outcome, Error> {
    let (done_tx, done_rx) = oneshot::channel();
    let mut done_tx = Some(done_tx);
    let mut params = Some(params);

    let agent = Agent
        .v2()
        .on_receive_request(
            async |request: v2::InitializeRequest,
                   responder: Responder<v2::InitializeResponse>,
                   _connection: V2ConnectionTo<Client>| {
                // Advertised here and not in `bee::acp::agent()`: a client only
                // sends `McpServer::Acp` to an agent that claims it can take one.
                responder.respond(
                    v2::InitializeResponse::new(
                        request.protocol_version,
                        v2::Implementation::new("mcp-spike-agent", "0"),
                    )
                    .capabilities(
                        v2::AgentCapabilities::new()
                            .session(v2::SessionCapabilities::new().mcp(
                                v2::McpCapabilities::new().acp(v2::McpAcpCapabilities::new()),
                            )),
                    ),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: v2::NewSessionRequest,
                        responder: Responder<v2::NewSessionResponse>,
                        connection: V2ConnectionTo<Client>| {
                let declared = request.mcp_servers.clone();
                let server_id = declared
                    .iter()
                    .find_map(|server| match server {
                        v2::McpServer::Acp(server) => Some(server.server_id.clone()),
                        _ => None,
                    })
                    .ok_or_else(|| Error::invalid_params().data("no ACP-transport MCP server"))?;
                let done_tx = done_tx.take().expect("the spike opens one session");
                let params = params.take().expect("the spike opens one session");

                let call_connection = connection.clone();
                // Spawned, not awaited: the client polls its MCP server while
                // `session/new` is pending, but this handler must still return
                // for that request to complete. Awaiting the call here would
                // make the handler wait on a response that needs it to finish.
                connection.spawn(async move {
                    let response = call_connection
                        .send_request(
                            // The id only has to be unique among active
                            // requests; a reused one is rejected with -32602.
                            v2::MessageMcpRequest::new(server_id, "spike-1", method).params(params),
                        )
                        .block_task()
                        .await;
                    // The receiver outlives the connection, so a failure here
                    // means the test already gave up and has nothing to report.
                    drop(done_tx.send(Outcome { declared, response }));
                    Ok(())
                })?;

                responder.respond(v2::NewSessionResponse::new(v2::SessionId::new("spike")))
            },
            agent_client_protocol::on_receive_request!(),
        );

    Client
        .v2()
        .connect_with(agent, async move |connection| {
            connection
                .send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    v2::Implementation::new("mcp-spike-client", "0"),
                ))
                .block_task()
                .await?;

            let server = McpServer::<Agent>::builder("echo-server")
                .tool_fn(
                    "echo",
                    "Echoes the message back",
                    async |input: EchoInput, _context| {
                        Ok(EchoOutput {
                            echoed: input.message,
                        })
                    },
                    agent_client_protocol_rmcp::tool_fn!(),
                )
                .build();

            connection
                .build_session(std::env::current_dir().map_err(Error::into_internal_error)?)
                .with_mcp_server(server)?
                .start_session()
                .block_task()
                .await?;

            done_rx.await.map_err(Error::into_internal_error)
        })
        .await
}

#[tokio::test(flavor = "current_thread")]
async fn an_agent_can_call_a_tool_hosted_by_the_client_over_acp() -> Result<(), Error> {
    let outcome = agent_sends(
        "tools/call",
        mcp_params(json!({ "name": "echo", "arguments": { "message": "hello" } })),
    )
    .await?;

    let v2::MessageMcpResponse::Result { result, .. } = outcome.response? else {
        panic!("expected a result, not an MCP error");
    };
    assert_eq!(result["structuredContent"]["echoed"], "hello");
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn the_client_declares_its_mcp_server_to_the_agent_with_the_acp_transport()
-> Result<(), Error> {
    let outcome = agent_sends(
        "tools/call",
        mcp_params(json!({ "name": "echo", "arguments": { "message": "hi" } })),
    )
    .await?;

    // The agent never sees a command or a URL: the ACP transport names the
    // server by id, and every message to it is routed through the client.
    assert!(
        matches!(outcome.declared.as_slice(), [v2::McpServer::Acp(server)] if server.name == "echo-server"),
        "unexpected declaration: {:?}",
        outcome.declared,
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn an_agent_can_list_the_tools_the_client_hosts_over_acp() -> Result<(), Error> {
    let outcome = agent_sends("tools/list", mcp_params(json!({}))).await?;

    let v2::MessageMcpResponse::Result { result, .. } = outcome.response? else {
        panic!("expected a result, not an MCP error");
    };
    let tools = result["tools"].as_array().expect("a tools array");
    assert_eq!(tools.len(), 1, "unexpected tools: {result}");
    assert_eq!(tools[0]["name"], "echo");
    // The input schema is what an LLM provider needs to describe the tool.
    assert!(
        tools[0]["inputSchema"]["properties"]["message"].is_object(),
        "unexpected schema: {}",
        tools[0]["inputSchema"],
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn a_call_to_an_unknown_tool_comes_back_as_an_mcp_error() -> Result<(), Error> {
    let outcome = agent_sends(
        "tools/call",
        mcp_params(json!({ "name": "missing", "arguments": {} })),
    )
    .await?;

    // Current SDK behaviour, pinned so an upgrade that changes the shape of a
    // tool failure shows up here instead of in the agent loop. It is an MCP
    // error inside a successful ACP response, not an ACP-level `Err`.
    let v2::MessageMcpResponse::Error { error, .. } = outcome.response? else {
        panic!("expected an MCP error, not a result");
    };
    assert_eq!(error.code, -32602);
    assert!(error.message.contains("missing"), "{}", error.message);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn a_tool_call_without_the_mcp_protocol_metadata_is_rejected() -> Result<(), Error> {
    let outcome = agent_sends(
        "tools/call",
        bare_params(json!({ "name": "echo", "arguments": { "message": "hello" } })),
    )
    .await?;

    // Current SDK behaviour: the version rides on every request, so a call
    // without it is an invalid-params error rather than a guess at a default.
    let v2::MessageMcpResponse::Error { error, .. } = outcome.response? else {
        panic!("expected an MCP error for missing metadata");
    };
    assert_eq!(error.code, -32602);
    Ok(())
}
