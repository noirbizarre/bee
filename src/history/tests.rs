use super::*;
use journal::{Fault, Reader};
use serde_json::json;

fn plan() -> crate::plan::Entries {
    serde_json::from_value(json!({"entries":[
        {"content":"Done","priority":"high","status":"completed"},
        {"content":"Work","priority":"medium","status":"in_progress"}
    ]}))
    .unwrap()
}

#[tokio::test]
async fn plan_revisions_are_durable_deduplicated_and_recovered_without_context() {
    let (_root, store, id, handle) = create().await;
    handle.accept(1, json!([])).await.unwrap();
    handle.plan(1, plan(), || {}).await.unwrap().unwrap();
    handle.plan(1, plan(), || {}).await.unwrap().unwrap();
    assert_eq!(
        events(&handle.0.path)
            .iter()
            .filter(|event| matches!(event, Event::PlanUpdated { .. }))
            .count(),
        1
    );
    handle.release().await.unwrap();
    let (snapshot, restored, interrupted) = store.attach(&id).await.unwrap();
    assert!(interrupted);
    assert!(snapshot.messages.is_empty());
    restored.recover().await.unwrap();
    let retained = restored
        .run(|journal, _| Ok(journal.projection.plans.clone()))
        .await
        .unwrap();
    let mut expected = plan();
    expected.cancel_unfinished();
    assert_eq!(retained.get(&1), Some(&expected));
    restored.release().await.unwrap();
    let (_, restored, interrupted) = store.attach(&id).await.unwrap();
    assert!(!interrupted);
    assert_eq!(
        restored
            .run(|journal, _| Ok(journal.projection.plans.clone()))
            .await
            .unwrap(),
        retained
    );
}

#[rstest::rstest]
#[case(false)]
#[case(true)]
#[tokio::test]
async fn failed_plan_writes_never_publish_and_failed_finishes_recover_unfinished_work(
    #[case] fail_finish: bool,
) {
    let (_root, store, id, handle) = create().await;
    handle.accept(1, json!([])).await.unwrap();
    if fail_finish {
        handle.plan(1, plan(), || {}).await.unwrap().unwrap();
    }
    handle
        .run(|journal, _| {
            journal.fault = Some(Fault::BeforeWrite);
            Ok(())
        })
        .await
        .unwrap();
    if fail_finish {
        assert!(handle.finish(1, "completed", None).await.is_err());
    } else {
        let published = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = published.clone();
        assert!(
            handle
                .plan(1, plan(), move || {
                    observed.store(true, Ordering::SeqCst);
                })
                .await
                .unwrap()
                .is_err()
        );
        assert!(!published.load(Ordering::SeqCst));
    }
    handle.release().await.unwrap();
    let (_, restored, _) = store.attach(&id).await.unwrap();
    restored.recover().await.unwrap();
    let plans = restored
        .run(|journal, _| Ok(journal.projection.plans.clone()))
        .await
        .unwrap();
    assert_eq!(plans.len(), usize::from(fail_finish));
    if fail_finish {
        assert_eq!(plans[&1].entries[1].status, crate::plan::Status::Cancelled);
    }
}

#[tokio::test]
async fn plan_replay_reserves_the_entire_budget_before_delivering_messages_or_plans() {
    let (_root, _, _, handle) = create().await;
    handle.accept(1, json!([])).await.unwrap();
    handle.plan(1, plan(), || {}).await.unwrap().unwrap();
    handle.finish(1, "cancelled", None).await.unwrap();
    let bytes = Arc::new(ReplayBudget::new(Settings::default()));
    bytes.0.store(1, Ordering::SeqCst);
    assert!(
        handle
            .replay_with_plans(
                bytes,
                |_| panic!("message before budget reservation"),
                |_, _| panic!("plan before budget reservation")
            )
            .await
            .is_err()
    );
}

fn snapshot() -> Snapshot {
    Snapshot {
        cwd: std::env::temp_dir(),
        turns: 0,
        provider: Some("saved".into()),
        model: Some("custom".into()),
        protocol: None,
        messages: Vec::new(),
        instructions: Default::default(),
    }
}

fn budget() -> Arc<ReplayBudget> {
    Arc::new(ReplayBudget::new(Settings::default()))
}

async fn create() -> (tempfile::TempDir, Store, String, Handle) {
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(root.path().join("bee")).unwrap();
    let (id, handle) = store.create(snapshot()).await.unwrap();
    (root, store, id, handle)
}

fn events(path: &Path) -> Vec<Event> {
    let mut reader = Reader::open(path, u64::MAX).unwrap();
    let mut events = Vec::new();
    while let Some(event) = reader.next().unwrap() {
        events.push(event);
    }
    events
}

async fn replay(handle: &Handle) -> Vec<Message> {
    let result = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&result);
    handle
        .replay(budget(), move |message| {
            captured.lock().unwrap().push(message);
            Ok(())
        })
        .await
        .unwrap();
    Arc::try_unwrap(result).unwrap().into_inner().unwrap()
}

async fn committed(handle: &Handle) -> Snapshot {
    handle
        .run(|journal, _| Ok(journal.projection.snapshot.clone()))
        .await
        .unwrap()
}

async fn finish_text(
    handle: &Handle,
    turn: u64,
    user: &str,
    text: &str,
    mut next: Snapshot,
) -> Snapshot {
    handle
        .accept(turn, json!([{"type":"text","text":user}]))
        .await
        .unwrap();
    handle.text(turn, text.into(), || {});
    next.turns = turn;
    next.messages.extend([
        crate::llm::Message::User(user.into()),
        crate::llm::Message::Assistant {
            text: text.into(),
            calls: Vec::new(),
            native: None,
        },
    ]);
    handle
        .finish(turn, "completed", Some(next.clone()))
        .await
        .unwrap();
    next
}

#[test]
fn the_state_root_is_injected_and_obeys_xdg_empty_and_relative_rules() {
    let root = tempfile::tempdir().unwrap();
    let environment = crate::config::Environment::new(None, Some(root.path().into()), None);
    assert_eq!(
        environment.history_path().unwrap(),
        root.path().join(".local/state/bee")
    );
    assert_eq!(
        environment
            .clone()
            .with_xdg_state_home(Some(root.path().join("state")))
            .history_path()
            .unwrap(),
        root.path().join("state/bee")
    );
    for invalid in ["", "relative"] {
        assert_eq!(
            environment
                .clone()
                .with_xdg_state_home(Some(invalid.into()))
                .history_path(),
            environment.history_path()
        );
    }
    assert!(Store::from_environment(&crate::config::Environment::default()).is_err());
}

#[tokio::test]
async fn incomplete_creations_are_not_published_to_the_session_listing() {
    let (_root, store, id, handle) = create().await;
    let pending = store
        .directory("bee-session-00000000000000000000000000000000")
        .unwrap();
    create_directory(&pending).unwrap();
    let staging = pending.join(".session.jsonl.pending");
    std::io::Write::write_all(&mut open_private_file(&staging, true, true).unwrap(), b"{").unwrap();
    assert_eq!(store.list().await.unwrap()[0].id, id);
    assert_eq!(
        handle.0.path,
        store.directory(&id).unwrap().join("session.jsonl")
    );
    let first: serde_json::Value = serde_json::from_str(
        std::fs::read_to_string(&handle.0.path)
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(first["version"], 2);
    assert_eq!(first["seq"], 0);
    assert_eq!(first["event"]["id"], id);
}

#[rstest::rstest]
#[case::chat(crate::provider::Protocol::OpenAiChat)]
#[case::responses(crate::provider::Protocol::OpenAiResponses)]
#[case::messages(crate::provider::Protocol::AnthropicMessages)]
#[tokio::test]
async fn interrupted_partials_replay_but_never_replace_successful_native_context(
    #[case] protocol: crate::provider::Protocol,
) {
    let (_root, store, id, handle) = create().await;
    handle
        .accept(1, json!([{"type":"text","text":"successful"}]))
        .await
        .unwrap();
    handle.text(1, "answer".into(), || {});
    let mut successful = snapshot();
    successful.turns = 1;
    successful.protocol = Some(protocol);
    successful.messages = vec![
        crate::llm::Message::User("successful".into()),
        crate::llm::Message::Assistant {
            text: "answer".into(),
            calls: vec![crate::llm::Call {
                id: "call".into(),
                name: "read".into(),
                arguments: json!({"path":"file"}),
            }],
            native: Some(crate::llm::Native {
                protocol,
                items: vec![json!({"opaque":"continuation"})],
            }),
        },
        crate::llm::Message::Tool {
            id: "call".into(),
            output: "tool-result".into(),
            is_error: false,
        },
    ];
    successful.instructions = serde_json::from_value(
        json!({"scopes":{"project":"instructions"},"active":{"skill":"semantic snapshot"}}),
    )
    .unwrap();
    handle
        .finish(1, "completed", Some(successful.clone()))
        .await
        .unwrap();
    handle
        .accept(2, json!([{"type":"text","text":"interrupted"}]))
        .await
        .unwrap();
    handle.text(2, "partial".into(), || {});
    handle.drain().await.unwrap();
    handle.release().await.unwrap();
    let (restored, handle, interrupted) = store.attach(&id).await.unwrap();
    assert!(interrupted);
    assert_eq!(restored.turns, 2);
    assert_eq!(restored.messages, successful.messages);
    assert_eq!(restored.instructions, successful.instructions);
    handle.recover().await.unwrap();
    let before = std::fs::read(&handle.0.path).unwrap();
    let first = replay(&handle).await;
    let second = replay(&handle).await;
    assert_eq!(
        serde_json::to_value(&first).unwrap(),
        serde_json::to_value(&second).unwrap()
    );
    assert_eq!(first.len(), 4);
    assert_eq!(first[3].content[0]["text"], "partial");
    assert_eq!(
        std::fs::read(&handle.0.path).unwrap(),
        before,
        "replay must never append records"
    );
    handle.release().await.unwrap();
    assert!(!store.attach(&id).await.unwrap().2);
}

#[tokio::test]
async fn a_torn_commit_is_repaired_only_after_ownership_and_preserves_previous_success() {
    let (_root, store, id, handle) = create().await;
    let successful = finish_text(&handle, 1, "keep", "committed", snapshot()).await;
    handle
        .accept(2, json!([{"type":"text","text":"failed"}]))
        .await
        .unwrap();
    handle.text(2, "partial".into(), || {});
    handle.drain().await.unwrap();
    let prefix = std::fs::read(&handle.0.path).unwrap();
    handle
        .run(|journal, _| {
            journal.fault = Some(Fault::PartialWrite(37));
            Ok(())
        })
        .await
        .unwrap();
    let mut next = successful.clone();
    next.turns = 2;
    next.messages
        .push(crate::llm::Message::User("failed".into()));
    assert!(handle.finish(2, "completed", Some(next)).await.is_err());
    let path = handle.0.path.clone();
    handle.release().await.unwrap();
    let damaged = std::fs::read(&path).unwrap();
    assert!(damaged.starts_with(&prefix));
    assert!(damaged.len() > prefix.len());
    // Listing and even failed/invalid resume preparation must not repair data.
    assert_eq!(store.list().await.unwrap()[0].id, id);
    let (restored, handle, interrupted) = store.attach(&id).await.unwrap();
    assert!(interrupted);
    assert_eq!(restored.messages, successful.messages);
    assert_eq!(std::fs::read(&path).unwrap(), damaged);
    handle.recover().await.unwrap();
    assert!(std::fs::read(&path).unwrap().starts_with(&prefix));
    assert_eq!(replay(&handle).await[3].content[0]["text"], "partial");
    handle.release().await.unwrap();
    let (restored, _, interrupted) = store.attach(&id).await.unwrap();
    assert!(!interrupted);
    assert_eq!(restored.messages, successful.messages);
}

#[tokio::test]
async fn a_failed_append_is_redacted_and_never_emitted_or_committed_as_success() {
    let (_root, _store, _id, handle) = create().await;
    handle
        .accept(1, json!([{"type":"text","text":"private-user"}]))
        .await
        .unwrap();
    handle
        .run(|journal, _| {
            journal.fault = Some(Fault::BeforeWrite);
            Ok(())
        })
        .await
        .unwrap();
    handle.text(1, "private-answer".into(), || {
        panic!("uncommitted text was emitted")
    });
    let failure = handle.failed().await;
    assert!(!format!("{failure:?} {failure}").contains("private-"));
    assert!(
        handle
            .finish(1, "completed", Some(snapshot()))
            .await
            .is_err()
    );
    assert!(handle.accept(2, json!([])).await.is_err());
    assert_eq!(replay(&handle).await.len(), 1);
}

#[tokio::test]
async fn failed_prompt_insertion_does_not_allocate_a_persisted_turn() {
    let (_root, store, id, handle) = create().await;
    let before = std::fs::read(&handle.0.path).unwrap();
    handle
        .run(|journal, _| {
            journal.fault = Some(Fault::BeforeWrite);
            Ok(())
        })
        .await
        .unwrap();
    assert!(handle.accept(1, json!([])).await.is_err());
    assert_eq!(std::fs::read(&handle.0.path).unwrap(), before);
    handle.release().await.unwrap();
    let (restored, _, interrupted) = store.attach(&id).await.unwrap();
    assert_eq!(restored.turns, 0);
    assert!(!interrupted);
}

#[tokio::test]
async fn a_complete_unacknowledged_record_is_recovered_as_data_without_restarting_work() {
    let (_root, store, id, handle) = create().await;
    handle
        .run(|journal, _| {
            journal.fault = Some(Fault::BeforeSync);
            Ok(())
        })
        .await
        .unwrap();
    assert!(
        handle
            .accept(1, json!([{"type":"text","text":"possibly durable"}]))
            .await
            .is_err()
    );
    handle.release().await.unwrap();
    let (restored, handle, interrupted) = store.attach(&id).await.unwrap();
    assert_eq!(restored.turns, 1);
    assert!(restored.messages.is_empty());
    assert!(interrupted);
    handle.recover().await.unwrap();
    assert_eq!(replay(&handle).await.len(), 1);
    handle.accept(2, json!([])).await.unwrap();
}

#[tokio::test]
async fn an_ambiguous_sync_error_recovers_the_complete_successful_commit_as_written() {
    let (_root, store, id, handle) = create().await;
    handle
        .accept(1, json!([{"type":"text","text":"question"}]))
        .await
        .unwrap();
    handle.text(1, "answer".into(), || {});
    handle.drain().await.unwrap();
    let mut next = snapshot();
    next.turns = 1;
    next.messages = vec![
        crate::llm::Message::User("question".into()),
        crate::llm::Message::Assistant {
            text: "answer".into(),
            calls: Vec::new(),
            native: None,
        },
    ];
    handle
        .run(|journal, _| {
            journal.fault = Some(Fault::BeforeSync);
            Ok(())
        })
        .await
        .unwrap();
    assert!(
        handle
            .finish(1, "completed", Some(next.clone()))
            .await
            .is_err()
    );
    assert!(committed(&handle).await.messages.is_empty());
    handle.release().await.unwrap();
    let (restored, restored_handle, interrupted) = store.attach(&id).await.unwrap();
    assert!(!interrupted);
    assert_eq!(restored.messages, next.messages);
    let before = std::fs::read(&restored_handle.0.path).unwrap();
    restored_handle
        .run(|journal, _| {
            journal.fault = Some(Fault::BeforeSync);
            Ok(())
        })
        .await
        .unwrap();
    assert!(
        restored_handle.recover().await.is_err(),
        "even a clean-looking recovered commit must be synchronized"
    );
    restored_handle.recover().await.unwrap();
    assert_eq!(std::fs::read(&restored_handle.0.path).unwrap(), before);
}

#[tokio::test]
async fn deeply_nested_tool_arguments_are_rejected_before_the_journal_becomes_unreadable() {
    let (_root, store, id, handle) = create().await;
    let mut arguments = json!({});
    for _ in 0..124 {
        arguments = json!({"nested":arguments});
    }
    let standalone = serde_json::to_string(&arguments).unwrap();
    assert!(serde_json::from_str::<serde_json::Value>(&standalone).is_ok());
    handle.accept(1, json!([])).await.unwrap();
    let before = std::fs::read(&handle.0.path).unwrap();
    let mut next = snapshot();
    next.turns = 1;
    next.messages = vec![
        crate::llm::Message::User("question".into()),
        crate::llm::Message::Assistant {
            text: String::new(),
            calls: vec![crate::llm::Call {
                id: "call".into(),
                name: "tool".into(),
                arguments,
            }],
            native: None,
        },
    ];
    let failure = handle
        .finish(1, "completed", Some(next))
        .await
        .err()
        .unwrap();
    assert!(failure.reason.contains("nesting"));
    assert_eq!(std::fs::read(&handle.0.path).unwrap(), before);
    handle.release().await.unwrap();
    let (restored, _, interrupted) = store.attach(&id).await.unwrap();
    assert!(interrupted);
    assert!(restored.messages.is_empty());
}

#[tokio::test]
async fn unknown_corrupt_future_and_wrong_identity_history_never_become_fresh_sessions() {
    let (_root, store, id, handle) = create().await;
    assert!(store.attach("../../outside").await.is_err());
    assert!(
        store
            .attach("bee-session-00000000000000000000000000000000")
            .await
            .is_err()
    );
    let path = handle.0.path.clone();
    handle.release().await.unwrap();
    let good = std::fs::read_to_string(&path).unwrap();
    let mut future: serde_json::Value = serde_json::from_str(good.trim()).unwrap();
    future["version"] = 999.into();
    std::fs::write(&path, format!("{future}\n")).unwrap();
    assert!(
        store
            .attach(&id)
            .await
            .err()
            .unwrap()
            .reason
            .contains("version")
    );
    assert!(store.list().await.is_err());
    std::fs::write(
        &path,
        format!("{good}{{\"private\":\"corrupt complete line\"}}\n"),
    )
    .unwrap();
    let failure = store.attach(&id).await.err().unwrap();
    assert!(!format!("{failure:?} {failure}").contains("private"));
    std::fs::write(&path, &good).unwrap();
    let wrong = "bee-session-ffffffffffffffffffffffffffffffff";
    let header = Event::SessionCreated {
        id: wrong.into(),
        created: 1,
        snapshot: snapshot(),
    };
    std::fs::write(&path, journal::encode(0, &header, &path).unwrap()).unwrap();
    assert!(
        store
            .attach(&id)
            .await
            .err()
            .unwrap()
            .reason
            .contains("identity")
    );
}

#[tokio::test]
async fn syntactically_valid_damage_and_missing_records_are_detected() {
    let (_root, store, id, handle) = create().await;
    handle.accept(1, json!([])).await.unwrap();
    let path = handle.0.path.clone();
    handle.release().await.unwrap();
    let good = std::fs::read_to_string(&path).unwrap();
    let mut records: Vec<serde_json::Value> = good
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    records[1]["event"]["turn"] = 2.into();
    let corrupted = records
        .iter()
        .map(|record| format!("{record}\n"))
        .collect::<String>();
    std::fs::write(&path, corrupted).unwrap();
    assert!(
        store
            .attach(&id)
            .await
            .err()
            .unwrap()
            .reason
            .contains("checksum")
    );
    let skipped = journal::encode(
        2,
        &Event::TurnAccepted {
            turn: 1,
            content: json!([]),
        },
        &path,
    )
    .unwrap();
    let mut bytes = good.lines().next().unwrap().as_bytes().to_vec();
    bytes.push(b'\n');
    bytes.extend(skipped);
    std::fs::write(&path, bytes).unwrap();
    assert!(
        store
            .attach(&id)
            .await
            .err()
            .unwrap()
            .reason
            .contains("sequence")
    );
}

#[tokio::test]
async fn oversized_incomplete_records_fail_instead_of_being_silently_discarded() {
    let (_root, store, id, handle) = create().await;
    let path = handle.0.path.clone();
    let before = std::fs::metadata(&path).unwrap().len();
    handle.release().await.unwrap();
    OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(before + journal::MAX_RECORD_BYTES as u64 + 1)
        .unwrap();
    assert!(
        store
            .attach(&id)
            .await
            .err()
            .unwrap()
            .reason
            .contains("64 MiB")
    );
    assert_eq!(
        std::fs::metadata(path).unwrap().len(),
        before + journal::MAX_RECORD_BYTES as u64 + 1
    );
}

#[tokio::test]
async fn committed_context_and_guidance_are_deltas_while_visible_history_survives_eviction() {
    let (_root, store, id, handle) = create().await;
    let guidance = "saved-skill-payload".repeat(200);
    let mut next = snapshot();
    next.instructions =
        serde_json::from_value(json!({"scopes":{},"active":{"skill":guidance}})).unwrap();
    for turn in 1..=20 {
        next = finish_text(
            &handle,
            turn,
            &format!("question-{turn}"),
            &format!("answer-{turn}"),
            next,
        )
        .await;
    }
    let mut trimmed = next;
    trimmed.messages.drain(..36);
    let trimmed = finish_text(&handle, 21, "last question", "last answer", trimmed).await;
    let path = handle.0.path.clone();
    let log = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        log.matches(&guidance).count(),
        1,
        "unchanged guidance is not a per-turn full snapshot"
    );
    let rows: Vec<serde_json::Value> = log
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    for row in rows
        .iter()
        .filter(|row| row["event"]["type"] == "turn_finished")
    {
        assert_eq!(
            row["event"]["context"]["append"].as_array().unwrap().len(),
            2
        );
    }
    assert_eq!(replay(&handle).await.len(), 42);
    handle.release().await.unwrap();
    let (restored, _, _) = store.attach(&id).await.unwrap();
    assert_eq!(restored.messages, trimmed.messages);
    assert_eq!(restored.messages.len(), 6);
    assert_eq!(restored.instructions, trimmed.instructions);
}

#[rstest::rstest]
#[case::selection(true)]
#[case::catalog_protocol(false)]
#[tokio::test]
async fn selection_and_protocol_changes_reconstruct_portable_context(#[case] selection: bool) {
    let (_root, store, id, handle) = create().await;
    let mut first = snapshot();
    first.protocol = Some(crate::provider::Protocol::OpenAiChat);
    let mut first = finish_text(&handle, 1, "first", "answer", first).await;
    // Add real native state through a successful second turn, not an out-of-band mutation.
    handle.accept(2, json!([])).await.unwrap();
    first.turns = 2;
    first.messages.extend([
        crate::llm::Message::User("second".into()),
        crate::llm::Message::Assistant {
            text: "native".into(),
            calls: Vec::new(),
            native: Some(crate::llm::Native {
                protocol: crate::provider::Protocol::OpenAiChat,
                items: vec![json!({"opaque":"saved"})],
            }),
        },
    ]);
    handle
        .finish(2, "completed", Some(first.clone()))
        .await
        .unwrap();
    for message in &mut first.messages {
        if let crate::llm::Message::Assistant { native, .. } = message {
            *native = None;
        }
    }
    if selection {
        first.model = Some("other".into());
        handle.save(first.clone()).await.unwrap();
    }
    first.protocol = Some(crate::provider::Protocol::OpenAiResponses);
    let expected = finish_text(&handle, 3, "third", "continued", first).await;
    handle.release().await.unwrap();
    let (restored, _, _) = store.attach(&id).await.unwrap();
    assert_eq!(restored.messages, expected.messages);
    assert_eq!(restored.model, expected.model);
    assert_eq!(restored.protocol, expected.protocol);
}

#[tokio::test]
async fn deltas_are_appended_once_and_replay_storage_scales_with_new_text() {
    let (_root, _store, _id, handle) = create().await;
    handle.accept(1, json!([])).await.unwrap();
    for _ in 0..512 {
        handle.text(1, "small".into(), || {});
    }
    handle.finish(1, "completed", None).await.unwrap();
    let parts: Vec<_> = events(&handle.0.path)
        .into_iter()
        .filter_map(|event| match event {
            Event::AssistantDelta { text, .. } => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(parts, vec!["small"; 512]);
    assert_eq!(
        replay(&handle).await[1].content[0]["text"],
        "small".repeat(512)
    );
}

#[tokio::test]
async fn replay_reserves_the_whole_cumulative_budget_before_emitting() {
    let (_root, _store, _id, handle) = create().await;
    finish_text(&handle, 1, "question", "answer", snapshot()).await;
    let messages = replay(&handle).await;
    let required = messages
        .iter()
        .map(|message| ReplayBudget::message_bytes(message).unwrap())
        .sum();
    let budget = Arc::new(ReplayBudget::new(Settings {
        replay_max_bytes: required,
    }));
    let emitted = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&emitted);
    handle
        .replay(Arc::clone(&budget), move |_| {
            count.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })
        .await
        .unwrap();
    let count = Arc::clone(&emitted);
    assert!(
        handle
            .replay(budget, move |_| {
                count.fetch_add(1, Ordering::Relaxed);
                Ok(())
            })
            .await
            .is_err()
    );
    assert_eq!(emitted.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn state_publication_cannot_overtake_an_in_progress_replay() {
    let (_root, _store, _id, handle) = create().await;
    finish_text(&handle, 1, "question", "answer", snapshot()).await;
    let (started, receive) = tokio::sync::oneshot::channel();
    let (release, pause) = mpsc::channel();
    let (events, observed) = mpsc::channel();
    let replayed = events.clone();
    let worker = handle.clone();
    let replay = tokio::spawn(async move {
        let mut started = Some(started);
        worker
            .replay(budget(), move |_| {
                if let Some(started) = started.take() {
                    let _ = started.send(());
                    pause
                        .recv_timeout(std::time::Duration::from_secs(5))
                        .unwrap();
                }
                replayed.send("message").unwrap();
                Ok(())
            })
            .await
            .unwrap();
    });
    receive.await.unwrap();
    handle
        .enqueue(move || {
            events.send("idle").unwrap();
        })
        .unwrap();
    assert!(observed.try_recv().is_err());
    release.send(()).unwrap();
    replay.await.unwrap();
    handle.drain().await.unwrap();
    assert_eq!(
        observed.try_iter().collect::<Vec<_>>(),
        ["message", "message", "idle"]
    );
}

#[tokio::test]
async fn output_queue_exhaustion_is_explicit_and_keeps_previous_context() {
    let (_root, _store, _id, handle) = create().await;
    handle.accept(1, json!([])).await.unwrap();
    handle.text(1, "x".repeat(4 * 1024 * 1024), || {
        panic!("over-budget output was emitted")
    });
    assert!(handle.failed().await.reason.contains("queue"));
    assert!(
        handle
            .finish(1, "completed", Some(snapshot()))
            .await
            .is_err()
    );
}

#[test]
fn replay_settings_validate_positive_budgets_without_exposing_values() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("selected.toml");
    for text in [
        "[history]\nreplay_max_bytes=0",
        "[history]\nreplay_max_bytes='private-value'",
        "[history]\nprivate_value=123",
        "history='private-value'",
    ] {
        std::fs::write(&file, text).unwrap();
        let config = crate::config::Config::load(
            &crate::config::Environment::new(None, None, Some(file.clone())),
            root.path(),
        )
        .unwrap();
        let failure = Settings::from_config(&config).unwrap_err();
        assert!(!format!("{failure:?} {failure}").contains("private"));
    }
    std::fs::write(&file, "[history]\nreplay_max_bytes=4096").unwrap();
    let config = crate::config::Config::load(
        &crate::config::Environment::new(None, None, Some(file)),
        root.path(),
    )
    .unwrap();
    let settings = Settings::from_config(&config).unwrap();
    assert_eq!(settings.replay_max_bytes, 4096);
    assert!(!format!("{settings:?}").contains("4096"));
}

#[cfg(unix)]
#[tokio::test]
async fn history_files_are_private_and_permissive_existing_roots_are_refused() {
    use std::os::unix::fs::PermissionsExt;
    let (_root, store, id, handle) = create().await;
    for entry in std::fs::read_dir(store.directory(&id).unwrap()).unwrap() {
        assert_eq!(
            entry.unwrap().metadata().unwrap().permissions().mode() & 0o077,
            0
        );
    }
    std::fs::set_permissions(&store.root, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(Store::open(&store.root).is_err());
    handle.release().await.unwrap();
}

#[cfg(windows)]
#[tokio::test]
async fn private_windows_history_survives_publication_and_ownership_handoff() {
    let (_root, store, id, handle) = create().await;
    finish_text(&handle, 1, "question", "answer", snapshot()).await;
    let directory = store.directory(&id).unwrap();
    windows::check_private(&directory, true).unwrap();
    for entry in std::fs::read_dir(&directory).unwrap() {
        windows::check_private(&entry.unwrap().path(), false).unwrap();
    }
    handle.release().await.unwrap();
    let (restored, handle, interrupted) = store.attach(&id).await.unwrap();
    assert_eq!(restored.turns, 1);
    assert!(!interrupted);
    assert_eq!(store.list().await.unwrap()[0].id, id);
    handle.release().await.unwrap();
}

#[cfg(windows)]
#[test]
fn inherited_permissions_are_not_mistaken_for_a_protected_private_root() {
    let root = tempfile::tempdir().unwrap();
    let broad = root.path().join("inherited");
    std::fs::create_dir(&broad).unwrap();
    assert!(Store::open(broad).is_err());
}
