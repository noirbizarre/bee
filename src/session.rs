//! Session state, independent of the wire protocol.
//!
//! Kept free of the ACP SDK on purpose: `acp.rs` is the only module that may
//! import it (ADR-001), so what a session *is* lives here and how it is spoken
//! lives there.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use thiserror::Error;

/// Why a session operation was refused.
///
/// These become JSON-RPC error data for the client, never CLI output, so they
/// are plain `thiserror` errors rather than miette diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum SessionError {
    /// The id names no session this connection created.
    #[error("unknown session `{0}`; create one with `session/new` first")]
    Unknown(String),

    /// The session was closed.
    #[error("session `{0}` is closed; reopen it with `session/resume` or create a new one")]
    Closed(String),

    /// A resume named a different working directory than the session has.
    #[error(
        "session `{id}` has the working directory `{expected}`, not `{actual}`; \
         resume it with its original `cwd`"
    )]
    CwdMismatch {
        /// The session being resumed.
        id: String,
        /// The directory the session was created with.
        expected: String,
        /// The directory the request named.
        actual: String,
    },

    /// A prompt arrived while the session's previous turn was still running.
    #[error(
        "session `{0}` is already running a turn; wait for it to go idle, \
         or stop it with `session/cancel`, before prompting again"
    )]
    Busy(String),
}

/// The identifiers of one prompt turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    /// The id of the user message the prompt inserted.
    pub user_message_id: String,
    /// The id of the agent message that answers it.
    pub agent_message_id: String,
    /// Names this turn within its session. [`Sessions::finish_turn`] and
    /// [`Sessions::is_current`] take it, so a turn that was cancelled cannot be
    /// mistaken for the one that replaced it.
    pub token: u64,
}

/// What stops a running turn. Boxed so this module needs no async runtime: the
/// protocol layer decides how work is aborted.
type Cancel = Box<dyn FnOnce() + Send>;

/// What `session/list` reports about one session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    /// The session's id.
    pub id: String,
    /// The session's working directory.
    pub cwd: PathBuf,
}

#[derive(Debug)]
struct Session {
    cwd: PathBuf,
    open: bool,
    /// Turns started so far. Per session, so one session's prompts can never
    /// shift another's message ids.
    turns: u64,
    /// Creation order, which `session/list` reports. Ids are strings and
    /// would sort `bee-session-10` before `bee-session-2`.
    created: u64,
    /// The turn in flight, if any, and how to stop it. Whoever takes it out of
    /// here is the only one that reports the turn's terminal state.
    active: Option<ActiveTurn>,
}

struct ActiveTurn {
    token: u64,
    cancel: Cancel,
}

// Hand-written because `Cancel` is a closure, which has no `Debug`.
impl std::fmt::Debug for ActiveTurn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActiveTurn")
            .field("token", &self.token)
            .finish_non_exhaustive()
    }
}

/// Every session one connection has created.
///
/// One per connection: nothing here is shared between connections, so two
/// clients cannot observe or disturb each other's sessions.
#[derive(Debug, Default)]
pub struct Sessions {
    created: u64,
    by_id: HashMap<String, Session>,
}

impl Sessions {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Open a session in `cwd` and return its id.
    pub fn create(&mut self, cwd: impl Into<PathBuf>) -> String {
        self.created += 1;
        let id = format!("bee-session-{}", self.created);
        self.by_id.insert(
            id.clone(),
            Session {
                cwd: cwd.into(),
                open: true,
                turns: 0,
                created: self.created,
                active: None,
            },
        );
        id
    }

    /// Start a prompt turn in `id` and return the ids it uses.
    ///
    /// `cancel` is called, once, if the turn is stopped by [`Sessions::cancel`]
    /// or [`Sessions::close`]. It is not called when the turn finishes.
    ///
    /// # Errors
    ///
    /// [`SessionError::Unknown`], [`SessionError::Closed`], or
    /// [`SessionError::Busy`] if the session's previous turn is still running.
    pub fn begin_turn(
        &mut self,
        id: &str,
        cancel: impl FnOnce() + Send + 'static,
    ) -> Result<Turn, SessionError> {
        let session = self.open_session(id)?;
        // Refused rather than queued: two turns writing to one conversation
        // would interleave their updates, and a client cannot tell whose is whose.
        if session.active.is_some() {
            return Err(SessionError::Busy(id.to_owned()));
        }
        session.turns += 1;
        session.active = Some(ActiveTurn {
            token: session.turns,
            cancel: Box::new(cancel),
        });
        Ok(Turn {
            user_message_id: format!("user-{}", session.turns),
            agent_message_id: format!("agent-{}", session.turns),
            token: session.turns,
        })
    }

    /// Whether `token` still names the turn running in `id`.
    ///
    /// False once the turn finished, was cancelled, or its session is gone:
    /// the caller must then stop reporting progress for it.
    #[must_use]
    pub fn is_current(&self, id: &str, token: u64) -> bool {
        self.by_id
            .get(id)
            .and_then(|session| session.active.as_ref())
            .is_some_and(|active| active.token == token)
    }

    /// Mark the turn `token` of `id` finished.
    ///
    /// Returns whether it was still running. False means a cancel or a close
    /// already took it, and has reported (or will report) its terminal state:
    /// reporting another would give the client two.
    pub fn finish_turn(&mut self, id: &str, token: u64) -> bool {
        if !self.is_current(id, token) {
            return false;
        }
        if let Some(session) = self.by_id.get_mut(id) {
            session.active = None;
        }
        true
    }

    /// Stop the turn running in `id`, if any.
    ///
    /// Returns whether one was running, which is whether the caller owes the
    /// client a `cancelled` terminal state. An unknown or idle session is not
    /// an error: `session/cancel` is a notification and has no reply to carry one.
    pub fn cancel(&mut self, id: &str) -> bool {
        self.by_id
            .get_mut(id)
            .and_then(|session| session.active.take())
            .map(|active| (active.cancel)())
            .is_some()
    }

    /// Close `id`, stopping its running turn. Closing a closed session
    /// succeeds: the client's goal, a session that is not running, is already
    /// met.
    ///
    /// Returns whether a running turn was stopped, which is whether the caller
    /// owes the client a `cancelled` terminal state before its response.
    ///
    /// # Errors
    ///
    /// [`SessionError::Unknown`].
    pub fn close(&mut self, id: &str) -> Result<bool, SessionError> {
        // Checked first so an unknown id cancels nothing.
        self.session(id)?;
        let cancelled = self.cancel(id);
        self.session(id)?.open = false;
        Ok(cancelled)
    }

    /// Reopen `id`, which must have been created with the same `cwd`.
    ///
    /// # Errors
    ///
    /// [`SessionError::Unknown`] or [`SessionError::CwdMismatch`].
    pub fn resume(&mut self, id: &str, cwd: &Path) -> Result<(), SessionError> {
        let session = self.session(id)?;
        if session.cwd != cwd {
            return Err(SessionError::CwdMismatch {
                id: id.to_owned(),
                expected: session.cwd.display().to_string(),
                actual: cwd.display().to_string(),
            });
        }
        session.open = true;
        Ok(())
    }

    /// Every session, oldest first, optionally only those in `cwd`.
    #[must_use]
    pub fn list(&self, cwd: Option<&Path>) -> Vec<Listing> {
        let mut found: Vec<_> = self
            .by_id
            .iter()
            .filter(|(_, session)| cwd.is_none_or(|cwd| session.cwd == cwd))
            .collect();
        found.sort_by_key(|(_, session)| session.created);
        found
            .into_iter()
            .map(|(id, session)| Listing {
                id: id.clone(),
                cwd: session.cwd.clone(),
            })
            .collect()
    }

    fn session(&mut self, id: &str) -> Result<&mut Session, SessionError> {
        self.by_id
            .get_mut(id)
            .ok_or_else(|| SessionError::Unknown(id.to_owned()))
    }

    fn open_session(&mut self, id: &str) -> Result<&mut Session, SessionError> {
        let session = self.session(id)?;
        if session.open {
            Ok(session)
        } else {
            Err(SessionError::Closed(id.to_owned()))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    #[test]
    fn every_created_session_gets_a_distinct_id() {
        let mut sessions = Sessions::new();

        let first = sessions.create("/a");
        let second = sessions.create("/a");

        assert_ne!(first, second);
    }

    #[test]
    fn message_ids_are_counted_per_session() {
        let mut sessions = Sessions::new();
        let a = sessions.create("/a");
        let b = sessions.create("/b");

        let first_of_a = sessions.begin_turn(&a, || {}).unwrap();
        sessions.finish_turn(&a, first_of_a.token);
        let second_of_a = sessions.begin_turn(&a, || {}).unwrap();
        let first_of_b = sessions.begin_turn(&b, || {}).unwrap();

        assert_eq!(second_of_a.user_message_id, "user-2");
        assert_eq!(first_of_b.user_message_id, "user-1");
    }

    #[test]
    fn a_turn_in_an_unknown_session_is_refused() {
        let mut sessions = Sessions::new();

        assert_eq!(
            sessions.begin_turn("nope", || {}),
            Err(SessionError::Unknown("nope".into()))
        );
    }

    #[test]
    fn a_turn_in_a_closed_session_is_refused_until_it_is_resumed() {
        let mut sessions = Sessions::new();
        let id = sessions.create("/a");
        sessions.close(&id).unwrap();

        assert_eq!(
            sessions.begin_turn(&id, || {}),
            Err(SessionError::Closed(id.clone()))
        );
        sessions.resume(&id, Path::new("/a")).unwrap();
        assert!(sessions.begin_turn(&id, || {}).is_ok());
    }

    #[test]
    fn closing_twice_is_not_an_error() {
        let mut sessions = Sessions::new();
        let id = sessions.create("/a");

        sessions.close(&id).unwrap();

        assert_eq!(sessions.close(&id), Ok(false));
    }

    #[test]
    fn resuming_with_another_cwd_is_refused() {
        let mut sessions = Sessions::new();
        let id = sessions.create("/a");

        assert!(matches!(
            sessions.resume(&id, Path::new("/b")),
            Err(SessionError::CwdMismatch { .. })
        ));
    }

    #[test]
    fn listing_is_in_creation_order_even_past_ten_sessions() {
        let mut sessions = Sessions::new();
        let ids: Vec<_> = (0..12).map(|_| sessions.create("/a")).collect();

        let listed: Vec<_> = sessions.list(None).into_iter().map(|l| l.id).collect();

        assert_eq!(listed, ids);
    }

    #[test]
    fn listing_can_be_filtered_by_cwd() {
        let mut sessions = Sessions::new();
        sessions.create("/a");
        let b = sessions.create("/b");

        let listed = sessions.list(Some(Path::new("/b")));

        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, b);
    }

    /// A cancel hook that records that it ran.
    fn flag() -> (Arc<AtomicBool>, impl FnOnce() + Send + 'static) {
        let called = Arc::new(AtomicBool::new(false));
        let seen = Arc::clone(&called);
        (called, move || seen.store(true, Ordering::SeqCst))
    }

    #[test]
    fn a_second_turn_is_refused_while_the_first_is_running() {
        let mut sessions = Sessions::new();
        let id = sessions.create("/a");
        sessions.begin_turn(&id, || {}).unwrap();

        assert_eq!(
            sessions.begin_turn(&id, || {}),
            Err(SessionError::Busy(id.clone()))
        );
    }

    #[test]
    fn a_finished_turn_frees_the_session_for_the_next_one() {
        let mut sessions = Sessions::new();
        let id = sessions.create("/a");
        let turn = sessions.begin_turn(&id, || {}).unwrap();

        assert!(sessions.finish_turn(&id, turn.token));

        assert!(sessions.begin_turn(&id, || {}).is_ok());
    }

    #[test]
    fn busy_sessions_do_not_block_other_sessions() {
        let mut sessions = Sessions::new();
        let a = sessions.create("/a");
        let b = sessions.create("/b");
        sessions.begin_turn(&a, || {}).unwrap();

        assert!(sessions.begin_turn(&b, || {}).is_ok());
    }

    #[test]
    fn cancelling_stops_the_running_turn_and_reports_it() {
        let mut sessions = Sessions::new();
        let id = sessions.create("/a");
        let (called, hook) = flag();
        let turn = sessions.begin_turn(&id, hook).unwrap();

        assert!(sessions.cancel(&id));

        assert!(called.load(Ordering::SeqCst));
        assert!(!sessions.is_current(&id, turn.token));
    }

    #[test]
    fn a_turn_finishes_exactly_once_even_if_cancelled_first() {
        let mut sessions = Sessions::new();
        let id = sessions.create("/a");
        let turn = sessions.begin_turn(&id, || {}).unwrap();
        sessions.cancel(&id);

        // The cancel owns the terminal update, so a late finish must not claim it.
        assert!(!sessions.finish_turn(&id, turn.token));
    }

    #[test]
    fn a_stale_token_cannot_finish_the_turn_that_replaced_it() {
        let mut sessions = Sessions::new();
        let id = sessions.create("/a");
        let old = sessions.begin_turn(&id, || {}).unwrap();
        sessions.cancel(&id);
        let new = sessions.begin_turn(&id, || {}).unwrap();

        assert!(!sessions.finish_turn(&id, old.token));
        assert!(sessions.is_current(&id, new.token));
    }

    #[test]
    fn cancelling_an_idle_or_unknown_session_reports_nothing_to_cancel() {
        let mut sessions = Sessions::new();
        let id = sessions.create("/a");

        assert!(!sessions.cancel(&id));
        assert!(!sessions.cancel("nope"));
    }

    #[test]
    fn closing_a_running_session_cancels_its_turn_and_reports_it() {
        let mut sessions = Sessions::new();
        let id = sessions.create("/a");
        let (called, hook) = flag();
        sessions.begin_turn(&id, hook).unwrap();

        assert_eq!(sessions.close(&id), Ok(true));

        assert!(called.load(Ordering::SeqCst));
    }

    #[test]
    fn closing_an_unknown_session_cancels_nothing() {
        let mut sessions = Sessions::new();

        assert_eq!(
            sessions.close("nope"),
            Err(SessionError::Unknown("nope".into()))
        );
    }

    #[test]
    fn finishing_a_turn_does_not_call_its_cancel_hook() {
        let mut sessions = Sessions::new();
        let id = sessions.create("/a");
        let (called, hook) = flag();
        let turn = sessions.begin_turn(&id, hook).unwrap();

        sessions.finish_turn(&id, turn.token);

        assert!(!called.load(Ordering::SeqCst));
    }
}
