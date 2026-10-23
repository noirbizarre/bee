//! The ACPv2 SDK integration, exercised in-process.
//!
//! A subprocess stdio test needs the binary to speak ACP, which is a later
//! step; the SDK's in-memory transport covers the same `initialize` exchange.

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
async fn the_bee_agent_does_not_advertise_sessions_it_cannot_serve() -> Result<(), Error> {
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

            assert!(response.capabilities.session.is_none());
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
