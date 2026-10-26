use super::*;
use agent_client_protocol::schema::{ProtocolVersion, v2};
use agent_client_protocol::{Client, Error, ErrorCode};

#[rstest::rstest]
#[case(0)]
#[case(401)]
#[case(403)]
#[tokio::test]
async fn account_changes_and_inference_authentication_rejection_durably_block_automatic_selection(
    #[case] status: u16,
) {
    let root = tempfile::tempdir().unwrap();
    let env = Environment::default().with_xdg_data_home(Some(root.path().join("data")));
    let credentials = crate::auth::FileStore::from_environment(&env).unwrap();
    let audience = crate::auth::Audience::Provider("account");
    let original = crate::auth::Credential::new(crate::auth::Secret::new("original-key"));
    credentials.put(audience, &original).unwrap();
    let responses = if status == 0 {
        Vec::new()
    } else {
        let mut response =
            crate::test_support::Response::json(serde_json::json!({"error":"rejected"}));
        response.status = status;
        vec![response]
    };
    let server = crate::test_support::Server::start(responses);
    let profiles = crate::test_support::configured_providers(&format!(
        "[providers.account]\ncredential={{store=true}}\nmodel='default-model'\nprotocols={{openai-chat={{base_url='{}'}}}}",
        server.url
    ));
    let preferences = Store::default();
    preferences
        .remember(
            "default",
            Reference::new(
                profiles.get("account").unwrap(),
                "recent".into(),
                Some(&original),
            ),
        )
        .unwrap();
    let mut saved: Option<v2::SessionId> = None;
    for _ in 0..2 {
        let history = crate::history::Store::open(root.path().join("state")).unwrap();
        let runtime = crate::acp::Runtime::new(Default::default(), Default::default())
            .with_history(history)
            .with_preferences(preferences.clone())
            .with_auth(env.clone(), Arc::new(crate::http::AsyncLoginHttp));
        let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
        let client = Client.v2().on_receive_notification(
            async move |update: v2::UpdateSessionNotification, _| {
                tx.send(update).map_err(Error::into_internal_error)
            },
            agent_client_protocol::on_receive_notification!(),
        );
        let previous = saved.take();
        let credentials = &credentials;
        let cwd = root.path().to_owned();
        saved = Some(
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                client.connect_with(
                    crate::acp::agent_with_runtime(profiles.clone(), runtime),
                    async move |connection| {
                        connection
                            .send_request(v2::InitializeRequest::new(
                                ProtocolVersion::V2,
                                v2::Implementation::new("test", "0"),
                            ))
                            .block_task()
                            .await?;
                        let id = if let Some(id) = previous {
                            connection
                                .send_request(v2::ResumeSessionRequest::new(id.clone(), cwd))
                                .block_task()
                                .await?;
                            id
                        } else {
                            let id = connection
                                .send_request(v2::NewSessionRequest::new(cwd))
                                .block_task()
                                .await?
                                .session_id;
                            if status == 0 {
                                credentials
                                    .put(
                                        audience,
                                        &crate::auth::Credential::new(crate::auth::Secret::new(
                                            "replacement-key",
                                        )),
                                    )
                                    .unwrap();
                            }
                            connection
                                .send_request(v2::PromptRequest::new(
                                    id.clone(),
                                    vec!["hello".into()],
                                ))
                                .block_task()
                                .await?;
                            loop {
                                let update = updates.recv().await.unwrap();
                                if let v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle)) =
                                    update.update
                                {
                                    assert!(matches!(
                                        idle.stop_reason,
                                        Some(v2::StopReason::Error(_))
                                    ));
                                    break;
                                }
                            }
                            id
                        };
                        let error = connection
                            .send_request(v2::PromptRequest::new(id.clone(), vec!["hello".into()]))
                            .block_task()
                            .await
                            .unwrap_err();
                        assert_eq!(error.code, ErrorCode::InvalidRequest);
                        connection
                            .send_request(v2::CloseSessionRequest::new(id.clone()))
                            .block_task()
                            .await?;
                        Ok(id)
                    },
                ),
            )
            .await
            .unwrap()
            .unwrap(),
        );
        // Returning the original account must not implicitly clear a durably recorded guard.
        credentials.put(audience, &original).unwrap();
    }
}
