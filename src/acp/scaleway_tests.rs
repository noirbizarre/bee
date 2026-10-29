//! Selected account authority must survive the production ACP availability gate.

use super::*;
use crate::test_support::{Response, Server};
use serde_json::json;

#[tokio::test]
async fn scaleway_acp_selection_keeps_project_routing_private_and_account_bound() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config.toml");
    let project = "78e655b5-feb0-417c-bb3f-8c448bd0e8da";
    std::fs::write(&config, format!("[accounts.work]\nprovider='scaleway'\nsettings={{project_id='{project}',region='fr-par'}}\ncredential={{env='WORK_KEY'}}\nmodels=['raw/model']\ncontext_window=32000\n[accounts.personal]\nprovider='scaleway'\ncredential={{env='PERSONAL_KEY'}}\nmodels=['raw/model']\ncontext_window=32000")).unwrap();
    let env = crate::config::Environment::new(None, Some(root.path().into()), Some(config))
        .with_variable("WORK_KEY", "private-work-key")
        .with_variable("PERSONAL_KEY", "private-personal-key");
    let accounts =
        Providers::from_config(&crate::config::Config::load(&env, root.path()).unwrap()).unwrap();
    let server = Server::start((0..2).map(|_| Response::sse(format!("data: {}\n\ndata: [DONE]\n\n", json!({"choices":[{"index":0,"delta":{"content":"done"},"finish_reason":"stop"}]})))).collect());
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
        client.connect_with(agent_with_runtime(accounts, runtime), async |connection| {
            connection
                .send_request(v2::InitializeRequest::new(
                    2.into(),
                    v2::Implementation::new("scaleway-test", "0"),
                ))
                .block_task()
                .await?;
            let created = connection
                .send_request(v2::NewSessionRequest::new(root.path()))
                .block_task()
                .await?;
            let wire = serde_json::to_string(&created).unwrap();
            assert!(wire.contains("work/raw/model"));
            assert!(wire.contains("personal/raw/model"));
            assert!(!wire.contains(project));
            assert!(!wire.contains("private-work-key"));
            let session = created.session_id;
            for account in ["work", "personal"] {
                let selected = connection
                    .send_request(v2::SetSessionConfigOptionRequest::new(
                        session.clone(),
                        "model",
                        format!("{account}/raw/model").as_str(),
                    ))
                    .block_task()
                    .await?;
                let wire = serde_json::to_string(&selected).unwrap();
                assert!(!wire.contains(project));
                assert!(!wire.contains("private-"));
                connection
                    .send_request(v2::PromptRequest::new(
                        session.clone(),
                        vec!["hello".into()],
                    ))
                    .block_task()
                    .await?;
                loop {
                    let update = updates.recv().await.unwrap();
                    let wire = serde_json::to_string(&update).unwrap();
                    for private in [project, "private-work-key", "private-personal-key"] {
                        assert!(!wire.contains(private));
                    }
                    if let v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle)) =
                        update.update
                    {
                        assert_eq!(idle.stop_reason, Some(v2::StopReason::EndTurn));
                        break;
                    }
                }
            }
            Ok(())
        }),
    )
    .await
    .unwrap()
    .unwrap();
    for (path, key) in [
        (
            format!("/{project}/v1/chat/completions"),
            "private-work-key",
        ),
        ("/v1/chat/completions".into(), "private-personal-key"),
    ] {
        let request = server.request();
        assert!(request.head.starts_with(&format!("POST /prefix{path} ")));
        assert!(
            request
                .head
                .contains(&format!("authorization: Bearer {key}"))
        );
        assert_eq!(request.body["model"], "raw/model");
        assert!(!request.body.to_string().contains(project));
        assert!(!request.body.to_string().contains("private-"));
    }
    server.assert_no_more_requests();
}
