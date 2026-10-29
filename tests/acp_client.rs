//! Public outbound API against an independent deterministic stdio peer.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use bee::acp::client::{
    self, ClientOptions, Error, HostHandlers, MaybeUndefined, config::Agents, v2,
};
use bee::config::{Config, Environment};
use tokio_util::sync::CancellationToken;

fn fixture() -> &'static Path {
    static FIXTURE: OnceLock<PathBuf> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let manifest =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/acp-agent/Cargo.toml");
        let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/acp-client-fixture");
        // A separate target avoids a nested Cargo invocation waiting on the parent test build.
        let output = std::process::Command::new(env!("CARGO"))
            .args(["build", "--offline", "--locked", "--manifest-path"])
            .arg(manifest)
            .arg("--target-dir")
            .arg(&target)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        target.join("debug").join(format!(
            "bee-acp-client-fixture{}",
            std::env::consts::EXE_SUFFIX
        ))
    })
}

fn configuration(text: &str) -> (tempfile::TempDir, Config) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, text).unwrap();
    let config = Config::load(&Environment::new(None, None, Some(path)), dir.path()).unwrap();
    (dir, config)
}

fn agents(mode: &str, env: &[(&str, &Path)]) -> (tempfile::TempDir, Agents) {
    // toml's parser-only feature cannot serialize; JSON strings are valid TOML basic strings.
    let mut text = format!(
        "[agents.test]\ncommand = {}\nargs = [{}]\n",
        serde_json::to_string(&fixture().display().to_string()).unwrap(),
        serde_json::to_string(mode).unwrap()
    );
    text.push_str("[agents.test.env]\n");
    for (key, path) in env {
        text.push_str(&format!(
            "{key} = {}\n",
            serde_json::to_string(&path.display().to_string()).unwrap()
        ));
    }
    let (dir, config) = configuration(&text);
    (dir, Agents::from_config(&config).unwrap())
}

fn options() -> ClientOptions {
    ClientOptions {
        request_timeout: Duration::from_secs(2),
        prompt_timeout: Duration::from_secs(2),
        cleanup_timeout: Duration::from_millis(150),
        ..ClientOptions::default()
    }
}

#[tokio::test]
async fn acceptance_and_terminal_idle_are_independent_and_sessions_support_repeated_prompts() {
    for mode in ["normal", "idle-first"] {
        let markers = tempfile::tempdir().unwrap();
        let closed = markers.path().join("closed");
        let (_dir, agents) = agents(mode, &[("FIXTURE_CLOSED", &closed)]);
        client::connect_with(agents.get("test").unwrap(), HostHandlers::default(), options(), CancellationToken::new(), |connection| Box::pin(async move {
            connection.new_session(Path::new(env!("CARGO_MANIFEST_DIR"))).await?;
            for _ in 0..2 {
                let result = connection.start_prompt(vec!["hello".into()], CancellationToken::new())?.finish().await?;
                assert_eq!(result.acceptance.message_id, v2::MessageId::new("u"));
                let MaybeUndefined::Value(content) = &result.messages[0].content else { panic!("missing content"); };
                assert!(matches!(&content[0], v2::ContentBlock::Text(text) if text.text == "replacement"));
            }
            Ok(())
        })).await.unwrap();
        assert_eq!(std::fs::read_to_string(closed).unwrap(), "closed");
    }
}

#[tokio::test]
async fn invalid_peers_and_timeouts_produce_value_redacted_errors() {
    for mode in [
        "v1",
        "no-sessions",
        "malformed",
        "hang-init",
        "eof",
        "stderr",
        "reject",
        "hang",
    ] {
        let (_dir, agents) = agents(mode, &[]);
        let mut settings = options();
        settings.request_timeout = Duration::from_millis(150);
        settings.prompt_timeout = Duration::from_millis(150);
        let error = client::connect_with(
            agents.get("test").unwrap(),
            HostHandlers::default(),
            settings,
            CancellationToken::new(),
            |connection| {
                Box::pin(async move {
                    connection
                        .new_session(Path::new(env!("CARGO_MANIFEST_DIR")))
                        .await?;
                    connection
                        .start_prompt(vec!["hello".into()], CancellationToken::new())?
                        .finish()
                        .await?;
                    Ok(())
                })
            },
        )
        .await
        .unwrap_err();
        let rendered = format!("{error:?} {error} {:?}", miette::Report::new(error.clone()));
        assert!(!rendered.contains("SECRET_FIXTURE"), "{mode}: {rendered}");
    }
}

#[tokio::test]
async fn permission_allow_reject_and_cancel_preserve_offered_structured_outcomes() {
    for outcome in ["yes", "no", "cancel"] {
        let markers = tempfile::tempdir().unwrap();
        let response_file = markers.path().join("response");
        let (_dir, agents) = agents("permission", &[("FIXTURE_PERMISSION", &response_file)]);
        let handlers = HostHandlers {
            permission: Some(Arc::new(move |request, _| {
                Box::pin(async move {
                    assert_eq!(request.options.len(), 2);
                    let response = if outcome == "cancel" {
                        v2::RequestPermissionOutcome::Cancelled
                    } else {
                        v2::RequestPermissionOutcome::Selected(v2::SelectedPermissionOutcome::new(
                            outcome,
                        ))
                    };
                    Ok(v2::RequestPermissionResponse::new(response))
                })
            })),
            ..HostHandlers::default()
        };
        client::connect_with(
            agents.get("test").unwrap(),
            handlers,
            options(),
            CancellationToken::new(),
            |connection| {
                Box::pin(async move {
                    connection
                        .new_session(Path::new(env!("CARGO_MANIFEST_DIR")))
                        .await?;
                    connection
                        .start_prompt(vec!["hello".into()], CancellationToken::new())?
                        .finish()
                        .await?;
                    Ok(())
                })
            },
        )
        .await
        .unwrap();
        let response: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(response_file).unwrap()).unwrap();
        if outcome == "cancel" {
            assert_eq!(response["outcome"]["outcome"], "cancelled");
        } else {
            assert_eq!(response["outcome"]["optionId"], outcome);
        }
    }
}

#[tokio::test]
async fn missing_permission_handlers_fail_closed_and_limits_do_not_deadlock_dispatch() {
    for mode in ["permission", "flood"] {
        let (_dir, agents) = agents(mode, &[]);
        let mut settings = options();
        settings.max_updates = 1;
        let error = client::connect_with(
            agents.get("test").unwrap(),
            HostHandlers::default(),
            settings,
            CancellationToken::new(),
            |connection| {
                Box::pin(async move {
                    connection
                        .new_session(Path::new(env!("CARGO_MANIFEST_DIR")))
                        .await?;
                    connection
                        .start_prompt(vec!["hello".into()], CancellationToken::new())?
                        .finish()
                        .await?;
                    Ok(())
                })
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            Error::Interaction { .. } | Error::Limit { .. }
        ));
    }
}

#[test]
fn configuration_validates_all_definitions_without_launching_or_exposing_values() {
    for text in [
        "[agents.bad]\nargs = []",
        "[agents.bad]\ncommand = 'SECRET_COMMAND'\nargs = [3]",
        "[agents.bad]\ncommand = 'SECRET_COMMAND'\ntransport = 'remote'",
        "[agents.bad]\ncommand = 'SECRET_COMMAND'\nunknown = 'SECRET_VALUE'",
        "[agents.bad]\ncommand = 'SECRET_COMMAND'\n[agents.bad.env]\n'BAD=KEY' = 'SECRET_VALUE'",
    ] {
        let (_dir, config) = configuration(text);
        let error = Agents::from_config(&config).unwrap_err();
        assert!(!format!("{error:?} {error}").contains("SECRET"));
    }
    let (_dir, config) = configuration(
        "[agents.first]\ncommand = 'SECRET_MISSING_EXECUTABLE'\n[agents.second]\ncommand = 'another-missing-executable'",
    );
    let agents = Agents::from_config(&config).unwrap();
    assert_eq!(agents.names().collect::<Vec<_>>(), ["first", "second"]);
    assert!(!format!("{agents:?} {:?}", agents.get("first")).contains("SECRET"));
    assert!(matches!(agents.get("absent"), Err(Error::UnknownAgent)));
}

#[tokio::test]
async fn forms_preserve_typed_accept_decline_and_cancel_including_before_session_creation() {
    for mode in ["form", "form-setup"] {
        for action in ["accept", "decline", "cancel"] {
            let markers = tempfile::tempdir().unwrap();
            let response_file = markers.path().join("response");
            let (_dir, agents) = agents(mode, &[("FIXTURE_ELICITATION", &response_file)]);
            let handlers = HostHandlers {
                form: Some(Arc::new(move |request, _| {
                    Box::pin(async move {
                        assert!(matches!(&request.mode, v2::ElicitationMode::Form(_)));
                        assert_eq!(
                            matches!(request.scope(), v2::ElicitationScope::Request(_)),
                            mode == "form-setup"
                        );
                        let action = match action {
                            "accept" => v2::ElicitationAction::Accept(
                                v2::ElicitationAcceptAction::new().content(
                                    std::collections::BTreeMap::from([(
                                        "count".into(),
                                        v2::ElicitationContentValue::Integer(7),
                                    )]),
                                ),
                            ),
                            "decline" => v2::ElicitationAction::Decline,
                            _ => v2::ElicitationAction::Cancel,
                        };
                        Ok(v2::CreateElicitationResponse::new(action))
                    })
                })),
                ..HostHandlers::default()
            };
            client::connect_with(
                agents.get("test").unwrap(),
                handlers,
                options(),
                CancellationToken::new(),
                |connection| {
                    Box::pin(async move {
                        connection
                            .new_session(Path::new(env!("CARGO_MANIFEST_DIR")))
                            .await?;
                        connection
                            .start_prompt(vec!["hello".into()], CancellationToken::new())?
                            .finish()
                            .await?;
                        Ok(())
                    })
                },
            )
            .await
            .unwrap();
            let response: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(response_file).unwrap()).unwrap();
            assert_eq!(response["result"]["action"], action, "{mode}: {response}");
            if action == "accept" {
                assert_eq!(response["result"]["content"]["count"], 7);
            }
        }
    }
}

#[tokio::test]
async fn url_acceptance_is_distinct_from_later_external_completion() {
    let (_dir, agents) = agents("url", &[]);
    let completed = CancellationToken::new();
    let observed = completed.clone();
    let handlers = HostHandlers {
        url: Some(Arc::new(|request, _| {
            Box::pin(async move {
                assert!(matches!(request.mode, v2::ElicitationMode::Url(_)));
                Ok(v2::CreateElicitationResponse::new(
                    v2::ElicitationAction::Accept(v2::ElicitationAcceptAction::new()),
                ))
            })
        })),
        url_complete: Some(Arc::new(move |notification, _| {
            let completed = observed.clone();
            Box::pin(async move {
                assert_eq!(notification.elicitation_id, v2::ElicitationId::new("url-1"));
                completed.cancel();
                Ok(())
            })
        })),
        ..HostHandlers::default()
    };
    client::connect_with(
        agents.get("test").unwrap(),
        handlers,
        options(),
        CancellationToken::new(),
        move |connection| {
            Box::pin(async move {
                connection
                    .new_session(Path::new(env!("CARGO_MANIFEST_DIR")))
                    .await?;
                connection
                    .start_prompt(vec!["hello".into()], CancellationToken::new())?
                    .finish()
                    .await?;
                tokio::time::timeout(Duration::from_secs(1), completed.cancelled())
                    .await
                    .unwrap();
                Ok(())
            })
        },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn unsupported_elicitation_and_invalid_form_answers_remain_distinct_from_user_decline() {
    for invalid_answer in [false, true] {
        let (_dir, agents) = agents("form", &[]);
        let handlers = if invalid_answer {
            HostHandlers {
                form: Some(Arc::new(|_, _| {
                    Box::pin(async {
                        Ok(v2::CreateElicitationResponse::new(
                            v2::ElicitationAction::Accept(v2::ElicitationAcceptAction::new()),
                        ))
                    })
                })),
                ..HostHandlers::default()
            }
        } else {
            HostHandlers::default()
        };
        let error = client::connect_with(
            agents.get("test").unwrap(),
            handlers,
            options(),
            CancellationToken::new(),
            |connection| {
                Box::pin(async move {
                    connection
                        .new_session(Path::new(env!("CARGO_MANIFEST_DIR")))
                        .await?;
                    connection
                        .start_prompt(vec!["hello".into()], CancellationToken::new())?
                        .finish()
                        .await?;
                    Ok(())
                })
            },
        )
        .await
        .unwrap_err();
        if invalid_answer {
            assert!(matches!(error, Error::Interaction { .. }));
        } else {
            assert!(matches!(error, Error::Unsupported { .. }));
        }
    }
}

#[tokio::test]
async fn cancellation_resolves_a_pending_permission_without_blocking_protocol_dispatch() {
    let markers = tempfile::tempdir().unwrap();
    let response_file = markers.path().join("response");
    let closed = markers.path().join("closed");
    let (_dir, agents) = agents(
        "permission-wait",
        &[
            ("FIXTURE_PERMISSION", &response_file),
            ("FIXTURE_CLOSED", &closed),
        ],
    );
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let handlers = HostHandlers {
        permission: Some(Arc::new(move |_, _| {
            let trigger = trigger.clone();
            Box::pin(async move {
                trigger.cancel();
                std::future::pending().await
            })
        })),
        ..HostHandlers::default()
    };
    let error = client::connect_with(
        agents.get("test").unwrap(),
        handlers,
        options(),
        cancel,
        |connection| {
            Box::pin(async move {
                connection
                    .new_session(Path::new(env!("CARGO_MANIFEST_DIR")))
                    .await?;
                connection
                    .start_prompt(vec!["hello".into()], CancellationToken::new())?
                    .finish()
                    .await?;
                Ok(())
            })
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(error, Error::Cancelled));
    let response: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(response_file).unwrap()).unwrap();
    assert_eq!(response["outcome"]["outcome"], "cancelled");
    assert!(closed.exists());
}

#[tokio::test]
async fn dropping_a_prompt_prevents_reuse_until_session_cleanup_completes() {
    let (_dir, agents) = agents("hang", &[]);
    client::connect_with(
        agents.get("test").unwrap(),
        HostHandlers::default(),
        options(),
        CancellationToken::new(),
        |connection| {
            Box::pin(async move {
                connection
                    .new_session(Path::new(env!("CARGO_MANIFEST_DIR")))
                    .await?;
                drop(connection.start_prompt(vec!["hello".into()], CancellationToken::new())?);
                assert!(matches!(
                    connection.start_prompt(vec!["again".into()], CancellationToken::new()),
                    Err(Error::Busy)
                ));
                connection.close_session().await?;
                Ok(())
            })
        },
    )
    .await
    .unwrap();
}

#[cfg(unix)]
async fn assert_process_exited(path: &Path) {
    let pid: i32 = std::fs::read_to_string(path).unwrap().parse().unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("SDK must terminate and reap its child");
}

#[cfg(unix)]
#[tokio::test]
async fn dropping_the_connection_future_terminates_and_reaps_the_owned_child() {
    let markers = tempfile::tempdir().unwrap();
    let pid = markers.path().join("pid");
    let (_dir, agents) = agents("hang", &[("FIXTURE_PID", &pid)]);
    let started = CancellationToken::new();
    let signal = started.clone();
    let mut future = Box::pin(client::connect_with(
        agents.get("test").unwrap(),
        HostHandlers::default(),
        options(),
        CancellationToken::new(),
        move |connection| {
            Box::pin(async move {
                connection
                    .new_session(Path::new(env!("CARGO_MANIFEST_DIR")))
                    .await?;
                signal.cancel();
                std::future::pending::<client::Result<()>>().await
            })
        },
    ));
    tokio::select! {
        result = &mut future => panic!("scope ended unexpectedly: {result:?}"),
        () = started.cancelled() => {}
    }
    drop(future);
    assert_process_exited(&pid).await;
}

#[tokio::test]
async fn snapshot_null_and_omission_preserve_content_and_metadata_patch_semantics() {
    let (_dir, agents) = agents("projection", &[]);
    client::connect_with(
        agents.get("test").unwrap(),
        HostHandlers::default(),
        options(),
        CancellationToken::new(),
        |connection| {
            Box::pin(async move {
                connection
                    .new_session(Path::new(env!("CARGO_MANIFEST_DIR")))
                    .await?;
                let result = connection
                    .start_prompt(vec!["hello".into()], CancellationToken::new())?
                    .finish()
                    .await?;
                let message = serde_json::to_value(&result.messages[0]).unwrap();
                assert_eq!(message["content"][0]["text"], "after clear");
                assert_eq!(message["_meta"], serde_json::json!({"kept":1,"added":3}));
                Ok(())
            })
        },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn unresponsive_cleanup_is_bounded_and_invalid_permission_selections_fail_closed() {
    for mode in ["ignore-close", "ignore-cancel", "permission"] {
        let (_dir, agents) = agents(mode, &[]);
        let handlers = HostHandlers {
            permission: Some(Arc::new(|_, _| {
                Box::pin(async {
                    Ok(v2::RequestPermissionResponse::new(
                        v2::RequestPermissionOutcome::Selected(v2::SelectedPermissionOutcome::new(
                            "not-offered",
                        )),
                    ))
                })
            })),
            ..HostHandlers::default()
        };
        let mut settings = options();
        settings.prompt_timeout = Duration::from_millis(150);
        let error = client::connect_with(
            agents.get("test").unwrap(),
            handlers,
            settings,
            CancellationToken::new(),
            |connection| {
                Box::pin(async move {
                    connection
                        .new_session(Path::new(env!("CARGO_MANIFEST_DIR")))
                        .await?;
                    connection
                        .start_prompt(vec!["hello".into()], CancellationToken::new())?
                        .finish()
                        .await?;
                    Ok(())
                })
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            Error::Timeout { .. } | Error::Interaction { .. }
        ));
    }
}

#[tokio::test]
async fn launch_failures_exclude_executable_settings() {
    let (_dir, config) = configuration(
        "[agents.missing]\ncommand = '/SECRET_MISSING_EXECUTABLE'\nargs = ['SECRET_ARGUMENT']",
    );
    let agents = Agents::from_config(&config).unwrap();
    let error = client::connect_with(
        agents.get("missing").unwrap(),
        HostHandlers::default(),
        options(),
        CancellationToken::new(),
        |_| Box::pin(async { Ok(()) }),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, Error::Connection { .. }));
    assert!(!format!("{error:?} {error}").contains("SECRET"));
}

#[tokio::test]
async fn dropping_setup_or_close_does_not_allow_uncertain_session_reuse() {
    for mode in ["hang-new", "ignore-close"] {
        let (_dir, agents) = agents(mode, &[]);
        let result = client::connect_with(
            agents.get("test").unwrap(),
            HostHandlers::default(),
            options(),
            CancellationToken::new(),
            move |connection| {
                Box::pin(async move {
                    if mode == "hang-new" {
                        assert!(
                            tokio::time::timeout(
                                Duration::from_millis(50),
                                connection.new_session(Path::new(env!("CARGO_MANIFEST_DIR")))
                            )
                            .await
                            .is_err()
                        );
                    } else {
                        connection
                            .new_session(Path::new(env!("CARGO_MANIFEST_DIR")))
                            .await?;
                        assert!(
                            tokio::time::timeout(
                                Duration::from_millis(50),
                                connection.close_session()
                            )
                            .await
                            .is_err()
                        );
                    }
                    assert!(matches!(
                        connection
                            .new_session(Path::new(env!("CARGO_MANIFEST_DIR")))
                            .await,
                        Err(Error::Busy)
                    ));
                    Ok(())
                })
            },
        )
        .await;
        if mode == "hang-new" {
            result.unwrap();
        } else {
            assert!(matches!(
                result,
                Err(Error::Timeout {
                    stage: "session close"
                })
            ));
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn connection_drop_terminates_descendants_in_the_sdk_process_group() {
    let markers = tempfile::tempdir().unwrap();
    let pid = markers.path().join("pid");
    let descendant = markers.path().join("descendant");
    let (_dir, agents) = agents(
        "descendants",
        &[
            ("FIXTURE_PID", &pid),
            ("FIXTURE_DESCENDANT_PID", &descendant),
        ],
    );
    let started = CancellationToken::new();
    let signal = started.clone();
    let child_path = descendant.clone();
    let mut future = Box::pin(client::connect_with(
        agents.get("test").unwrap(),
        HostHandlers::default(),
        options(),
        CancellationToken::new(),
        move |connection| {
            Box::pin(async move {
                connection
                    .new_session(Path::new(env!("CARGO_MANIFEST_DIR")))
                    .await?;
                tokio::time::timeout(Duration::from_secs(2), async {
                    while !child_path.exists() {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .unwrap();
                signal.cancel();
                std::future::pending::<client::Result<()>>().await
            })
        },
    ));
    tokio::select! {
        result = &mut future => panic!("scope ended unexpectedly: {result:?}"),
        () = started.cancelled() => {}
    }
    drop(future);
    assert_process_exited(&pid).await;
    let pid: i32 = std::fs::read_to_string(descendant)
        .unwrap()
        .parse()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err() {
                break;
            }
            // A dead orphan may remain a zombie until the system's reaper runs.
            #[cfg(target_os = "linux")]
            if std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .is_ok_and(|s| s.contains(") Z "))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("descendant must stop when the SDK process group is dropped");
}
