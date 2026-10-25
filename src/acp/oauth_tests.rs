//! Successful browser login, renewal and native tool continuation through production ACP.

use super::*;
use crate::auth::{Audience, CredentialStore, FileStore};
use crate::test_support::{Response, Server, event};
use agent_client_protocol::{Client, Responder};
use serde_json::{Value, json};

fn stream(items: Vec<Value>) -> String {
    let mut wire = event(
        "response.created",
        json!({"type":"response.created","response":{"id":"response","status":"in_progress"}}),
    );
    for (index, item) in items.iter().enumerate() {
        wire += &event(
            "response.output_item.added",
            json!({"type":"response.output_item.added","output_index":index,"item":{"id":item["id"],"type":item["type"]}}),
        );
        if item["type"] == "function_call" {
            wire += &event(
                "response.function_call_arguments.delta",
                json!({"type":"response.function_call_arguments.delta","output_index":index,"item_id":item["id"],"delta":item["arguments"]}),
            );
            wire += &event(
                "response.function_call_arguments.done",
                json!({"type":"response.function_call_arguments.done","output_index":index,"item_id":item["id"],"arguments":item["arguments"]}),
            );
        }
        wire += &event(
            "response.output_item.done",
            json!({"type":"response.output_item.done","output_index":index,"item":item}),
        );
    }
    wire + &event(
        "response.completed",
        json!({"type":"response.completed","response":{"id":"response","status":"completed","output":items}}),
    )
}

async fn idle(updates: &mut tokio::sync::mpsc::UnboundedReceiver<v2::UpdateSessionNotification>) {
    loop {
        let update = updates.recv().await.unwrap();
        let wire = serde_json::to_string(&update).unwrap();
        for secret in [
            crate::test_chatgpt::ACCESS,
            crate::test_chatgpt::RENEWED,
            "fixture-refresh-secret",
            "fixture-authorization-secret",
        ] {
            assert!(!wire.contains(secret));
        }
        if let v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle)) = update.update {
            assert_eq!(idle.stop_reason, Some(v2::StopReason::EndTurn));
            return;
        }
    }
}

#[tokio::test]
async fn chatgpt_browser_login_renews_between_native_tool_requests_and_commits_history() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("payload.txt"), "fixture-file-content").unwrap();
    let config_path = root.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[providers.account]\npreset='chatgpt'\nmodel='available-model'",
    )
    .unwrap();
    let env = crate::config::Environment::new(None, Some(root.path().into()), Some(config_path));
    let config = crate::config::Config::load(&env, root.path()).unwrap();
    let profiles = Providers::from_config(&config).unwrap();
    let store = FileStore::from_environment(&env).unwrap();
    let server_store = store.clone();
    let mut request_index = 0;
    let server = Server::respond(3, move |_| {
        request_index += 1;
        if request_index == 1 {
            // Expiry during the first response proves that the next step renews independently.
            let credential = server_store
                .get(Audience::Provider("account"))
                .unwrap()
                .unwrap()
                .expiring_at(1);
            server_store
                .put(Audience::Provider("account"), &credential)
                .unwrap();
            Response::sse(stream(vec![
                json!({"id":"reasoning","type":"reasoning","summary":[],"encrypted_content":"opaque-reasoning"}),
                json!({"id":"call-item","type":"function_call","namespace":"bee","name":"read","call_id":"read-1","arguments":"{\"path\":\"payload.txt\"}"}),
            ]))
        } else {
            Response::sse(stream(vec![
                json!({"id":"answer","type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}),
            ]))
        }
    });
    let oauth = Arc::new(crate::test_chatgpt::OAuth::new());
    let mut runtime = Runtime::builtins(crate::agent::Limits::default())
        .unwrap()
        .with_auth(env, oauth.clone());
    runtime.inference_http = Some(crate::http::InferenceHttp::test_routed(server.url.clone()));
    let observed_oauth = oauth.clone();
    let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let (completed, mut completions) = tokio::sync::mpsc::unbounded_channel();
    let client = Client
        .v2()
        .on_receive_request(
            async move |request: v2::CreateElicitationRequest,
                        responder: Responder<v2::CreateElicitationResponse>,
                        _| {
                let request = serde_json::to_value(request).unwrap();
                let callback = observed_oauth.callback(request["url"].as_str().unwrap());
                assert!(!request.to_string().contains("id_token_hint"));
                responder.respond(serde_json::from_value(json!({"action":"accept"})).unwrap())?;
                assert!(reqwest::get(callback).await.unwrap().status().is_success());
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |update: v2::UpdateSessionNotification, _| {
                tx.send(update).unwrap();
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_notification(
            async move |update: v2::CompleteElicitationNotification, _| {
                completed.send(update).unwrap();
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        );
    tokio::time::timeout(std::time::Duration::from_secs(15),client.connect_with(agent_with_runtime(profiles,runtime),async |connection| {
        connection.send_request(serde_json::from_value::<v2::InitializeRequest>(json!({"protocolVersion":2,"info":{"name":"oauth-success","version":"0"},"capabilities":{"elicitation":{"url":{}}}})).unwrap()).block_task().await?;
        let session = connection.send_request(v2::NewSessionRequest::new(root.path())).block_task().await?.session_id;
        connection.send_request(v2::PromptRequest::new(session.clone(),vec!["read the payload".into()])).block_task().await?;
        idle(&mut updates).await;
        assert!(completions.try_recv().is_ok());
        connection.send_request(v2::PromptRequest::new(session,vec!["continue with the previous result".into()])).block_task().await?;
        idle(&mut updates).await;
        assert!(completions.try_recv().is_err(),"stored credentials must avoid a second login");
        Ok(())
    })).await.unwrap().unwrap();
    assert_eq!(
        *oauth.grants.lock().unwrap(),
        ["authorization_code", "refresh_token"]
    );
    let first = server.request();
    let second = server.request();
    let third = server.request();
    assert!(first.head.contains(&format!(
        "authorization: Bearer {}",
        crate::test_chatgpt::ACCESS
    )));
    assert!(second.head.contains(&format!(
        "authorization: Bearer {}",
        crate::test_chatgpt::RENEWED
    )));
    assert!(first.body.get("max_output_tokens").is_none());
    assert_eq!(first.body["tools"][0]["type"], "namespace");
    assert_eq!(
        second.body["input"][1]["encrypted_content"],
        "opaque-reasoning"
    );
    assert_eq!(second.body["input"][2]["namespace"], "bee");
    assert_eq!(second.body["input"][3]["call_id"], "read-1");
    assert!(
        second.body["input"][3]["output"]
            .as_str()
            .unwrap()
            .contains("fixture-file-content")
    );
    assert!(
        third.body["input"]
            .to_string()
            .contains("fixture-file-content")
    );
    assert!(third.body["input"].to_string().contains("read the payload"));
    assert_eq!(
        store
            .get(Audience::Provider("account"))
            .unwrap()
            .unwrap()
            .secret()
            .expose(),
        crate::test_chatgpt::RENEWED
    );
}
