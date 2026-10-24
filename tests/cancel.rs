//! Prompt cancellation and protocol error handling, exercised in-process.
//!
//! These tests inject a controlled turn body with
//! `bee::acp::agent_with`: one that never ends, fails or panics. What they pin
//! is the protocol around the turn, independent of model I/O.

use std::future::{self, Future};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc as std_mpsc};
use std::time::Duration;

use agent_client_protocol::schema::{ProtocolVersion, v2};
use agent_client_protocol::{Agent, Client, Error, ErrorCode, V2ConnectionTo};
use bee::acp::{Reply, TurnFailure};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

// Every wait is bounded: a hung agent must fail the test, not stall CI.
const TIMEOUT: Duration = Duration::from_secs(10);

type Updates = UnboundedReceiver<v2::UpdateSessionNotification>;

/// Connect a v2 client to a Bee agent whose turns run `body`, initialize, and
/// run `test`.
async fn run<F, Fut, T>(
    body: F,
    test: impl AsyncFnOnce(V2ConnectionTo<Agent>, Updates) -> Result<T, Error>,
) -> T
where
    F: Fn(Reply) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), TurnFailure>> + Send + 'static,
{
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
        .connect_with(bee::acp::agent_with(body), async move |connection| {
            connection
                .send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    v2::Implementation::new("bee-test", "0"),
                ))
                .block_task()
                .await?;
            test(connection, updates).await
        });

    tokio::time::timeout(TIMEOUT, client)
        .await
        .expect("the exchange finishes within the timeout")
        .expect("the exchange succeeds")
}

async fn new_session(connection: &V2ConnectionTo<Agent>) -> Result<v2::SessionId, Error> {
    let cwd: PathBuf = std::env::temp_dir().join("bee-cancel");
    Ok(connection
        .send_request(v2::NewSessionRequest::new(cwd))
        .block_task()
        .await?
        .session_id)
}

async fn prompt_with(
    connection: &V2ConnectionTo<Agent>,
    session: &v2::SessionId,
    prompt: Vec<v2::ContentBlock>,
) -> Result<v2::PromptResponse, Error> {
    connection
        .send_request(v2::PromptRequest::new(session.clone(), prompt))
        .block_task()
        .await
}

async fn prompt(
    connection: &V2ConnectionTo<Agent>,
    session: &v2::SessionId,
) -> Result<v2::PromptResponse, Error> {
    prompt_with(connection, session, vec!["hello".into()]).await
}

fn cancel(connection: &V2ConnectionTo<Agent>, session: &v2::SessionId) -> Result<(), Error> {
    connection.send_notification(v2::CancelSessionNotification::new(session.clone()))
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
        let done = idle_reason(&update.update).is_some();
        seen.push(update);
        if done {
            return seen;
        }
    }
}

/// The stop reason of an `idle` update, or `None` if it is another update.
///
/// The outer `Option` is "is it idle"; the inner one is the idle's reason.
fn idle_reason(update: &v2::SessionUpdate) -> Option<Option<v2::StopReason>> {
    match update {
        v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle)) => {
            Some(idle.stop_reason.clone())
        }
        _ => None,
    }
}

/// The reason of the terminal update that ends `seen`.
fn terminal_reason(seen: &[v2::UpdateSessionNotification]) -> v2::StopReason {
    idle_reason(&seen.last().expect("an idle update ended the read").update)
        .expect("the last update is idle")
        .expect("the idle update says why work stopped")
}

fn is_agent_text(update: &v2::SessionUpdate) -> bool {
    matches!(update, v2::SessionUpdate::AgentMessageChunk(_))
}

/// Prove the connection has delivered everything sent so far, then assert no
/// update is waiting.
///
/// A request's response is ordered after every notification the agent sent
/// before it, so once `session/list` answers, a late update would already be
/// in the channel.
async fn assert_no_further_updates(connection: &V2ConnectionTo<Agent>, updates: &mut Updates) {
    connection
        .send_request(v2::ListSessionsRequest::new())
        .block_task()
        .await
        .expect("the connection still answers");
    assert!(
        updates.try_recv().is_err(),
        "no update was expected after the terminal one"
    );
}

/// Tells a test, by being dropped, that the turn body holding it was dropped.
struct DropSignal(UnboundedSender<()>);

impl Drop for DropSignal {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

/// A body that waits forever after one chunk, signalling when it is dropped.
macro_rules! never_ending_body {
    ($dropped:expr) => {{
        let dropped = $dropped;
        move |reply: Reply| {
            let signal = DropSignal(dropped.clone());
            async move {
                // Owned by the future, so it is dropped with it.
                let _signal = signal;
                reply.text("partial");
                future::pending::<()>().await;
                Ok(())
            }
        }
    }};
}

#[tokio::test(flavor = "current_thread")]
async fn cancelling_a_running_turn_ends_it_with_a_cancelled_idle_update() {
    let (dropped, mut dropped_signal) = mpsc::unbounded_channel();
    run(
        never_ending_body!(dropped),
        async |connection, mut updates| {
            let session = new_session(&connection).await?;
            prompt(&connection, &session).await?;
            // Wait for the turn to be underway, so this cancels work in flight
            // and not a turn that never started.
            loop {
                if is_agent_text(&next(&mut updates).await.update) {
                    break;
                }
            }

            cancel(&connection, &session)?;

            let seen = until_idle(&mut updates).await;
            assert_eq!(terminal_reason(&seen), v2::StopReason::Cancelled);
            // Terminal idle is a cleanup barrier, not just an abort request.
            dropped_signal
                .try_recv()
                .expect("the turn was dropped before idle");
            Ok(())
        },
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn resuming_an_active_session_reports_running_without_ending_its_turn() {
    let (dropped, _signal) = mpsc::unbounded_channel();
    run(
        never_ending_body!(dropped),
        async |connection, mut updates| {
            let session = new_session(&connection).await?;
            prompt(&connection, &session).await?;
            while !is_agent_text(&next(&mut updates).await.update) {}
            connection
                .send_request(v2::ResumeSessionRequest::new(
                    session.clone(),
                    std::env::temp_dir().join("bee-cancel"),
                ))
                .block_task()
                .await?;
            assert!(matches!(
                next(&mut updates).await.update,
                v2::SessionUpdate::StateUpdate(v2::StateUpdate::Running(_))
            ));
            cancel(&connection, &session)?;
            assert_eq!(
                terminal_reason(&until_idle(&mut updates).await),
                v2::StopReason::Cancelled
            );
            assert_no_further_updates(&connection, &mut updates).await;
            Ok(())
        },
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_cancel_sent_right_after_the_prompt_response_still_cancels() {
    let (dropped, _signal) = mpsc::unbounded_channel();
    run(
        never_ending_body!(dropped),
        async |connection, mut updates| {
            let session = new_session(&connection).await?;
            prompt(&connection, &session).await?;

            // No waiting: this races the turn's registration, which is the window a
            // handler that registered its turn late would lose.
            cancel(&connection, &session)?;

            let seen = until_idle(&mut updates).await;
            assert_eq!(terminal_reason(&seen), v2::StopReason::Cancelled);
            Ok(())
        },
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_cancelled_turn_never_reports_end_turn() {
    let (dropped, _signal) = mpsc::unbounded_channel();
    run(
        never_ending_body!(dropped),
        async |connection, mut updates| {
            let session = new_session(&connection).await?;
            prompt(&connection, &session).await?;
            cancel(&connection, &session)?;

            let seen = until_idle(&mut updates).await;

            // Exactly one terminal state, and it is not a success.
            let idles = seen
                .iter()
                .filter(|update| idle_reason(&update.update).is_some())
                .count();
            assert_eq!(idles, 1);
            assert_ne!(terminal_reason(&seen), v2::StopReason::EndTurn);
            assert_no_further_updates(&connection, &mut updates).await;
            Ok(())
        },
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn no_agent_message_follows_the_cancelled_idle_update() {
    // A turn that hands its reply to another thread, which speaks only when
    // released: the case a plain drop of the turn future cannot stop.
    let release = Arc::new(Mutex::new(None::<std_mpsc::Sender<()>>));
    let (spoke, spoke_signal) = std_mpsc::channel::<()>();
    let spoke = Mutex::new(spoke);
    let body = {
        let release = Arc::clone(&release);
        move |reply: Reply| {
            let (sender, receiver) = std_mpsc::channel::<()>();
            *release.lock().unwrap() = Some(sender);
            let spoke = spoke.lock().unwrap().clone();
            std::thread::spawn(move || {
                let _ = receiver.recv();
                reply.text("too late");
                let _ = spoke.send(());
            });
            async { future::pending::<Result<(), TurnFailure>>().await }
        }
    };

    run(body, async |connection, mut updates| {
        let session = new_session(&connection).await?;
        prompt(&connection, &session).await?;
        cancel(&connection, &session)?;
        let seen = until_idle(&mut updates).await;
        assert_eq!(terminal_reason(&seen), v2::StopReason::Cancelled);

        // Only now may the other thread speak, after the client was told the
        // turn is over.
        let sender = release
            .lock()
            .unwrap()
            .take()
            .expect("the turn body started");
        sender.send(()).expect("the helper thread is waiting");
        spoke_signal
            .recv_timeout(TIMEOUT)
            .expect("the helper thread tried to speak");

        assert_no_further_updates(&connection, &mut updates).await;
        Ok(())
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_prompt_after_a_cancelled_turn_runs_to_completion() {
    // The first turn never ends; the second one answers.
    let calls = Arc::new(AtomicUsize::new(0));
    let body = move |reply: Reply| {
        let first = calls.fetch_add(1, Ordering::SeqCst) == 0;
        async move {
            if first {
                future::pending::<()>().await;
            }
            reply.text("second answer");
            Ok(())
        }
    };

    run(body, async |connection, mut updates| {
        let session = new_session(&connection).await?;
        prompt(&connection, &session).await?;
        cancel(&connection, &session)?;
        until_idle(&mut updates).await;

        prompt(&connection, &session).await?;

        let seen = until_idle(&mut updates).await;
        assert_eq!(terminal_reason(&seen), v2::StopReason::EndTurn);
        assert!(seen.iter().any(|update| is_agent_text(&update.update)));
        Ok(())
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn cancelling_an_unknown_session_is_ignored_and_the_connection_survives() {
    let (dropped, _signal) = mpsc::unbounded_channel();
    run(
        never_ending_body!(dropped),
        async |connection, mut updates| {
            cancel(&connection, &v2::SessionId::new("no-such-session"))?;

            // The notification got no reply and no update, and did not end the
            // connection.
            let session = new_session(&connection).await?;
            assert!(updates.try_recv().is_err());
            assert!(!session.to_string().is_empty());
            Ok(())
        },
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_second_prompt_while_a_turn_runs_is_rejected_and_leaves_the_first_turn_running() {
    let (dropped, _signal) = mpsc::unbounded_channel();
    run(
        never_ending_body!(dropped),
        async |connection, mut updates| {
            let session = new_session(&connection).await?;
            prompt(&connection, &session).await?;

            let error = prompt(&connection, &session)
                .await
                .expect_err("a prompt during a running turn is refused");

            // A well-formed request at the wrong time: not `invalid_params`.
            assert_eq!(error.code, ErrorCode::InvalidRequest);
            assert!(
                format!("{error:?}").contains("session/cancel"),
                "the error says what to do: {error:?}"
            );
            // The first turn is unaffected: it is still the one a cancel ends.
            cancel(&connection, &session)?;
            let seen = until_idle(&mut updates).await;
            assert_eq!(terminal_reason(&seen), v2::StopReason::Cancelled);
            // The refused prompt inserted no user message.
            let user_messages = seen
                .iter()
                .filter(|update| matches!(update.update, v2::SessionUpdate::UserMessage(_)))
                .count();
            assert_eq!(user_messages, 1);
            Ok(())
        },
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn closing_a_running_session_sends_the_cancelled_idle_before_the_close_response() {
    let (dropped, _signal) = mpsc::unbounded_channel();
    run(
        never_ending_body!(dropped),
        async |connection, mut updates| {
            let session = new_session(&connection).await?;
            prompt(&connection, &session).await?;

            connection
                .send_request(v2::CloseSessionRequest::new(session.clone()))
                .block_task()
                .await?;

            // The client handles messages in order, so everything the agent sent
            // before the close response is already here without waiting.
            let mut seen = Vec::new();
            while let Ok(update) = updates.try_recv() {
                seen.push(update);
            }
            assert_eq!(terminal_reason(&seen), v2::StopReason::Cancelled);

            // And the session is closed for good, not still running.
            let error = prompt(&connection, &session)
                .await
                .expect_err("a closed session refuses prompts");
            assert_eq!(error.code, ErrorCode::InvalidParams);
            Ok(())
        },
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn closing_an_idle_session_sends_no_update() {
    let (dropped, _signal) = mpsc::unbounded_channel();
    run(
        never_ending_body!(dropped),
        async |connection, mut updates| {
            let session = new_session(&connection).await?;

            connection
                .send_request(v2::CloseSessionRequest::new(session))
                .block_task()
                .await?;

            assert!(updates.try_recv().is_err());
            Ok(())
        },
    )
    .await;
}

/// The error a failed turn reports, with the session reusable afterwards.
async fn assert_failed_then_recovers(
    connection: &V2ConnectionTo<Agent>,
    updates: &mut Updates,
    expected_detail: &str,
) -> Result<(), Error> {
    let session = new_session(connection).await?;

    // Insertion succeeded, so the prompt itself is answered successfully.
    prompt(connection, &session).await?;
    let seen = until_idle(updates).await;

    let v2::StopReason::Error(failure) = terminal_reason(&seen) else {
        panic!("a failed turn ends with the `error` stop reason");
    };
    let error = failure.error.expect("the failure carries its error");
    assert_eq!(error.code, v2::ErrorCode::InternalError);
    assert!(
        format!("{error:?}").contains(expected_detail),
        "the error says what failed: {error:?}"
    );

    // The failure did not poison the session or the connection.
    prompt(connection, &session).await?;
    let seen = until_idle(updates).await;
    assert_eq!(terminal_reason(&seen), v2::StopReason::EndTurn);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn a_failing_turn_ends_with_an_error_idle_update_and_the_session_stays_usable() {
    let calls = Arc::new(AtomicUsize::new(0));
    let body = move |_reply: Reply| {
        let first = calls.fetch_add(1, Ordering::SeqCst) == 0;
        async move {
            if first {
                Err(TurnFailure::new("the provider was unreachable"))
            } else {
                Ok(())
            }
        }
    };

    run(body, async |connection, mut updates| {
        assert_failed_then_recovers(&connection, &mut updates, "the provider was unreachable").await
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_panicking_turn_ends_with_an_error_idle_update_and_the_connection_survives() {
    let calls = Arc::new(AtomicUsize::new(0));
    let body = move |_reply: Reply| {
        let first = calls.fetch_add(1, Ordering::SeqCst) == 0;
        async move {
            assert!(!first, "the turn blew up");
            Ok(())
        }
    };

    run(body, async |connection, mut updates| {
        assert_failed_then_recovers(&connection, &mut updates, "the turn panicked").await
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_prompt_with_image_content_is_rejected_with_invalid_params_and_starts_no_turn() {
    let (dropped, _signal) = mpsc::unbounded_channel();
    run(
        never_ending_body!(dropped),
        async |connection, mut updates| {
            let session = new_session(&connection).await?;
            let image = v2::ContentBlock::Image(v2::ImageContent::new("aGk=", "image/png"));

            let error = prompt_with(&connection, &session, vec![image])
                .await
                .expect_err("Bee advertises no image capability");

            assert_eq!(error.code, ErrorCode::InvalidParams);
            assert!(
                format!("{error:?}").contains("image"),
                "the error names the content: {error:?}"
            );
            // Nothing started: no update, and the session is not busy.
            assert_no_further_updates(&connection, &mut updates).await;
            prompt(&connection, &session).await?;
            Ok(())
        },
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_prompt_with_a_resource_link_is_accepted() {
    run(
        |_reply: Reply| async { Ok(()) },
        async |connection, mut updates| {
            let session = new_session(&connection).await?;
            let link = v2::ContentBlock::ResourceLink(v2::ResourceLink::new(
                "notes",
                "file:///tmp/notes.md",
            ));

            prompt_with(&connection, &session, vec![link]).await?;

            let seen = until_idle(&mut updates).await;
            assert_eq!(terminal_reason(&seen), v2::StopReason::EndTurn);
            Ok(())
        },
    )
    .await;
}
