//! Composer drafts set aside for a machinery turn. The text is held in memory
//! only: a person's half-written input is not replicated state, and a restart
//! that loses it loses no more than the terminal itself would.

use std::{collections::HashMap, sync::Mutex};

use tracing::{info, warn};

use crate::{TerminalPool, TerminalSubmission};

/// Drafts cleared from each session's composer and not yet typed back.
#[derive(Debug, Default)]
pub struct ComposerDrafts {
    held: Mutex<HashMap<String, Vec<String>>>,
}

impl ComposerDrafts {
    /// Submit a turn over an abandoned draft: hold the draft, clear exactly it
    /// under the controller guard, submit the turn, then type every held draft
    /// back without submitting. A draft that cannot be typed back stays held,
    /// traced, and is retried after the session's next turn.
    ///
    /// Reading or clearing the draft writes no input, so its failures are
    /// refusals: `draft-changed` when the composer moved under the guard,
    /// `draft-unavailable` when the pool could not read or clear it.
    pub async fn guarded_deliver(
        &self,
        pool: &dyn TerminalPool,
        session: &str,
        text: &str,
        key: &str,
    ) -> Result<TerminalSubmission, String> {
        match pool.abandoned_draft(session).await {
            Ok(None) => {}
            Ok(Some(draft)) => {
                self.hold(session, draft.clone());
                match pool.clear_draft(session, &draft).await {
                    Ok(true) => info!(%session, chars = draft.chars().count(), "abandoned composer draft held in memory for a turn"),
                    Ok(false) => {
                        // Still in the composer, or replaced by newer input: not ours to restore.
                        self.release(session, &draft);
                        return Ok(TerminalSubmission::Refused("draft-changed: composer changed before guarded clear".into()));
                    }
                    Err(error) => {
                        self.release(session, &draft);
                        return Ok(TerminalSubmission::Refused(format!("draft-unavailable: {error}")));
                    }
                }
            }
            Err(error) => return Ok(TerminalSubmission::Refused(format!("draft-unavailable: {error}"))),
        }
        let submission = pool.guarded_deliver(session, text, key).await;
        self.restore(pool, session).await;
        submission
    }

    /// The drafts held for a session, oldest first.
    pub fn held(&self, session: &str) -> Vec<String> {
        self.held.lock().expect("composer drafts lock poisoned").get(session).cloned().unwrap_or_default()
    }

    fn hold(&self, session: &str, draft: String) {
        let mut held = self.held.lock().expect("composer drafts lock poisoned");
        let drafts = held.entry(session.to_string()).or_default();
        if !drafts.contains(&draft) {
            drafts.push(draft);
        }
    }

    fn release(&self, session: &str, draft: &str) {
        let mut held = self.held.lock().expect("composer drafts lock poisoned");
        if let Some(drafts) = held.get_mut(session) {
            drafts.retain(|held| held != draft);
            if drafts.is_empty() {
                held.remove(session);
            }
        }
    }

    async fn restore(&self, pool: &dyn TerminalPool, session: &str) {
        for draft in self.held(session) {
            match pool.type_without_submit(session, &draft).await {
                Ok(()) => {
                    info!(%session, chars = draft.chars().count(), "composer draft typed back");
                    self.release(session, &draft);
                }
                Err(error) => warn!(%session, %error, %draft, "composer draft not typed back; held for the next turn"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use flotilla_paths::path_context::ExecutionEnvironmentPath;
    use flotilla_protocol::arg::Arg;

    use super::*;
    use crate::{TerminalEnvVars, TerminalSession, TerminalSessionTag};

    #[derive(Default)]
    struct Composer {
        value: Mutex<String>,
        clear_refused: bool,
        typing_refused: Mutex<bool>,
        log: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl TerminalPool for Composer {
        async fn abandoned_draft(&self, _session: &str) -> Result<Option<String>, String> {
            let value = self.value.lock().unwrap().clone();
            Ok((!value.is_empty()).then_some(value))
        }
        async fn clear_draft(&self, _session: &str, draft: &str) -> Result<bool, String> {
            let mut value = self.value.lock().unwrap();
            if self.clear_refused || *value != draft {
                return Ok(false);
            }
            value.clear();
            Ok(true)
        }
        async fn guarded_deliver(&self, _session: &str, text: &str, _key: &str) -> Result<TerminalSubmission, String> {
            if !self.value.lock().unwrap().is_empty() {
                return Ok(TerminalSubmission::Refused("not-matched: composer not empty".into()));
            }
            self.log.lock().unwrap().push(format!("submit {text}"));
            Ok(TerminalSubmission::Submitted { keyed: false })
        }
        async fn type_without_submit(&self, _session: &str, text: &str) -> Result<(), String> {
            if *self.typing_refused.lock().unwrap() {
                return Err("controller-active: controller input is not idle".into());
            }
            self.value.lock().unwrap().push_str(text);
            self.log.lock().unwrap().push(format!("type {text}"));
            Ok(())
        }
        async fn list_sessions(&self) -> Result<Vec<TerminalSession>, String> {
            Ok(Vec::new())
        }
        async fn ensure_session(
            &self,
            _: &str,
            _: &str,
            _: &ExecutionEnvironmentPath,
            _: &TerminalEnvVars,
            _: &[TerminalSessionTag],
        ) -> Result<(), String> {
            Ok(())
        }
        fn attach_args(&self, _: &str, _: Option<&str>, _: &ExecutionEnvironmentPath, _: &TerminalEnvVars) -> Result<Vec<Arg>, String> {
            Ok(Vec::new())
        }
        async fn kill_session(&self, _: &str) -> Result<(), String> {
            Ok(())
        }
    }

    fn composer(value: &str) -> Composer {
        Composer { value: Mutex::new(value.into()), ..Default::default() }
    }

    #[tokio::test]
    async fn a_turn_over_an_abandoned_draft_types_the_draft_back_unsubmitted() {
        let pool = composer("half a thought");
        let drafts = ComposerDrafts::default();
        assert_eq!(drafts.guarded_deliver(&pool, "agent", "turn", "k").await.unwrap(), TerminalSubmission::Submitted { keyed: false });
        assert_eq!(*pool.log.lock().unwrap(), ["submit turn", "type half a thought"]);
        assert_eq!(*pool.value.lock().unwrap(), "half a thought");
        assert!(drafts.held("agent").is_empty());
    }

    #[tokio::test]
    async fn a_composer_that_changes_under_the_guard_is_left_alone() {
        let pool = Composer { clear_refused: true, ..composer("someone typing") };
        let drafts = ComposerDrafts::default();
        let refused = drafts.guarded_deliver(&pool, "agent", "turn", "k").await.unwrap();
        assert!(matches!(refused, TerminalSubmission::Refused(reason) if reason.starts_with("draft-changed:")));
        assert!(pool.log.lock().unwrap().is_empty(), "no turn and no retyping");
        assert!(drafts.held("agent").is_empty());
    }

    #[tokio::test]
    async fn a_draft_that_cannot_be_typed_back_is_held_until_the_next_turn() {
        let pool = composer("half a thought");
        *pool.typing_refused.lock().unwrap() = true;
        let drafts = ComposerDrafts::default();
        assert_eq!(drafts.guarded_deliver(&pool, "agent", "first", "k1").await.unwrap(), TerminalSubmission::Submitted { keyed: false });
        assert_eq!(drafts.held("agent"), ["half a thought"]);
        *pool.typing_refused.lock().unwrap() = false;
        assert_eq!(drafts.guarded_deliver(&pool, "agent", "second", "k2").await.unwrap(), TerminalSubmission::Submitted { keyed: false });
        assert_eq!(*pool.log.lock().unwrap(), ["submit first", "submit second", "type half a thought"]);
        assert!(drafts.held("agent").is_empty());
    }
}
