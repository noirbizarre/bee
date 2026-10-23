//! Provider and model selection over ACP, in-process.
//!
//! The provider and model a session uses are session settings the client may
//! change; the profile's endpoint, headers and credential are configuration it
//! may not (ADR-007). No LLM is called yet, so these pin the options and their
//! precedence, which outlive the placeholder turn.

use std::fs;
use std::future;
use std::path::PathBuf;
use std::time::Duration;

use agent_client_protocol::schema::{ProtocolVersion, v2};
use agent_client_protocol::{Agent, Client, Error, ErrorCode, V2ConnectionTo};
use bee::config::{Config, Environment};
use bee::provider::Providers;

// Every wait is bounded: a hung agent must fail the test, not stall CI.
const TIMEOUT: Duration = Duration::from_secs(10);

const TWO_PROFILES: &str = r#"
provider = "work"

[providers.work]
protocol = "openai-chat"
base_url = "https://gateway.corp.example/v1"
model = "corp/ft-2026"
models = ["corp/base"]

[providers.local]
protocol = "openai-chat"
base_url = "http://localhost:11434/v1"
model = "llama3:8b"
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

        assert_eq!(
            selector(&options, "provider"),
            (
                "work".to_owned(),
                vec!["local".to_owned(), "work".to_owned()]
            )
        );
        assert_eq!(
            selector(&options, "model"),
            (
                "corp/ft-2026".to_owned(),
                vec!["corp/ft-2026".to_owned(), "corp/base".to_owned()]
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
async fn the_client_can_choose_another_provider_for_its_session() {
    run(providers(TWO_PROFILES), async |connection| {
        let session = new_session(&connection).await?.session_id;

        let options = set(&connection, &session, "provider", "local").await?;

        assert_eq!(selector(&options, "provider").0, "local");
        // The model follows the provider: ids belong to one.
        assert_eq!(selector(&options, "model").0, "llama3:8b");
        Ok(())
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_model_absent_from_every_list_can_be_selected_and_is_then_offered() {
    run(providers(TWO_PROFILES), async |connection| {
        let session = new_session(&connection).await?.session_id;

        let options = set(&connection, &session, "model", "me/private-finetune:v7").await?;

        let (current, offered) = selector(&options, "model");
        assert_eq!(current, "me/private-finetune:v7");
        assert!(offered.contains(&"me/private-finetune:v7".to_owned()));
        Ok(())
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_selection_is_per_session() {
    run(providers(TWO_PROFILES), async |connection| {
        let first = new_session(&connection).await?.session_id;
        let second = new_session(&connection).await?.session_id;

        set(&connection, &first, "provider", "local").await?;

        let untouched = set(&connection, &second, "model", "corp/base").await?;
        assert_eq!(selector(&untouched, "provider").0, "work");
        Ok(())
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_resumed_session_reports_what_the_client_chose() {
    run(providers(TWO_PROFILES), async |connection| {
        let session = new_session(&connection).await?.session_id;
        set(&connection, &session, "provider", "local").await?;
        connection
            .send_request(v2::CloseSessionRequest::new(session.clone()))
            .block_task()
            .await?;

        let resumed = connection
            .send_request(v2::ResumeSessionRequest::new(session, cwd()))
            .block_task()
            .await?;

        assert_eq!(selector(&resumed.config_options, "provider").0, "local");
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
        let options = set(&connection, &session, "model", "corp/base").await?;
        assert_eq!(selector(&options, "provider").0, "work");
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

        let error = set(&connection, &session, "model", "corp/base")
            .await
            .unwrap_err();

        assert_eq!(error.code, ErrorCode::InvalidRequest);
        Ok(())
    })
    .await;
}
