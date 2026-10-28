//! The ACPv2 SDK integration, exercised in-process.
//!
//! The SDK's in-memory transport covers the `initialize` exchange and the
//! transport seam without a subprocess; `stdio.rs` covers the binary and
//! `session.rs` the session lifecycle.

use agent_client_protocol::schema::{ProtocolVersion, v2};
use agent_client_protocol::{Client, ConnectTo, Error};

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
async fn the_bee_agent_advertises_sessions_and_only_acp_mcp_transport() -> Result<(), Error> {
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

            // Each extra capability is a promise a client acts on.
            let session = response
                .capabilities
                .session
                .expect("the session lifecycle is implemented");
            assert!(session.prompt.is_none());
            let mcp = serde_json::to_value(session.mcp.expect("MCP is implemented")).unwrap();
            assert!(mcp.get("acp").is_some());
            assert!(mcp.get("http").is_none());
            assert!(mcp.get("stdio").is_none());
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

    let (client, served) = tokio::join!(
        client,
        bee::acp::serve(agent_side, bee::account::Accounts::default())
    );

    client.expect("the client completes its exchange");
    // The client leaving is a normal end of the connection, not an error.
    served.expect("serve ends cleanly when the peer disconnects");
}

#[tokio::test(flavor = "current_thread")]
async fn a_client_leaving_mid_turn_ends_the_connection_and_drops_the_turn() {
    use std::future;
    use std::time::Duration;

    // Signals, by being dropped, that the turn body was dropped with the
    // connection and not leaked running.
    struct Dropped(tokio::sync::mpsc::UnboundedSender<()>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }

    let (dropped, mut dropped_signal) = tokio::sync::mpsc::unbounded_channel();
    let body = move |_reply: bee::acp::Reply| {
        let signal = Dropped(dropped.clone());
        async move {
            let _signal = signal;
            future::pending::<Result<(), bee::acp::TurnFailure>>().await
        }
    };
    let (client_side, agent_side) = agent_client_protocol::Channel::duplex();

    let client = Client.v2().connect_with(client_side, async |connection| {
        connection
            .send_request(v2::InitializeRequest::new(
                ProtocolVersion::V2,
                v2::Implementation::new("bee-test", "0"),
            ))
            .block_task()
            .await?;
        let session = connection
            .send_request(v2::NewSessionRequest::new(std::env::temp_dir()))
            .block_task()
            .await?
            .session_id;
        connection
            .send_request(v2::PromptRequest::new(session, vec!["hi".into()]))
            .block_task()
            .await?;
        // Leave with the turn still running.
        Ok(())
    });

    let both = async { tokio::join!(client, bee::acp::agent_with(body).connect_to(agent_side)) };
    // Bounded: a turn that kept the connection open would hang here.
    let (client, served) = tokio::time::timeout(Duration::from_secs(10), both)
        .await
        .expect("the connection ends once the client has left");

    client.expect("the client completes its exchange");
    served.expect("the agent ends cleanly when the peer disconnects");
    tokio::time::timeout(Duration::from_secs(10), dropped_signal.recv())
        .await
        .expect("the running turn is dropped with the connection");
}
