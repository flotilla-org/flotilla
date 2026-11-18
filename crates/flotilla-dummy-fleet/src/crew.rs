//! Scripted crews. A crew is launched by the dummy terminal pool, receives its
//! turns as delivered text, and acts only through the crew commands a real
//! agent would send: promises and submissions, handoffs, stalls, failures and
//! completion.
//!
//! A brief selects its script with a line `dummy-script: <script> [key=value]…`:
//!
//! - `coder [review=<role>] [repo=<owner/name>] [kept-<kind>=<reference>]…`
//!   promises a change request, opens one on the dummy forge (or on the
//!   configured shared forge) and submits it, optionally asks a peer to review
//!   it and waits for the reply, then completes. A kind its role has kept, which
//!   the adapter reads from the brief's [`KEPT_PROMISES_HEADING`] list, it
//!   skips: a kept `pr` with its review, a kept `decision-ledger` submission.
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

use flotilla_agent::adapter::KEPT_PROMISES_HEADING;
use flotilla_config::dummy_fleet::DummyFleetConfig;
use flotilla_environment::environment::dummy::dummy_draw;
use flotilla_protocol::{
    commands::{CommandValue, CrewPromiseOperation, CrewSupervisionAction, StallProposedDisposition, StallReason},
    query::CrewCommandContext,
    CommandAction,
};
use tokio::sync::mpsc;

use crate::{
    fleet::{Counter, DummyFleet},
    link::{CrewHooks, CrewLink},
    shared_forge::SharedForge,
    state::{Event, LaunchedCrew, Outcome},
    terminal::DummySession,
};

pub const SCRIPT_LINE_PREFIX: &str = "dummy-script:";
/// Marks the final task of an exchange; its responder completes after replying.
pub const LAST_TURN_MARKER: &str = "[dummy:last]";
const DEFAULT_REPOSITORY: &str = "dummy/fleet";
const PROMISE_KIND: &str = "pr";
/// Attempts at a refused command before the crew stalls with the error.
const COMMAND_ATTEMPTS: usize = 3;
/// Longest wait between completion attempts while a promise is unsettled.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CrewScript {
    Coder {
        reviewer: Option<String>,
        repository: String,
        /// The reference of each promise kind its role has kept, by kind.
        kept: BTreeMap<String, String>,
    },
    Responder {
        reply_to: String,
    },
    Governor {
        aide: String,
        turns: u32,
    },
    /// A standing governor: it resumes each stalled crew a supervision turn hands it, and runs
    /// until its convoy is stopped.
    Supervisor,
}

impl CrewScript {
    /// The script named in a brief, or a lone coder; a coder also takes the promises its role
    /// has kept, as the brief lists them.
    pub fn from_brief(brief: &str, default_turns: u32) -> Result<Self, String> {
        let mut script = match brief.lines().find_map(|line| line.trim().strip_prefix(SCRIPT_LINE_PREFIX)) {
            Some(line) => Self::parse(line, default_turns)?,
            None => Self::coder(None, DEFAULT_REPOSITORY.into()),
        };
        if let Self::Coder { kept, .. } = &mut script {
            kept.extend(kept_in_brief(brief));
        }
        Ok(script)
    }

    /// A coder whose role has kept nothing.
    pub fn coder(reviewer: Option<String>, repository: String) -> Self {
        Self::Coder { reviewer, repository, kept: BTreeMap::new() }
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
                kept: {
                    let kinds = parameters.keys().filter_map(|key| key.strip_prefix(KEPT_PREFIX)).map(str::to_owned).collect::<Vec<_>>();
                    kinds
                        .into_iter()
                        .filter_map(|kind| Some((kind.clone(), take(&mut parameters, &format!("{KEPT_PREFIX}{kind}"))?)))
                        .collect()
                },
            },
            "responder" => Self::Responder { reply_to: take(&mut parameters, "reply").ok_or("responder script needs reply=<role>")? },
            "governor" => Self::Governor {
                aide: take(&mut parameters, "aide").ok_or("governor script needs aide=<role>")?,
                turns: match take(&mut parameters, "turns") {
                    Some(turns) => turns.parse().map_err(|_| format!("governor turns `{turns}` is not a number"))?,
                    None => default_turns,
                },
            },
            "supervisor" => Self::Supervisor,
            other => return Err(format!("unknown dummy script `{other}`")),
        };
        match parameters.keys().next() {
            Some(unknown) => Err(format!("dummy script `{name}` has no parameter `{unknown}`")),
            None => Ok(script),
        }
    }

    pub fn render(&self) -> String {
        match self {
            Self::Coder { reviewer, repository, kept } => {
                let mut line = "coder".to_string();
                if let Some(reviewer) = reviewer {
                    line.push_str(&format!(" review={reviewer}"));
                }
                line.push_str(&format!(" repo={repository}"));
                for (kind, reference) in kept {
                    line.push_str(&format!(" {KEPT_PREFIX}{kind}={reference}"));
                }
                line
            }
            Self::Responder { reply_to } => format!("responder reply={reply_to}"),
            Self::Governor { aide, turns } => format!("governor aide={aide} turns={turns}"),
            Self::Supervisor => "supervisor".into(),
        }
    }
}

/// Prefixes a coder's parameter naming a promise kind its role has kept.
const KEPT_PREFIX: &str = "kept-";

/// The promises a brief lists under [`KEPT_PROMISES_HEADING`], as kind and reference: lines
/// "- `<id>` (<kind>): <reference>", up to the next heading.
fn kept_in_brief(brief: &str) -> BTreeMap<String, String> {
    let Some((_, listed)) = brief.split_once(KEPT_PROMISES_HEADING) else { return BTreeMap::new() };
    listed
        .lines()
        .skip(1)
        .take_while(|line| !line.starts_with('#'))
        .filter_map(|line| {
            let (_, rest) = line.strip_prefix("- `")?.split_once("` (")?;
            let (kind, reference) = rest.split_once("): ")?;
            Some((kind.to_owned(), reference.trim().to_owned()))
        })
        .collect()
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
    /// Where its Claude Code hooks post over HTTP; without it they run as commands.
    pub(crate) hooks: Option<CrewHooks>,
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
            hooks: CrewHooks::from_environment(environment),
        })
    }

    /// The identity variable `key` of the crew's launch environment, as `flotilla` reads it for
    /// the claims its Hello states.
    pub(crate) fn claim(&self, key: &str) -> Option<String> {
        match key {
            "FLOTILLA_CREW_ID" => Some(self.crew_id.clone()),
            "FLOTILLA_TERMINAL_SESSION" => self.session.clone(),
            "FLOTILLA_NAMESPACE" => Some(self.namespace.clone()),
            "FLOTILLA_CONVOY" => Some(self.convoy.clone()),
            "FLOTILLA_VESSEL" => Some(self.vessel.clone()),
            "FLOTILLA_CREW_ROLE" => Some(self.role.clone()),
            _ => None,
        }
    }

    /// The crew as the state file names it, running `script`.
    pub(crate) fn launched(self, script: &str) -> LaunchedCrew {
        LaunchedCrew {
            crew_id: self.crew_id,
            terminal_session: self.session,
            namespace: self.namespace,
            convoy: self.convoy,
            vessel: self.vessel,
            role: self.role,
            script: script.into(),
            hooks: self.hooks,
        }
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

impl From<&LaunchedCrew> for CrewIdentity {
    fn from(crew: &LaunchedCrew) -> Self {
        Self {
            crew_id: crew.crew_id.clone(),
            session: crew.terminal_session.clone(),
            namespace: crew.namespace.clone(),
            convoy: crew.convoy.clone(),
            vessel: crew.vessel.clone(),
            role: crew.role.clone(),
            hooks: crew.hooks.clone(),
        }
    }
}

/// A crew runs its script over its session's turns. A crew resumed in another process runs it
/// again from the start over the turns its session recorded, taking each command's recorded
/// outcome rather than sending it, without pausing and without counting or reporting what it
/// did before; once those outcomes run out it carries on from where its process ended.
pub(crate) struct Crew {
    fleet: Arc<DummyFleet>,
    session: Arc<DummySession>,
    identity: CrewIdentity,
    turns: mpsc::UnboundedReceiver<String>,
    /// Peer tasks that arrived while the crew waited for guidance.
    held: VecDeque<String>,
    script_finished: bool,
    /// Outcomes a previous process recorded, not yet taken again.
    replay: VecDeque<Outcome>,
    /// Whether the last command's outcome was a recorded one: what followed it was counted then.
    replayed: bool,
    /// Where the crew's session runs, as its hooks report it.
    cwd: String,
    /// Turns worked on since the crew started, keying each turn's hook-count draw.
    turns_taken: u64,
    /// The key of the command the crew is sending until it succeeds, which its retries carry
    /// so the daemon runs it once.
    request_key: Option<String>,
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
        replay: VecDeque<Outcome>,
    ) -> Self {
        let replayed = !replay.is_empty();
        let cwd = session.cwd().as_path().display().to_string();
        Self {
            fleet,
            session,
            identity,
            turns,
            held: VecDeque::new(),
            script_finished: false,
            replay,
            replayed,
            cwd,
            turns_taken: 0,
            request_key: None,
        }
    }

    fn resuming(&self) -> bool {
        !self.replay.is_empty()
    }

    /// The crew's way to its daemon, as its process in the vessel has.
    fn link(&self) -> CrewLink<'_> {
        CrewLink {
            fleet: &self.fleet,
            environment: &self.session.environment,
            identity: &self.identity,
            cwd: &self.cwd,
            request_key: self.request_key.as_deref(),
        }
    }

    /// Send a crew command or publish, or take its recorded outcome while resuming. Queries are
    /// read again.
    async fn act(&mut self, act: Act) -> Result<CommandValue, String> {
        if matches!(&act, Act::Command(action) if action.is_query()) {
            return self.send(act).await;
        }
        if let Some(outcome) = self.take_recorded().await {
            return match outcome {
                Outcome::Done { value } => Ok(*value),
                Outcome::Refused { error } => Err(error),
                Outcome::Opened { .. } => Err("a recorded change request stands where a command's outcome was".into()),
            };
        }
        self.replayed = false;
        let result = self.send(act).await;
        self.acted(match &result {
            Ok(value) => Outcome::Done { value: Box::new(value.clone()) },
            Err(error) => Outcome::Refused { error: error.clone() },
        });
        result
    }

    async fn send(&self, act: Act) -> Result<CommandValue, String> {
        let link = self.link();
        match act {
            Act::Command(action) => self.fleet.execute(&link, action).await,
            Act::PublishLedger(body) => self.fleet.publish_ledger(&link, &body).await,
        }
    }

    /// The next recorded outcome; when it is the last, the crew's harness reports where it is.
    async fn take_recorded(&mut self) -> Option<Outcome> {
        let outcome = self.replay.pop_front()?;
        if self.replay.is_empty() {
            self.report(self.session.working()).await;
        }
        Some(outcome)
    }

    fn acted(&self, outcome: Outcome) {
        self.fleet.record(&Event::Acted { session: self.session.name.clone(), outcome: outcome.clone() });
        self.session.record().outcomes.push(outcome);
    }

    fn count(&self, counter: Counter) {
        if !self.replayed {
            self.fleet.count(counter);
        }
    }

    pub(crate) async fn run(mut self, script: CrewScript) {
        // Claude Code starts its session, then takes the launch prompt as the first turn.
        if !self.resuming() {
            self.hook("session-start").await;
            self.hook("user-prompt-submit").await;
        }
        self.mark(true).await;
        self.think().await;
        let ending = match self.begin().await {
            Some(ending) => Some(ending),
            None => match script {
                CrewScript::Coder { reviewer, repository, kept } => self.code(reviewer, &repository, &kept).await,
                CrewScript::Responder { reply_to } => self.respond(&reply_to).await,
                CrewScript::Governor { aide, turns } => self.govern(&aide, turns).await,
                CrewScript::Supervisor => self.supervise().await,
            },
        };
        if matches!(ending, Some(Ending::Failed)) {
            self.count(Counter::CrewsFailed);
        }
        // Completion leaves the conversation alive. A handoff can reopen Done
        // work, so consume the delivered turn and report again after acting on it.
        // Keep completion counters per script, rather than per follow-up report.
        if matches!(ending, Some(Ending::Completed)) {
            while self.next_turn().await.is_some() {
                // Report this follow-up without repeating the original completion message or URL.
                if self.settle_promises().await.is_none() {
                    break;
                }
            }
        } else {
            self.mark(false).await;
            while self.turns.recv().await.is_some() {}
        }
    }

    /// What the crew's fail and stall draws are keyed on: its convoy's role, its vessel's name in
    /// the workflow and its crew role. The role is the name a dispatcher chose, so a scenario can
    /// pick a case by it; the convoy's record name is generated at admission.
    async fn draw_subject(&self) -> String {
        let identity = &self.identity;
        let role = self.convoy_role().await.unwrap_or_else(|| identity.convoy.clone());
        // The vessel is a launch, `<convoy launch_of>-<work>#<n>`; every launch of a work draws as
        // the work.
        let of = flotilla_resources::convoy::launch_of(&identity.convoy);
        let vessel = identity.vessel.strip_prefix(&format!("{of}-")).unwrap_or(&identity.vessel);
        let vessel = vessel.rsplit_once('#').map_or(vessel, |(work, _)| work);
        format!("{role}/{vessel}/{}", identity.role)
    }

    /// The crew's convoy's role, as `flotilla convoy explain` shows it to the crew.
    async fn convoy_role(&self) -> Option<String> {
        let action =
            CommandAction::QueryExplainConvoy { namespace: Some(self.identity.namespace.clone()), name: self.identity.convoy.clone() };
        match self.fleet.execute(&self.link(), action).await.ok()? {
            CommandValue::ConvoyExplanation(explanation) if !explanation.role.is_empty() => Some(explanation.role),
            _ => None,
        }
    }

    /// Fail or stall at the configured rates before doing any work.
    async fn begin(&mut self) -> Option<Ending> {
        let agent = &self.fleet.config.agent;
        if agent.fail_rate <= 0.0 && agent.stall_rate <= 0.0 {
            return None;
        }
        let subject = self.draw_subject().await;
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
                self.count(Counter::CrewsStalled);
            }
            self.await_guidance().await?;
        }
        None
    }

    async fn code(&mut self, reviewer: Option<String>, repository: &str, kept: &BTreeMap<String, String>) -> Option<Ending> {
        // A ledger its role has kept is not submitted again.
        if kept.contains_key("decision-ledger") {
            self.script_finished = true;
        }
        // A change request its role has kept is merged: it is not opened, submitted or reviewed again.
        if kept.contains_key(PROMISE_KIND) {
            return self.settle_promises().await;
        }
        // One branch per convoy launch and role: every launch of the role's crew works on the same
        // change request, and a standing convoy's next launch on a new one.
        let convoy = &self.identity.convoy;
        let branch = format!("dummy/{convoy}/{}", self.identity.role);
        let title = format!("Dummy work for {convoy}");
        let url = match self.fleet.shared_forge.clone() {
            Some(shared) => {
                let url = self.open_on_shared_forge(&shared, repository, &branch, &title).await?;
                self.count(Counter::ChangeRequestsOpened);
                url
            }
            None => {
                let (_, url, opened) = self.fleet.forge.open(&self.fleet, repository, &branch, &title);
                if opened {
                    self.fleet.count(Counter::ChangeRequestsOpened);
                }
                url
            }
        };
        let submit =
            CrewPromiseOperation::Submit { promise: None, kind: PROMISE_KIND.into(), reference: url.clone(), metadata: BTreeMap::new() };
        self.persist(CommandAction::CrewPromise { context: self.identity.context(), operation: submit }).await?;
        if let Some(reviewer) = reviewer {
            self.hand_off(&reviewer, &format!("Please review {url} {LAST_TURN_MARKER}")).await?;
            self.next_turn().await?;
        }
        self.settle_promises().await
    }

    async fn respond(&mut self, reply_to: &str) -> Option<Ending> {
        loop {
            let turn = self.next_turn().await?;
            self.hand_off(reply_to, "Done.").await?;
            if turn.contains(LAST_TURN_MARKER) {
                return self.settle_promises().await;
            }
        }
    }

    /// Resume every stalled crew a supervision turn names, with the command the turn gives, until
    /// the session ends.
    async fn supervise(&mut self) -> Option<Ending> {
        loop {
            let turn = self.next_turn().await?;
            for (convoy, vessel, role) in resume_commands(&turn) {
                self.attempt(CommandAction::CrewSupervise {
                    namespace: Some(self.identity.namespace.clone()),
                    convoy,
                    vessel,
                    role,
                    operation: CrewSupervisionAction::Resume,
                    message: "Carry on.".into(),
                    actor_crew_id: Some(self.identity.crew_id.clone()),
                })
                .await;
            }
        }
    }

    async fn govern(&mut self, aide: &str, turns: u32) -> Option<Ending> {
        for task in 1..=turns {
            let last = if task == turns { LAST_TURN_MARKER } else { "" };
            self.hand_off(aide, &format!("Task {task} of {turns}. {last}")).await?;
            self.next_turn().await?;
        }
        self.settle_promises().await
    }

    async fn hand_off(&mut self, target: &str, message: &str) -> Option<CommandValue> {
        // A peer is named by its role, as `flotilla crew <role> handoff` names it.
        let target = target.to_string();
        self.persist(CommandAction::CrewHandoff {
            context: self.identity.context(),
            target,
            message: message.into(),
            carries: Vec::new(),
            reassign: Vec::new(),
        })
        .await
    }

    /// Send a command the script needs, as a real crew would: retry a refusal
    /// a few times, then stall with the error and retry after guidance.
    /// `None` once the session is gone.
    async fn persist(&mut self, act: impl Into<Act>) -> Option<CommandValue> {
        let act = act.into();
        // Every retry of this command carries one key: a retry after a lost answer follows the
        // first execution, while one after a refusal runs again.
        let key = uuid::Uuid::new_v4().to_string();
        loop {
            let mut backoff = Duration::from_millis(self.fleet.config.agent.retry_backoff_ms);
            let mut last_error = String::new();
            for _ in 0..COMMAND_ATTEMPTS {
                self.request_key = Some(key.clone());
                let result = self.act(act.clone()).await;
                self.request_key = None;
                match result {
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
                self.count(Counter::CrewsStalled);
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
            let mut backoff = Duration::from_millis(self.fleet.config.agent.retry_backoff_ms);
            let mut last_error = String::new();
            for attempt in 1..=COMMAND_ATTEMPTS {
                let opened = match self.take_recorded().await {
                    Some(Outcome::Opened { url }) => Ok(url),
                    Some(Outcome::Refused { error }) => Err(error),
                    Some(Outcome::Done { .. }) => Err("a recorded command outcome stands where the change request's was".into()),
                    None => {
                        self.replayed = false;
                        let opened = shared.open(repository, branch, title, &body).await;
                        self.acted(match &opened {
                            Ok(url) => Outcome::Opened { url: url.clone() },
                            Err(error) => Outcome::Refused { error: error.clone() },
                        });
                        opened
                    }
                };
                match opened {
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
                self.count(Counter::CrewsStalled);
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

    /// Publish the script's final evidence; the daemon settles the promises.
    async fn settle_promises(&mut self) -> Option<Ending> {
        // The dummy artifact stands in for a crew publishing its finished ledger.
        // Use the same resource command boundary as artifact put, so the real
        // promise observer (rather than a completion command) settles the role.
        let name = flotilla_resources::address::artifact_slot_record_name(
            &self.identity.convoy,
            &self.identity.role,
            "decision-ledger",
            &self.identity.convoy,
        )
        .ok()?;
        // Open obligations can move to a peer during handoffs. Submit this
        // script's own final ledger explicitly, even if its template moved,
        // under a label naming that deliverable: labels are unique per convoy.
        if !self.script_finished {
            self.persist(CommandAction::CrewPromise {
                context: self.identity.context(),
                operation: CrewPromiseOperation::Submit {
                    promise: Some(format!("script-ledger-{}", self.identity.role)),
                    kind: "decision-ledger".into(),
                    reference: name.clone(),
                    metadata: BTreeMap::new(),
                },
            })
            .await?;
            // Count finished scripts before publishing evidence triggers teardown.
            self.count(Counter::CrewsCompleted);
            self.script_finished = true;
        }
        // Publish the ledger as `flotilla artifact put` does; the daemon validates and stores it.
        let role = &self.identity.role;
        let ledger = format!(
            "## Decision ledger\n\n1. **Brief silence:** the dummy script for {role} leaves nothing open.\n   \
             - **Choice:** {role} ran its script as written.\n   - **Alternative:** stall and ask its supervisor.\n   \
             - **If asking were free:** the same; the script decides.\n"
        );
        self.persist(Act::PublishLedger(ledger)).await?;
        Some(Ending::Completed)
    }

    async fn attempt(&mut self, action: CommandAction) -> Option<CommandValue> {
        match self.act(Act::Command(action)).await {
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

    /// Wait before retrying, still within the turn, as an agent retrying a command does.
    async fn idle_for(&self, duration: Duration) {
        if !self.resuming() {
            tokio::time::sleep(duration).await;
        }
    }

    /// Show working or idle on the screen. A change is what Claude Code's hooks report: a
    /// prompt submitted, or the turn stopped. While resuming, the screen follows the script and
    /// only where it ends up is reported.
    async fn mark(&self, working: bool) {
        let was = self.session.working();
        self.session.set_working(working);
        if was != working && !self.resuming() {
            self.report(working).await;
        }
    }

    async fn report(&self, working: bool) {
        self.hook(if working { "user-prompt-submit" } else { "stop" }).await;
    }

    /// Work on the turn: Claude Code's tool calls, a pre- and post-tool-use hook each, spread
    /// over the turn's length. The turn's hook count, with its prompt and stop, is drawn from
    /// the configured spread.
    async fn think(&mut self) {
        if self.resuming() {
            return;
        }
        self.turns_taken += 1;
        let tool_hooks = self.hooks_this_turn().saturating_sub(2);
        let turns = self.session.record().turns.len();
        let turn = turn_length(&self.fleet.config, &self.session.name, turns);
        let pause = turn / (tool_hooks + 1);
        for hook in 0..tool_hooks {
            tokio::time::sleep(pause).await;
            self.hook(if hook % 2 == 0 { "pre-tool-use" } else { "post-tool-use" }).await;
        }
        tokio::time::sleep(pause).await;
    }

    /// This turn's hook count: log-normal with the configured mean and median, at least two
    /// (prompt and stop) and at most [`MAX_HOOKS_PER_TURN`], drawn deterministically per turn.
    fn hooks_this_turn(&self) -> u32 {
        let agent = &self.fleet.config.agent;
        let (mean, median) = (agent.hooks_per_turn, agent.hooks_per_turn_median);
        if mean <= 0.0 || median <= 0.0 || mean < median {
            return 2;
        }
        let subject = format!("{}#{}", self.identity.subject(), self.turns_taken);
        // Box-Muller over two uniform draws; the first is kept off zero.
        let first = dummy_draw(self.fleet.config.seed, "hooks-per-turn-a", &subject).max(f64::MIN_POSITIVE);
        let second = dummy_draw(self.fleet.config.seed, "hooks-per-turn-b", &subject);
        let normal = (-2.0 * first.ln()).sqrt() * (std::f64::consts::TAU * second).cos();
        // A log-normal's mean is exp(mu + sigma^2 / 2) and its median exp(mu).
        let sigma = (2.0 * (mean / median).ln()).sqrt();
        let count = (median.ln() + sigma * normal).exp().round();
        count.clamp(2.0, f64::from(MAX_HOOKS_PER_TURN)) as u32
    }

    /// Deliver Claude Code's hook `event` for the crew's session. A refused or lost hook is
    /// logged and counted, as Claude Code goes on after a failed hook.
    async fn hook(&self, event: &str) {
        if let Err(error) = self.fleet.hook(&self.link(), event).await {
            tracing::debug!(crew = %self.identity.subject(), event, %error, "dummy crew hook failed");
        }
    }
}

/// The most hook events one turn sends, bounding the spread's tail.
const MAX_HOOKS_PER_TURN: u32 = 1_000;

/// What a crew sends to its daemon: a crew command, or a decision ledger published as
/// `flotilla artifact put` publishes one.
#[derive(Clone)]
enum Act {
    Command(CommandAction),
    PublishLedger(String),
}

impl From<CommandAction> for Act {
    fn from(action: CommandAction) -> Self {
        Self::Command(action)
    }
}

/// A turn's time: `turn_delay_ms`, plus a draw over `turn_delay_spread_ms` by the session and how
/// many turns it has had, so turns differ in length yet a replay spends the same.
fn turn_length(config: &DummyFleetConfig, session: &str, turns: usize) -> Duration {
    let agent = &config.agent;
    let spread = dummy_draw(config.seed, "turn-length", &format!("{session}#{turns}")) * agent.turn_delay_spread_ms as f64;
    Duration::from_millis(agent.turn_delay_ms + spread as u64)
}

/// Delivered turns are framed `[from <sender> (<relation>)…]`; a peer's
/// handoff is a task, anything else (supervision, the operator) is guidance.
fn is_peer_turn(turn: &str) -> bool {
    turn.lines().next().is_some_and(|header| header.starts_with("[from ") && header.contains(" (peer)"))
}

/// The (convoy, vessel, role) of each `flotilla crew supervise … resume` command a supervision
/// turn offers, as a supervising agent would read them off the turn.
fn resume_commands(turn: &str) -> Vec<(String, String, String)> {
    turn.lines()
        .filter_map(|line| {
            let words = shell_words(line.trim().trim_matches('`'));
            if !words.starts_with(&["flotilla".into(), "crew".into(), "supervise".into()]) || !words.iter().any(|word| word == "resume") {
                return None;
            }
            let flag = |name: &str| words.iter().position(|word| word == name).and_then(|at| words.get(at + 1)).cloned();
            Some((flag("--convoy")?, flag("--vessel")?, flag("--role")?))
        })
        .collect()
}

/// Split a command line into words, honouring single quotes (`'it'\''s'` is one word).
fn shell_words(line: &str) -> Vec<String> {
    let (mut words, mut word, mut quoted, mut started) = (Vec::new(), String::new(), false, false);
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => (quoted, started) = (!quoted, true),
            '\\' if !quoted => {
                word.extend(chars.next());
                started = true;
            }
            c if c.is_whitespace() && !quoted => {
                if started {
                    words.push(std::mem::take(&mut word));
                }
                started = false;
            }
            c => {
                word.push(c);
                started = true;
            }
        }
    }
    if started {
        words.push(word);
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    // Turns take the configured delay plus a per-turn draw over the spread: lengths vary across
    // turns and sessions, stay within the bounds, and are the same for the same turn. Without a
    // spread every turn takes the delay.
    #[test]
    fn turn_lengths_spread_over_the_configured_range() {
        let mut config = DummyFleetConfig::default();
        config.agent.turn_delay_ms = 1_500;
        let lengths = |config: &DummyFleetConfig| {
            (0..50).flat_map(|turn| ["a", "b"].map(|session| turn_length(config, session, turn).as_millis())).collect::<Vec<_>>()
        };
        assert!(lengths(&config).iter().all(|&length| length == 1_500));
        config.agent.turn_delay_spread_ms = 3_000;
        let spread = lengths(&config);
        assert!(spread.iter().all(|&length| (1_500..4_500).contains(&length)), "{spread:?}");
        assert!(spread.iter().collect::<std::collections::BTreeSet<_>>().len() > 40, "lengths vary: {spread:?}");
        assert_eq!(spread, lengths(&config), "the same turn takes the same time");
    }

    #[test]
    fn a_supervisor_reads_the_resume_command_off_a_stall_turn() {
        let turn = "[from flotilla (supervision)]\nSupervise stalled crew coder@coder in convoy fleet/c#1 (resource ref: c-1). \
            Reason: infra. Evidence: x. Resume it with guidance, convert it to failed, or escalate it.\n\
            `flotilla crew supervise --convoy 'c-1' --vessel 'co'\\''der' --role 'coder' resume --message 'guidance'`\n\
            `flotilla crew supervise --convoy 'c-1' --vessel 'co'\\''der' --role 'coder' convert-to-failed --message 'guidance'`";
        assert_eq!(resume_commands(turn), vec![("c-1".into(), "co'der".into(), "coder".into())]);
        assert!(resume_commands("[from coder (peer)] review this").is_empty());
    }

    #[test]
    fn scripts_round_trip_through_their_brief_line() {
        for script in [
            CrewScript::coder(None, "dummy/fleet".into()),
            CrewScript::coder(Some("reviewer".into()), "acme/app".into()),
            CrewScript::Coder {
                reviewer: Some("reviewer".into()),
                repository: "acme/app".into(),
                kept: [("pr".into(), "https://forge/acme/app/pulls/1".into()), ("decision-ledger".into(), "ledger-1".into())].into(),
            },
            CrewScript::Responder { reply_to: "coder".into() },
            CrewScript::Governor { aide: "aide".into(), turns: 7 },
            CrewScript::Supervisor,
        ] {
            let brief = format!("# Brief\n\nSome prose.\n{SCRIPT_LINE_PREFIX} {}\n", script.render());
            assert_eq!(CrewScript::from_brief(&brief, 3), Ok(script));
        }
    }

    #[test]
    fn a_coder_takes_the_promises_its_brief_lists_as_kept() {
        let brief = format!(
            "{SCRIPT_LINE_PREFIX} coder review=reviewer\n\n{KEPT_PROMISES_HEADING}\n\nProse.\n\n\
             - `template/pr` (pr): https://forge/dummy/fleet/pulls/3\n- `script-ledger-coder` (decision-ledger): ledger-1\n\n## Next\n- `x` (y): z\n"
        );
        let CrewScript::Coder { kept, .. } = CrewScript::from_brief(&brief, 3).expect("a coder") else { panic!("a coder") };
        assert_eq!(
            kept,
            BTreeMap::from([
                ("decision-ledger".to_string(), "ledger-1".to_string()),
                ("pr".to_string(), "https://forge/dummy/fleet/pulls/3".to_string()),
            ])
        );
    }

    #[test]
    fn a_brief_without_a_script_is_a_lone_coder_and_defaults_apply() {
        assert_eq!(CrewScript::from_brief("Implement the thing.", 3), Ok(CrewScript::coder(None, DEFAULT_REPOSITORY.into())));
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
