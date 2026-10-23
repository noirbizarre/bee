//! The ACPv2 boundary: the only module that imports the ACP Rust SDK.
//!
//! Keeping the SDK behind one file means an upgrade, which is likely while v2
//! is a draft, touches one file instead of the whole crate.
//!
//! Two layers, so the agent never learns which transport carries it (ADR-003):
//! [`agent`] is the transport-free core, [`serve`] runs it over any transport
//! the SDK can connect, and [`serve_stdio`] picks stdin/stdout. A remote
//! transport later supplies another [`ConnectTo`] and changes nothing else.

use agent_client_protocol::schema::v2;
use agent_client_protocol::{Agent, Client, ConnectTo, Responder, Stdio, V2ConnectionTo};

use crate::{Error, Result};

/// Run the agent over `transport` until the peer disconnects.
///
/// # Errors
///
/// Returns [`Error::Connection`] if the connection ends with a protocol or
/// transport error. A peer that simply closes its end is not an error.
pub async fn serve(transport: impl ConnectTo<Agent>) -> Result<()> {
    // `agent()` is `ConnectTo<Client>`, and the transport is the client's end,
    // so it is the transport that the agent connects to.
    agent()
        .connect_to(transport)
        .await
        .map_err(|error| Error::Connection {
            // Stringified so the SDK's error type stays inside this module.
            reason: error.to_string(),
        })
}

/// Run the agent over the process's stdin and stdout.
///
/// # Errors
///
/// As [`serve`].
pub async fn serve_stdio() -> Result<()> {
    // No debug callback on purpose: stdout carries protocol traffic only, and a
    // trace line written there would corrupt the client's JSON-RPC stream.
    serve(Stdio::new()).await
}

/// Build the ACPv2 agent side of a connection, able to answer `initialize`.
///
/// The result connects to any [`Client`] transport, which is how the tests
/// drive it in-process without a subprocess.
pub fn agent() -> impl ConnectTo<Client> {
    // `Agent.v2()`, not `Agent.builder()`: the latter is the stable v1 surface.
    Agent.v2().name("bee").on_receive_request(
        async |request: v2::InitializeRequest,
               responder: Responder<v2::InitializeResponse>,
               _connection: V2ConnectionTo<Client>| {
            // No capabilities are advertised. The SDK treats advertising
            // `SessionCapabilities` as a commitment to the whole session
            // lifecycle, which does not exist yet; claiming it would make a
            // client send `session/new` to an agent that cannot answer.
            responder.respond(v2::InitializeResponse::new(
                // Hand back the version the SDK gives us rather than choosing
                // one: the v2 builder negotiates the version on the wire, so a
                // hard-coded value here would fight it.
                request.protocol_version,
                v2::Implementation::new("bee", env!("CARGO_PKG_VERSION")),
            ))
        },
        // The SDK requires this macro to tie the handler to the request type.
        agent_client_protocol::on_receive_request!(),
    )
}
