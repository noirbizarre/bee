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
}

/// The identifiers of one prompt turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    /// The id of the user message the prompt inserted.
    pub user_message_id: String,
    /// The id of the agent message that answers it.
    pub agent_message_id: String,
}

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
            },
        );
        id
    }

    /// Start a prompt turn in `id` and return the ids it uses.
    ///
    /// # Errors
    ///
    /// [`SessionError::Unknown`] or [`SessionError::Closed`].
    pub fn begin_turn(&mut self, id: &str) -> Result<Turn, SessionError> {
        let session = self.open_session(id)?;
        session.turns += 1;
        Ok(Turn {
            user_message_id: format!("user-{}", session.turns),
            agent_message_id: format!("agent-{}", session.turns),
        })
    }

    /// Close `id`. Closing a closed session succeeds: the client's goal, a
    /// session that is not running, is already met.
    ///
    /// # Errors
    ///
    /// [`SessionError::Unknown`].
    pub fn close(&mut self, id: &str) -> Result<(), SessionError> {
        self.session(id)?.open = false;
        Ok(())
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

        sessions.begin_turn(&a).unwrap();
        let second_of_a = sessions.begin_turn(&a).unwrap();
        let first_of_b = sessions.begin_turn(&b).unwrap();

        assert_eq!(second_of_a.user_message_id, "user-2");
        assert_eq!(first_of_b.user_message_id, "user-1");
    }

    #[test]
    fn a_turn_in_an_unknown_session_is_refused() {
        let mut sessions = Sessions::new();

        assert_eq!(
            sessions.begin_turn("nope"),
            Err(SessionError::Unknown("nope".into()))
        );
    }

    #[test]
    fn a_turn_in_a_closed_session_is_refused_until_it_is_resumed() {
        let mut sessions = Sessions::new();
        let id = sessions.create("/a");
        sessions.close(&id).unwrap();

        assert_eq!(
            sessions.begin_turn(&id),
            Err(SessionError::Closed(id.clone()))
        );
        sessions.resume(&id, Path::new("/a")).unwrap();
        assert!(sessions.begin_turn(&id).is_ok());
    }

    #[test]
    fn closing_twice_is_not_an_error() {
        let mut sessions = Sessions::new();
        let id = sessions.create("/a");

        sessions.close(&id).unwrap();

        assert_eq!(sessions.close(&id), Ok(()));
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
}
