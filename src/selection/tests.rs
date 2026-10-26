use super::*;
#[cfg(unix)]
mod late;
use crate::config::Environment;
use crate::models::{EndpointListing, Snapshot};

fn resolve(
    reference: &Reference,
    profiles: &Providers,
    env: &Environment,
    snapshot: &Snapshot,
) -> Result<Reference, Reason> {
    let profile = profiles
        .get(&reference.connection)
        .ok_or(Reason::ConnectionRemoved)?;
    let credential = credential(profile, env)?;
    super::resolve_visible(reference, profiles, profile, credential.as_ref(), snapshot)
}

fn providers(extra: &str) -> Providers {
    crate::test_support::configured_providers(&format!(
        "provider='account'\n[providers.account]\nprotocols={{openai-chat={{base_url='https://example.test/v1'}}}}\nmodel='default-model'\n{extra}"
    ))
}

fn reference() -> Reference {
    Reference::new(
        providers("").get("account").unwrap(),
        "recent-model".into(),
        None,
    )
}

fn visible(models: &[&str]) -> Snapshot {
    let mut snapshot = Snapshot::default();
    snapshot.endpoints.insert(
        "account".into(),
        EndpointListing {
            fetched_at: Some(crate::auth::unix_now()),
            models: models.iter().map(|model| (*model).into()).collect(),
            ..Default::default()
        },
    );
    snapshot
}

#[test]
fn complete_current_visibility_constrains_recency_but_unknown_and_stale_visibility_do_not() {
    let profiles = providers("");
    let env = Environment::default();
    assert_eq!(
        resolve(&reference(), &profiles, &env, &Snapshot::default())
            .unwrap()
            .model,
        "recent-model"
    );
    assert_eq!(
        resolve(&reference(), &profiles, &env, &visible(&[])),
        Err(Reason::ModelUnavailable)
    );
    let fallback = resolve(&reference(), &profiles, &env, &visible(&["default-model"])).unwrap();
    assert_eq!(fallback.model, "default-model");
    assert!(fallback.fallback_notice().is_some());
    let mut stale = visible(&[]);
    stale.endpoints.get_mut("account").unwrap().stale = true;
    assert!(resolve(&reference(), &profiles, &env, &stale).is_ok());
    let listing = stale.endpoints.get_mut("account").unwrap();
    listing.stale = false;
    listing.fetched_at = Some(crate::auth::unix_now().saturating_sub(301));
    assert!(resolve(&reference(), &profiles, &env, &stale).is_ok());
    stale.endpoints.get_mut("account").unwrap().error = Some(crate::models::Error::Unauthorized);
    assert_eq!(
        resolve(&reference(), &profiles, &env, &stale),
        Err(Reason::CredentialsUnavailable)
    );
}

#[test]
fn unavailable_recency_never_falls_back_to_another_connection_or_account() {
    let profiles = crate::test_support::configured_providers(
        "provider='other'\n[providers.account]\nprotocols={openai-chat={}}\nmodel='default-model'\n[providers.other]\nprotocols={openai-chat={}}\nmodel='other-model'",
    );
    let recent = Reference::new(profiles.get("account").unwrap(), "gone".into(), None);
    assert_eq!(
        resolve(
            &recent,
            &profiles,
            &Environment::default(),
            &visible(&["default-model"])
        ),
        Err(Reason::ModelUnavailable)
    );
    assert_eq!(
        resolve(
            &reference(),
            &profiles,
            &Environment::default(),
            &Snapshot::default()
        ),
        Err(Reason::AccountChanged)
    );
    assert_eq!(
        resolve(
            &reference(),
            &Providers::default(),
            &Environment::default(),
            &Snapshot::default()
        ),
        Err(Reason::ConnectionRemoved)
    );
}

#[test]
fn credentials_and_target_continuity_are_checked_without_leaking_values() {
    let profiles = providers("credential={env='KEY'}\nheaders={Authorization='header-secret'}");
    let profile = profiles.get("account").unwrap();
    let env = Environment::default().with_variable("KEY", "first-secret");
    let key = credential(profile, &env).unwrap();
    let recent = Reference::new(profile, "custom".into(), key.as_ref());
    assert!(resolve(&recent, &profiles, &env, &Snapshot::default()).is_ok());
    assert_eq!(
        resolve(
            &recent,
            &profiles,
            &env.clone().with_variable("KEY", "second-secret"),
            &Snapshot::default()
        ),
        Err(Reason::AccountChanged)
    );
    assert_eq!(
        resolve(
            &recent,
            &profiles,
            &Environment::default(),
            &Snapshot::default()
        ),
        Err(Reason::CredentialsUnavailable)
    );
    let serialized = serde_json::to_string(&recent).unwrap();
    for secret in ["first-secret", "header-secret"] {
        assert!(!serialized.contains(secret));
        assert!(!format!("{recent:?}").contains(secret));
    }
    let defaults_changed = providers(
        "credential={env='KEY'}\nheaders={Authorization='header-secret'}\nmodels=['new-suggestion']",
    );
    assert!(resolve(&recent, &defaults_changed, &env, &Snapshot::default()).is_ok());
}

#[test]
fn concurrent_preference_updates_preserve_profiles_and_private_atomic_state() {
    let root = tempfile::tempdir().unwrap();
    let history = crate::history::Store::open(root.path().join("state")).unwrap();
    std::thread::scope(|scope| {
        for index in 0..12 {
            let store = Store::open(&history).unwrap();
            scope.spawn(move || {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                loop {
                    match store.remember(&format!("profile/{index}"), reference()) {
                        Ok(()) => break,
                        Err(error) => {
                            // Windows ACL work under coverage can exhaust the production one-second best-effort deadline.
                            // Retry only reported contention so this test verifies merging without weakening that deadline.
                            assert_eq!(error.reason, "recent model state is busy; retry later");
                            assert!(std::time::Instant::now() < deadline, "{error}");
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                    }
                }
            });
        }
    });
    let restarted = Store::open(&history).unwrap();
    for index in 0..12 {
        assert_eq!(
            restarted.get(&format!("profile/{index}")),
            Some(reference())
        );
    }
    crate::history::check_private(&history.root.join("model-preferences.json"), false).unwrap();
    assert!(!format!("{restarted:?}").contains(&reference().binding));
}

#[test]
fn invalid_preference_state_is_actionable_and_failed_writes_retain_memory_and_disk() {
    let root = tempfile::tempdir().unwrap();
    let history = crate::history::Store::open(root.path().join("state")).unwrap();
    let store = Store::open(&history).unwrap();
    store.remember("work", reference()).unwrap();
    let path = history.root.join("model-preferences.json");
    let invalid = b"{private-file-contents";
    std::fs::write(&path, invalid).unwrap();
    let error = Store::open(&history).unwrap_err();
    assert!(!format!("{error:?} {error}").contains("private-file-contents"));
    assert!(store.remember("personal", reference()).is_err());
    assert_eq!(store.get("personal"), Some(reference()));
    assert_eq!(std::fs::read(&path).unwrap(), invalid);
    std::fs::write(&path, br#"{"version":99,"profiles":{}}"#).unwrap();
    assert!(Store::open(&history).is_err());
}

#[test]
fn preference_lock_contention_is_bounded_and_retains_the_in_memory_recent_choice() {
    let root = tempfile::tempdir().unwrap();
    let history = crate::history::Store::open(root.path().join("state")).unwrap();
    let preferences = Store::open(&history).unwrap();
    preferences.remember("original", reference()).unwrap();
    let lock_path = history.root.join("model-preferences.lock");
    let owner = crate::history::open_private_file(&lock_path, false, true).unwrap();
    owner.lock_exclusive().unwrap();
    let started = Instant::now();
    assert!(preferences.remember("recent", reference()).is_err());
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_eq!(preferences.get("recent"), Some(reference()));
    assert!(Store::open(&history).unwrap().get("recent").is_none());
}

#[tokio::test]
async fn remembered_accounts_ignore_visibility_evidence_from_another_accounts_cached_listing() {
    struct Listing;
    impl crate::models::Http for Listing {
        fn get<'a>(
            &'a self,
            _: &'a str,
            _: reqwest::header::HeaderMap,
            _: usize,
        ) -> futures::future::BoxFuture<'a, Result<Vec<u8>, crate::models::Error>> {
            Box::pin(async { Ok(br#"{"data":[{"id":"different-model"}]}"#.to_vec()) })
        }
    }
    let profiles = providers("credential={env='KEY'}");
    let account_a = Environment::default().with_variable("KEY", "account-a");
    let account_b = Environment::default().with_variable("KEY", "account-b");
    let profile = profiles.get("account").unwrap();
    let reference = Reference::new(
        profile,
        "recent-model".into(),
        credential(profile, &account_a).unwrap().as_ref(),
    );
    let service = crate::models::Service::new(
        Default::default(),
        Arc::new(Listing),
        Arc::new(crate::models::FileCache(None)),
        Arc::new(crate::models::SystemClock),
    );
    service.list_endpoint(profile, &account_b).await.unwrap();
    assert_eq!(
        super::resolve(&reference, &profiles, &account_a, Some(&service))
            .unwrap()
            .model,
        "recent-model"
    );
    // The exact same listing is authoritative when the remembered credential matches its owner.
    let reference_b = Reference::new(
        profile,
        "recent-model".into(),
        credential(profile, &account_b).unwrap().as_ref(),
    );
    assert_eq!(
        super::resolve(&reference_b, &profiles, &account_b, Some(&service)),
        Err(Reason::ModelUnavailable)
    );
}

#[test]
fn new_sessions_pin_recent_choices_without_changing_existing_sessions_or_other_profiles() {
    let preferences = Store::default();
    preferences.remember("default", reference()).unwrap();
    let mut sessions = crate::session::Sessions::with_providers(providers(""))
        .with_preferences(preferences.clone(), Environment::default());
    let first = sessions.create(".");
    let profile = providers("");
    preferences
        .remember(
            "default",
            Reference::new(profile.get("account").unwrap(), "next-model".into(), None),
        )
        .unwrap();
    let second = sessions.create(".");
    assert_eq!(
        sessions.settings(&first).unwrap().unwrap().model.as_deref(),
        Some("recent-model")
    );
    assert_eq!(
        sessions
            .settings(&second)
            .unwrap()
            .unwrap()
            .model
            .as_deref(),
        Some("next-model")
    );
    let other = crate::test_support::configured_providers(
        "profile='other'\n[providers.account]\nprotocols={openai-chat={}}\nmodel='other-default'\n[profiles.other]\nconnections=['account']",
    );
    let mut other = crate::session::Sessions::with_providers(other)
        .with_preferences(preferences, Environment::default());
    let id = other.create(".");
    assert_eq!(
        other.settings(&id).unwrap().unwrap().model.as_deref(),
        Some("other-default")
    );
}

#[tokio::test]
async fn blocked_selection_survives_restart_and_explicit_settings_clear_it_durably() {
    let root = tempfile::tempdir().unwrap();
    let history = crate::history::Store::open(root.path().join("state")).unwrap();
    let preferences = Store::default();
    preferences.remember("default", reference()).unwrap();
    let profiles = providers("headers={Authorization='different-account'}");
    let mut sessions = crate::session::Sessions::with_providers(profiles.clone())
        .with_preferences(preferences, Environment::default());
    let memory_id = sessions.create(root.path());
    assert!(matches!(
        sessions.begin_turn(&memory_id, || {}),
        Err(crate::session::SessionError::SelectionRequired)
    ));
    assert!(!sessions.initialize_native(&memory_id, None));
    let snapshot = sessions.snapshot(&memory_id).unwrap();
    let (id, handle) = history.create(snapshot).await.unwrap();
    handle.release().await.unwrap();
    let (snapshot, handle, _) = history.attach(&id).await.unwrap();
    let mut restarted = crate::session::Sessions::with_providers(profiles);
    restarted.restore(id.clone(), snapshot.clone(), handle.clone());
    assert!(matches!(
        restarted.begin_turn(&id, || {}),
        Err(crate::session::SessionError::SelectionRequired)
    ));
    restarted.set_provider(&id, "account").unwrap();
    restarted.rollback(&id, snapshot);
    assert!(restarted.selection_notice(&id).is_some());
    restarted.set_model(&id, "custom-confirmed").unwrap();
    handle.save(restarted.snapshot(&id).unwrap()).await.unwrap();
    handle.release().await.unwrap();
    let (snapshot, _, _) = history.attach(&id).await.unwrap();
    assert_eq!(snapshot.selection_state, State::Explicit);
    assert_eq!(snapshot.model.as_deref(), Some("custom-confirmed"));
}

#[tokio::test]
async fn acp_reports_account_changes_and_rejects_prompts_until_explicit_confirmation_after_restart()
{
    use agent_client_protocol::schema::{ProtocolVersion, v2};
    use agent_client_protocol::{Client, Error, ErrorCode};
    let root = tempfile::tempdir().unwrap();
    let preferences = Store::default();
    preferences.remember("default", reference()).unwrap();
    let profiles = providers("headers={Authorization='changed-account'}");
    let mut saved: Option<v2::SessionId> = None;
    for restart in [false, true] {
        let history = crate::history::Store::open(root.path().join("state")).unwrap();
        let runtime = crate::acp::Runtime::new(Default::default(), Default::default())
            .with_history(history)
            .with_preferences(preferences.clone())
            .with_auth(
                Environment::default(),
                Arc::new(crate::http::AsyncLoginHttp),
            );
        let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
        let client = Client.v2().on_receive_notification(
            async move |update: v2::UpdateSessionNotification, _| {
                tx.send(update).map_err(Error::into_internal_error)
            },
            agent_client_protocol::on_receive_notification!(),
        );
        let previous = saved.take();
        let cwd = root.path().to_owned();
        saved = Some(
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                client.connect_with(
                    crate::acp::agent_with_runtime(profiles.clone(), runtime),
                    async move |connection| {
                        connection
                            .send_request(v2::InitializeRequest::new(
                                ProtocolVersion::V2,
                                v2::Implementation::new("test", "0"),
                            ))
                            .block_task()
                            .await?;
                        let id = if let Some(id) = previous {
                            connection
                                .send_request(v2::ResumeSessionRequest::new(id.clone(), cwd))
                                .block_task()
                                .await?;
                            id
                        } else {
                            connection
                                .send_request(v2::NewSessionRequest::new(cwd))
                                .block_task()
                                .await?
                                .session_id
                        };
                        let mut notice = false;
                        while let Ok(update) = updates.try_recv() {
                            notice |= matches!(update.update, v2::SessionUpdate::Notice(_));
                        }
                        assert!(notice, "selection warning precedes new/resume response");
                        let error = connection
                            .send_request(v2::PromptRequest::new(id.clone(), vec!["hello".into()]))
                            .block_task()
                            .await
                            .unwrap_err();
                        assert_eq!(error.code, ErrorCode::InvalidRequest);
                        if restart {
                            let response = connection
                                .send_request(v2::SetSessionConfigOptionRequest::new(
                                    id.clone(),
                                    "provider",
                                    "account",
                                ))
                                .block_task()
                                .await?;
                            assert!(
                                response
                                    .config_options
                                    .iter()
                                    .any(|option| option.config_id.to_string() == "model")
                            );
                        }
                        connection
                            .send_request(v2::CloseSessionRequest::new(id.clone()))
                            .block_task()
                            .await?;
                        Ok(id)
                    },
                ),
            )
            .await
            .unwrap()
            .unwrap(),
        );
    }
}
