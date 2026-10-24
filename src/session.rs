//! Session state, independent of the wire protocol.
//!
//! Kept free of the ACP SDK on purpose: `acp.rs` is the only module that may
//! import it (ADR-001), so what a session *is* lives here and how it is spoken
//! lives there.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::llm::Message;
use crate::provider::{Providers, check_model_id};

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

    /// A setting change arrived while the session's turn was running.
    // A turn uses the settings it started with; changing them underneath would
    // make the answer's provider and model depend on timing.
    #[error(
        "session `{0}` is running a turn; wait for it to go idle, or stop it \
         with `session/cancel`, before changing its settings"
    )]
    SettingsBusy(String),

    /// The client chose a provider profile that is not configured.
    #[error("unknown provider `{name}`; the configured providers are: {available}")]
    UnknownProvider {
        /// What the client asked for.
        name: String,
        /// The configured profiles, comma separated.
        available: String,
    },

    /// The client chose a model id that cannot be one.
    #[error("invalid model id: {0}")]
    InvalidModel(&'static str),

    /// A provider or model was chosen but no provider profile is configured.
    #[error(
        "no provider is configured, so there is nothing to select; \
         define one under `[providers]` in Bee's configuration"
    )]
    NoProvider,
}

/// A session's provider and model, as the client sees and may change them.
///
/// These are *session settings*, chosen over ACP, and distinct from provider
/// *configuration* (endpoint, headers, credential), which only the
/// configuration files set (ADR-007).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// The effective provider profile.
    pub provider: String,
    /// Every configured profile name, sorted.
    pub providers: Vec<String>,
    /// The effective model, if the profile or the client gave one.
    pub model: Option<String>,
    /// The model ids to suggest: the profile's, plus the client's choice if it
    /// is not among them. A suggestion, never a limit.
    pub models: Vec<String>,
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

fn portable(history: &mut [Message]) {
    for message in history {
        if let Message::Assistant { native, .. } = message {
            *native = None;
        }
    }
}

/// What `session/list` reports about one session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    /// The session's id.
    pub id: String,
    /// The session's working directory.
    pub cwd: PathBuf,
}

/// One atomic turn snapshot, including the execution base for native tools.
#[derive(Clone)]
pub(crate) struct InferenceInput {
    pub(crate) instructions: crate::instructions::State,
    pub(crate) profile: crate::provider::Profile,
    pub(crate) model: String,
    pub(crate) messages: Vec<Message>,
    pub(crate) cwd: PathBuf,
}

#[derive(Debug)]
struct Session {
    instructions: crate::instructions::State,
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
    /// The profile the client chose, if it overrode the configured default.
    provider: Option<String>,
    /// The model the client chose, if it overrode the profile's.
    model: Option<String>,
    history: Vec<Message>,
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
    providers: Providers,
}

impl Sessions {
    /// An empty registry with no provider configured.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty registry whose sessions choose among `providers`.
    #[must_use]
    pub fn with_providers(providers: Providers) -> Self {
        Self {
            providers,
            ..Self::default()
        }
    }

    /// The provider and model `id` would use for its next turn.
    ///
    /// Precedence: the client's choice for this session, then the
    /// configuration's default, then (for the provider) the only profile.
    /// `None` when no provider is configured at all.
    ///
    /// # Errors
    ///
    /// [`SessionError::Unknown`]. A closed session is still described, so a
    /// resume can report it.
    pub fn settings(&self, id: &str) -> Result<Option<Settings>, SessionError> {
        let session = self
            .by_id
            .get(id)
            .ok_or_else(|| SessionError::Unknown(id.to_owned()))?;
        Ok(Self::describe(&self.providers, session))
    }

    /// Snapshot the selected configuration and committed model context.
    /// Called under the same registry lock that registers the turn.
    pub(crate) fn inference(&self, id: &str) -> Option<InferenceInput> {
        let settings = self.settings(id).ok()??;
        let profile = self.providers.get(&settings.provider)?.clone();
        let session = self.by_id.get(id)?;
        Some(InferenceInput {
            instructions: session.instructions.clone(),
            profile,
            model: settings.model?,
            messages: session.history.clone(),
            cwd: session.cwd.clone(),
        })
    }

    /// Commit only while this turn owns the session; cancelled work is stale.
    pub(crate) fn commit(
        &mut self,
        id: &str,
        token: u64,
        history: Vec<Message>,
        instructions: crate::instructions::State,
    ) {
        if self.is_current(id, token)
            && let Some(session) = self.by_id.get_mut(id)
        {
            session.history = history;
            session.instructions = instructions;
        }
    }

    fn describe(providers: &Providers, session: &Session) -> Option<Settings> {
        let name = session
            .provider
            .as_deref()
            .or_else(|| providers.default_name())?;
        let profile = providers.get(name)?;
        let mut models: Vec<String> = profile
            .suggested_models()
            .into_iter()
            .map(str::to_owned)
            .collect();
        let model = session
            .model
            .clone()
            .or_else(|| profile.default_model().map(str::to_owned));
        if let Some(chosen) = &model
            && !models.contains(chosen)
        {
            models.push(chosen.clone());
        }
        Some(Settings {
            provider: name.to_owned(),
            providers: providers.names().map(str::to_owned).collect(),
            model,
            models,
        })
    }

    /// Choose the provider profile `id` uses. Choosing another provider drops
    /// the session's model choice: model ids belong to a provider.
    ///
    /// # Errors
    ///
    /// [`SessionError::NoProvider`], [`SessionError::UnknownProvider`],
    /// [`SessionError::Unknown`], [`SessionError::Closed`] or
    /// [`SessionError::SettingsBusy`].
    pub fn set_provider(&mut self, id: &str, name: &str) -> Result<(), SessionError> {
        if self.providers.is_empty() {
            return Err(SessionError::NoProvider);
        }
        if self.providers.get(name).is_none() {
            return Err(SessionError::UnknownProvider {
                name: name.to_owned(),
                available: self.providers.names().collect::<Vec<_>>().join(", "),
            });
        }
        let current = self.settings(id)?.map(|settings| settings.provider);
        let session = self.settable_session(id)?;
        if current.as_deref() != Some(name) {
            session.model = None;
            portable(&mut session.history);
        }
        session.provider = Some(name.to_owned());
        Ok(())
    }

    /// Choose the model `id` uses. Any well-formed id is accepted, listed or
    /// not: catalog presence is never required.
    ///
    /// # Errors
    ///
    /// [`SessionError::NoProvider`], [`SessionError::InvalidModel`],
    /// [`SessionError::Unknown`], [`SessionError::Closed`] or
    /// [`SessionError::SettingsBusy`].
    pub fn set_model(&mut self, id: &str, model: &str) -> Result<(), SessionError> {
        if self.providers.is_empty() {
            return Err(SessionError::NoProvider);
        }
        check_model_id(model).map_err(SessionError::InvalidModel)?;
        let session = self.settable_session(id)?;
        if session.model.as_deref() != Some(model) {
            portable(&mut session.history);
        }
        session.model = Some(model.to_owned());
        Ok(())
    }

    /// An open session with no turn running.
    fn settable_session(&mut self, id: &str) -> Result<&mut Session, SessionError> {
        let session = self.open_session(id)?;
        if session.active.is_some() {
            return Err(SessionError::SettingsBusy(id.to_owned()));
        }
        Ok(session)
    }

    /// Open a session in `cwd` and return its id.
    pub fn create(&mut self, cwd: impl Into<PathBuf>) -> String {
        self.created += 1;
        let id = format!("bee-session-{}", self.created);
        self.by_id.insert(
            id.clone(),
            Session {
                instructions: crate::instructions::State::default(),
                cwd: cwd.into(),
                open: true,
                turns: 0,
                created: self.created,
                active: None,
                provider: None,
                model: None,
                history: Vec::new(),
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

    /// Registry with the providers a TOML document defines.
    fn with(toml: &str) -> Sessions {
        let dir = tempfile::tempdir().unwrap();
        let file = crate::config::project_config_path(dir.path());
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, toml).unwrap();
        let config = crate::config::Config::load(
            &crate::config::Environment::new(None, None, None),
            dir.path(),
        )
        .unwrap();
        Sessions::with_providers(Providers::from_config(&config).unwrap())
    }

    const TWO: &str = "provider = \"a\"\n\
        [providers.a]\nprotocols = { openai-chat = {} }\nmodel = \"m1\"\nmodels = [\"m2\"]\n\
        [providers.b]\nprotocols = { openai-chat = {} }\nmodel = \"n1\"\n";

    #[test]
    fn without_providers_a_session_has_no_settings_and_nothing_to_select() {
        let mut sessions = Sessions::new();
        let id = sessions.create("/a");

        assert_eq!(sessions.settings(&id), Ok(None));
        assert_eq!(sessions.set_model(&id, "m"), Err(SessionError::NoProvider));
        assert_eq!(
            sessions.set_provider(&id, "p"),
            Err(SessionError::NoProvider)
        );
    }

    #[test]
    fn the_configured_defaults_apply_until_the_client_chooses() {
        let mut sessions = with(TWO);
        let id = sessions.create("/a");

        let settings = sessions.settings(&id).unwrap().unwrap();

        assert_eq!(settings.provider, "a");
        assert_eq!(settings.model.as_deref(), Some("m1"));
        assert_eq!(settings.models, ["m1", "m2"]);
        assert_eq!(settings.providers, ["a", "b"]);
    }

    #[test]
    fn a_model_the_profile_never_listed_is_accepted_and_suggested_back() {
        let mut sessions = with(TWO);
        let id = sessions.create("/a");

        sessions.set_model(&id, "my/private-finetune").unwrap();

        let settings = sessions.settings(&id).unwrap().unwrap();
        assert_eq!(settings.model.as_deref(), Some("my/private-finetune"));
        assert_eq!(settings.models, ["m1", "m2", "my/private-finetune"]);
    }

    #[test]
    fn choosing_another_provider_drops_the_model_choice_but_the_same_one_keeps_it() {
        let mut sessions = with(TWO);
        let id = sessions.create("/a");
        sessions.set_model(&id, "custom").unwrap();

        sessions.set_provider(&id, "a").unwrap();
        assert_eq!(
            sessions.settings(&id).unwrap().unwrap().model.as_deref(),
            Some("custom")
        );

        sessions.set_provider(&id, "b").unwrap();
        let settings = sessions.settings(&id).unwrap().unwrap();
        assert_eq!(settings.provider, "b");
        assert_eq!(settings.model.as_deref(), Some("n1"));
    }

    #[test]
    fn one_sessions_choice_never_reaches_another() {
        let mut sessions = with(TWO);
        let first = sessions.create("/a");
        let second = sessions.create("/a");

        sessions.set_provider(&first, "b").unwrap();

        assert_eq!(sessions.settings(&second).unwrap().unwrap().provider, "a");
    }

    #[test]
    fn an_unknown_provider_and_a_blank_model_are_refused() {
        let mut sessions = with(TWO);
        let id = sessions.create("/a");

        assert!(matches!(
            sessions.set_provider(&id, "zzz"),
            Err(SessionError::UnknownProvider { .. })
        ));
        assert!(matches!(
            sessions.set_model(&id, " "),
            Err(SessionError::InvalidModel(_))
        ));
    }

    #[test]
    fn settings_cannot_change_while_a_turn_runs() {
        let mut sessions = with(TWO);
        let id = sessions.create("/a");
        sessions.begin_turn(&id, || {}).unwrap();

        assert_eq!(
            sessions.set_model(&id, "x"),
            Err(SessionError::SettingsBusy(id.clone()))
        );
        assert_eq!(
            sessions.set_provider(&id, "b"),
            Err(SessionError::SettingsBusy(id))
        );
    }

    #[test]
    fn a_resumed_session_keeps_its_choice() {
        let mut sessions = with(TWO);
        let id = sessions.create("/a");
        sessions.set_provider(&id, "b").unwrap();
        sessions.close(&id).unwrap();
        sessions.resume(&id, Path::new("/a")).unwrap();

        assert_eq!(sessions.settings(&id).unwrap().unwrap().provider, "b");
    }
}
