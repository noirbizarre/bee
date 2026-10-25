//! The session and prompt lifecycle, exercised in-process over the SDK's
//! in-memory transport.
//!
//! Deterministic injected replies isolate lifecycle behavior from inference.

use std::path::PathBuf;
use std::time::Duration;

use agent_client_protocol::schema::{ProtocolVersion, v2};
use agent_client_protocol::{Agent, Client, Error, ErrorCode, V2ConnectionTo};
use tokio::sync::mpsc::{self, UnboundedReceiver};

// Every wait is bounded: a hung agent must fail the test, not stall CI.
const TIMEOUT: Duration = Duration::from_secs(10);

type Updates = UnboundedReceiver<v2::UpdateSessionNotification>;

/// Connect a v2 client to a fresh Bee agent, initialize, and run `body`.
///
/// Every `session/update` the client receives lands in the `Updates` channel,
/// in arrival order.
async fn run<T>(body: impl AsyncFnOnce(V2ConnectionTo<Agent>, Updates) -> Result<T, Error>) -> T {
    let (sender, updates) = mpsc::unbounded_channel();

    let client = Client
        .v2()
        .on_receive_notification(
            async move |update: v2::UpdateSessionNotification,
                        _connection: V2ConnectionTo<Agent>| {
                sender.send(update).map_err(Error::into_internal_error)
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(
            bee::acp::agent_with(|reply| async move {
                reply.text("deterministic test answer");
                Ok(())
            }),
            async move |connection| {
                initialize(&connection).await?;
                body(connection, updates).await
            },
        );

    tokio::time::timeout(TIMEOUT, client)
        .await
        .expect("the exchange finishes within the timeout")
        .expect("the exchange succeeds")
}

async fn initialize(connection: &V2ConnectionTo<Agent>) -> Result<(), Error> {
    connection
        .send_request(v2::InitializeRequest::new(
            ProtocolVersion::V2,
            v2::Implementation::new("bee-test", "0"),
        ))
        .block_task()
        .await?;
    Ok(())
}

fn dir(name: &str) -> PathBuf {
    std::env::temp_dir().join(name)
}

async fn new_session(
    connection: &V2ConnectionTo<Agent>,
    cwd: PathBuf,
) -> Result<v2::SessionId, Error> {
    let response = connection
        .send_request(v2::NewSessionRequest::new(cwd))
        .block_task()
        .await?;
    Ok(response.session_id)
}

async fn prompt(
    connection: &V2ConnectionTo<Agent>,
    session: &v2::SessionId,
    text: &str,
) -> Result<v2::PromptResponse, Error> {
    connection
        .send_request(v2::PromptRequest::new(session.clone(), vec![text.into()]))
        .block_task()
        .await
}

async fn next(updates: &mut Updates) -> v2::UpdateSessionNotification {
    tokio::time::timeout(TIMEOUT, updates.recv())
        .await
        .expect("an update arrives within the timeout")
        .expect("the update channel stays open")
}

/// Read updates up to and including the next `idle` one.
async fn until_idle(updates: &mut Updates) -> Vec<v2::UpdateSessionNotification> {
    let mut seen = Vec::new();
    loop {
        let update = next(updates).await;
        let done = matches!(
            update.update,
            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(_))
        );
        seen.push(update);
        if done {
            return seen;
        }
    }
}

fn is_running(update: &v2::SessionUpdate) -> bool {
    matches!(
        update,
        v2::SessionUpdate::StateUpdate(v2::StateUpdate::Running(_))
    )
}

#[tokio::test(flavor = "current_thread")]
async fn a_client_can_create_a_session_and_submit_a_prompt() {
    run(async |connection, _updates| {
        let session = new_session(&connection, dir("a")).await?;

        let accepted = prompt(&connection, &session, "hello").await?;

        assert!(!accepted.message_id.to_string().is_empty());
        Ok(())
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_prompt_produces_the_user_message_running_a_reply_and_a_terminal_idle_in_order() {
    run(async |connection, mut updates| {
        let session = new_session(&connection, dir("a")).await?;

        prompt(&connection, &session, "hello").await?;
        let seen = until_idle(&mut updates).await;

        let [user, running, reply, idle] = seen.as_slice() else {
            panic!("expected four updates, got {seen:?}");
        };
        assert!(matches!(user.update, v2::SessionUpdate::UserMessage(_)));
        assert!(is_running(&running.update));
        assert!(matches!(
            reply.update,
            v2::SessionUpdate::AgentMessageChunk(_)
        ));
        // The terminal state: the turn completed, and says so.
        assert!(matches!(
            &idle.update,
            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle))
                if idle.stop_reason == Some(v2::StopReason::EndTurn)
        ));
        assert!(seen.iter().all(|update| update.session_id == session));
        Ok(())
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn the_user_message_update_carries_the_id_the_prompt_response_returned() {
    run(async |connection, mut updates| {
        let session = new_session(&connection, dir("a")).await?;

        let accepted = prompt(&connection, &session, "hello").await?;
        let user = next(&mut updates).await;

        let v2::SessionUpdate::UserMessage(message) = user.update else {
            panic!("expected the user message first, got {:?}", user.update);
        };
        assert_eq!(message.message_id, accepted.message_id);
        Ok(())
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn an_injected_turn_streams_its_text_to_the_client() {
    run(async |connection, mut updates| {
        let session = new_session(&connection, dir("a")).await?;

        prompt(&connection, &session, "what is 2 + 2?").await?;
        let seen = until_idle(&mut updates).await;

        let text = seen
            .iter()
            .find_map(|update| match &update.update {
                v2::SessionUpdate::AgentMessageChunk(chunk) => Some(format!("{:?}", chunk.content)),
                _ => None,
            })
            .expect("the turn has an agent message");
        assert!(text.contains("deterministic test answer"), "{text}");
        Ok(())
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn an_injected_turn_can_answer_successive_prompts() {
    run(async |connection, mut updates| {
        let session = new_session(&connection, dir("a")).await?;

        prompt(&connection, &session, "one").await?;
        let first = until_idle(&mut updates).await;
        prompt(&connection, &session, "something else entirely").await?;
        let second = until_idle(&mut updates).await;

        let reply = |seen: &[v2::UpdateSessionNotification]| match &seen[2].update {
            v2::SessionUpdate::AgentMessageChunk(chunk) => chunk.content.clone(),
            other => panic!("expected a reply, got {other:?}"),
        };
        // Deterministic by design, so nothing mistakes it for model output.
        assert_eq!(reply(&first), reply(&second));
        Ok(())
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn two_sessions_get_distinct_ids() {
    run(async |connection, _updates| {
        let first = new_session(&connection, dir("a")).await?;
        let second = new_session(&connection, dir("a")).await?;

        assert_ne!(first, second);
        Ok(())
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_prompt_in_one_session_sends_updates_only_for_that_session() {
    run(async |connection, mut updates| {
        let first = new_session(&connection, dir("a")).await?;
        let second = new_session(&connection, dir("b")).await?;

        prompt(&connection, &first, "hello").await?;
        let seen = until_idle(&mut updates).await;

        assert!(seen.iter().all(|update| update.session_id == first));
        assert!(updates.try_recv().is_err(), "nothing was sent for {second}");
        Ok(())
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn message_ids_are_counted_per_session() {
    run(async |connection, _updates| {
        let first = new_session(&connection, dir("a")).await?;
        let second = new_session(&connection, dir("b")).await?;

        prompt(&connection, &first, "one").await?;
        prompt(&connection, &first, "two").await?;
        let in_second = prompt(&connection, &second, "one").await?;
        let again_in_first = prompt(&connection, &first, "three").await?;

        // A turn in another session must not have advanced this session's ids.
        assert_eq!(in_second.message_id, v2::MessageId::new("user-1"));
        assert_eq!(again_in_first.message_id, v2::MessageId::new("user-3"));
        Ok(())
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_second_prompt_is_accepted_after_the_first_turn_ended() {
    run(async |connection, mut updates| {
        let session = new_session(&connection, dir("a")).await?;
        prompt(&connection, &session, "one").await?;
        until_idle(&mut updates).await;

        prompt(&connection, &session, "two").await?;
        let seen = until_idle(&mut updates).await;

        assert_eq!(seen.len(), 4);
        Ok(())
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn two_connections_do_not_share_sessions() {
    let id = run(async |connection, _updates| new_session(&connection, dir("a")).await).await;

    let error = run(async |connection, _updates| {
        // The same id, on a connection that never created it.
        Ok(prompt(&connection, &id, "hello")
            .await
            .expect_err("unknown"))
    })
    .await;

    assert_eq!(error.code, ErrorCode::InvalidParams);
}

#[tokio::test(flavor = "current_thread")]
async fn a_prompt_to_an_unknown_session_is_rejected_with_invalid_params() {
    run(async |connection, _updates| {
        let error = prompt(&connection, &v2::SessionId::new("nope"), "hi")
            .await
            .expect_err("no such session");

        assert_eq!(error.code, ErrorCode::InvalidParams);
        // The error says what to do, not just what failed.
        assert!(format!("{error:?}").contains("session/new"), "{error:?}");
        Ok(())
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_prompt_to_a_closed_session_is_rejected_and_sends_no_update() {
    run(async |connection, mut updates| {
        let session = new_session(&connection, dir("a")).await?;
        connection
            .send_request(v2::CloseSessionRequest::new(session.clone()))
            .block_task()
            .await?;

        let error = prompt(&connection, &session, "hi")
            .await
            .expect_err("the session is closed");

        assert_eq!(error.code, ErrorCode::InvalidParams);
        // A refused prompt never started a turn, so there is no idle to report.
        assert!(updates.try_recv().is_err());
        Ok(())
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn closing_a_session_twice_is_not_an_error() {
    run(async |connection, _updates| {
        let session = new_session(&connection, dir("a")).await?;

        for _ in 0..2 {
            connection
                .send_request(v2::CloseSessionRequest::new(session.clone()))
                .block_task()
                .await?;
        }
        Ok(())
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn closing_an_unknown_session_is_rejected() {
    run(async |connection, _updates| {
        let error = connection
            .send_request(v2::CloseSessionRequest::new("nope"))
            .block_task()
            .await
            .expect_err("no such session");

        assert_eq!(error.code, ErrorCode::InvalidParams);
        Ok(())
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_closed_session_can_be_resumed_and_prompted_again() {
    run(async |connection, mut updates| {
        let cwd = dir("a");
        let session = new_session(&connection, cwd.clone()).await?;
        connection
            .send_request(v2::CloseSessionRequest::new(session.clone()))
            .block_task()
            .await?;

        connection
            .send_request(v2::ResumeSessionRequest::new(session.clone(), cwd))
            .block_task()
            .await?;
        // Drop the resume's idle snapshot so the turn is read on its own.
        until_idle(&mut updates).await;
        prompt(&connection, &session, "hello").await?;
        let seen = until_idle(&mut updates).await;

        assert_eq!(seen.len(), 4);
        Ok(())
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn resume_reports_an_idle_snapshot() {
    run(async |connection, mut updates| {
        let cwd = dir("a");
        let session = new_session(&connection, cwd.clone()).await?;

        connection
            .send_request(v2::ResumeSessionRequest::new(session.clone(), cwd))
            .block_task()
            .await?;

        // The protocol requires the snapshot because a resume response alone
        // does not imply idle.
        let update = next(&mut updates).await;
        assert_eq!(update.session_id, session);
        assert!(matches!(
            update.update,
            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(_))
        ));
        Ok(())
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn resuming_with_another_cwd_is_rejected() {
    run(async |connection, _updates| {
        let session = new_session(&connection, dir("a")).await?;

        let error = connection
            .send_request(v2::ResumeSessionRequest::new(session, dir("b")))
            .block_task()
            .await
            .expect_err("the cwd differs");

        assert_eq!(error.code, ErrorCode::InvalidParams);
        Ok(())
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn resuming_from_start_replays_full_messages_before_current_state() {
    run(async |connection, mut updates| {
        let cwd = dir("a");
        let session = new_session(&connection, cwd.clone()).await?;
        prompt(&connection, &session, "previous turn").await?;
        until_idle(&mut updates).await;
        let mut request = v2::ResumeSessionRequest::new(session.clone(), cwd);
        request.replay_from = Some(v2::ReplayFrom::Start(v2::ReplayFromStart::new()));

        connection.send_request(request).block_task().await?;
        let user = next(&mut updates).await;
        assert!(matches!(user.update, v2::SessionUpdate::UserMessage(_)));
        let agent = next(&mut updates).await;
        assert!(matches!(agent.update, v2::SessionUpdate::AgentMessage(_)));
        let snapshot = next(&mut updates).await;
        assert_eq!(snapshot.session_id, session);
        assert!(matches!(
            snapshot.update,
            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(_))
        ));
        // The next turn's exact updates detect historical or extra snapshots.
        prompt(&connection, &session, "next turn").await?;
        assert_eq!(until_idle(&mut updates).await.len(), 4);
        Ok(())
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn unsupported_replay_cursors_do_not_reopen_closed_sessions() {
    run(async |connection, mut updates| {
        let cwd = dir("a");
        let session = new_session(&connection, cwd.clone()).await?;
        connection
            .send_request(v2::CloseSessionRequest::new(session.clone()))
            .block_task()
            .await?;
        let mut request = v2::ResumeSessionRequest::new(session.clone(), cwd);
        request.replay_from =
            Some(serde_json::from_value(serde_json::json!({"type":"future_cursor"})).unwrap());
        let error = connection
            .send_request(request.clone())
            .block_task()
            .await
            .expect_err("unknown cursors are rejected");
        assert_eq!(error.code, ErrorCode::InvalidParams);
        assert!(prompt(&connection, &session, "still closed").await.is_err());
        request.replay_from = Some(v2::ReplayFrom::Start(v2::ReplayFromStart::new()));
        connection.send_request(request).block_task().await?;
        assert!(matches!(
            next(&mut updates).await.update,
            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(_))
        ));
        prompt(&connection, &session, "recovered").await?;
        assert_eq!(until_idle(&mut updates).await.len(), 4);
        Ok(())
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn list_returns_sessions_in_creation_order_and_filters_by_cwd() {
    run(async |connection, _updates| {
        let a1 = new_session(&connection, dir("a")).await?;
        let b = new_session(&connection, dir("b")).await?;
        let a2 = new_session(&connection, dir("a")).await?;

        let all = connection
            .send_request(v2::ListSessionsRequest::new())
            .block_task()
            .await?;
        let only_a = connection
            .send_request(v2::ListSessionsRequest::new().cwd(v2::AbsolutePath::new(dir("a"))))
            .block_task()
            .await?;

        let ids = |response: &v2::ListSessionsResponse| {
            response
                .sessions
                .iter()
                .map(|info| info.session_id.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&all), vec![a1.clone(), b, a2.clone()]);
        assert_eq!(ids(&only_a), vec![a1, a2]);
        assert!(all.next_cursor.is_none());
        Ok(())
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn list_with_a_cursor_is_rejected_because_bee_never_issues_one() {
    run(async |connection, _updates| {
        let error = connection
            .send_request(v2::ListSessionsRequest::new().cursor("made-up"))
            .block_task()
            .await
            .expect_err("Bee has a single page");

        assert_eq!(error.code, ErrorCode::InvalidParams);
        Ok(())
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn cancelling_an_idle_session_sends_no_update_and_leaves_it_usable() {
    run(async |connection, mut updates| {
        let session = new_session(&connection, dir("a")).await?;

        connection.send_notification(v2::CancelSessionNotification::new(session.clone()))?;
        // The prompt's own updates are read next: a stray update from the
        // cancel would show up first and break the expected order.
        prompt(&connection, &session, "hello").await?;
        let seen = until_idle(&mut updates).await;

        assert!(matches!(seen[0].update, v2::SessionUpdate::UserMessage(_)));
        assert_eq!(seen.len(), 4);
        Ok(())
    })
    .await;
}
