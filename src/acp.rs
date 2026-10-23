//! The ACPv2 boundary: the only module that imports the ACP Rust SDK.
//!
//! Keeping the SDK behind one file means an upgrade, which is likely while v2
//! is a draft, touches one file instead of the whole crate.
//!
//! This is a feasibility spike (ADR-001), not the agent. It is deliberately not
//! wired to [`crate::run`] or the binary, so Bee still reports ACPv2 as not
//! implemented.

use agent_client_protocol::schema::v2;
use agent_client_protocol::{Agent, Client, ConnectTo, Responder, V2ConnectionTo};

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
