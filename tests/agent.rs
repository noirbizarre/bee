//! Turn budgets and dispatch validation independent of ACP and native tools.

mod support;

use std::sync::atomic::{AtomicUsize, Ordering};

use bee::agent::{Executor, Limits, ToolResult, run_with_limits};
use bee::config::{Config, Environment, project_config_path};
use bee::llm::{Error, Message, Model};
use serde_json::{Value, json};
use support::{Response, Server, chat_text, profile, tool};

fn calls(ids: &[(&str, &str)]) -> Value {
    json!({"choices":[{"index":0,"message":{"role":"assistant","tool_calls":ids.iter().map(|(id, name)| json!({"id":id,"type":"function","function":{"name":name,"arguments":"{\"text\":\"hi\"}"}})).collect::<Vec<_>>()},"finish_reason":"tool_calls"}]})
}

#[derive(Default)]
struct Counting {
    count: AtomicUsize,
    failure: bool,
    oversized: bool,
}

impl Executor for Counting {
    fn execute<'a>(
        &'a self,
        _: &'a bee::llm::Call,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ToolResult, Error>> + Send + 'a>>
    {
        Box::pin(async move {
            self.count.fetch_add(1, Ordering::SeqCst);
            if self.failure {
                return Err(Error::Tool);
            }
            Ok(ToolResult {
                output: if self.oversized {
                    "x".repeat(bee::tool::MAX_OUTPUT_BYTES + 1)
                } else {
                    "ok".into()
                },
                is_error: false,
            })
        })
    }
}

fn model(server: &Server) -> Model {
    Model::new(profile("openai-chat", &server.url, ""), "test".into(), None).unwrap()
}

#[tokio::test]
async fn exact_step_and_call_budgets_allow_a_final_response() {
    let server = Server::start(vec![
        Response::json(calls(&[("a", "echo"), ("b", "echo")])),
        Response::json(chat_text("done")),
    ]);
    let executor = Counting::default();
    let outcome = run_with_limits(
        &model(&server),
        vec![Message::User("go".into())],
        &[tool()],
        Some(&executor),
        &mut |_| {},
        Limits::new(2, 2).unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(executor.count.load(Ordering::SeqCst), 2);
    assert_eq!(outcome.messages.len(), 5);
    server.request();
    let request = server.request();
    assert_eq!(request.body["messages"][2]["tool_call_id"], "a");
    assert_eq!(request.body["messages"][3]["tool_call_id"], "b");
}

#[rstest::rstest]
#[case::last_step(1, 64, vec![("a", "echo")], false)]
#[case::call_budget(2, 1, vec![("a", "echo"), ("b", "echo")], false)]
#[case::unknown_later_call(2, 64, vec![("a", "echo"), ("b", "unknown")], true)]
#[case::duplicate_id(2, 64, vec![("a", "echo"), ("a", "echo")], true)]
#[tokio::test]
async fn rejected_batches_execute_no_tools(
    #[case] steps: usize,
    #[case] budget: usize,
    #[case] batch: Vec<(&str, &str)>,
    #[case] tool_error: bool,
) {
    let server = Server::start(vec![Response::json(calls(&batch))]);
    let executor = Counting::default();
    let error = run_with_limits(
        &model(&server),
        vec![],
        &[tool()],
        Some(&executor),
        &mut |_| {},
        Limits::new(steps, budget).unwrap(),
    )
    .await
    .unwrap_err();
    assert!(if tool_error {
        matches!(error, Error::Tool)
    } else {
        matches!(error, Error::Limit)
    });
    assert_eq!(executor.count.load(Ordering::SeqCst), 0);
    server.request();
    server.assert_no_more_requests();
}

#[tokio::test]
async fn a_call_id_repeated_in_a_later_step_is_not_executed_twice() {
    let server = Server::start(vec![
        Response::json(calls(&[("a", "echo")])),
        Response::json(calls(&[("a", "echo")])),
    ]);
    let executor = Counting::default();
    let error = run_with_limits(
        &model(&server),
        vec![],
        &[tool()],
        Some(&executor),
        &mut |_| {},
        Limits::new(3, 64).unwrap(),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, Error::Tool));
    assert_eq!(executor.count.load(Ordering::SeqCst), 1);
}

#[rstest::rstest]
#[case::fatal(true, false)]
#[case::oversized(false, true)]
#[tokio::test]
async fn executor_failures_stop_before_another_model_request(
    #[case] failure: bool,
    #[case] oversized: bool,
) {
    let server = Server::start(vec![Response::json(calls(&[("a", "echo")]))]);
    let executor = Counting {
        failure,
        oversized,
        ..Counting::default()
    };
    let error = run_with_limits(
        &model(&server),
        vec![],
        &[tool()],
        Some(&executor),
        &mut |_| {},
        Limits::default(),
    )
    .await
    .unwrap_err();
    assert!(if failure {
        matches!(error, Error::Tool)
    } else {
        matches!(error, Error::Limit)
    });
    assert_eq!(executor.count.load(Ordering::SeqCst), 1);
    server.request();
    server.assert_no_more_requests();
}

#[tokio::test]
async fn duplicate_definitions_are_rejected_before_inference() {
    let server = Server::start(vec![]);
    let error = run_with_limits(
        &model(&server),
        vec![],
        &[tool(), tool()],
        None,
        &mut |_| {},
        Limits::default(),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, Error::Tool));
    server.assert_no_more_requests();
}

fn config(text: &str) -> Config {
    let root = tempfile::tempdir().unwrap();
    let path = project_config_path(root.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
    Config::load(&Environment::new(None, None, None), root.path()).unwrap()
}

#[test]
fn absent_and_partial_settings_preserve_default_budgets() {
    let defaults = Limits::from_config(&config("")).unwrap();
    assert_eq!((defaults.max_steps(), defaults.max_tool_calls()), (16, 64));
    let partial = Limits::from_config(&config("[agent]\nmax_steps = 2")).unwrap();
    assert_eq!((partial.max_steps(), partial.max_tool_calls()), (2, 64));
    assert_eq!(format!("{partial:?}"), "Limits([redacted])");
    assert!(Limits::new(0, 1).is_err());
    assert!(Limits::new(1, 0).is_err());
}

#[rstest::rstest]
#[case("agent = 'sensitive-value'")]
#[case("[agent]\nmax_steps = 'sensitive-value'")]
#[case("[agent]\nmax_tool_calls = false")]
#[case("[agent]\nmax_steps = 0")]
#[case("[agent]\nmax_tool_calls = -1")]
#[case("[agent]\nmax_steps = 1.5")]
#[case("[agent]\nmax_steps = []")]
#[case("[agent]\nmax_steps = {}")]
#[case("[agent]\nmisspelled = 'sensitive-value'")]
#[test]
fn invalid_settings_have_actionable_redacted_diagnostics(#[case] text: &str) {
    let error = Limits::from_config(&config(text)).unwrap_err();
    assert!(matches!(error, bee::Error::AgentSetting { .. }));
    let display = error.to_string();
    let debug = format!("{error:?}");
    let diagnostic = format!("{:?}", miette::Report::new(error));
    for rendered in [display, debug, diagnostic] {
        assert!(rendered.contains("agent"));
        assert!(!rendered.contains("sensitive-value"));
    }
}

#[test]
fn layered_limits_keep_the_origin_of_each_leaf() {
    let root = tempfile::tempdir().unwrap();
    let xdg = root.path().join("xdg");
    let global = xdg.join("bee/config.toml");
    std::fs::create_dir_all(global.parent().unwrap()).unwrap();
    std::fs::write(&global, "[agent]\nmax_steps = 4\nmax_tool_calls = 8").unwrap();
    let selected = root.path().join("selected.toml");
    std::fs::write(&selected, "[agent]\nmax_steps = 3").unwrap();
    let project = project_config_path(root.path());
    std::fs::create_dir_all(project.parent().unwrap()).unwrap();
    std::fs::write(&project, "[agent]\nmax_tool_calls = 2").unwrap();
    let env = Environment::new(Some(xdg), None, Some(selected.clone()));
    let merged = Config::load(&env, root.path()).unwrap();
    let limits = Limits::from_config(&merged).unwrap();
    assert_eq!((limits.max_steps(), limits.max_tool_calls()), (3, 2));
    std::fs::write(&selected, "[agent]\nmax_steps = 0").unwrap();
    let error = Limits::from_config(&Config::load(&env, root.path()).unwrap()).unwrap_err();
    match error {
        bee::Error::AgentSetting { key, path, .. } => {
            assert_eq!(key, "agent.max_steps");
            assert_eq!(path, selected.display().to_string());
        }
        _ => panic!("expected an agent policy diagnostic"),
    }
}
