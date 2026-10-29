use super::*;

fn shared(options: ClientOptions) -> Shared {
    Shared {
        interactions: interactions::State::new(options.max_interactions),
        inbox: Mutex::new(Inbox {
            session: Some(v2::SessionId::new("s")),
            collecting: true,
            ..Inbox::default()
        }),
        wake: Notify::new(),
        closed: CancellationToken::new(),
        shutdown: CancellationToken::new(),
        session_cancel: Mutex::new(CancellationToken::new()),
        options,
    }
}

fn state(state: v2::StateUpdate) -> v2::UpdateSessionNotification {
    v2::UpdateSessionNotification::new("s", v2::SessionUpdate::StateUpdate(state))
}

#[test]
fn initial_idle_does_not_complete_work_and_requires_action_can_end_without_running() {
    let shared = shared(ClientOptions::default());
    shared.update(state(v2::StateUpdate::Idle(v2::IdleStateUpdate::new())));
    assert!(lock(&shared.inbox).idle.is_none());
    shared.update(state(v2::StateUpdate::RequiresAction(
        v2::RequiresActionStateUpdate::new(),
    )));
    shared.update(state(v2::StateUpdate::Idle(v2::IdleStateUpdate::new())));
    assert!(lock(&shared.inbox).idle.is_some());
    assert!(!lock(&shared.inbox).active);
}

#[test]
fn terminal_control_state_survives_a_full_progress_queue() {
    let shared = shared(ClientOptions {
        max_updates: 1,
        ..ClientOptions::default()
    });
    shared.update(state(v2::StateUpdate::Running(
        v2::RunningStateUpdate::new(),
    )));
    shared.update(state(v2::StateUpdate::Idle(v2::IdleStateUpdate::new())));
    let inbox = lock(&shared.inbox);
    assert_eq!(inbox.queue.len(), 1);
    assert!(matches!(inbox.error, Some(Error::Limit { .. })));
    assert!(inbox.idle.is_some());
}

#[test]
fn agent_definitions_merge_with_project_overrides_and_redact_dynamic_keys() {
    use crate::config::{Config, Environment};
    let root = tempfile::tempdir().unwrap();
    let global = root.path().join("xdg/bee");
    let project = root.path().join("project");
    std::fs::create_dir_all(&global).unwrap();
    std::fs::create_dir_all(project.join(".config")).unwrap();
    std::fs::write(global.join("config.toml"), "[agents.test]\ncommand = 'base'\nargs = ['old']\n[agents.test.env]\nINHERITED = 'kept'\nOVERRIDE = 'old'").unwrap();
    let selected = root.path().join("selected.toml");
    std::fs::write(
        &selected,
        "[agents.test]\nargs = ['new', 'argument']\n[agents.test.env]\nOVERRIDE = 'new'",
    )
    .unwrap();
    let path = project.join(".config/bee.toml");
    std::fs::write(
        &path,
        "[agents.test]\ncommand = 'project'\n[agents.test.env]\nPROJECT = ''",
    )
    .unwrap();
    let environment = Environment::new(Some(root.path().join("xdg")), None, Some(selected));
    let config = Config::load(&environment, &project).unwrap();
    let agents = config::Agents::from_config(&config).unwrap();
    let definition = agents.get("test").unwrap();
    assert_eq!(definition.command, "project");
    assert_eq!(definition.args, ["new", "argument"]);
    assert_eq!(definition.env.get("INHERITED").unwrap(), "kept");
    assert_eq!(definition.env.get("OVERRIDE").unwrap(), "new");
    assert_eq!(definition.env.get("PROJECT").unwrap(), "");
    std::fs::write(&path, "[agents.test]\nargs = 5").unwrap();
    let error = Config::load(&environment, &project).unwrap_err();
    assert!(matches!(error, crate::Error::ConfigConflict { .. }));
    std::fs::write(&path, "[agents.test.env]\nINHERITED = 5").unwrap();
    let error = Config::load(&environment, &project).unwrap_err();
    assert!(!format!("{error:?} {error}").contains("INHERITED"));
}

#[test]
fn invalid_runtime_limits_are_diagnostics_instead_of_timer_or_semaphore_panics() {
    for options in [
        ClientOptions {
            max_interactions: usize::MAX,
            ..ClientOptions::default()
        },
        ClientOptions {
            prompt_timeout: Duration::MAX,
            ..ClientOptions::default()
        },
        ClientOptions {
            max_updates: 0,
            ..ClientOptions::default()
        },
    ] {
        assert!(options.validate().is_err());
    }
}
