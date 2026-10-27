//! Cloud credentials and native tools exercised through the production ACP boundary.

use super::*;
use crate::auth::google_cloud::{Http, Request};
use crate::test_support::{Response, Server};
use agent_client_protocol::Client;
use serde_json::{Value, json};

struct Tokens;
impl Http for Tokens {
    fn request(&self, request: Request) -> futures::future::BoxFuture<'_, crate::Result<Value>> {
        Box::pin(async move {
            assert!(matches!(request, Request::Token(_)));
            Ok(json!({"access_token":"cloud-acp-secret","token_type":"Bearer","expires_in":3600}))
        })
    }
}

fn setup(root: &std::path::Path) -> (Providers, crate::config::Environment) {
    let config = root.join("config.toml");
    let adc = root.join("adc.json");
    std::fs::write(&adc, json!({"type":"authorized_user","client_id":"client","client_secret":"private-client","refresh_token":"private-refresh"}).to_string()).unwrap();
    std::fs::write(&config, "provider='work'\n[providers.work]\npreset='google-cloud'\nmodel='google/gemini-2.5-flash'\ngoogle_cloud={project='fixture-project'}").unwrap();
    let env = crate::config::Environment::new(None, Some(root.into()), Some(config))
        .with_variable("GOOGLE_APPLICATION_CREDENTIALS", adc.to_str().unwrap());
    let providers =
        Providers::from_config(&crate::config::Config::load(&env, root).unwrap()).unwrap();
    (providers, env)
}

async fn idle(
    updates: &mut tokio::sync::mpsc::UnboundedReceiver<v2::UpdateSessionNotification>,
) -> v2::StopReason {
    loop {
        let update = updates.recv().await.unwrap();
        let wire = serde_json::to_string(&update).unwrap();
        for secret in [
            "cloud-acp-secret",
            "private-client",
            "private-refresh",
            "private-thought",
        ] {
            assert!(!wire.contains(secret));
        }
        if let v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle)) = update.update {
            return idle.stop_reason.unwrap();
        }
    }
}

#[tokio::test]
async fn cloud_acp_executes_signed_native_tools_and_retains_successful_context() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("payload.txt"), "fixture-payload").unwrap();
    let (providers, env) = setup(root.path());
    let mut index = 0;
    let server = Server::respond(3, move |_| {
        index += 1;
        let (delta, finish) = if index == 1 {
            (
                json!({"tool_calls":[{"index":0,"id":"read-1","type":"function","function":{"name":"read","arguments":"{\"path\":\"payload.txt\"}"},"extra_content":{"google":{"thought_signature":"private-thought"}}}]}),
                "tool_calls",
            )
        } else {
            (json!({"content":"done"}), "stop")
        };
        Response::sse(format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"index":0,"delta":delta,"finish_reason":finish}]})
        ))
    });
    let mut runtime = Runtime::builtins(crate::agent::Limits::default())
        .unwrap()
        .with_auth(env, Arc::new(crate::http::AsyncLoginHttp))
        .with_google_cloud_http(Arc::new(Tokens))
        .with_history(crate::history::Store::open(root.path().join("state")).unwrap());
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
                    v2::Implementation::new("cloud-test", "0"),
                ))
                .block_task()
                .await?;
            let session = connection
                .send_request(v2::NewSessionRequest::new(root.path()))
                .block_task()
                .await?
                .session_id;
            connection
                .send_request(v2::PromptRequest::new(
                    session.clone(),
                    vec!["read payload".into()],
                ))
                .block_task()
                .await?;
            assert_eq!(idle(&mut updates).await, v2::StopReason::EndTurn);
            connection
                .send_request(v2::PromptRequest::new(session, vec!["continue".into()]))
                .block_task()
                .await?;
            assert_eq!(idle(&mut updates).await, v2::StopReason::EndTurn);
            Ok(())
        }),
    )
    .await
    .unwrap()
    .unwrap();
    let first = server.request();
    let second = server.request();
    let third = server.request();
    assert!(
        first
            .head
            .contains("authorization: Bearer cloud-acp-secret")
    );
    assert_eq!(first.body["model"], "google/gemini-2.5-flash");
    assert!(
        second.body["messages"]
            .to_string()
            .contains("fixture-payload")
    );
    assert!(
        second.body["messages"]
            .to_string()
            .contains("private-thought")
    );
    assert!(
        third.body["messages"]
            .to_string()
            .contains("fixture-payload")
    );
    server.assert_no_more_requests();
}

struct PendingTokens {
    started: tokio::sync::Notify,
    dropped: std::sync::atomic::AtomicBool,
}
impl Http for PendingTokens {
    fn request(&self, _: Request) -> futures::future::BoxFuture<'_, crate::Result<Value>> {
        Box::pin(async move {
            struct Guard<'a>(&'a std::sync::atomic::AtomicBool);
            impl Drop for Guard<'_> {
                fn drop(&mut self) {
                    self.0.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }
            let _guard = Guard(&self.dropped);
            self.started.notify_one();
            futures::future::pending().await
        })
    }
}

#[tokio::test]
async fn cloud_authentication_is_dropped_before_cancelled_acp_idle() {
    let root = tempfile::tempdir().unwrap();
    let (providers, env) = setup(root.path());
    let tokens = Arc::new(PendingTokens {
        started: tokio::sync::Notify::new(),
        dropped: std::sync::atomic::AtomicBool::new(false),
    });
    let runtime = Runtime::builtins(crate::agent::Limits::default())
        .unwrap()
        .with_auth(env, Arc::new(crate::http::AsyncLoginHttp))
        .with_google_cloud_http(tokens.clone());
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
                    v2::Implementation::new("cloud-test", "0"),
                ))
                .block_task()
                .await?;
            let session = connection
                .send_request(v2::NewSessionRequest::new(root.path()))
                .block_task()
                .await?
                .session_id;
            connection
                .send_request(v2::PromptRequest::new(
                    session.clone(),
                    vec!["hello".into()],
                ))
                .block_task()
                .await?;
            tokens.started.notified().await;
            connection.send_notification(v2::CancelSessionNotification::new(session))?;
            assert_eq!(idle(&mut updates).await, v2::StopReason::Cancelled);
            assert!(tokens.dropped.load(std::sync::atomic::Ordering::SeqCst));
            Ok(())
        }),
    )
    .await
    .unwrap()
    .unwrap();
}
