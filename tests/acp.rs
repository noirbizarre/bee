//! The ACPv2 SDK integration, exercised in-process.
//!
//! The SDK's in-memory transport covers the `initialize` exchange and the
//! transport seam without a subprocess; `stdio.rs` covers the binary and
//! `session.rs` the session lifecycle.

use agent_client_protocol::schema::{ProtocolVersion, v2};
use agent_client_protocol::{Client, Error};

#[tokio::test(flavor = "current_thread")]
async fn a_v2_client_can_initialize_the_bee_agent_in_process() -> Result<(), Error> {
    Client
        .v2()
        .connect_with(bee::acp::agent(), async |connection| {
            let response = connection
                .send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    v2::Implementation::new("bee-test", "0"),
                ))
                .block_task()
                .await?;

            assert_eq!(response.info.name, "bee");
            assert_eq!(response.protocol_version, ProtocolVersion::V2);
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "current_thread")]
async fn the_bee_agent_advertises_only_the_baseline_session_capability() -> Result<(), Error> {
    Client
        .v2()
        .connect_with(bee::acp::agent(), async |connection| {
            let response = connection
                .send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    v2::Implementation::new("bee-test", "0"),
                ))
                .block_task()
                .await?;

            // Each extra capability is a promise a client acts on, and none is
            // implemented: an extension must be added here with its code.
            let session = response
                .capabilities
                .session
                .expect("the session lifecycle is implemented");
            assert!(session.prompt.is_none());
            assert!(session.mcp.is_none());
            assert!(session.delete.is_none());
            assert!(session.additional_directories.is_none());
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "current_thread")]
async fn the_sdk_negotiates_a_v2_agent_up_from_a_v1_request() -> Result<(), Error> {
    Client
        .v2()
        .connect_with(bee::acp::agent(), async |connection| {
            // Current SDK behaviour, pinned so an upgrade that changes the
            // negotiation shows up here instead of in a client.
            let response = connection
                .send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V1,
                    v2::Implementation::new("bee-test", "0"),
                ))
                .block_task()
                .await?;

            assert_eq!(response.protocol_version, ProtocolVersion::V2);
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "current_thread")]
async fn serve_runs_the_agent_over_any_transport_the_sdk_can_connect() {
    // `Channel` is not stdio: this is what shows the agent core is not coupled
    // to the transport `run()` picks.
    let (client_side, agent_side) = agent_client_protocol::Channel::duplex();

    let client = Client.v2().connect_with(client_side, async |connection| {
        let response = connection
            .send_request(v2::InitializeRequest::new(
                ProtocolVersion::V2,
                v2::Implementation::new("bee-test", "0"),
            ))
            .block_task()
            .await?;
        assert_eq!(response.info.name, "bee");
        Ok(())
    });

    let (client, served) = tokio::join!(client, bee::acp::serve(agent_side));

    client.expect("the client completes its exchange");
    // The client leaving is a normal end of the connection, not an error.
    served.expect("serve ends cleanly when the peer disconnects");
}
