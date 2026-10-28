//! Provider and model selection over ACP, in-process.
//!
//! The provider and model a session uses are session settings the client may
//! change; the profile's endpoint, headers and credential are configuration it
//! may not (ADR-007). Wire fixtures also exercise successful-run recency.

mod support;

use std::fs;
use std::future;
use std::path::PathBuf;
use std::time::Duration;

use agent_client_protocol::schema::{ProtocolVersion, v2};
use agent_client_protocol::{Agent, Client, Error, ErrorCode, V2ConnectionTo};
use bee::account::Accounts as Providers;
use bee::config::{Config, Environment};

// Every wait is bounded: a hung agent must fail the test, not stall CI.
const TIMEOUT: Duration = Duration::from_secs(10);

const TWO_PROFILES: &str = r#"
model = "work/corp/ft-2026"

[accounts.work]
provider = "custom"
discovery = false
protocols = { openai-chat = { base_url = "https://gateway.corp.example/v1" } }
models = ["corp/ft-2026", "corp/base"]

[accounts.local]
provider = "custom"
discovery = false
protocols = { openai-chat = { base_url = "http://localhost:11434/v1" } }
models = ["llama3:8b"]
"#;

fn providers(toml: &str) -> Providers {
    let dir = tempfile::tempdir().unwrap();
    let file = bee::config::project_config_path(dir.path());
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, toml).unwrap();
    let config = Config::load(&Environment::new(None, None, None), dir.path()).unwrap();
    Providers::from_config(&config).unwrap()
}

/// Connect to an agent with `providers` and run `test`.
async fn run<T>(
    providers: Providers,
    test: impl AsyncFnOnce(V2ConnectionTo<Agent>) -> Result<T, Error>,
) -> T {
    // A turn that never ends, so a test can hold a session busy.
    let body = |_reply: bee::acp::Reply| future::pending::<Result<(), bee::acp::TurnFailure>>();
    let client = Client.v2().connect_with(
        bee::acp::agent_configured(providers, body),
        async move |connection| {
            connection
                .send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    v2::Implementation::new("bee-test", "0"),
                ))
                .block_task()
                .await?;
            test(connection).await
        },
    );
    tokio::time::timeout(TIMEOUT, client)
        .await
        .expect("the exchange finishes within the timeout")
        .expect("the exchange succeeds")
}

fn cwd() -> PathBuf {
    std::env::temp_dir().join("bee-selection")
}

async fn new_session(connection: &V2ConnectionTo<Agent>) -> Result<v2::NewSessionResponse, Error> {
    connection
        .send_request(v2::NewSessionRequest::new(cwd()))
        .block_task()
        .await
}

async fn set(
    connection: &V2ConnectionTo<Agent>,
    session: &v2::SessionId,
    option: &str,
    value: &str,
) -> Result<Vec<v2::SessionConfigOption>, Error> {
    Ok(connection
        .send_request(v2::SetSessionConfigOptionRequest::new(
            session.clone(),
            option,
            value,
        ))
        .block_task()
        .await?
        .config_options)
}

/// The current value and the offered values of the selector `id`.
fn selector(options: &[v2::SessionConfigOption], id: &str) -> (String, Vec<String>) {
    let option = options
        .iter()
        .find(|option| option.config_id.to_string() == id)
        .unwrap_or_else(|| panic!("no `{id}` option in {options:?}"));
    let v2::SessionConfigKind::Select(select) = &option.kind else {
        panic!("`{id}` is not a selector");
    };
    let v2::SessionConfigSelectOptions::Ungrouped(offered) = &select.options else {
        panic!("`{id}` has grouped values");
    };
    (
        select.current_value.to_string(),
        offered.iter().map(|o| o.value.to_string()).collect(),
    )
}

fn invalid_params(error: &Error) -> bool {
    error.code == ErrorCode::InvalidParams
}

#[test]
fn session_settings_debug_never_discloses_configured_display_names() {
    let mut sessions = bee::session::Sessions::with_accounts(providers(
        "[accounts.test]\nprovider='openai'\ndisplay_name='label-secret-sentinel'",
    ));
    let id = sessions.create(cwd());
    let settings = sessions.settings(&id).unwrap().unwrap();
    assert_eq!(settings.account, None);
    assert_eq!(settings.model, None);
    assert_eq!(settings.accounts, ["test"]);
    assert_eq!(
        settings.account_display_names["test"],
        "label-secret-sentinel"
    );
    assert!(!format!("{settings:?}").contains("label-secret-sentinel"));
}

#[tokio::test(flavor = "current_thread")]
async fn provider_labels_disambiguate_connections_while_selection_and_resume_keep_ids() {
    let config = "model='personal/gemini-test'\n[accounts.personal]\nprovider='google-aistudio'\ndiscovery=false\nmodels=['gemini-test']\n[accounts.work]\nprovider='google-aistudio'\ndiscovery=false\nmodels=['gemini-test']\n[accounts.local]\nprovider='custom'\nprotocols={openai-chat={base_url='http://localhost:11434/v1'}}";
    run(providers(config), async |connection| {
        let session = new_session(&connection).await?;
        let labels = |options: &[v2::SessionConfigOption]| {
            let option = options
                .iter()
                .find(|o| o.config_id.to_string() == "model")
                .unwrap();
            let v2::SessionConfigKind::Select(select) = &option.kind else {
                panic!("selector")
            };
            let v2::SessionConfigSelectOptions::Ungrouped(offered) = &select.options else {
                panic!("ungrouped")
            };
            offered
                .iter()
                .map(|o| (o.value.to_string(), o.name.clone()))
                .collect::<Vec<_>>()
        };
        let expected = vec![
            (
                "personal/gemini-test".into(),
                "Google AI Studio / gemini-test (personal/gemini-test)".into(),
            ),
            (
                "work/gemini-test".into(),
                "Google AI Studio / gemini-test (work/gemini-test)".into(),
            ),
        ];
        assert_eq!(labels(&session.config_options), expected);
        assert_eq!(
            selector(&session.config_options, "model").0,
            "personal/gemini-test"
        );
        assert!(invalid_params(
            &set(
                &connection,
                &session.session_id,
                "provider",
                "Google AI Studio (work)"
            )
            .await
            .unwrap_err()
        ));
        let options = set(
            &connection,
            &session.session_id,
            "model",
            "work/gemini-test",
        )
        .await?;
        assert_eq!(selector(&options, "model").0, "work/gemini-test");
        assert_eq!(labels(&options), expected);
        connection
            .send_request(v2::CloseSessionRequest::new(session.session_id.clone()))
            .block_task()
            .await?;
        let resumed = connection
            .send_request(v2::ResumeSessionRequest::new(session.session_id, cwd()))
            .block_task()
            .await?;
        assert_eq!(labels(&resumed.config_options), expected);
        assert_eq!(
            selector(&resumed.config_options, "model").0,
            "work/gemini-test"
        );
        Ok(())
    })
    .await;
}

#[rstest::rstest]
#[case("error")]
#[case("refusal")]
#[case("truncated_refusal")]
#[case("cancel")]
#[case("max_tokens")]
#[case("preference_write_failure")]
#[case("preference_lock_contention")]
#[tokio::test]
async fn only_committed_successful_inference_changes_recency_across_new_sessions_and_restarts(
    #[case] outcome: &str,
) {
    use std::sync::Arc;
    let root = tempfile::tempdir().unwrap();
    let mut second = support::Response::json(support::chat_text("second answer"));
    match outcome {
        "error" => second.status = 500,
        "refusal" => second.body = serde_json::json!({"choices":[{"index":0,"message":{"role":"assistant","refusal":"declined"},"finish_reason":"stop"}]}).to_string(),
        "truncated_refusal" => second.body = serde_json::json!({"choices":[{"index":0,"message":{"role":"assistant","refusal":"declined"},"finish_reason":"length"}]}).to_string(),
        "max_tokens" => second.body = serde_json::json!({"choices":[{"index":0,"message":{"role":"assistant","content":"partial"},"finish_reason":"length"}]}).to_string(),
        "cancel" => second.delay = Duration::from_millis(300),
        "preference_write_failure" | "preference_lock_contention" => {}
        _ => unreachable!(),
    }
    let server = support::Server::start(vec![
        support::Response::json(support::chat_text("first answer")),
        second,
    ]);
    let profiles = providers(&format!(
        "model='test/private-model'\n[accounts.test]\nprovider='custom'\ndiscovery=false\nmodels=['private-model','recent','candidate']\nprotocols.openai-chat.base_url='{}'",
        server.url
    ));
    for phase in 0..3 {
        let history = bee::history::Store::open(root.path().join("state")).unwrap();
        let preferences = bee::selection::Store::open(&history).unwrap();
        let runtime = bee::acp::Runtime::new(Default::default(), Default::default())
            .with_history(history)
            .with_preferences(preferences)
            .with_auth(Environment::default(), Arc::new(bee::http::AsyncLoginHttp));
        let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
        let client = Client.v2().on_receive_notification(
            async move |update: v2::UpdateSessionNotification, _| {
                tx.send(update).map_err(Error::into_internal_error)
            },
            agent_client_protocol::on_receive_notification!(),
        );
        let server = &server;
        let cwd = root.path().to_owned();
        tokio::time::timeout(
            TIMEOUT,
            client.connect_with(
                bee::acp::agent_with_runtime(profiles.clone(), runtime),
                async move |connection| {
                    connection
                        .send_request(v2::InitializeRequest::new(
                            ProtocolVersion::V2,
                            v2::Implementation::new("test", "0"),
                        ))
                        .block_task()
                        .await?;
                    let session = connection
                        .send_request(v2::NewSessionRequest::new(cwd.clone()))
                        .block_task()
                        .await?;
                    let expected = if phase == 0 {
                        "test/private-model"
                    } else if phase == 2 && outcome == "max_tokens" {
                        "test/candidate"
                    } else {
                        "test/recent"
                    };
                    assert_eq!(selector(&session.config_options, "model").0, expected);
                    if phase < 2 {
                        let model = if phase == 0 { "recent" } else { "candidate" };
                        set(
                            &connection,
                            &session.session_id,
                            "model",
                            &format!("test/{model}"),
                        )
                        .await?;
                        // A selector change alone must not seed the next session.
                        let sibling = connection
                            .send_request(v2::NewSessionRequest::new(cwd.clone()))
                            .block_task()
                            .await?;
                        assert_eq!(selector(&sibling.config_options, "model").0, expected);
                        let preference_path = cwd.join("state/model-preferences.json");
                        let preference_lock =
                            if phase == 1 && outcome == "preference_lock_contention" {
                                let file = fs::OpenOptions::new()
                                    .read(true)
                                    .write(true)
                                    .open(cwd.join("state/model-preferences.lock"))
                                    .unwrap();
                                fs2::FileExt::lock_exclusive(&file).unwrap();
                                Some(file)
                            } else {
                                None
                            };
                        let previous_state = if phase == 1
                            && matches!(
                                outcome,
                                "preference_write_failure" | "preference_lock_contention"
                            ) {
                            let bytes = fs::read(&preference_path).unwrap();
                            // Corruption after startup exercises a failing second transaction without harming history.
                            if outcome == "preference_write_failure" {
                                fs::write(&preference_path, b"invalid-state").unwrap();
                            }
                            Some(bytes)
                        } else {
                            None
                        };
                        connection
                            .send_request(v2::PromptRequest::new(
                                session.session_id.clone(),
                                vec!["hello".into()],
                            ))
                            .block_task()
                            .await?;
                        if phase == 1 && outcome == "cancel" {
                            let request = server.request_async().await;
                            assert_eq!(request.body["model"], model);
                            connection.send_notification(v2::CancelSessionNotification::new(
                                session.session_id.clone(),
                            ))?;
                        }
                        let mut warned = false;
                        loop {
                            let update = updates.recv().await.unwrap();
                            warned |= matches!(&update.update, v2::SessionUpdate::Notice(_))
                                && serde_json::to_string(&update.update)
                                    .unwrap()
                                    .contains("could not be saved");
                            if let v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle)) =
                                update.update
                            {
                                let reason = idle.stop_reason.unwrap();
                                if phase == 0 {
                                    assert_eq!(reason, v2::StopReason::EndTurn);
                                } else {
                                    match outcome {
                                        "error" => {
                                            assert!(matches!(reason, v2::StopReason::Error(_)))
                                        }
                                        "refusal" | "truncated_refusal" => {
                                            assert_eq!(reason, v2::StopReason::Refusal)
                                        }
                                        "cancel" => assert_eq!(reason, v2::StopReason::Cancelled),
                                        "max_tokens" => {
                                            assert_eq!(reason, v2::StopReason::MaxTokens)
                                        }
                                        "preference_write_failure"
                                        | "preference_lock_contention" => {
                                            assert_eq!(reason, v2::StopReason::EndTurn)
                                        }
                                        _ => unreachable!(),
                                    }
                                }
                                break;
                            }
                        }
                        if phase == 0 || outcome != "cancel" {
                            assert_eq!(server.request_async().await.body["model"], model);
                        }
                        if let Some(previous_state) = previous_state {
                            assert!(warned);
                            let expected_bytes: &[u8] = if outcome == "preference_write_failure" {
                                b"invalid-state"
                            } else {
                                &previous_state
                            };
                            assert_eq!(fs::read(&preference_path).unwrap(), expected_bytes);
                            let fresh = connection
                                .send_request(v2::NewSessionRequest::new(cwd.clone()))
                                .block_task()
                                .await?;
                            assert_eq!(
                                selector(&fresh.config_options, "model").0,
                                "test/candidate"
                            );
                            connection
                                .send_request(v2::CloseSessionRequest::new(fresh.session_id))
                                .block_task()
                                .await?;
                            // Restore the old valid disk state: the next process must still see its prior recency.
                            if outcome == "preference_write_failure" {
                                fs::write(&preference_path, previous_state).unwrap();
                            }
                        }
                        drop(preference_lock);
                        // Existing siblings never track later profile-wide recency.
                        let resumed = connection
                            .send_request(v2::ResumeSessionRequest::new(
                                sibling.session_id.clone(),
                                cwd.clone(),
                            ))
                            .block_task()
                            .await?;
                        assert_eq!(selector(&resumed.config_options, "model").0, expected);
                        connection
                            .send_request(v2::CloseSessionRequest::new(sibling.session_id))
                            .block_task()
                            .await?;
                    }
                    connection
                        .send_request(v2::CloseSessionRequest::new(session.session_id))
                        .block_task()
                        .await?;
                    Ok(())
                },
            ),
        )
        .await
        .unwrap()
        .unwrap();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn an_active_profile_exposes_only_its_connections_and_model_preferences() {
    let config = format!(
        "profile='work-context'\n{TWO_PROFILES}\n[profiles.work-context]\naccounts=['work']\nmodel='work/corp/base'\n"
    );
    run(providers(&config), async |connection| {
        let session = new_session(&connection).await?;
        assert_eq!(
            selector(&session.config_options, "model"),
            (
                "work/corp/base".into(),
                vec!["work/corp/ft-2026".into(), "work/corp/base".into()]
            )
        );
        assert!(invalid_params(
            &set(&connection, &session.session_id, "provider", "local")
                .await
                .unwrap_err()
        ));
        Ok(())
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn without_providers_no_option_is_offered() {
    run(Providers::default(), async |connection| {
        let session = new_session(&connection).await?;

        assert!(session.config_options.is_empty());
        let refused = set(&connection, &session.session_id, "model", "x").await;
        assert!(invalid_params(&refused.unwrap_err()));
        Ok(())
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_new_session_is_offered_the_configured_defaults() {
    run(providers(TWO_PROFILES), async |connection| {
        let options = new_session(&connection).await?.config_options;

        assert_eq!(options.len(), 1);
        assert_eq!(
            selector(&options, "model"),
            (
                "work/corp/ft-2026".to_owned(),
                vec![
                    "local/llama3:8b".to_owned(),
                    "work/corp/ft-2026".to_owned(),
                    "work/corp/base".to_owned()
                ]
            )
        );
        let model = options
            .iter()
            .find(|option| option.config_id.to_string() == "model")
            .unwrap();
        assert_eq!(model.category, Some(v2::SessionConfigOptionCategory::Model));
        Ok(())
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_sole_available_model_does_not_become_an_implicit_selection() {
    run(providers("[accounts.only]\nprovider='custom'\ndiscovery=false\nmodels=['vendor/model']\nprotocols.openai-chat.base_url='http://localhost:11434/v1'"), async |connection| {
        let session = new_session(&connection).await?;
        assert_eq!(selector(&session.config_options, "model"), (String::new(), vec![String::new(), "only/vendor/model".into()]));
        assert!(invalid_params(&set(&connection, &session.session_id, "model", "").await.unwrap_err()));
        let options = set(&connection, &session.session_id, "model", "only/vendor/model").await?;
        assert_eq!(selector(&options, "model"), ("only/vendor/model".into(), vec!["only/vendor/model".into()]));
        Ok(())
    }).await;
}

#[tokio::test]
async fn refreshed_account_exclusion_gates_both_explicit_selection_and_an_already_selected_prompt()
{
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    struct Listing(Arc<AtomicBool>);
    impl bee::models::Http for Listing {
        fn get<'a>(
            &'a self,
            _: &'a str,
            _: reqwest::header::HeaderMap,
            _: usize,
        ) -> futures::future::BoxFuture<'a, Result<Vec<u8>, bee::models::Error>> {
            let visible = self.0.load(Ordering::SeqCst);
            Box::pin(async move {
                Ok(if visible {
                    br#"{"data":[{"id":"vendor/model"}]}"#.to_vec()
                } else {
                    br#"{"data":[]}"#.to_vec()
                })
            })
        }
    }
    let endpoint =
        support::Server::start(vec![support::Response::json(support::chat_text("done"))]);
    let accounts = providers(&format!(
        "[accounts.a]\nprovider='custom'\ndiscovery=true\nprotocols.openai-chat.base_url='{}'",
        endpoint.url
    ));
    let visible = Arc::new(AtomicBool::new(true));
    let discovery = Arc::new(bee::models::Service::new(
        bee::models::Settings {
            enabled: false,
            ..Default::default()
        },
        Arc::new(Listing(visible.clone())),
        Arc::new(bee::models::FileCache(None)),
        Arc::new(bee::models::SystemClock),
    ));
    let account = accounts.get("a").unwrap().clone();
    discovery
        .list_endpoint(&account, &Environment::default())
        .await
        .unwrap();
    let runtime = bee::acp::Runtime::new(Default::default(), Default::default())
        .with_discovery(discovery.clone());
    let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let client = Client.v2().on_receive_notification(
        async move |update: v2::UpdateSessionNotification, _| {
            tx.send(update).map_err(Error::into_internal_error)
        },
        agent_client_protocol::on_receive_notification!(),
    );
    tokio::time::timeout(
        TIMEOUT,
        client.connect_with(
            bee::acp::agent_with_runtime(accounts, runtime),
            async move |connection| {
                connection
                    .send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("test", "0"),
                    ))
                    .block_task()
                    .await?;
                let session = new_session(&connection).await?;
                set(&connection, &session.session_id, "model", "a/vendor/model").await?;
                connection
                    .send_request(v2::PromptRequest::new(
                        session.session_id.clone(),
                        vec!["hello".into()],
                    ))
                    .block_task()
                    .await?;
                loop {
                    if let v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle)) =
                        updates.recv().await.unwrap().update
                    {
                        assert_eq!(idle.stop_reason, Some(v2::StopReason::EndTurn));
                        break;
                    }
                }
                assert_eq!(endpoint.request_async().await.body["model"], "vendor/model");
                visible.store(false, Ordering::SeqCst);
                discovery
                    .list_endpoint(&account, &Environment::default())
                    .await
                    .unwrap();
                assert!(invalid_params(
                    &set(&connection, &session.session_id, "model", "a/vendor/model")
                        .await
                        .unwrap_err()
                ));
                connection
                    .send_request(v2::PromptRequest::new(
                        session.session_id.clone(),
                        vec!["try again".into()],
                    ))
                    .block_task()
                    .await?;
                loop {
                    if let v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle)) =
                        updates.recv().await.unwrap().update
                    {
                        assert!(matches!(idle.stop_reason, Some(v2::StopReason::Error(_))));
                        break;
                    }
                }
                Ok::<_, Error>(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn the_client_can_choose_another_provider_for_its_session() {
    run(providers(TWO_PROFILES), async |connection| {
        let session = new_session(&connection).await?.session_id;

        let options = set(&connection, &session, "model", "local/llama3:8b").await?;

        // The model follows the provider: ids belong to one.
        assert_eq!(selector(&options, "model").0, "local/llama3:8b");
        Ok(())
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_model_absent_from_account_availability_is_rejected_without_changing_selection() {
    run(providers(TWO_PROFILES), async |connection| {
        let session = new_session(&connection).await?.session_id;

        assert!(invalid_params(
            &set(
                &connection,
                &session,
                "model",
                "work/me/private-finetune:v7"
            )
            .await
            .unwrap_err()
        ));
        let options = set(&connection, &session, "model", "work/corp/base").await?;
        assert_eq!(selector(&options, "model").0, "work/corp/base");
        Ok(())
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_selection_is_per_session() {
    run(providers(TWO_PROFILES), async |connection| {
        let first = new_session(&connection).await?.session_id;
        let second = new_session(&connection).await?.session_id;

        set(&connection, &first, "model", "local/llama3:8b").await?;

        let untouched = set(&connection, &second, "model", "work/corp/base").await?;
        assert_eq!(selector(&untouched, "model").0, "work/corp/base");
        Ok(())
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_resumed_session_reports_what_the_client_chose() {
    run(providers(TWO_PROFILES), async |connection| {
        let session = new_session(&connection).await?.session_id;
        set(&connection, &session, "model", "local/llama3:8b").await?;
        connection
            .send_request(v2::CloseSessionRequest::new(session.clone()))
            .block_task()
            .await?;

        let resumed = connection
            .send_request(v2::ResumeSessionRequest::new(session, cwd()))
            .block_task()
            .await?;

        assert_eq!(
            selector(&resumed.config_options, "model").0,
            "local/llama3:8b"
        );
        Ok(())
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_bad_selection_is_invalid_params() {
    run(providers(TWO_PROFILES), async |connection| {
        let session = new_session(&connection).await?.session_id;

        for (option, value) in [("provider", "ghost"), ("model", "  "), ("temperature", "1")] {
            let error = set(&connection, &session, option, value).await.unwrap_err();
            assert!(invalid_params(&error), "{option}={value}: {error:?}");
        }
        // The refusals left the session as it was.
        let options = set(&connection, &session, "model", "work/corp/base").await?;
        assert_eq!(selector(&options, "model").0, "work/corp/base");
        Ok(())
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_boolean_value_for_a_selector_is_invalid_params() {
    run(providers(TWO_PROFILES), async |connection| {
        let session = new_session(&connection).await?.session_id;

        let error = connection
            .send_request(v2::SetSessionConfigOptionRequest::new(
                session.clone(),
                "model",
                v2::SessionConfigOptionValue::boolean(true),
            ))
            .block_task()
            .await
            .unwrap_err();

        assert!(invalid_params(&error), "{error:?}");
        Ok(())
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn the_selection_cannot_change_while_a_turn_runs() {
    run(providers(TWO_PROFILES), async |connection| {
        let session = new_session(&connection).await?.session_id;
        connection
            .send_request(v2::PromptRequest::new(session.clone(), vec!["hi".into()]))
            .block_task()
            .await?;

        let error = set(&connection, &session, "model", "work/corp/base")
            .await
            .unwrap_err();

        assert_eq!(error.code, ErrorCode::InvalidRequest);
        Ok(())
    })
    .await;
}

#[rstest::rstest]
#[case::cancel(false)]
#[case::close(true)]
#[tokio::test]
async fn pending_selection_releases_the_registry_and_cancel_or_close_revokes_its_gate(
    #[case] close: bool,
) {
    use std::sync::Arc;
    struct Pending;
    impl bee::models::Http for Pending {
        fn get<'a>(
            &'a self,
            _: &'a str,
            _: reqwest::header::HeaderMap,
            _: usize,
        ) -> futures::future::BoxFuture<'a, Result<Vec<u8>, bee::models::Error>> {
            Box::pin(future::pending())
        }
    }
    let accounts = providers(
        "[accounts.pending]\nprovider='custom'\ndiscovery=true\nprotocols.openai-chat.base_url='https://pending.example/v1'\n[accounts.ready]\nprovider='custom'\nmodels=['vendor/model']\nprotocols.openai-chat.base_url='https://ready.example/v1'",
    );
    let discovery = Arc::new(bee::models::Service::new(
        bee::models::Settings {
            enabled: false,
            ..Default::default()
        },
        Arc::new(Pending),
        Arc::new(bee::models::FileCache(None)),
        Arc::new(bee::models::SystemClock),
    ));
    let runtime =
        bee::acp::Runtime::new(Default::default(), Default::default()).with_discovery(discovery);
    tokio::time::timeout(
        TIMEOUT,
        Client.v2().connect_with(
            bee::acp::agent_with_runtime(accounts, runtime),
            async move |connection| {
                connection
                    .send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("test", "0"),
                    ))
                    .block_task()
                    .await?;
                let pending = new_session(&connection).await?.session_id;
                let independent = new_session(&connection).await?.session_id;
                let waiting =
                    async { set(&connection, &pending, "model", "pending/vendor/model").await };
                let revoke = async {
                    // Ordered requests prove the pending operation owns its gate, without relying on sleeps.
                    let busy = set(&connection, &pending, "model", "ready/vendor/model")
                        .await
                        .unwrap_err();
                    assert_eq!(busy.code, ErrorCode::InvalidRequest);
                    let options =
                        set(&connection, &independent, "model", "ready/vendor/model").await?;
                    assert_eq!(selector(&options, "model").0, "ready/vendor/model");
                    if close {
                        connection
                            .send_request(v2::CloseSessionRequest::new(pending.clone()))
                            .block_task()
                            .await?;
                    } else {
                        connection.send_notification(v2::CancelSessionNotification::new(
                            pending.clone(),
                        ))?;
                    }
                    Ok::<_, Error>(())
                };
                let (cancelled, revoked) = futures::join!(waiting, revoke);
                assert!(invalid_params(&cancelled.unwrap_err()));
                revoked?;
                if close {
                    let resumed = connection
                        .send_request(v2::ResumeSessionRequest::new(pending.clone(), cwd()))
                        .block_task()
                        .await?;
                    assert_eq!(selector(&resumed.config_options, "model").0, "");
                }
                let options = set(&connection, &pending, "model", "ready/vendor/model").await?;
                assert_eq!(selector(&options, "model").0, "ready/vendor/model");
                Ok::<_, Error>(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
}
