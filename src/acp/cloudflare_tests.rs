//! The production account/availability gate retains Cloudflare routing on every protocol.

use super::*;
use crate::test_support::{Response, Server};
use serde_json::json;

#[tokio::test]
async fn cloudflare_acp_uses_selected_private_routing_for_all_three_protocols() {
    for (protocol, suffix, response) in [
        (
            "openai-chat",
            "chat/completions",
            Response::sse(format!(
                "data: {}\n\ndata: [DONE]\n\n",
                json!({"choices":[{"index":0,"delta":{"content":"done"},"finish_reason":"stop"}]})
            )),
        ),
        (
            "openai-responses",
            "responses",
            Response::json(
                json!({"status":"completed","output":[{"id":"message-1","type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}]}),
            ),
        ),
        (
            "anthropic-messages",
            "messages",
            Response::json(
                json!({"id":"message-2","type":"message","role":"assistant","content":[{"type":"text","text":"done"}],"stop_reason":"end_turn","usage":{"input_tokens":2,"output_tokens":1}}),
            ),
        ),
    ] {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.toml");
        std::fs::write(&config, format!("model='work/vendor/model'\n[accounts.work]\nprovider='cloudflare'\nsettings={{account_id='private-account',gateway_id='private-gateway'}}\nprotocol='{protocol}'\nmodels=['vendor/model']\ncontext_window=32000")).unwrap();
        let env = crate::config::Environment::new(None, Some(root.path().into()), Some(config))
            .with_variable("CLOUDFLARE_API_TOKEN", "private-token");
        let providers =
            Providers::from_config(&crate::config::Config::load(&env, root.path()).unwrap())
                .unwrap();
        let server = Server::start(vec![response]);
        let mut runtime = Runtime::builtins(crate::agent::Limits::default())
            .unwrap()
            .with_auth(env, Arc::new(crate::http::AsyncLoginHttp));
        runtime.inference_http = Some(crate::http::InferenceHttp::test_routed(server.url.clone()));
        let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
        let client = Client.v2().on_receive_notification(
            async move |update: v2::UpdateSessionNotification, _| {
                tx.send(update).unwrap();
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        );
        tokio::time::timeout(
            std::time::Duration::from_secs(15),
            client.connect_with(agent_with_runtime(providers, runtime), async |connection| {
                connection
                    .send_request(v2::InitializeRequest::new(
                        2.into(),
                        v2::Implementation::new("cloudflare-test", "0"),
                    ))
                    .block_task()
                    .await?;
                let session = connection
                    .send_request(v2::NewSessionRequest::new(root.path()))
                    .block_task()
                    .await?
                    .session_id;
                connection
                    .send_request(v2::PromptRequest::new(session, vec!["hello".into()]))
                    .block_task()
                    .await?;
                loop {
                    let update = updates.recv().await.unwrap();
                    let wire = serde_json::to_string(&update).unwrap();
                    for private in ["private-account", "private-gateway", "private-token"] {
                        assert!(!wire.contains(private));
                    }
                    if let v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle)) =
                        update.update
                    {
                        assert_eq!(idle.stop_reason, Some(v2::StopReason::EndTurn));
                        break;
                    }
                }
                Ok(())
            }),
        )
        .await
        .unwrap()
        .unwrap();
        let request = server.request();
        assert!(request.head.starts_with(&format!(
            "POST /prefix/client/v4/accounts/private-account/ai/v1/{suffix} "
        )));
        assert!(request.head.contains("authorization: Bearer private-token"));
        assert!(request.head.contains("cf-aig-gateway-id: private-gateway"));
        assert_eq!(request.body["model"], "vendor/model");
        server.assert_no_more_requests();
    }
}
