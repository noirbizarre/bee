//! Bash behavior through the public native-tool boundary.

use bee::tool::{
    Context, Registry,
    bash::{Bash, Settings},
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

fn configuration(text: &str) -> (TempDir, bee::config::Config) {
    let root = TempDir::new().unwrap();
    let path = root.path().join("config.toml");
    std::fs::write(&path, text).unwrap();
    let config = bee::config::Config::load(
        &bee::config::Environment::new(None, None, Some(path)),
        root.path(),
    )
    .unwrap();
    (root, config)
}

fn registry(settings: Settings) -> Registry {
    let mut registry = Registry::default();
    registry.register(Bash::new(settings)).unwrap();
    registry
}

async fn run(root: &TempDir, input: Value, budget: usize) -> Value {
    let context = Context::new(root.path(), CancellationToken::new(), budget).unwrap();
    let output = registry(Settings::default())
        .invoke("bash", &input, &context)
        .await
        .unwrap();
    assert!(!output.is_error, "{}", output.json);
    assert!(output.json.len() <= budget);
    serde_json::from_str(&output.json).unwrap()
}

#[tokio::test]
async fn commands_receive_the_session_directory_and_separate_streams_and_exit_status() {
    let root = TempDir::new().unwrap();
    std::fs::write(root.path().join("note"), "from-cwd").unwrap();
    let result = run(&root, json!({"command":"read -r value < note; printf '%s' \"$value\"; printf problem >&2; exit 7"}), 4096).await;
    assert_eq!(result["data"]["stdout"], "from-cwd");
    assert_eq!(result["data"]["stderr"], "problem");
    assert_eq!(result["data"]["exit_code"], 7);
    assert_eq!(result["data"]["timed_out"], false);
    assert_eq!(
        run(&root, json!({"command":":"}), 256).await["data"]["exit_code"],
        0
    );
    assert_eq!(
        run(
            &root,
            json!({"command":"read -r value; printf '%s' $?"}),
            4096
        )
        .await["data"]["stdout"],
        "1"
    );
}

#[tokio::test]
async fn concurrent_contexts_do_not_change_the_parent_working_directory() {
    let before = std::env::current_dir().unwrap();
    let first = TempDir::new().unwrap();
    let second = TempDir::new().unwrap();
    std::fs::write(first.path().join("note"), "first").unwrap();
    std::fs::write(second.path().join("note"), "second").unwrap();
    let (a, b) = tokio::join!(
        run(&first, json!({"command":"printf '%s' \"$(<note)\""}), 4096),
        run(&second, json!({"command":"printf '%s' \"$(<note)\""}), 4096)
    );
    assert_eq!(a["data"]["stdout"], "first");
    assert_eq!(b["data"]["stdout"], "second");
    assert_eq!(std::env::current_dir().unwrap(), before);
}

#[tokio::test]
async fn missing_commands_are_completed_processes_but_missing_shells_are_launch_failures() {
    let root = TempDir::new().unwrap();
    let result = run(
        &root,
        json!({"command":"bee_command_that_does_not_exist"}),
        4096,
    )
    .await;
    assert_eq!(result["data"]["exit_code"], 127);
    assert!(!result["data"]["stderr"].as_str().unwrap().is_empty());
    let (_files, config) = configuration("[tools.bash]\nshell = 'bee_shell_that_does_not_exist'");
    let context = Context::new(root.path(), CancellationToken::new(), 4096).unwrap();
    let output = registry(Settings::from_config(&config).unwrap())
        .invoke("bash", &json!({"command":"true"}), &context)
        .await
        .unwrap();
    assert!(output.is_error);
    assert!(output.json.contains("install Bash"));
    let context =
        Context::new(root.path().join("missing"), CancellationToken::new(), 4096).unwrap();
    assert!(
        registry(Settings::default())
            .invoke("bash", &json!({"command":"true"}), &context)
            .await
            .unwrap()
            .is_error
    );
}

#[tokio::test]
async fn both_streams_are_drained_without_deadlock_and_truncation_keeps_valid_json() {
    let root = TempDir::new().unwrap();
    for budget in [256, 4096] {
        let result = run(&root, json!({"command":"for ((i=0;i<10000;i++)); do printf '\\001\\377é\"\\\\'; printf 'err\\n' >&2; done"}), budget).await;
        assert_eq!(result["data"]["exit_code"], 0);
        assert_eq!(result["data"]["stdout_truncated"], true);
        assert_eq!(result["data"]["stderr_truncated"], true);
        assert!(!result["data"]["stdout"].as_str().unwrap().is_empty());
        assert!(!result["data"]["stderr"].as_str().unwrap().is_empty());
    }
    let result = run(&root, json!({"command":"printf 'é\\377'"}), 4096).await;
    assert_eq!(result["data"]["stdout"], "é�");
    let result = run(
        &root,
        json!({"command":"printf '%1000s' ''; printf error >&2", "output_limit":256}),
        4096,
    )
    .await;
    assert_eq!(result["data"]["stdout_truncated"], true);
    assert_eq!(result["data"]["stderr"], "error");
}

#[tokio::test]
async fn a_timeout_retains_partial_output_and_reports_termination() {
    let root = TempDir::new().unwrap();
    let result = run(
        &root,
        json!({"command":"printf started; printf problem >&2; sleep 30", "timeout_ms":200}),
        4096,
    )
    .await;
    assert_eq!(result["data"]["stdout"], "started");
    assert_eq!(result["data"]["stderr"], "problem");
    assert_eq!(result["data"]["timed_out"], true);
}

#[tokio::test]
async fn invalid_arguments_and_host_ceiling_overrides_are_rejected() {
    let root = TempDir::new().unwrap();
    let context = Context::new(root.path(), CancellationToken::new(), 4096).unwrap();
    let registry = registry(Settings::default());
    for input in [
        json!({}),
        json!({"command":""}),
        json!({"command":"a\0b"}),
        json!({"command":"true","cwd":"."}),
        json!({"command":"true","timeout_ms":0}),
        json!({"command":"true","timeout_ms":600001}),
        json!({"command":"true","output_limit":255}),
        json!({"command":"true","output_limit":1048577}),
    ] {
        let output = registry.invoke("bash", &input, &context).await.unwrap();
        assert!(output.is_error);
        assert!(output.json.contains("invalid_arguments"));
    }
}

#[test]
fn settings_validate_types_ceilings_and_provenance_without_disclosing_values() {
    for text in [
        "[tools.bash]\nshell = 'secret/relative'",
        "[tools.bash]\nshell = 123",
        "[tools.bash]\nsecret_key = 'secret_value'",
        "[tools.bash]\ntimeout_ms = 0",
        "[tools.bash]\nmax_timeout_ms = 1",
        "[tools.bash]\noutput_limit = 255",
        "[tools.bash]\nmax_output_limit = 256",
        "tools = 'secret_value'",
        "[tools]\nbash = 'secret_value'",
    ] {
        let (_root, config) = configuration(text);
        let error = Settings::from_config(&config).unwrap_err();
        let diagnostic = format!("{error:?} {error}");
        assert!(diagnostic.contains("config.toml"));
        assert!(!diagnostic.contains("secret"));
    }
    let (root, config) = configuration(
        "[tools.bash]\ntimeout_ms=50\nmax_timeout_ms=100\noutput_limit=256\nmax_output_limit=512",
    );
    std::fs::create_dir(root.path().join(".config")).unwrap();
    std::fs::write(
        root.path().join(".config/bee.toml"),
        "[tools.bash]\ntimeout_ms=25",
    )
    .unwrap();
    let merged = bee::config::Config::load(
        &bee::config::Environment::new(None, None, Some(root.path().join("config.toml"))),
        root.path(),
    )
    .unwrap();
    assert!(Settings::from_config(&merged).is_ok());
    assert_eq!(
        merged.get("tools.bash.timeout_ms").unwrap().as_integer(),
        Some(25)
    );
    assert!(!format!("{:?}", Settings::from_config(&config).unwrap()).contains("50"));
}

#[tokio::test]
async fn a_pre_cancelled_invocation_never_starts_a_process() {
    let root = TempDir::new().unwrap();
    let token = CancellationToken::new();
    token.cancel();
    let context = Context::new(root.path(), token, 4096).unwrap();
    assert_eq!(
        registry(Settings::default())
            .invoke("bash", &json!({"command":"printf bad > started"}), &context)
            .await
            .unwrap_err(),
        bee::tool::Error::Cancelled
    );
    assert!(!root.path().join("started").exists());
}

#[cfg(unix)]
#[rstest::rstest]
#[case::cancel(0)]
#[case::drop_future(1)]
#[case::timeout(2)]
#[case::normal_exit(3)]
#[tokio::test]
async fn descendants_cannot_continue_after_the_invocation_ends(#[case] mode: u8) {
    let root = TempDir::new().unwrap();
    let registry = registry(Settings::default());
    let token = CancellationToken::new();
    let context = Context::new(root.path(), token.clone(), 4096).unwrap();
    // The child announces readiness before performing a delayed observable side effect.
    let suffix = if mode == 3 {
        "while [ ! -e ready ]; do sleep 0.01; done"
    } else {
        "wait"
    };
    let input = json!({"command":format!("(printf ready > ready; sleep 0.4; printf leaked > leaked) & {suffix}"), "timeout_ms":if mode == 2 {200} else {3000}});
    {
        let call = registry.invoke("bash", &input, &context);
        tokio::pin!(call);
        if mode >= 2 {
            assert!(!call.await.unwrap().is_error);
        } else {
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                loop {
                    tokio::select! {
                        result = &mut call => panic!("command ended before cancellation: {result:?}"),
                        () = tokio::time::sleep(std::time::Duration::from_millis(5)) => {
                            if root.path().join("ready").exists() { break; }
                        }
                    }
                }
            }).await.unwrap();
            if mode == 0 {
                token.cancel();
                assert_eq!(call.await.unwrap_err(), bee::tool::Error::Cancelled);
            }
        }
    }
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(!root.path().join("leaked").exists());
}
