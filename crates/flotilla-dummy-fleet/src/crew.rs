//! Scripted crews. A crew is launched by the dummy terminal pool, receives its
//! turns as delivered text, and acts only through the crew commands a real
//! agent would send: promises and submissions, handoffs, stalls, failures and
//! completion.
//!
//! A brief selects its script with a line `dummy-script: <script> [key=value]…`:
//!
//! - `coder [review=<role>] [repo=<owner/name>]` promises a change request,
//!   opens one on the dummy forge (or on the configured shared forge) and
//!   submits it, optionally asks a peer to review it and waits for the reply,
//!   then completes.
//! - `responder reply=<role>` answers every turn with a handoff to `reply`,
//!   and completes after answering a turn marked [`LAST_TURN_MARKER`].
//! - `governor aide=<role> [turns=<n>]` is a long runner: it hands its aide
//!   `n` tasks one at a time, waiting for each reply, then completes.
//!
//! Without a script line a crew is a lone coder.
//!
//! A crew cannot fail its own work: at its configured failure rate it stalls
//! proposing `fail`, and a supervisor decides, as with a real crew.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
    time::Duration,
};

use flotilla_environment::environment::dummy::dummy_draw;
use flotilla_protocol::{
    commands::{CommandValue, CrewPromiseOperation, StallProposedDisposition, StallReason},
    query::CrewCommandContext,
    CommandAction,
};
use tokio::sync::mpsc;

use crate::{
    fleet::{Counter, DummyFleet},
    shared_forge::SharedForge,
    terminal::DummySession,
};

pub const SCRIPT_LINE_PREFIX: &str = "dummy-script:";
/// Marks the final task of an exchange; its responder completes after replying.
pub const LAST_TURN_MARKER: &str = "[dummy:last]";
const DEFAULT_REPOSITORY: &str = "dummy/fleet";
const PROMISE_ID: &str = "pr";
const PROMISE_KIND: &str = "pr";
/// Attempts at a refused command before the crew stalls with the error.
const COMMAND_ATTEMPTS: usize = 3;
/// Longest wait between completion attempts while a promise is unsettled.
const MAX_COMPLETION_BACKOFF: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CrewScript {
    Coder { reviewer: Option<String>, repository: String },
    Responder { reply_to: String },
    Governor { aide: String, turns: u32 },
}

impl CrewScript {
    /// The script named in a brief, or a lone coder.
    pub fn from_brief(brief: &str, default_turns: u32) -> Result<Self, String> {
        match brief.lines().find_map(|line| line.trim().strip_prefix(SCRIPT_LINE_PREFIX)) {
            Some(line) => Self::parse(line, default_turns),
            None => Ok(Self::Coder { reviewer: None, repository: DEFAULT_REPOSITORY.into() }),
        }
    }

    pub fn parse(line: &str, default_turns: u32) -> Result<Self, String> {
        let mut words = line.split_whitespace();
        let name = words.next().ok_or("dummy script line names no script")?;
        let mut parameters = BTreeMap::new();
        for word in words {
            let (key, value) = word.split_once('=').ok_or_else(|| format!("dummy script parameter `{word}` is not key=value"))?;
            parameters.insert(key, value);
        }
        let take = |parameters: &mut BTreeMap<&str, &str>, key: &str| parameters.remove(key).map(str::to_string);
        let script = match name {
            "coder" => Self::Coder {
                reviewer: take(&mut parameters, "review"),
                repository: take(&mut parameters, "repo").unwrap_or_else(|| DEFAULT_REPOSITORY.into()),
            },
            "responder" => Self::Responder { reply_to: take(&mut parameters, "reply").ok_or("responder script needs reply=<role>")? },
            "governor" => Self::Governor {
                aide: take(&mut parameters, "aide").ok_or("governor script needs aide=<role>")?,
                turns: match take(&mut parameters, "turns") {
                    Some(turns) => turns.parse().map_err(|_| format!("governor turns `{turns}` is not a number"))?,
                    None => default_turns,
                },
            },
            other => return Err(format!("unknown dummy script `{other}`")),
        };
        match parameters.keys().next() {
            Some(unknown) => Err(format!("dummy script `{name}` has no parameter `{unknown}`")),
            None => Ok(script),
        }
    }

    pub fn render(&self) -> String {
        match self {
            Self::Coder { reviewer: Some(reviewer), repository } => format!("coder review={reviewer} repo={repository}"),
            Self::Coder { reviewer: None, repository } => format!("coder repo={repository}"),
            Self::Responder { reply_to } => format!("responder reply={reply_to}"),
            Self::Governor { aide, turns } => format!("governor aide={aide} turns={turns}"),
        }
    }
}

/// Who a crew is, from the environment its terminal session was launched with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CrewIdentity {
    pub(crate) crew_id: String,
    /// The crew's TerminalSession, which hook-style attention reports name.
    pub(crate) session: Option<String>,
    pub(crate) namespace: String,
    pub(crate) convoy: String,
    pub(crate) vessel: String,
    pub(crate) role: String,
}

impl CrewIdentity {
    pub(crate) fn from_environment(environment: &[(String, String)]) -> Option<Self> {
        let get = |key: &str| environment.iter().rev().find(|(name, _)| name == key).map(|(_, value)| value.clone());
        Some(Self {
            crew_id: get("FLOTILLA_CREW_ID")?,
            session: get("FLOTILLA_TERMINAL_SESSION"),
            namespace: get("FLOTILLA_NAMESPACE")?,
            convoy: get("FLOTILLA_CONVOY")?,
            vessel: get("FLOTILLA_VESSEL")?,
            role: get("FLOTILLA_CREW_ROLE")?,
        })
    }

    fn context(&self) -> CrewCommandContext {
        CrewCommandContext {
            crew_id: Some(self.crew_id.clone()),
            namespace: Some(self.namespace.clone()),
            convoy: Some(self.convoy.clone()),
            vessel_ref: Some(self.vessel.clone()),
            role: Some(self.role.clone()),
        }
    }

    fn subject(&self) -> String {
        format!("{}/{}/{}", self.convoy, self.vessel, self.role)
    }
}

pub(crate) struct Crew {
    fleet: Arc<DummyFleet>,
    session: Arc<DummySession>,
    identity: CrewIdentity,
    turns: mpsc::UnboundedReceiver<String>,
    /// Peer tasks that arrived while the crew waited for guidance.
    held: VecDeque<String>,
}

enum Ending {
    Completed,
    Failed,
}

impl Crew {
    pub(crate) fn new(
        fleet: Arc<DummyFleet>,
        session: Arc<DummySession>,
        identity: CrewIdentity,
        turns: mpsc::UnboundedReceiver<String>,
    ) -> Self {
        Self { fleet, session, identity, turns, held: VecDeque::new() }
    }

    pub(crate) async fn run(mut self, script: CrewScript) {
        // The launch prompt is the first turn.
        self.mark(true).await;
        self.think().await;
        let ending = match self.begin().await {
            Some(ending) => Some(ending),
            None => match script {
                CrewScript::Coder { reviewer, repository } => self.code(reviewer, &repository).await,
                CrewScript::Responder { reply_to } => self.respond(&reply_to).await,
                CrewScript::Governor { aide, turns } => self.govern(&aide, turns).await,
            },
        };
        match ending {
            Some(Ending::Completed) => self.fleet.count(Counter::CrewsCompleted),
            Some(Ending::Failed) => self.fleet.count(Counter::CrewsFailed),
            None => {}
        }
        // Completion leaves the conversation alive. A handoff can reopen Done
        // work, so consume the delivered turn and report again after acting on it.
        // Keep completion counters per script, rather than per follow-up report.
        if matches!(ending, Some(Ending::Completed)) {
            while self.next_turn().await.is_some() {
                // Report this follow-up without repeating the original completion message or URL.
                if self.complete(None).await.is_none() {
                    break;
                }
            }
        } else {
            self.mark(false).await;
            while self.turns.recv().await.is_some() {}
        }
    }

    /// Fail or stall at the configured rates before doing any work.
    async fn begin(&mut self) -> Option<Ending> {
        let agent = &self.fleet.config.agent;
        let subject = self.identity.subject();
        if dummy_draw(self.fleet.config.seed, "crew-failure", &subject) < agent.fail_rate {
            let action = CommandAction::CrewStall {
                context: self.identity.context(),
                reason: StallReason::Infra,
                proposed_disposition: Some(StallProposedDisposition::Fail),
                message: "dummy crew failed at its configured rate".into(),
            };
            self.attempt(action).await;
            return Some(Ending::Failed);
        }
        if dummy_draw(self.fleet.config.seed, "crew-stall", &subject) < agent.stall_rate {
            let action = CommandAction::CrewStall {
                context: self.identity.context(),
                reason: StallReason::Other,
                proposed_disposition: Some(StallProposedDisposition::Resume),
                message: "dummy crew stalled at its configured rate".into(),
            };
            if self.attempt(action).await.is_some() {
                self.fleet.count(Counter::CrewsStalled);
            }
            self.await_guidance().await?;
        }
        None
    }

    async fn code(&mut self, reviewer: Option<String>, repository: &str) -> Option<Ending> {
        let promise = CrewPromiseOperation::Promise { id: PROMISE_ID.into(), kind: PROMISE_KIND.into() };
        // A crew resumed after a restart may have promised already.
        let _ = self.fleet.execute(CommandAction::CrewPromise { context: self.identity.context(), operation: promise }).await;
        let branch = format!("dummy/{}/{}", self.identity.convoy, self.identity.vessel);
        let title = format!("Dummy work for {}", self.identity.convoy);
        let url = match self.fleet.shared_forge.clone() {
            Some(shared) => self.open_on_shared_forge(&shared, repository, &branch, &title).await?,
            None => self.fleet.forge.open(repository, &branch, &title).1,
        };
        self.fleet.count(Counter::ChangeRequestsOpened);
        let submit = CrewPromiseOperation::Submit {
            promise: Some(PROMISE_ID.into()),
            kind: PROMISE_KIND.into(),
            reference: url.clone(),
            metadata: BTreeMap::new(),
        };
        self.persist(CommandAction::CrewPromise { context: self.identity.context(), operation: submit }).await?;
        if let Some(reviewer) = reviewer {
            self.hand_off(&reviewer, &format!("Please review {url} {LAST_TURN_MARKER}")).await?;
            self.next_turn().await?;
        }
        self.complete(Some(url)).await
    }

    async fn respond(&mut self, reply_to: &str) -> Option<Ending> {
        loop {
            let turn = self.next_turn().await?;
            self.hand_off(reply_to, "Done.").await?;
            if turn.contains(LAST_TURN_MARKER) {
                return self.complete(None).await;
            }
        }
    }

    async fn govern(&mut self, aide: &str, turns: u32) -> Option<Ending> {
        for task in 1..=turns {
            let last = if task == turns { LAST_TURN_MARKER } else { "" };
            self.hand_off(aide, &format!("Task {task} of {turns}. {last}")).await?;
            self.next_turn().await?;
        }
        self.complete(None).await
    }

    async fn hand_off(&mut self, target: &str, message: &str) -> Option<CommandValue> {
        self.persist(CommandAction::CrewHandoff {
            context: self.identity.context(),
            target: target.into(),
            message: message.into(),
            carries: Vec::new(),
        })
        .await
    }

    /// Send a command the script needs, as a real crew would: retry a refusal
    /// a few times, then stall with the error and retry after guidance.
    /// `None` once the session is gone.
    async fn persist(&mut self, action: CommandAction) -> Option<CommandValue> {
        loop {
            let mut backoff = Duration::from_secs(1);
            let mut last_error = String::new();
            for _ in 0..COMMAND_ATTEMPTS {
                match self.fleet.execute(action.clone()).await {
                    Ok(value) => return Some(value),
                    Err(error) => {
                        tracing::warn!(crew = %self.identity.subject(), %error, "dummy crew command refused; retrying");
                        last_error = error;
                        self.idle_for(backoff).await;
                        backoff *= 2;
                    }
                }
            }
            let stall = CommandAction::CrewStall {
                context: self.identity.context(),
                reason: StallReason::Infra,
                proposed_disposition: Some(StallProposedDisposition::Resume),
                message: format!("dummy crew command kept failing: {last_error}"),
            };
            if self.attempt(stall).await.is_some() {
                self.fleet.count(Counter::CrewsStalled);
            }
            self.await_guidance().await?;
        }
    }

    /// Open the change request on the shared forge as a real crew would:
    /// retry a failure a few times, then stall with it and retry after
    /// guidance. `None` once the session is gone.
    async fn open_on_shared_forge(&mut self, shared: &SharedForge, repository: &str, branch: &str, title: &str) -> Option<String> {
        let body = format!("Opened by dummy crew {}.", self.identity.subject());
        loop {
            let mut backoff = Duration::from_secs(1);
            let mut last_error = String::new();
            for attempt in 1..=COMMAND_ATTEMPTS {
                match shared.open(repository, branch, title, &body).await {
                    Ok(url) => return Some(url),
                    Err(error) => {
                        tracing::warn!(crew = %self.identity.subject(), attempt, %error, "dummy crew could not open its change request");
                        last_error = error;
                        // Back off between attempts; after the last, stall at once.
                        if attempt < COMMAND_ATTEMPTS {
                            self.idle_for(backoff).await;
                            backoff *= 2;
                        }
                    }
                }
            }
            let stall = CommandAction::CrewStall {
                context: self.identity.context(),
                reason: StallReason::Infra,
                proposed_disposition: Some(StallProposedDisposition::Resume),
                message: format!("dummy crew could not open its change request on the shared forge: {last_error}"),
            };
            if self.attempt(stall).await.is_some() {
                self.fleet.count(Counter::CrewsStalled);
            }
            self.await_guidance().await?;
        }
    }

    /// While stalled, wait for guidance from supervision (any non-peer turn).
    /// Peer tasks arriving meanwhile are held for the script.
    async fn await_guidance(&mut self) -> Option<()> {
        loop {
            self.mark(false).await;
            let turn = self.turns.recv().await?;
            self.mark(true).await;
            if is_peer_turn(&turn) {
                self.held.push_back(turn);
            } else {
                self.think().await;
                return Some(());
            }
        }
    }

    /// Complete, waiting out refusals while a promise is still unsettled.
    async fn complete(&mut self, message: Option<String>) -> Option<Ending> {
        let mut backoff = Duration::from_secs(1);
        loop {
            let action = CommandAction::CrewComplete {
                context: self.identity.context(),
                message: message.clone(),
                disposition: None,
                decision_ledger_ref: None,
                force: false,
            };
            match self.fleet.execute(action).await {
                Ok(CommandValue::CrewCompletionWaiting { retry_at, .. }) => {
                    let wait = (retry_at - chrono::Utc::now()).to_std().unwrap_or_default();
                    self.idle_for(wait.clamp(Duration::from_millis(100), MAX_COMPLETION_BACKOFF)).await;
                }
                // A supervisor's follow-up arrives as a turn; answer it, then retry.
                Ok(CommandValue::CrewFollowUpDelivered) => {
                    self.next_turn().await?;
                }
                Ok(_) => return Some(Ending::Completed),
                Err(error) => {
                    tracing::debug!(crew = %self.identity.subject(), %error, "dummy crew completion refused; retrying");
                    self.idle_for(backoff).await;
                    backoff = (backoff * 2).min(MAX_COMPLETION_BACKOFF);
                }
            }
        }
    }

    async fn attempt(&self, action: CommandAction) -> Option<CommandValue> {
        match self.fleet.execute(action).await {
            Ok(value) => Some(value),
            Err(error) => {
                tracing::warn!(crew = %self.identity.subject(), %error, "dummy crew command failed");
                None
            }
        }
    }

    /// Wait idle for the next turn, then work on it. `None` once the session
    /// is gone.
    async fn next_turn(&mut self) -> Option<String> {
        self.mark(false).await;
        let turn = match self.held.pop_front() {
            Some(turn) => turn,
            None => self.turns.recv().await?,
        };
        self.mark(true).await;
        self.think().await;
        Some(turn)
    }

    async fn idle_for(&self, duration: Duration) {
        self.mark(false).await;
        tokio::time::sleep(duration).await;
        self.mark(true).await;
    }

    /// Show working or idle on the screen, and report it as a harness hook would.
    async fn mark(&self, working: bool) {
        self.session.set_working(working);
        if let Some(session) = &self.identity.session {
            self.fleet.report_attention(&self.identity.namespace, session, working).await;
        }
    }

    async fn think(&self) {
        tokio::time::sleep(Duration::from_millis(self.fleet.config.agent.turn_delay_ms)).await;
    }
}

/// Delivered turns are framed `[from <sender> (<relation>)…]`; a peer's
/// handoff is a task, anything else (supervision, the operator) is guidance.
fn is_peer_turn(turn: &str) -> bool {
    turn.lines().next().is_some_and(|header| header.starts_with("[from ") && header.contains(" (peer)"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripts_round_trip_through_their_brief_line() {
        for script in [
            CrewScript::Coder { reviewer: None, repository: "dummy/fleet".into() },
            CrewScript::Coder { reviewer: Some("reviewer".into()), repository: "acme/app".into() },
            CrewScript::Responder { reply_to: "coder".into() },
            CrewScript::Governor { aide: "aide".into(), turns: 7 },
        ] {
            let brief = format!("# Brief\n\nSome prose.\n{SCRIPT_LINE_PREFIX} {}\n", script.render());
            assert_eq!(CrewScript::from_brief(&brief, 3), Ok(script));
        }
    }

    #[test]
    fn a_brief_without_a_script_is_a_lone_coder_and_defaults_apply() {
        assert_eq!(
            CrewScript::from_brief("Implement the thing.", 3),
            Ok(CrewScript::Coder { reviewer: None, repository: DEFAULT_REPOSITORY.into() })
        );
        assert_eq!(CrewScript::parse("governor aide=aide", 4), Ok(CrewScript::Governor { aide: "aide".into(), turns: 4 }));
    }

    // Frames come from the daemon's message delivery: a peer's handoff is a
    // task; supervision, operator and system turns are guidance.
    #[test]
    fn only_peer_frames_are_tasks() {
        assert!(is_peer_turn("[from implement/coder (peer)]\n\nTask 1"));
        assert!(!is_peer_turn("[from the operator (supervisor)]\n\nCarry on."));
        assert!(!is_peer_turn("[from flotilla (system) · turn delivery]\n\nBrief"));
        assert!(!is_peer_turn("Task 1 (peer)"), "an unframed turn is guidance");
        assert!(!is_peer_turn(""));
    }

    #[test]
    fn malformed_script_lines_are_refused() {
        assert!(CrewScript::parse("", 1).is_err());
        assert!(CrewScript::parse("pilot", 1).is_err());
        assert!(CrewScript::parse("responder", 1).is_err());
        assert!(CrewScript::parse("coder review", 1).is_err());
        assert!(CrewScript::parse("coder colour=blue", 1).is_err());
        assert!(CrewScript::parse("governor aide=a turns=many", 1).is_err());
    }

    #[test]
    fn identity_comes_from_the_launch_environment() {
        let environment = [
            ("FLOTILLA_CREW_ID", "crew-1"),
            ("FLOTILLA_NAMESPACE", "flotilla"),
            ("FLOTILLA_CONVOY", "convoy-a"),
            ("FLOTILLA_VESSEL", "implement"),
            ("FLOTILLA_CREW_ROLE", "coder"),
        ]
        .map(|(key, value)| (key.to_string(), value.to_string()));
        let identity = CrewIdentity::from_environment(&environment).expect("crew identity");
        assert_eq!(identity.subject(), "convoy-a/implement/coder");
        assert_eq!(identity.context().crew_id.as_deref(), Some("crew-1"));
        assert_eq!(CrewIdentity::from_environment(&environment[1..]), None, "a tool session has no crew identity");
    }
}
