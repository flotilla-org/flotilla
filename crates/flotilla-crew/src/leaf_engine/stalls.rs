use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use flotilla_change_request::observation::ChangeRequestKey;
use flotilla_protocol::{arg::shell_quote, AgentActivity, Leaf, LeafAddress, LeafOperator};
use flotilla_provider_runtime::ObservationError;
use flotilla_resources::agent_activity::AgentActivityView;
use flotilla_resources::attentions::{Attention, Fact, Sender, Subject};
use flotilla_resources::{
    actor_obligation,
    address::{leaf_address, record_name},
    select_convoy_children, Convoy, ConvoyAttention, ConvoyPhase, ConvoyStatus, CrewWorkPhase, LeafMaker, NudgeObligation, ResourceObject,
    StallEvidenceSource, StallNudge, StallRung, StallSupervisor, StalledCondition, Stalls, SupervisionTarget, TerminalAttention,
    TerminalAttentionSource, TerminalAttentionState, TerminalSession, TerminalSessionPhase, TerminalSessionSource, TurnDeliveryRule,
    WorkPhase, CONVOY_LABEL, ROLE_LABEL, VESSEL_LABEL,
};
use flotilla_store::{load_metrics::carry, off_frame};

use super::schedule::{Pass, SessionKey, StallSchedule, Waits, RETRY_AFTER};
use super::sources::freshest_change_request_refs;
use super::wake::{ConvoyRead, ReconcilerWake};
use super::{CrewTurnIntent, LeafSubscriptionRow, LeafWatcher, Party, UnableEvidenceKey};
use crate::crew_ops::convoy_message_address;

// Observe lifecycle movement rather than refreshes or failed retry attempts.
const LAUNCH_PROGRESS_BOUND: chrono::Duration = chrono::Duration::minutes(15);
const LAUNCH_PROGRESS_SOURCE: &str = "launch-progress";

#[derive(Default)]
pub(super) struct PendingLaunches {
    convoys: HashMap<(String, String), BTreeMap<String, LaunchProgress>>,
}

struct LaunchProgress {
    signature: String,
    since: DateTime<Utc>,
}

/// The seat roles this home told a person of at the unnamed operator rung, by namespace and
/// launch. After a start it is rebuilt once per namespace from the open stall-judge attentions,
/// which are the durable record of those stalls (#3108).
#[derive(Default)]
pub(super) struct OperatorTold {
    reseeded: HashSet<String>,
    seats: HashMap<(String, String), BTreeSet<String>>,
}

/// Whose supervision lookup a context is cached for: a namespace, a launch, and the seat its
/// stall is about.
pub(super) type SupervisorContextKey = (String, String, Option<(String, String)>);

/// A stall's seat, owned.
fn owned_seat(seat: Option<(&str, &str)>) -> Option<(String, String)> {
    seat.map(|(work, role)| (work.to_owned(), role.to_owned()))
}

/// The seat role `stall` holds at the unnamed operator rung, if it does.
fn at_operator(stall: &StalledCondition) -> Option<&str> {
    (stall.rung == StallRung::Operator && stall.supervisor.is_none()).then(|| stalled_source_actor(stall).map(|(_, role)| role))?
}

struct LaunchAttention<'a> {
    attention: Option<ConvoyAttention>,
    expected_crew: &'a BTreeMap<String, BTreeMap<String, flotilla_resources::CrewWorkState>>,
}

impl flotilla_resources::StatusPatch<ConvoyStatus> for LaunchAttention<'_> {
    fn apply(&self, status: &mut ConvoyStatus) {
        if status.attention.as_ref().is_some_and(|attention| attention.source != LAUNCH_PROGRESS_SOURCE) {
            return;
        }
        // Recovery may race the diagnostic write; do not re-arm a recovered crew.
        let pending = status.phase == ConvoyPhase::Active
            && status.crew_work.values().flat_map(|roles| roles.values()).any(|work| work.phase == CrewWorkPhase::Pending);
        if self.attention.is_some() && status.crew_work != *self.expected_crew {
            return;
        }
        let next = self.attention.clone().filter(|_| pending);
        if status.attention.as_ref().map(|attention| &attention.reason) != next.as_ref().map(|attention| &attention.reason) {
            status.attention = next;
        }
    }

    fn requires_write(&self, before: &ConvoyStatus, after: &ConvoyStatus) -> bool {
        before.attention != after.attention
    }
}

impl ReconcilerWake {
    async fn observe_launch_progress(
        &self,
        tick: StallTick<'_>,
        convoy: &ResourceObject<Convoy>,
        vessels: &BTreeMap<String, ResourceObject<flotilla_resources::Vessel>>,
        sessions: &BTreeMap<String, ResourceObject<TerminalSession>>,
    ) -> Result<(), String> {
        let StallTick { now, .. } = tick;
        let Some(status) = &convoy.status else { return Ok(()) };
        let mut pending = self.subscriptions.inner.pending_launches.lock().await;
        let stages = pending.convoys.entry((convoy.metadata.namespace.clone(), convoy.metadata.name.clone())).or_default();
        let mut active = HashSet::new();
        let mut diagnostics = Vec::new();
        if status.phase == ConvoyPhase::Active {
            for (vessel, roles) in &status.crew_work {
                if !status.work.get(vessel).is_some_and(|work| matches!(work.phase, WorkPhase::Launching | WorkPhase::Running)) {
                    continue;
                }
                for (role, work) in roles.iter().filter(|(_, work)| work.phase == CrewWorkPhase::Pending) {
                    let key = format!("{role}@{vessel}");
                    let vessel_record = vessels.values().find(|item| item.spec.vessel_name == *vessel);
                    let vessel_status = vessel_record.and_then(|item| item.status.as_ref());
                    let session =
                        sessions.values().find(|item| item.spec.role == *role && item.metadata.labels.get(VESSEL_LABEL) == Some(vessel));
                    // Latent agents await handoff, not launch. Only eager or explicitly
                    // assigned roles (or an existing launch) have launch work to diagnose.
                    let eager = status
                        .workflow_snapshot
                        .as_ref()
                        .and_then(|snapshot| snapshot.vessels.iter().find(|item| item.name == *vessel))
                        .is_some_and(|requirement| {
                            requirement
                                .crew
                                .iter()
                                .enumerate()
                                .any(|(index, member)| member.role == *role && requirement.starts_eagerly(index))
                        });
                    if session.is_none() && !eager && !work.task_assigned {
                        continue;
                    }
                    active.insert(key.clone());
                    let session_status = session.and_then(|item| item.status.as_ref());
                    let signature = format!(
                        "{:?}/{:?}/{:?}/{:?}/{:?}",
                        vessel_status.map(|status| (status.phase, &status.environment_ref, &status.checkout_refs)),
                        vessel_status.and_then(|status| status.ready_at),
                        session.map(|item| item.metadata.creation_timestamp),
                        session_status.map(|status| status.phase),
                        session_status.and_then(|status| status.started_at)
                    );
                    let began = work.started_at.or(status.started_at).unwrap_or(convoy.metadata.creation_timestamp);
                    let anchor = [
                        vessel_status.and_then(|status| status.started_at),
                        vessel_status.and_then(|status| status.ready_at),
                        session.map(|item| item.metadata.creation_timestamp),
                        session_status.and_then(|status| status.started_at),
                    ]
                    .into_iter()
                    .flatten()
                    .fold(began, std::cmp::max);
                    let stage = stages.entry(key.clone()).or_insert_with(|| LaunchProgress { signature: signature.clone(), since: anchor });
                    if stage.signature != signature {
                        *stage = LaunchProgress { signature, since: now.max(anchor) };
                    }
                    tick.due(stage.since + LAUNCH_PROGRESS_BOUND);
                    if now.signed_duration_since(stage.since) >= LAUNCH_PROGRESS_BOUND {
                        diagnostics.push(format!(
                            "launch progress stalled for {key}: Pending since {began}; no lifecycle progress since {}; vessel {:?}, session {:?}; retry evidence {:?}; inspect the launcher and retry or resume the crew after repair",
                            stage.since, vessel_status.map(|status| status.phase), session_status.map(|status| status.phase),
                            session_status.and_then(|status| status.message.as_deref())
                        ));
                    }
                }
            }
        }
        stages.retain(|key, _| active.contains(key));
        drop(pending);
        let attention = (!diagnostics.is_empty()).then(|| ConvoyAttention {
            source: LAUNCH_PROGRESS_SOURCE.into(),
            reason: diagnostics.join("\n"),
            raised_at: now,
        });
        if status.attention.as_ref().is_some_and(|attention| attention.source != LAUNCH_PROGRESS_SOURCE)
            || status.attention.as_ref().map(|attention| &attention.reason) == attention.as_ref().map(|attention| &attention.reason)
        {
            return Ok(());
        }
        flotilla_store::note_convoy(
            &self.subscriptions.inner.convoy_engine,
            &self.subscriptions.inner.backend.using::<Convoy>(&convoy.metadata.namespace),
            &convoy.metadata.name,
            &LaunchAttention { attention, expected_crew: &status.crew_work },
        )
        .await?;
        Ok(())
    }
}

pub(crate) use flotilla_resources::crew_role_address;

fn convoy_stall_subject(project: &str, convoy: &str) -> String {
    let project = flotilla_address::Address::project(project).expect("a stored convoy has a project");
    match flotilla_resources::convoy::engine::split_launch(convoy) {
        Some(_) => flotilla_resources::convoy::engine::launch_address(&project, convoy).expect("a launch has an address"),
        None => project.child(flotilla_address::Kind::Convoy, convoy).expect("a stored convoy has a name"),
    }
    .to_string()
}

/// A convoy stall has a subject even when no crew caused it.
pub(super) fn stall_subject(convoy: &ResourceObject<Convoy>, condition: &StalledCondition) -> String {
    let project = convoy.spec.project_ref.as_str();
    stalled_source_actor(condition)
        .map(|(vessel, role)| crew_role_address(project, &convoy.metadata.name, vessel, role))
        .unwrap_or_else(|| convoy_stall_subject(project, &convoy.metadata.name))
}

/// What a supervision send records of the stall it escalates: its subject, and the evidence its
/// stall record carries to every home.
pub(super) fn stall_of(convoy: &ResourceObject<Convoy>, condition: &StalledCondition) -> flotilla_messaging::StallOf {
    flotilla_messaging::StallOf {
        about: stall_subject(convoy, condition),
        report: convoy.status.as_ref().and_then(|status| condition.report(status)).map(str::to_owned),
        reason: condition.reason,
        proposed_disposition: condition.proposed_disposition,
    }
}

pub(super) fn supervision_expectation(rung: StallRung) -> flotilla_messaging::IntentExpectation {
    // An operator-rung escalation expects nothing: on the engine path the
    // person's acknowledgement settles it (#3938), and their ruling acts on the stall.
    if rung == StallRung::Operator {
        flotilla_messaging::IntentExpectation::Nothing
    } else {
        flotilla_messaging::IntentExpectation::Reply
    }
}

/// A message's reference to `convoy` at the revision it was read at.
pub(super) fn convoy_reference(convoy: &ResourceObject<Convoy>) -> Result<flotilla_messaging::IntentReference, String> {
    let address = flotilla_resources::convoy::engine::convoy_address(convoy)?;
    let revision = flotilla_address::Revision::new(convoy.metadata.resource_version.clone())
        .map_err(|error| format!("convoy `{}`: {error}", convoy.metadata.name))?;
    Ok(flotilla_messaging::IntentReference::new(address, revision))
}

/// The source of the escalation of a stall about `seat` in `convoy` at rung `index`: one per seat,
/// so two seats escalated to one supervisor are two messages.
pub(crate) fn supervision_message_source(convoy: &str, seat: Option<(&str, &str)>, index: usize) -> String {
    match seat {
        Some((work, role)) => format!("supervision-{convoy}-{role}@{work}-{index}"),
        None => format!("supervision-{convoy}-{index}"),
    }
}
// Bound silent turns, not their total runtime: long tool-using turns stay able.
pub(super) const TURN_INACTIVITY_BOUND: chrono::Duration = chrono::Duration::minutes(15);
// Bound missing loop progress, including a single long reconciliation. A
// legitimate reconcile exceeding this bound is deliberately surfaced for
// supervision; queue residence while other reconciliations progress is not.
const CONVOY_CONTROLLER_PROGRESS_BOUND: std::time::Duration = std::time::Duration::from_secs(60);

/// When a working turn's silence exceeds the turn inactivity bound unless it shows activity first:
/// its last tool or output activity, or its obligation's latest reply, plus the bound.
pub(super) fn turn_inactivity_deadline(agent: &AgentActivityView, obligation: &NudgeObligation) -> Option<DateTime<Utc>> {
    let since = agent.working_since()?;
    // Another home's progress is not seen here, so its turns are not bounded here.
    let progress = agent.progress?;
    let last_activity = [Some(progress.latest()), obligation.reply_after].into_iter().flatten().fold(since, std::cmp::max);
    Some(last_activity + TURN_INACTIVITY_BOUND)
}

pub(super) fn turn_inactivity_reason(agent: &AgentActivityView, obligation: &NudgeObligation, now: DateTime<Utc>) -> Option<String> {
    let since = agent.working_since()?;
    let progress = agent.progress?;
    let deadline = turn_inactivity_deadline(agent, obligation)?;
    let quiet = now.signed_duration_since(deadline - TURN_INACTIVITY_BOUND);
    (now >= deadline).then(|| format!(
        "turn inactivity bound exceeded: turn began {since}, last tool {:?}, last output {:?}, last prompt {:?}, watched from {}; no tool or output activity for {} seconds; automatic interrupt deferred to supervisor",
        progress.tool_at, progress.output_at, progress.prompt_submit_at, progress.watched_from, quiet.num_seconds()
    ))
}

// A long first turn may be legitimate; missing hooks are advisory, never a stall.
pub(super) const FIRST_HOOK_GRACE: chrono::Duration = chrono::Duration::minutes(15);

/// A running crew session that has delivered no hook since it started: its harness, its vessel
/// and its start.
fn unhooked_session<'a>(
    session: &'a ResourceObject<TerminalSession>,
    agent: Option<&AgentActivityView>,
) -> Option<(&'a str, &'a str, DateTime<Utc>)> {
    if !matches!(session.spec.source, TerminalSessionSource::Agent { .. }) {
        return None;
    }
    let status = session.status.as_ref()?;
    let crew = status.crew.as_ref()?;
    if status.phase != TerminalSessionPhase::Running || flotilla_agent::parser_for_harness(&crew.adapter).is_err() {
        return None;
    }
    let started = status.started_at?;
    let vessel = session.metadata.labels.get(VESSEL_LABEL)?;
    (!agent.is_some_and(|agent| agent.hook_seen_since(started))).then_some((crew.adapter.as_str(), vessel.as_str(), started))
}

/// The AgentSession of crew session `session`: the launch its view shows, at the home its record
/// came from.
pub(super) fn session_key(session: &ResourceObject<TerminalSession>) -> Option<SessionKey> {
    let name = flotilla_resources::terminal_records::launch_of(session)?;
    Some((name.to_owned(), session.metadata.annotations.get(flotilla_resources::ORIGIN_ROOT_ANNOTATION).cloned()))
}

/// When a crew session still without a hook is reported as missing it.
pub(super) fn missing_turn_hook_deadline(
    session: &ResourceObject<TerminalSession>,
    agent: Option<&AgentActivityView>,
) -> Option<DateTime<Utc>> {
    unhooked_session(session, agent).map(|(_, _, started)| started + FIRST_HOOK_GRACE)
}

pub(super) fn missing_turn_hook(
    session: &ResourceObject<TerminalSession>,
    agent: Option<&AgentActivityView>,
    now: DateTime<Utc>,
) -> Option<String> {
    let (adapter, vessel, started) = unhooked_session(session, agent)?;
    (now.signed_duration_since(started) >= FIRST_HOOK_GRACE).then(|| {
        format!("{adapter} crew {}@{vessel} has delivered no hook since session {} started at {started}; check turn hook wiring (Codex notify / Claude managed settings)",
            session.spec.role, session.metadata.name)
    })
}

pub(super) fn actor_is_working(
    agent: &AgentActivityView,
    obligations: &[NudgeObligation],
    vessel: &str,
    role: &str,
    now: DateTime<Utc>,
) -> bool {
    let silent = obligations.iter().any(|obligation| {
        matches!(&obligation.maker, LeafMaker::Actor { vessel: actor_vessel, role: actor_role } if actor_vessel == vessel && actor_role == role)
            && turn_inactivity_reason(agent, obligation, now).is_some()
    });
    agent.working_since().is_some() && !silent
}

/// Where an agent's recorded activity came from, as stall evidence names it.
fn activity_source(agent: &AgentActivityView) -> TerminalAttentionSource {
    if agent.from_screen() {
        TerminalAttentionSource::Screen
    } else {
        TerminalAttentionSource::Hook
    }
}

/// The attention state an activity is debounced under.
fn activity_state(activity: AgentActivity) -> TerminalAttentionState {
    match activity {
        AgentActivity::Working => TerminalAttentionState::Working,
        AgentActivity::NeedsInput => TerminalAttentionState::NeedsInput,
        AgentActivity::Idle | AgentActivity::Interrupted => TerminalAttentionState::Idle,
    }
}

/// Evidence for a Landing convoy whose workflow declares no exit (#4278).
pub const NO_EXIT_STALL_EVIDENCE: &str =
    "no exit: the workflow declares no exit, so nothing can settle this Landing convoy; an operator reaps it";

/// Evidence for a holding convoy with no actionable row. A Landing convoy whose
/// workflow omits `exit` is a Standing Convoy: nothing can settle it, so name
/// that dead end rather than an absent maker (#4278). A Done claim's observed
/// digest still anchors settlement without an exit; its own evidence is attention.
fn empty_row_evidence(status: &ConvoyStatus) -> String {
    let standing = status.workflow_snapshot.as_ref().and_then(|snapshot| snapshot.exit.as_ref()).is_none();
    let digest_anchored = !flotilla_resources::observed_digest_claims(status).is_empty();
    if status.phase == ConvoyPhase::Landing && standing && !digest_anchored {
        NO_EXIT_STALL_EVIDENCE.into()
    } else {
        "no armed row with an able maker".into()
    }
}

pub(super) fn stalled_source_actor(condition: &StalledCondition) -> Option<(&str, &str)> {
    condition.seat()
}

/// Whether the seat a stall is about still reads Stalled: a seat that handed its work back, failed
/// or settled has resolved its own stall, whatever other rows remain.
fn declared_seat_stalled(status: &ConvoyStatus, stall: &StalledCondition) -> bool {
    stall.seat().and_then(|(vessel, role)| status.crew_work.get(vessel)?.get(role)).is_some_and(|work| work.phase == CrewWorkPhase::Stalled)
}

/// The stall crew `role@vessel` declared, while its note is open.
pub(super) fn declared_stall<'a>(status: &'a ConvoyStatus, vessel: &str, role: &str) -> Option<&'a StalledCondition> {
    status.stalls.about(Some((vessel, role))).filter(|stall| stall.source == StallEvidenceSource::Crew)
}

/// Shared by supervisor delivery and the operator backstop, so both identify
/// the same crew and provide commands targeting the exact convoy record and crew.
pub(super) fn stall_supervision_brief(convoy: &ResourceObject<Convoy>, condition: &StalledCondition) -> String {
    let address = convoy_message_address(convoy);
    let actor =
        stalled_source_actor(condition).map(|(vessel, role)| format!("{role}@{vessel}")).unwrap_or_else(|| "unidentified crew".to_string());
    let reason = condition.reason.map_or_else(|| "inferred stall".to_string(), |reason| reason.to_string());
    let proposal = condition.proposed_disposition.map_or(String::new(), |disposition| format!(" Proposed disposition: {disposition}."));
    let mut brief = format!(
        "Supervise stalled crew {actor} in convoy {address} (resource ref: {}). Reason: {reason}. Evidence: {}.{proposal}",
        convoy.metadata.name, condition.evidence,
    );
    if let Some((vessel, role)) = stalled_source_actor(condition) {
        brief.push_str(" Resume it with guidance, convert it to failed, or escalate it.");
        for action in ["resume", "convert-to-failed", "escalate"] {
            write!(
                brief,
                "\n`flotilla crew supervise --convoy {} --vessel {} --role {} {action} --message 'guidance'`",
                shell_quote(&convoy.metadata.name),
                shell_quote(vessel),
                shell_quote(role),
            )
            .expect("writing to a String cannot fail");
        }
    }
    brief
}

pub(super) const DEFAULT_IDLE_GRACE_SECONDS: u32 = 180;

/// How long after the last of `delivered` nudges the next is due: the initial grace is 1x, and the
/// intervals after delivered nudges are 1x, 2x, 4x, ...
fn nudge_backoff(grace_seconds: u32, delivered: usize) -> chrono::Duration {
    chrono::Duration::seconds(i64::from(grace_seconds.max(1)).saturating_mul(1_i64 << delivered.saturating_sub(1).min(20)))
}

/// How long a resumed crew's turn holds off supervision while its brief is unconfirmed.
const RESUMED_TURN_GRACE: chrono::Duration = chrono::Duration::minutes(2);

pub(super) fn nudge_policy<'a>(status: &'a ConvoyStatus, vessel: &str, role: &str) -> Option<&'a flotilla_resources::StallNudgePolicy> {
    status.workflow_snapshot.as_ref()?.stall_nudges.get(&format!("{vessel}/{role}"))
}

pub(super) fn idle_grace_seconds(status: &ConvoyStatus, vessel: &str, role: &str) -> u32 {
    nudge_policy(status, vessel, role).and_then(|policy| policy.idle_grace_seconds).unwrap_or(DEFAULT_IDLE_GRACE_SECONDS)
}

pub(super) fn is_conflict_probe(leaf: &Leaf) -> bool {
    leaf.field_path == ".mergeable" && leaf.operator == LeafOperator::Equal && leaf.literal == "conflicting"
}

pub(super) fn is_merged_settlement_probe(leaf: &Leaf) -> bool {
    matches!(leaf.address, LeafAddress::ChangeRequest { .. })
        && leaf.field_path == ".state"
        && leaf.operator == LeafOperator::Equal
        && leaf.literal == "merged"
}

/// The launch has not ended: its crews work, including while a vessel is interrupted.
pub(super) fn launch_underway(phase: ConvoyPhase) -> bool {
    matches!(phase, ConvoyPhase::Active | ConvoyPhase::Interrupted)
}

/// Whether `role`'s own session in work `vessel` stopped. Every seat of an interrupted vessel reads
/// Interrupted; the Vessel names the roles whose sessions stopped.
fn seat_interrupted(vessels: &BTreeMap<String, ResourceObject<flotilla_resources::Vessel>>, vessel: &str, role: &str) -> bool {
    vessels.values().any(|object| {
        object.spec.vessel_name == vessel && object.status.as_ref().is_some_and(|status| status.interrupted_roles.contains(role))
    })
}

/// Workflow-declared checks, review and merged settlement rules run during active crew work too,
/// including custom rules. Merged settlement also resumes stalled crews so they
/// can resolve remaining promises and claim completion; conflict probes retain
/// their active delivery behavior.
pub(super) fn is_active_change_request_probe(
    status: &ConvoyStatus,
    rule: &TurnDeliveryRule,
    leaf: &Leaf,
    forges: &[flotilla_resources::ForgeSpec],
) -> bool {
    // Promise rejection rows are armed for their owner while work remains
    // active. The submission and the leaf may spell one PR differently.
    let rejected = matches!(leaf.address, LeafAddress::ChangeRequest { .. }).then(|| leaf_address(&leaf.address, forges).ok()).flatten();
    let promise_rejection = leaf.field_path == ".state"
        && leaf.operator == LeafOperator::Equal
        && leaf.literal == "closed"
        && rejected.as_ref().is_some_and(|rejected| {
            flotilla_resources::promises::owned(status, &rule.to.vessel, &rule.to.role).iter().any(|promise| {
                !promise.state.terminal() && promise.submissions.last().is_some_and(|submission| submission.names(rejected, forges))
            })
        });
    let active_field = promise_rejection
        || is_conflict_probe(leaf)
        || is_merged_settlement_probe(leaf)
        || (matches!(leaf.address, LeafAddress::ChangeRequest { .. })
            && matches!(leaf.field_path.as_str(), ".checks" | ".review.actionable-at-head"));
    launch_underway(status.phase)
        && active_field
        && status.crew_work.get(&rule.to.vessel).and_then(|crew| crew.get(&rule.to.role)).is_some_and(|work| {
            work.finished_at.is_none()
                && (is_conflict_probe(leaf)
                    || ((promise_rejection || is_merged_settlement_probe(leaf)) && work.phase == CrewWorkPhase::Stalled)
                    || matches!(work.phase, CrewWorkPhase::Working | CrewWorkPhase::Interrupted))
        })
}

/// One judging tick's request snapshot of the watched kinds.
struct StallInputs {
    projects: Arc<flotilla_resources::ReadResourceList<flotilla_resources::Project>>,
    available_convoys: Arc<flotilla_resources::ReadResourceList<Convoy>>,
    sessions_list: Arc<flotilla_resources::ReadResourceList<TerminalSession>>,
    observation_list: Arc<flotilla_resources::ReadResourceList<flotilla_resources::ChangeRequest>>,
    forges: Vec<flotilla_resources::ForgeSpec>,
    vessels_list: Arc<flotilla_resources::ReadResourceList<flotilla_resources::Vessel>>,
    messages: Vec<flotilla_messaging::SeatMessage>,
    receivers: flotilla_messaging::MessageReceiverSnapshot,
    declarations: flotilla_store::CascadeDeclarations,
}

impl StallInputs {
    fn routes(&self) -> super::supervision::SupervisionRoutes<'_> {
        super::supervision::SupervisionRoutes { declarations: &self.declarations, receivers: &self.receivers }
    }
}

/// What every step of one judging pass reads, and where a convoy's judgement records what it
/// waits on.
#[derive(Clone, Copy)]
struct StallTick<'a> {
    namespace: &'a str,
    now: DateTime<Utc>,
    inputs: &'a StallInputs,
    progress_observations: &'a HashMap<&'a str, &'a ResourceObject<flotilla_resources::ChangeRequest>>,
    rows: &'a [LeafSubscriptionRow],
    controller_queue: Option<&'a flotilla_store::controller::WorkQueueSender>,
    waits: &'a std::sync::Mutex<Waits>,
    /// The sessions' readings held between passes, and those read in this one.
    readings: &'a std::sync::Mutex<HashMap<SessionKey, Option<AgentActivityView>>>,
    /// The convoy is judged for the first time since the judge began.
    first: bool,
}

impl StallTick<'_> {
    /// Judges again at `at`, an instant the judgement turns on, once it is later than now.
    fn due(&self, at: DateTime<Utc>) {
        if at > self.now {
            self.waits.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).due(at);
        }
    }

    /// Judges again at `at`, an instant `session`'s judgement turns on, reading it again then;
    /// the convoy's, for a seat with no session.
    fn seat_due(&self, session: Option<&ResourceObject<TerminalSession>>, at: DateTime<Utc>) {
        match session.and_then(session_key) {
            Some(key) => self.session_due(&key, at),
            None => self.due(at),
        }
    }

    fn session_due(&self, session: &SessionKey, at: DateTime<Utc>) {
        if at > self.now {
            self.waits.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).session_due(session, at);
        }
    }

    /// Judges again after a failed write or withdrawal.
    fn retry(&self) {
        self.due(self.now + RETRY_AFTER);
    }

    /// Judges again when home `origin`, silent now, is heard from.
    fn unheard(&self, origin: &str) {
        self.waits.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).unheard.insert(origin.to_owned());
    }
}

/// A convoy's crew sessions and vessels, by name.
type ConvoyChildren = (BTreeMap<String, ResourceObject<TerminalSession>>, BTreeMap<String, ResourceObject<flotilla_resources::Vessel>>);

/// A judged convoy's crew: its sessions and vessels, nudge obligations, and whether its phase
/// holds crew work.
struct ConvoyCrew {
    sessions: BTreeMap<String, ResourceObject<TerminalSession>>,
    vessels: BTreeMap<String, ResourceObject<flotilla_resources::Vessel>>,
    obligations: Vec<NudgeObligation>,
    holding: bool,
}

/// One subscription row's judgement.
enum RowJudgement {
    Able,
    Unable(String, StallEvidenceSource),
    /// The evidence cannot judge the row yet.
    Unknown,
}

impl From<Result<(), (String, StallEvidenceSource)>> for RowJudgement {
    fn from(judgement: Result<(), (String, StallEvidenceSource)>) -> Self {
        match judgement {
            Ok(()) => RowJudgement::Able,
            Err((evidence, source)) => RowJudgement::Unable(evidence, source),
        }
    }
}

/// A convoy's rows, judged together.
struct RowsVerdict<'a> {
    progress_changed: bool,
    /// Each unable row, with its evidence; only the controller's once a controller is unable.
    unable: Vec<(&'a LeafSubscriptionRow, String, StallEvidenceSource)>,
    able: bool,
    unknown: bool,
    actionable_rows: usize,
}

/// What a stall is judged from: its leaves, the maker that cannot move them, and the evidence;
/// the unable row it came from, when one carried it.
struct StallCause<'a> {
    leaves: Vec<Leaf>,
    maker: LeafMaker,
    evidence: String,
    source: StallEvidenceSource,
    row: Option<&'a LeafSubscriptionRow>,
}

/// What each stall is judged from this tick, by the seat it is about; `None` for a convoy no row
/// moves. A crew's unable row declaring its stall is that stall's cause whatever other rows say
/// (#3893). A declaration with no row to carry it (no exit, nothing actionable) is its cause once
/// no maker is able or unjudged (`unmoved`): "I'm stuck" goes up the ladder either way. Every
/// other unable row is a cause only while the convoy is `stuck`. Of two rows about one seat, the
/// crew's declaration outranks other evidence, else the first stands.
#[allow(clippy::type_complexity)]
fn stall_causes<'a>(
    status: &ConvoyStatus,
    unable: Vec<(&'a LeafSubscriptionRow, String, StallEvidenceSource)>,
    holding: bool,
    unmoved: bool,
    stuck: bool,
) -> BTreeMap<Option<(String, String)>, Option<StallCause<'a>>> {
    let mut causes = BTreeMap::new();
    for (row, evidence, source) in unable {
        let crew = source == StallEvidenceSource::Crew;
        if !(stuck || (holding && crew)) {
            continue;
        }
        let seat = owned_seat(flotilla_resources::leaves_seat(&row.leaves));
        let outranked = |held: &Option<StallCause<'_>>| held.as_ref().is_some_and(|held| crew && held.source != StallEvidenceSource::Crew);
        if causes.get(&seat).is_none_or(outranked) {
            let cause = StallCause { leaves: row.leaves.clone(), maker: row.maker.clone(), evidence, source, row: Some(row) };
            causes.insert(seat, Some(cause));
        }
    }
    for declared in
        status.stalls.iter().filter(|stalled| stalled.source == StallEvidenceSource::Crew && declared_seat_stalled(status, stalled))
    {
        let Some((vessel, role)) = declared.seat().filter(|_| unmoved) else { continue };
        causes.entry(owned_seat(Some((vessel, role)))).or_insert_with(|| {
            Some(StallCause {
                leaves: declared.leaves.clone(),
                maker: LeafMaker::Actor { vessel: vessel.to_owned(), role: role.to_owned() },
                evidence: declared.evidence.clone(),
                source: StallEvidenceSource::Crew,
                row: None,
            })
        });
    }
    if stuck && causes.is_empty() {
        causes.insert(None, None);
    }
    causes
}

impl ReconcilerWake {
    /// A crew session's AgentSession, named by the launch its view shows, at the home its record
    /// came from; none when it cannot be read, which judges nothing.
    fn agent_view(&self, tick: StallTick<'_>, session: &ResourceObject<TerminalSession>) -> Option<AgentActivityView> {
        self.agent_view_at(tick, &session_key(session)?)
    }

    /// The AgentSession `session`, as held since it last changed or came due, else read now.
    /// Another home's reading is judged again when it stops being current, or, once it has, when
    /// that home is heard from.
    fn agent_view_at(&self, tick: StallTick<'_>, session: &SessionKey) -> Option<AgentActivityView> {
        let (name, origin) = (session.0.as_str(), session.1.as_deref());
        let held = tick.readings.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).get(session).cloned();
        let view = match held {
            Some(view) => view,
            None => match self.subscriptions.inner.lifecycle_reads.agent_sessions().and_then(|reads| reads.read(name, origin)) {
                Ok(view) => {
                    tick.readings.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).insert(session.clone(), view.clone());
                    view
                }
                // A failed read is not held: the next judgement reads it again.
                Err(error) => {
                    tracing::debug!(session = name, %error, "stall judgement has no AgentSession");
                    None
                }
            },
        };
        tick.waits.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).read.insert(session.clone());
        if let (Some(view), Some(origin)) = (&view, origin) {
            match view.stale_at {
                Some(stale_at) => tick.session_due(session, stale_at),
                None if !view.current => tick.unheard(origin),
                None => {}
            }
        }
        view
    }

    /// Withdraws records about the convoy or its roles once none of `stalls` names that subject;
    /// a failed withdrawal is judged again after a while.
    async fn end_settled_stalls(&self, tick: StallTick<'_>, convoy: &ResourceObject<Convoy>, status: &ConvoyStatus, stalls: &Stalls) {
        let StallTick { namespace, .. } = tick;
        let primitives = match self.subscriptions.message_primitives() {
            Ok(primitives) => primitives,
            Err(error) => {
                tracing::warn!(namespace, convoy = %convoy.metadata.name, %error, "ending settled stall records failed");
                tick.retry();
                return;
            }
        };
        let project = convoy.spec.project_ref.as_str();
        let address = |vessel: &str, role: &str| crew_role_address(project, &convoy.metadata.name, vessel, role);
        let stalled = stalls
            .iter()
            .filter(|condition| condition.rung != StallRung::Operator)
            .map(|condition| stall_subject(convoy, condition))
            .collect::<Vec<_>>();
        let seats = status
            .crew_work
            .iter()
            .flat_map(|(vessel, roles)| roles.keys().map(|role| address(vessel, role)))
            .chain(std::iter::once(convoy_stall_subject(project, &convoy.metadata.name)))
            .filter(|seat| !stalled.contains(seat))
            .collect::<Vec<_>>();
        match primitives.end_stalls(namespace, &seats, &stalled, "the seat's stall ended").await {
            Ok(ended) if !ended.is_empty() => tracing::debug!(namespace, convoy = %convoy.metadata.name, ?ended, "stall records ended"),
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(namespace, convoy = %convoy.metadata.name, %error, "ending settled stall records failed");
                tick.retry();
            }
        }
    }

    /// Judge locally authored convoys using the caller's current row snapshot.
    /// Obsolete, removed, or replica-only snapshots are skipped; the caller's
    /// convoy-watch handler must refresh the map and rebuild its rows.
    pub(super) async fn judge_stalls(&self, namespace: &str, convoys: &HashMap<String, impl ConvoyRead>) -> Result<(), String> {
        self.judge_stalls_at(namespace, convoys, self.subscriptions.inner.clock.now()).await
    }

    /// Judges every convoy of `convoys` at `now`, reading each one's seats' messages.
    pub(super) async fn judge_stalls_at(
        &self,
        namespace: &str,
        convoys: &HashMap<String, impl ConvoyRead>,
        now: DateTime<Utc>,
    ) -> Result<(), String> {
        self.judge_scheduled(namespace, convoys, now, &mut StallSchedule::default()).await
    }

    /// Judges the convoys of `convoys` that `schedule` holds changed or due at `now`, recording
    /// when each is next due. Each convoy is judged in named steps, each run off its caller's
    /// frame: a debug build's poll frame holds every local of its function, and turn delivery
    /// routes deep (#3108).
    pub(super) async fn judge_scheduled(
        &self,
        namespace: &str,
        convoys: &HashMap<String, impl ConvoyRead>,
        now: DateTime<Utc>,
        schedule: &mut StallSchedule,
    ) -> Result<(), String> {
        let held = convoys
            .iter()
            .map(|(name, convoy)| (name, convoy.convoy().status.as_ref().is_some_and(|status| status.phase.is_terminal())))
            .collect();
        let pass = schedule.take(&held, now);
        if pass.is_empty() {
            return Ok(());
        }
        let primitives = self.subscriptions.message_primitives().map_err(|error| error.to_string())?;
        if let Some(wait) = primitives.delivery_wait(namespace).await? {
            // The designation's change judges them.
            tracing::debug!(namespace, %wait, "stall judging waits");
            schedule.put_back(pass);
            return Ok(());
        }
        let inputs = off_frame(|| self.read_stall_inputs(namespace, convoys, &pass, schedule)).await?;
        let available_convoys = &inputs.available_convoys;
        let rows = self.subscriptions.rows().await;
        self.subscriptions.inner.supervisor_context.lock().await.retain(|(ns, name, _), _| ns != namespace || convoys.contains_key(name));
        let current_versions = available_convoys
            .items
            .iter()
            .filter(|current| current.provenance == flotilla_resources::ResourceProvenance::Local)
            .map(|current| (&current.object.metadata.name, &current.object.metadata.resource_version))
            .collect::<HashMap<_, _>>();
        // The handle is shared with the loop; one lookup per tick is enough.
        let controller_queue = self.subscriptions.inner.controller_queues.lock().await.get(namespace).cloned();
        self.subscriptions
            .inner
            .pending_launches
            .lock()
            .await
            .convoys
            .retain(|(ns, name), _| ns != namespace || convoys.contains_key(name));
        let progress_observations = freshest_change_request_refs(&inputs.observation_list.items);
        // The namespace's own steps wait here: a retry after one fails.
        let namespace_waits = std::sync::Mutex::new(Waits::default());
        let readings = std::sync::Mutex::new(schedule.take_readings());
        let tick = StallTick {
            namespace,
            now,
            inputs: &inputs,
            progress_observations: &progress_observations,
            rows: &rows,
            controller_queue: controller_queue.as_ref(),
            waits: &namespace_waits,
            readings: &readings,
            first: false,
        };
        let reseeded = self.reseed_operator_stalls(tick, convoys).await;
        // A launch no longer judged here has ended: its seat holds no stall.
        let gone: Vec<_> = (self.subscriptions.inner.operator_told.lock().await.seats.keys())
            .filter(|(ns, name)| ns == namespace && !convoys.contains_key(name))
            .map(|(_, name)| name.clone())
            .collect();
        for name in gone {
            self.settle_operator_attention(tick, &name, &Stalls::default()).await;
        }
        for convoy in convoys.values().map(ConvoyRead::convoy).filter(|convoy| pass.judged.contains(&convoy.metadata.name)) {
            // A stall noted again from its attention is judged from the snapshot its note
            // changes.
            if reseeded.contains(&convoy.metadata.name) {
                continue;
            }
            // Rows belong to the caller's convoy revision. A judge tick can win
            // the select before its convoy-watch wake; do not publish a new
            // episode from that stale revision. The watch will rebuild the rows.
            // Only the authority judges stalls: a removed local record or a
            // replica-only copy must not be actuated from an old caller map.
            let current_version = current_versions.get(&convoy.metadata.name).copied();
            if current_version != Some(&convoy.metadata.resource_version) {
                tracing::debug!(
                    namespace,
                    convoy = %convoy.metadata.name,
                    snapshot_version = %convoy.metadata.resource_version,
                    ?current_version,
                    "skipping obsolete or non-local stall snapshot"
                );
                continue;
            }
            let waits = std::sync::Mutex::new(Waits::default());
            let first = pass.first.contains(&convoy.metadata.name);
            self.judge_convoy_stall(StallTick { waits: &waits, first, ..tick }, convoy).await?;
            schedule.waits(&convoy.metadata.name, waits.into_inner().unwrap_or_else(|poisoned| poisoned.into_inner()));
        }
        schedule.namespace_waits(namespace_waits.into_inner().unwrap_or_else(|poisoned| poisoned.into_inner()));
        schedule.hold_readings(readings.into_inner().unwrap_or_else(|poisoned| poisoned.into_inner()));
        Ok(())
    }

    /// The judged convoys' seats' messages: those `schedule` holds, read again for each seat
    /// whose messages changed, or whose named turns differ from the last read's.
    async fn seat_messages(
        &self,
        namespace: &str,
        convoys: &HashMap<String, impl ConvoyRead>,
        pass: &Pass,
        schedule: &mut StallSchedule,
    ) -> Result<Vec<flotilla_messaging::SeatMessage>, String> {
        // The judge reads only the messages to the convoys' crew members, and the turns their
        // statuses name, each held by the seat it went to.
        let mut unread: Vec<(String, bool, Vec<String>)> = Vec::new();
        for convoy in pass.judged.iter().filter_map(|name| convoys.get(name)).map(ConvoyRead::convoy) {
            // Each seat, whether it is a crew member's, and the turns named to it.
            let mut seats: BTreeMap<String, (bool, Vec<String>)> =
                flotilla_resources::convoy_crew_seats(convoy).into_iter().map(|seat| (seat, (true, Vec::new()))).collect();
            for (seat, record) in flotilla_resources::convoy_seat_turns(convoy) {
                seats.entry(seat).or_default().1.push(record);
            }
            for (_, turns) in seats.values_mut() {
                turns.sort();
            }
            // A seat's inbox is read whole, as the seats it holds share it: they are read again
            // together.
            let inbox = |seat: &str| flotilla_resources::convoy::engine::role_inbox(seat).ok();
            let stale: HashSet<_> =
                seats.iter().filter(|(seat, (_, turns))| schedule.unread(seat, turns)).map(|(seat, _)| inbox(seat)).collect();
            schedule.seats(&convoy.metadata.name, seats.keys().cloned().collect());
            unread.extend(
                seats.into_iter().filter(|(seat, _)| stale.contains(&inbox(seat))).map(|(seat, (crew, turns))| (seat, crew, turns)),
            );
        }
        let receivers: Vec<String> = unread.iter().filter(|(_, crew, _)| *crew).map(|(seat, ..)| seat.clone()).collect();
        let named: Vec<(String, String)> =
            unread.iter().flat_map(|(seat, _, turns)| turns.iter().map(|record| (seat.clone(), record.clone()))).collect();
        let mut read: HashMap<String, Vec<_>> = HashMap::new();
        if !receivers.is_empty() || !named.is_empty() {
            let primitives = self.subscriptions.message_primitives().map_err(|error| error.to_string())?;
            for message in primitives.seat_standings(namespace, &receivers, &named).await.map_err(|error| error.to_string())? {
                read.entry(message.seat.clone()).or_default().push(message);
            }
        }
        for (seat, _, turns) in unread {
            let messages = read.remove(&seat).unwrap_or_default();
            schedule.read(&seat, turns, messages);
        }
        Ok(schedule.standings(pass.judged.iter()))
    }

    /// One pass's request snapshot of the watched kinds, after reconciling charter notifications
    /// against it when the pass is to.
    async fn read_stall_inputs(
        &self,
        namespace: &str,
        convoys: &HashMap<String, impl ConvoyRead>,
        pass: &Pass,
        schedule: &mut StallSchedule,
    ) -> Result<StallInputs, String> {
        let backend = &self.subscriptions.inner.backend;
        let cell = self.views(namespace).await?;
        let views = cell.get().expect("initialized views");
        let mut snapshot = flotilla_store::ViewSnapshot::new(backend.local_root().ok());
        let projects = views.projects.snapshot(namespace, &mut snapshot).await.map_err(|error| error.to_string())?;
        let available_convoys = views.convoys.snapshot(namespace, &mut snapshot).await.map_err(|error| error.to_string())?;
        let sessions_snapshot = views.sessions.snapshot(namespace, &mut snapshot).await.map_err(|error| error.to_string())?;
        let sessions_list = sessions_snapshot;
        let observation_list = views.change_requests.shared_list().await.map_err(|error| error.to_string())?;
        let forges =
            views.forges.definitions().await.map_err(|error| error.to_string())?.into_iter().map(|forge| forge.spec).collect::<Vec<_>>();
        let vessels_list = views.vessels.shared_list().await.map_err(|error| error.to_string())?;
        let messages = self.seat_messages(namespace, convoys, pass, schedule).await?;
        // Routing helpers retain their public Store-based interface, but all
        // their reads come from the same request snapshot of the watched kinds.
        views.defaults.insert_snapshot(namespace, &mut snapshot).await.map_err(|error| error.to_string())?;
        views.fleet.insert_snapshot(namespace, &mut snapshot).await.map_err(|error| error.to_string())?;
        let read_backend = snapshot.into_backend();
        let receivers = flotilla_messaging::MessageReceiverSnapshot::load_from_shared_observations(
            &read_backend,
            namespace,
            available_convoys.clone(),
            sessions_list.clone(),
        )
        .await
        .map_err(|error| error.to_string())?;
        // Charter notifications follow the projects, the receivers and the designation.
        if pass.charter {
            let notices = self.subscriptions.message_primitives().map_err(|error| error.to_string())?.charter_notices(namespace);
            flotilla_messaging::reconcile_charter_notifications_with_receivers(
                notices.as_ref(),
                self.subscriptions.inner.charter_brief_renderer.as_ref(),
                &read_backend,
                &receivers,
                &projects,
            )
            .await
            .map_err(|error| error.to_string())?;
        }
        let declarations = flotilla_store::CascadeDeclarations::load(&read_backend, namespace).await.map_err(|error| error.to_string())?;
        Ok(StallInputs {
            projects,
            available_convoys,
            sessions_list,
            observation_list,
            forges,
            vessels_list,
            messages,
            receivers,
            declarations,
        })
    }

    /// Judges one locally authored convoy and writes its stall status.
    async fn judge_convoy_stall(&self, tick: StallTick<'_>, convoy: &ResourceObject<Convoy>) -> Result<(), String> {
        let projects = &tick.inputs.projects;
        let Some(status) = &convoy.status else { return Ok(()) };
        // A new turn or the launch's end may have closed a told stall since it was last judged.
        self.settle_operator_attention(tick, &convoy.metadata.name, &status.stalls).await;
        let Some(crew) = self.observe_convoy_crew(tick, convoy, status).await? else { return Ok(()) };
        let ConvoyCrew { sessions: selected_sessions, vessels: selected_vessels, mut obligations, holding } = crew;
        let verdict = self.judge_convoy_rows(tick, convoy, status, &selected_sessions, &selected_vessels, &mut obligations).await?;
        let RowsVerdict { progress_changed, unable, able, unknown, actionable_rows } = verdict;
        // Nothing can move the convoy: no maker is able, and none is still to be judged.
        let unmoved = holding && !able && !unknown;
        let stuck = unmoved && (!launch_underway(status.phase) || actionable_rows > 0);
        let mut next = Stalls::default();
        for (seat, cause) in stall_causes(status, unable, holding, unmoved, stuck) {
            let seat = seat.as_ref().map(|(work, role)| (work.as_str(), role.as_str()));
            let held = status.stalls.about(seat);
            // A consumed ladder stays consumed until the crew makes progress.
            if let Some(held) = held.filter(|held| held.supervision_exhausted && !progress_changed && launch_underway(status.phase)) {
                next.set(held.clone());
                continue;
            }
            let policy =
                super::supervision::supervision_policy(tick.inputs.routes(), convoy, &projects.items, held.and_then(|held| held.seat()))?;
            let condition =
                self.next_stall_condition(tick, convoy, status, &selected_sessions, &mut obligations, cause, held, &policy).await?;
            next.set(condition);
        }
        // A held stall judged from nothing this tick stays while something still holds it; another
        // seat's stall never replaces it.
        for held in &status.stalls {
            // The Convoy controller owns a capacity wait: it sets and
            // clears it as the host's slots change (#4038).
            let kept = next.about(held.seat()).is_none()
                && (held.cause == Some(flotilla_resources::StallCause::Capacity)
                    || (launch_underway(status.phase)
                        && actionable_rows > 0
                        && (held.supervisor.is_some()
                            || (held.rung == StallRung::Operator
                                && held.source == StallEvidenceSource::Crew
                                && declared_seat_stalled(status, held)))));
            if kept {
                next.set(held.clone());
            }
        }
        self.write_stall_status(tick, convoy, status, obligations, next).await
    }

    /// The convoy's crew sessions and vessels, with its turn-hook health and a working stall
    /// source observed; none once the convoy has nothing left to judge. It runs for every judged
    /// convoy, so its steps are awaited in place rather than boxed.
    async fn observe_convoy_crew(
        &self,
        tick: StallTick<'_>,
        convoy: &ResourceObject<Convoy>,
        status: &ConvoyStatus,
    ) -> Result<Option<ConvoyCrew>, String> {
        let (selected_sessions, selected_vessels) = self.observe_crew_progress(tick, convoy).await?;
        let mut obligations = status.nudge_obligations.clone();
        let holding = launch_underway(status.phase) || status.phase == ConvoyPhase::Landing;
        if holding {
            self.observe_turn_hook_health(tick, convoy, status, &selected_sessions).await?;
        }
        if self.clear_working_source_stall(tick, convoy, status, &selected_sessions, &mut obligations).await? {
            return Ok(None);
        }
        // An ended launch's records are settled on the change that ended it; it is judged on
        // nothing else.
        if !holding && status.stalls.is_empty() {
            self.end_settled_stalls(tick, convoy, status, &Stalls::default()).await;
            return Ok(None);
        }
        Ok(Some(ConvoyCrew { sessions: selected_sessions, vessels: selected_vessels, obligations, holding }))
    }

    /// The convoy's crew sessions and vessels, after observing its queued turns and launch
    /// progress.
    async fn observe_crew_progress(&self, tick: StallTick<'_>, convoy: &ResourceObject<Convoy>) -> Result<ConvoyChildren, String> {
        let StallTick { namespace, now, .. } = tick;
        let sessions = &tick.inputs.sessions_list.items;
        let vessels = &tick.inputs.vessels_list.items;
        let messages = &tick.inputs.messages;
        let selected_sessions = select_convoy_children(convoy, sessions);
        self.observe_queued_turns_from_messages(namespace, convoy, &selected_sessions, messages, now).await?;
        let selected_vessels = select_convoy_children(convoy, vessels);
        self.observe_launch_progress(tick, convoy, &selected_vessels, &selected_sessions).await?;
        Ok((selected_sessions, selected_vessels))
    }

    /// Records which crew sessions lack a turn hook, skipping no-op writes.
    async fn observe_turn_hook_health(
        &self,
        tick: StallTick<'_>,
        convoy: &ResourceObject<Convoy>,
        status: &ConvoyStatus,
        selected_sessions: &BTreeMap<String, ResourceObject<TerminalSession>>,
    ) -> Result<(), String> {
        let StallTick { namespace, now, .. } = tick;
        let backend = &self.subscriptions.inner.backend;
        let mut missing_hooks = Vec::new();
        for session in selected_sessions.values() {
            let agent = self.agent_view(tick, session);
            if let Some(deadline) = missing_turn_hook_deadline(session, agent.as_ref()) {
                tick.seat_due(Some(session), deadline);
            }
            missing_hooks.extend(missing_turn_hook(session, agent.as_ref(), now));
        }
        missing_hooks.sort();
        let reason = (!missing_hooks.is_empty()).then(|| missing_hooks.join("\n"));
        let prior = status.attention.as_ref().filter(|attention| attention.source == ConvoyAttention::MISSING_TURN_HOOK_SOURCE);
        // Skip no-op writes here; the patch repeats the source guard after
        // an optimistic retry so concurrent settlement attention stays intact.
        if status.attention.as_ref().is_none_or(|attention| attention.source == ConvoyAttention::MISSING_TURN_HOOK_SOURCE)
            && prior.map(|attention| &attention.reason) != reason.as_ref()
        {
            flotilla_store::note_convoy(
                &self.subscriptions.inner.convoy_engine,
                &backend.clone().using::<Convoy>(namespace),
                &convoy.metadata.name,
                &flotilla_resources::ConvoyStatusPatch::ObserveTurnHookHealth { reason, observed_at: now },
            )
            .await
            .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    /// Clears each supervised stall whose source crew is working again; true once any is cleared.
    async fn clear_working_source_stall(
        &self,
        tick: StallTick<'_>,
        convoy: &ResourceObject<Convoy>,
        status: &ConvoyStatus,
        selected_sessions: &BTreeMap<String, ResourceObject<TerminalSession>>,
        obligations: &mut Vec<NudgeObligation>,
    ) -> Result<bool, String> {
        let StallTick { namespace, now, .. } = tick;
        let backend = &self.subscriptions.inner.backend;
        let working = status
            .stalls
            .iter()
            .filter(|stalled| stalled.supervisor.is_some() && stalled.source != StallEvidenceSource::Crew)
            .filter_map(stalled_source_actor)
            .filter(|(vessel, role)| {
                selected_sessions.values().any(|session| {
                    session.metadata.labels.get(VESSEL_LABEL).is_some_and(|name| name == vessel)
                        && session.metadata.labels.get(ROLE_LABEL).is_some_and(|name| name == role)
                        && self.agent_view(tick, session).is_some_and(|agent| actor_is_working(&agent, obligations, vessel, role, now))
                })
            })
            .collect::<Vec<_>>();
        if working.is_empty() {
            return Ok(false);
        }
        if status.nudge_obligations != *obligations {
            flotilla_store::note_convoy(
                &self.subscriptions.inner.convoy_engine,
                &backend.clone().using::<Convoy>(namespace),
                &convoy.metadata.name,
                &flotilla_resources::ConvoyStatusPatch::SetNudgeObligations { obligations: std::mem::take(obligations) },
            )
            .await
            .map_err(|error| error.to_string())?;
        }
        for seat in &working {
            flotilla_store::note_convoy(
                &self.subscriptions.inner.convoy_engine,
                &backend.clone().using::<Convoy>(namespace),
                &convoy.metadata.name,
                &flotilla_resources::ConvoyStatusPatch::EndStall { seat: owned_seat(Some(*seat)) },
            )
            .await
            .map_err(|error| error.to_string())?;
        }
        let mut remaining = status.stalls.clone();
        remaining.retain(|stalled| stalled.seat().is_none_or(|seat| !working.contains(&seat)));
        self.end_settled_stalls(tick, convoy, status, &remaining).await;
        Ok(true)
    }

    /// Judges the convoy's subscription rows: whether any maker is able, unable or unknown.
    async fn judge_convoy_rows<'a>(
        &self,
        tick: StallTick<'a>,
        convoy: &ResourceObject<Convoy>,
        status: &ConvoyStatus,
        selected_sessions: &BTreeMap<String, ResourceObject<TerminalSession>>,
        selected_vessels: &BTreeMap<String, ResourceObject<flotilla_resources::Vessel>>,
        obligations: &mut Vec<NudgeObligation>,
    ) -> Result<RowsVerdict<'a>, String> {
        let StallTick { namespace, now, rows, .. } = tick;
        // The checkouts held for the convoy, read from the engine once a stall's progress needs them.
        let mut selected_checkouts: Option<BTreeMap<String, flotilla_resources::checkout_records::CheckoutRecord>> = None;
        let convoy_rows = rows.iter().filter(|row| {
                row.namespace == namespace
                    && (!launch_underway(status.phase) || !matches!(row.watcher, LeafWatcher::TurnDelivery { .. }))
                    && matches!(&row.watcher, LeafWatcher::ReconcilerWake { convoy: name } | LeafWatcher::TurnDelivery { convoy: name, .. } if name == &convoy.metadata.name)
            }).collect::<Vec<_>>();
        obligations.retain(|obligation| match &obligation.maker {
            LeafMaker::Actor { vessel, role } => {
                !status.phase.is_terminal()
                    && !status.crew_work.get(vessel).and_then(|crew| crew.get(role)).is_some_and(|work| work.phase == CrewWorkPhase::Done)
            }
            _ => false,
        });
        let mut progress_changed = false;
        let mut unable = Vec::new();
        let mut able = false;
        let mut unknown = false;
        let mut actionable_rows = 0;
        let settlement_satisfied = convoy_rows.iter().any(|row| {
            matches!(&row.maker,
                LeafMaker::Controller { resource_kind, .. } if resource_kind == "Convoy")
        });
        for row in convoy_rows {
            // Once settlement is satisfied only the controller can write Landed.
            // The observer and actor rows no longer describe outstanding work.
            if settlement_satisfied && !matches!(row.maker, LeafMaker::Controller { .. }) {
                continue;
            }
            if row.leaves.iter().any(|leaf| {
                let LeafAddress::Work { work, .. } = &leaf.address else { return false };
                let Some(role) = leaf.field_path.strip_prefix(".crew.").and_then(|field| field.strip_suffix(".phase")) else {
                    return false;
                };
                status.crew_work.get(work).and_then(|crew| crew.get(role)).is_some_and(|state| {
                    state.phase == CrewWorkPhase::Done && leaf.operator == LeafOperator::Equal && leaf.literal == "Done"
                })
            }) {
                continue;
            }
            // A seat whose own session stopped is interrupted, not stalled: its relaunch is its
            // path. Live seats of the same launch are still judged.
            if let LeafMaker::Actor { vessel, role } = &row.maker {
                if seat_interrupted(selected_vessels, vessel, role) {
                    continue;
                }
            }
            actionable_rows += 1;
            let judgement = match &row.maker {
                LeafMaker::Actor { vessel, role } => {
                    self.judge_actor_row(
                        tick,
                        convoy,
                        status,
                        selected_sessions,
                        selected_vessels,
                        obligations,
                        &mut selected_checkouts,
                        &mut progress_changed,
                        row,
                        vessel,
                        role,
                    )
                    .await?
                }
                LeafMaker::Supervisor { convoy: supervisor_convoy, vessel, role } => {
                    self.judge_supervisor_row(tick, row, supervisor_convoy, vessel, role).await
                }
                LeafMaker::Observed { .. } => self.judge_observed_row(tick, row).await,
                LeafMaker::Controller { resource_kind, name, retry, ceiling } => {
                    let controller_queue = tick.controller_queue;
                    let reason = if resource_kind == "Convoy" {
                        if let Some(due) = controller_queue.and_then(|queue| queue.stall_due(CONVOY_CONTROLLER_PROGRESS_BOUND)) {
                            tick.due(due);
                        }
                        controller_queue
                            .as_ref()
                            .and_then(|queue| queue.stall_reason(now, CONVOY_CONTROLLER_PROGRESS_BOUND))
                            .map(|reason| format!("Convoy controller for {}: {reason}", name.as_deref().unwrap_or("unknown")))
                    } else {
                        if let Some(due) = retry.stall_due(*ceiling) {
                            tick.due(due);
                        }
                        retry.stall_reason(now, *ceiling)
                    };
                    match reason {
                        Some(reason) => RowJudgement::Unable(reason, StallEvidenceSource::LeafEngine),
                        None => RowJudgement::Able,
                    }
                }
            };
            match judgement {
                RowJudgement::Unknown => {
                    unknown = true;
                    continue;
                }
                RowJudgement::Able => {
                    able = true;
                    continue;
                }
                RowJudgement::Unable(evidence, source) => {
                    if matches!(row.maker, LeafMaker::Controller { .. }) {
                        unable = vec![(row, evidence, source)];
                        able = false;
                        break;
                    }
                    unable.push((row, evidence, source));
                }
            };
        }
        Ok(RowsVerdict { progress_changed, unable, able, unknown, actionable_rows })
    }

    /// Judges an actor row from its crew session, recording its progress on its obligation.
    #[allow(clippy::too_many_arguments)]
    async fn judge_actor_row(
        &self,
        tick: StallTick<'_>,
        convoy: &ResourceObject<Convoy>,
        status: &ConvoyStatus,
        selected_sessions: &BTreeMap<String, ResourceObject<TerminalSession>>,
        selected_vessels: &BTreeMap<String, ResourceObject<flotilla_resources::Vessel>>,
        obligations: &mut Vec<NudgeObligation>,
        selected_checkouts: &mut Option<BTreeMap<String, flotilla_resources::checkout_records::CheckoutRecord>>,
        progress_changed: &mut bool,
        row: &LeafSubscriptionRow,
        vessel: &str,
        role: &str,
    ) -> Result<RowJudgement, String> {
        let StallTick { now, progress_observations, .. } = tick;
        let messages = &tick.inputs.messages;
        let judgement: Result<(), (String, StallEvidenceSource)> = {
            let session = selected_sessions.values().find(|session| {
                session.metadata.labels.get(CONVOY_LABEL) == Some(&convoy.metadata.name)
                    && session.metadata.labels.get(VESSEL_LABEL).map(String::as_str) == Some(vessel)
                    && session.metadata.labels.get(ROLE_LABEL).map(String::as_str) == Some(role)
            });
            let index = obligations.iter().position(|obligation| obligation.maker == row.maker && obligation.leaves == row.leaves);
            let obligation = if let Some(index) = index {
                &mut obligations[index]
            } else {
                let history = status
                    .stalls
                    .iter()
                    .find(|stall| stall.maker.as_ref() == Some(&row.maker) && stall.leaves == row.leaves)
                    .map_or_else(Vec::new, |stall| stall.nudge_history.clone());
                obligations.push(NudgeObligation::builder().maker(row.maker.clone()).leaves(row.leaves.clone()).history(history).build());
                obligations.last_mut().expect("inserted obligation")
            };
            // Only observed revisions reset a budget. Missing/unknown
            // records and routine refresh timestamps are not progress.
            let checkout_refs = selected_vessels
                .values()
                .filter(|object| object.spec.vessel_name == *vessel)
                .flat_map(|object| object.status.iter().flat_map(|status| status.checkout_refs.values()))
                .collect::<HashSet<_>>();
            let mut progress = BTreeMap::new();
            if selected_checkouts.is_none() {
                let held = match flotilla_resources::convoy::engine::checkout_holder(convoy) {
                    Some(holder) => {
                        let reads = Arc::clone(&self.subscriptions.inner.lifecycle_reads);
                        tokio::task::spawn_blocking(carry(move || reads.checkouts()?.held(&holder)))
                            .await
                            .map_err(|error| format!("held checkouts: {error}"))??
                    }
                    None => Vec::new(),
                };
                *selected_checkouts = Some(flotilla_resources::checkout_records::latest_launches(held));
            }
            for checkout in selected_checkouts.iter().flat_map(BTreeMap::values) {
                let name = checkout.checkout_name();
                if !checkout_refs.contains(&name.to_owned()) {
                    continue;
                }
                let Some(inspection) = checkout.inspection() else { continue };
                if let Some(commit) = &inspection.head_revision {
                    progress.insert(format!("checkout/{name}"), commit.clone());
                }
                if let Some(digest) = &inspection.remote_digest {
                    let reference = flotilla_resources::checkout_integration::remote_ref_for_branch(checkout.branch());
                    progress.insert(format!("push/{name}/{reference}"), digest.clone());
                }
            }
            for subject in flotilla_resources::active_change_request_subjects(convoy)? {
                for object in progress_observations.values() {
                    if object.spec.service == subject.source.service
                        && object.spec.scope == subject.source.scope
                        && object.spec.number.to_string() == subject.id
                    {
                        if let Some(head) = object.status.as_ref().and_then(|status| status.head_sha.value.as_ref()) {
                            progress.insert(format!("cr/{}", object.metadata.name), head.clone());
                        }
                    }
                }
            }
            let changed = progress.iter().any(|(key, value)| obligation.progress.get(key).is_some_and(|prior| prior != value));
            if changed {
                // Observed progress restarts the quiet clock, as a delivered message does, until
                // the agent next goes idle.
                obligation.history.clear();
                obligation.reply_after = Some(now);
                *progress_changed = true;
            }
            obligation.progress.extend(progress);
            let receiver_address = crew_role_address(&convoy.spec.project_ref, &convoy.metadata.name, vessel, role);
            let current_message = messages
                .iter()
                .filter(|message| {
                    message.seat == receiver_address && (!message.phase.is_terminal() || message.phase.has_delivery_evidence())
                })
                .max_by_key(|message| (message.accepted_sequence, message.created));
            let session_status = session.and_then(|session| session.status.as_ref());
            let message_id = current_message.map(|message| message.name.clone());
            let delivered_id = messages
                .iter()
                .filter(|message| message.seat == receiver_address && message.phase.has_delivery_evidence())
                .max_by_key(|message| message.delivered_at)
                .map(|message| message.name.clone());
            if message_id != obligation.message_id || delivered_id != obligation.delivered_message_id {
                if message_id.is_some() || delivered_id.is_some() {
                    obligation.reply_after = Some(now);
                }
                obligation.message_id = message_id.clone();
                obligation.delivered_message_id = delivered_id;
            }
            let agent = session.and_then(|session| self.agent_view(tick, session));
            let pending_message = messages.iter().any(|message| message.seat == receiver_address && message.phase.is_waiting());
            // The agent's own record says when it went idle (#3108). A delivered message's reply
            // is consumed once the agent goes idle after it; until then the quiet clock counts
            // from the delivery.
            let idle_since = agent
                .as_ref()
                .filter(|agent| matches!(agent.live_activity(), Some(AgentActivity::Idle | AgentActivity::Interrupted)))
                .and_then(|agent| agent.reading.activity_since);
            if obligation.reply_after.is_some_and(|after| idle_since.is_some_and(|since| since > after)) && !pending_message {
                obligation.reply_after = None;
            }
            let quiet_since = agent.as_ref().and_then(AgentActivityView::quiet_since).map(|quiet| match obligation.reply_after {
                Some(after) => quiet.max(after),
                None => quiet,
            });
            if let Some(deadline) = agent.as_ref().and_then(|agent| turn_inactivity_deadline(agent, obligation)) {
                tick.seat_due(session, deadline);
            }
            let turn_inactivity = agent.as_ref().and_then(|agent| turn_inactivity_reason(agent, obligation, now));
            let idle_grace = chrono::Duration::seconds(i64::from(idle_grace_seconds(status, vessel, role)));
            let lost_reason = session
                .and_then(|session| session.status.as_ref())
                .filter(|status| status.phase == TerminalSessionPhase::Lost)
                .and_then(|status| status.message.as_deref());
            let agent = agent.filter(|_| session_status.is_some_and(|status| status.phase == TerminalSessionPhase::Running));
            let crew_phase = status.crew_work.get(vessel).and_then(|crew| crew.get(role)).map(|work| work.phase);
            let launching = crew_phase == Some(CrewWorkPhase::Pending)
                && (session.is_none()
                    || idle_since.is_some()
                    || session.is_some_and(|session| {
                        session.status.as_ref().is_none_or(|status| status.phase == TerminalSessionPhase::Starting)
                    }));
            if let Some(declared) = declared_stall(status, vessel, role) {
                // The crew's own stall note, whichever rung it has reached: its report, not the
                // diagnostics the judge added to the note's evidence.
                let report = status.crew_work.get(vessel).and_then(|crew| crew.get(role)).and_then(|work| work.message.clone());
                Err((report.unwrap_or_else(|| declared.evidence.clone()), StallEvidenceSource::Crew))
            } else if status.phase == ConvoyPhase::Landing
                && status.work.get(vessel).is_some_and(|work| work.phase == WorkPhase::Complete)
                && session.is_none_or(|session| {
                    session.metadata.deletion_timestamp.is_some()
                        || session.status.as_ref().is_some_and(|session_status| session_status.phase == TerminalSessionPhase::Stopped)
                })
            {
                // A cached actor row can outlive completed work and
                // its teardown. This actor has no remaining work;
                // other makers (especially failed controllers) still judge.
                self.subscriptions.inner.unable_since.lock().await.remove(&row.id);
                Ok(())
            } else if launching {
                // Pending launch is not a dead session or an ended turn.
                self.subscriptions.inner.unable_since.lock().await.remove(&row.id);
                return Ok(RowJudgement::Unknown);
            } else if let Some(reason) = lost_reason {
                self.subscriptions.inner.unable_since.lock().await.remove(&row.id);
                Err((format!("session dead: {reason}"), StallEvidenceSource::Session))
            } else if let Some(reason) = turn_inactivity {
                Err((reason, StallEvidenceSource::Session))
            } else {
                match agent.as_ref().map(|agent| (agent, agent.live_activity())) {
                    Some((agent, _)) if !agent.current => {
                        self.subscriptions.inner.unable_since.lock().await.remove(&row.id);
                        self.report_stale_attention(row, activity_source(agent)).await;
                        return Ok(RowJudgement::Unknown);
                    }
                    Some((_, Some(AgentActivity::Working))) => {
                        self.subscriptions.inner.unable_since.lock().await.remove(&row.id);
                        self.subscriptions.inner.stale_attention_reported.lock().await.remove(&row.id);
                        Ok(())
                    }
                    Some((_, None)) => {
                        self.subscriptions.inner.unable_since.lock().await.remove(&row.id);
                        return Ok(RowJudgement::Unknown);
                    }
                    Some((agent, Some(activity))) => {
                        self.subscriptions.inner.stale_attention_reported.lock().await.remove(&row.id);
                        let source = activity_source(agent);
                        let evidence_source = match source {
                            TerminalAttentionSource::Screen => StallEvidenceSource::Screen,
                            TerminalAttentionSource::Hook => StallEvidenceSource::Hook,
                        };
                        if activity == AgentActivity::NeedsInput {
                            let since = agent.reading.activity_since.unwrap_or(now);
                            let debounce = if source == TerminalAttentionSource::Hook {
                                chrono::Duration::zero()
                            } else {
                                TerminalAttention::DEBOUNCE_FOR
                            };
                            let key = UnableEvidenceKey::Attention { state: activity_state(activity), source };
                            if let Some(until) = self.maker_debouncing(row.id, key, since, debounce, now).await {
                                tick.due(until);
                                return Ok(RowJudgement::Unknown);
                            }
                            Err(("NeedsInput".into(), evidence_source))
                        } else {
                            // A pending message's delivery judges the seat again.
                            match quiet_since {
                                _ if pending_message => return Ok(RowJudgement::Unknown),
                                None => return Ok(RowJudgement::Unknown),
                                Some(since) if now.signed_duration_since(since) < idle_grace => {
                                    tick.seat_due(session, since + idle_grace);
                                    return Ok(RowJudgement::Unknown);
                                }
                                Some(_) => {}
                            }
                            Err(("idle".into(), evidence_source))
                        }
                    }
                    None => {
                        if session.is_some_and(|session| {
                            session.status.as_ref().is_some_and(|status| status.phase == TerminalSessionPhase::Running)
                        }) {
                            self.subscriptions.inner.unable_since.lock().await.remove(&row.id);
                            return Ok(RowJudgement::Unknown);
                        }
                        if let Some(until) =
                            self.maker_debouncing(row.id, UnableEvidenceKey::Absent, now, TerminalAttention::DEBOUNCE_FOR, now).await
                        {
                            tick.due(until);
                            return Ok(RowJudgement::Unknown);
                        }
                        Err(("session dead or absent".into(), StallEvidenceSource::Session))
                    }
                }
            }
        };
        Ok(RowJudgement::from(judgement))
    }

    /// Judges a supervisor row from the supervisor's session attention.
    async fn judge_supervisor_row(
        &self,
        tick: StallTick<'_>,
        row: &LeafSubscriptionRow,
        supervisor_convoy: &str,
        vessel: &str,
        role: &str,
    ) -> RowJudgement {
        let StallTick { now, .. } = tick;
        let sessions = &tick.inputs.sessions_list.items;
        let judgement: Result<(), (String, StallEvidenceSource)> = {
            let session = sessions.iter().find(|source| {
                let session = &source.object;
                session.metadata.labels.get(CONVOY_LABEL).map(String::as_str) == Some(supervisor_convoy)
                    && session.metadata.labels.get(VESSEL_LABEL).map(String::as_str) == Some(vessel)
                    && session.metadata.labels.get(ROLE_LABEL).map(String::as_str) == Some(role)
            });
            let running = session
                .and_then(|source| source.object.status.as_ref())
                .is_some_and(|status| status.phase == TerminalSessionPhase::Running);
            let agent = session.filter(|_| running).and_then(|source| {
                let origin = match &source.provenance {
                    flotilla_resources::ResourceProvenance::Local => None,
                    flotilla_resources::ResourceProvenance::Replica { origin_root, .. } => Some(origin_root.as_str().to_owned()),
                };
                self.agent_view_at(tick, &(flotilla_resources::terminal_records::launch_of(&source.object)?.to_owned(), origin))
            });
            match agent.as_ref().map(|agent| (agent, agent.live_activity())) {
                Some((agent, _)) if !agent.current => {
                    self.subscriptions.inner.unable_since.lock().await.remove(&row.id);
                    self.report_stale_attention(row, activity_source(agent)).await;
                    return RowJudgement::Unknown;
                }
                Some((_, Some(AgentActivity::Working))) => {
                    self.subscriptions.inner.stale_attention_reported.lock().await.remove(&row.id);
                    Ok(())
                }
                Some((agent, Some(activity @ (AgentActivity::Idle | AgentActivity::Interrupted)))) => {
                    self.subscriptions.inner.stale_attention_reported.lock().await.remove(&row.id);
                    let key = UnableEvidenceKey::Attention { state: activity_state(activity), source: activity_source(agent) };
                    if let Some(until) = self.maker_debouncing(row.id, key, row.created_at, TerminalAttention::DEBOUNCE_FOR, now).await {
                        tick.due(until);
                        return RowJudgement::Unknown;
                    }
                    Err(("supervisor idle".into(), StallEvidenceSource::Session))
                }
                Some((_, Some(AgentActivity::NeedsInput))) => {
                    self.subscriptions.inner.stale_attention_reported.lock().await.remove(&row.id);
                    Err(("supervisor needs input".into(), StallEvidenceSource::Hook))
                }
                Some((_, None)) => {
                    return RowJudgement::Unknown;
                }
                None if running => {
                    return RowJudgement::Unknown;
                }
                None => {
                    if let Some(until) =
                        self.maker_debouncing(row.id, UnableEvidenceKey::Absent, row.created_at, TerminalAttention::DEBOUNCE_FOR, now).await
                    {
                        tick.due(until);
                        return RowJudgement::Unknown;
                    }
                    Err(("supervisor session dead or absent".into(), StallEvidenceSource::Session))
                }
            }
        };
        RowJudgement::from(judgement)
    }

    /// Judges an observed row from the freshness of its change-request observations.
    async fn judge_observed_row(&self, tick: StallTick<'_>, row: &LeafSubscriptionRow) -> RowJudgement {
        let StallTick { namespace, now, .. } = tick;
        let forges = tick.inputs.forges.as_slice();
        let observations = &tick.inputs.observation_list.items;
        let judgement: Result<(), (String, StallEvidenceSource)> = {
            let mut reason = None;
            for leaf in &row.leaves {
                if let LeafAddress::ChangeRequest { .. } = &leaf.address {
                    let address = leaf_address(&leaf.address, forges);
                    let name = address.as_ref().ok().and_then(record_name);
                    let observation = observations
                        .iter()
                        .filter(|source| name.as_ref() == Some(&source.object.metadata.name))
                        .filter_map(|source| source.object.status.as_ref())
                        .max_by_key(|status| status.state.observed_at);
                    let has_value = observation.is_some_and(|status| match leaf.field_path.as_str() {
                        ".state" => status.state.value.is_some(),
                        ".ready" => status.state.value.is_some() && status.checks.value.is_some(),
                        ".head-sha" => status.head_sha.value.is_some(),
                        ".checks" => status.checks.value.is_some(),
                        ".review.actionable-at-head" => status.review.actionable_at_head.value.is_some(),
                        ".mergeable" => status.mergeable.value.is_some(),
                        _ => false,
                    });
                    let merged = observation
                        .is_some_and(|status| status.state.value == Some(flotilla_resources::ObservedChangeRequestState::Merged));
                    let final_settlement = merged;
                    // Until when the observation answers the row's demand: it goes stale then.
                    let fresh_until = observation.filter(|_| has_value).and_then(|status| {
                        let at = match leaf.field_path.as_str() {
                            ".ready" => status.state.observed_at.min(status.checks.observed_at),
                            ".head-sha" => status.head_sha.observed_at,
                            ".checks" => status.checks.observed_at,
                            ".review.actionable-at-head" => status.review.actionable_at_head.observed_at,
                            ".mergeable" => status.mergeable.observed_at,
                            _ => status.state.observed_at,
                        };
                        let stale_after =
                            chrono::Duration::from_std(self.subscriptions.change_request_stale_after()).expect("duration fits");
                        row.freshness_demand.is_none_or(|demand| at >= demand).then_some(at + stale_after)
                    });
                    if !final_settlement {
                        if let Some(until) = fresh_until {
                            tick.due(until);
                        }
                    }
                    let fresh = final_settlement || fresh_until.is_some_and(|until| now < until);
                    if !fresh {
                        let subject = address.and_then(|address| ChangeRequestKey::new(namespace, address));
                        let refresh_error = match &subject {
                            Ok(subject) => self.subscriptions.change_request_observation_error(subject).await,
                            Err(error) => Some(ObservationError::Failed(error.clone())),
                        };
                        let deadline = if let Ok(subject) = subject {
                            let key = (row.id, subject);
                            let cached = self.subscriptions.inner.observer_grace.lock().await.get(&key).copied();
                            // The pinned deadline is an episode property. Consult
                            // the observer's schedule only on the first stale tick,
                            // never while holding the grace-map lock.
                            let deadline = if let Some(deadline) = cached {
                                deadline
                            } else {
                                let proposed = self
                                    .subscriptions
                                    .inner
                                    .change_requests
                                    .refresh_grace_deadline(&key.1, row.freshness_demand, row.created_at)
                                    .await;
                                *self.subscriptions.inner.observer_grace.lock().await.entry(key.clone()).or_insert(proposed)
                            };
                            // Explicit forge backoff may extend a pinned episode;
                            // ordinary repeated failures cannot renew its grace.
                            let extended = refresh_error.as_ref().and_then(ObservationError::retry_at).map_or(deadline, |retry_at| {
                                deadline.max(self.subscriptions.inner.change_requests.retry_grace_deadline(retry_at))
                            });
                            let deadline = if extended > deadline {
                                let mut grace = self.subscriptions.inner.observer_grace.lock().await;
                                let stored = grace.entry(key).or_insert(deadline);
                                *stored = (*stored).max(extended);
                                *stored
                            } else {
                                deadline
                            };
                            Some(deadline)
                        } else {
                            // A formerly resolvable address no longer names an
                            // observer; do not retain its old grace entry.
                            self.subscriptions.inner.observer_grace.lock().await.retain(|(id, _), _| *id != row.id);
                            None
                        };
                        if let Some(deadline) = deadline.filter(|deadline| now < *deadline) {
                            tick.due(deadline);
                            return RowJudgement::Unknown;
                        }
                        // The deadline is stable evidence of observer lag. Do not
                        // rewrite the convoy each second merely to increment a
                        // duration; began_at already records the stall episode.
                        let lag = deadline
                            .map_or(String::new(), |deadline| format!("; observer behind scheduled refresh grace since {deadline}"));
                        reason = Some(format!(
                            "change_request observer: {}{lag}",
                            match (observation.is_some(), has_value, refresh_error) {
                                (true, true, Some(error)) => format!("stale; refresh failed: {error}"),
                                (true, true, None) => "stale".into(),
                                (_, _, Some(error)) => error.to_string(),
                                _ => "not refreshed".into(),
                            }
                        ));
                        break;
                    } else if let Ok(subject) = address.and_then(|address| ChangeRequestKey::new(namespace, address)) {
                        self.subscriptions.inner.observer_grace.lock().await.remove(&(row.id, subject));
                    }
                }
            }
            reason.map_or(Ok(()), |reason| Err((reason, StallEvidenceSource::Observation)))
        };
        RowJudgement::from(judgement)
    }

    /// A seat's next stall condition, from `cause` and the stall `held` about that seat, nudging an
    /// idle actor or escalating to its supervision policy as its rung requires.
    #[allow(clippy::too_many_arguments)]
    async fn next_stall_condition(
        &self,
        tick: StallTick<'_>,
        convoy: &ResourceObject<Convoy>,
        status: &ConvoyStatus,
        selected_sessions: &BTreeMap<String, ResourceObject<TerminalSession>>,
        obligations: &mut [NudgeObligation],
        cause: Option<StallCause<'_>>,
        held: Option<&StalledCondition>,
        policy: &[SupervisionTarget],
    ) -> Result<StalledCondition, String> {
        let StallTick { namespace, now, .. } = tick;
        let messages = &tick.inputs.messages;
        let (available_convoys, projects) = (&tick.inputs.available_convoys, &tick.inputs.projects);
        let cause_row = cause.as_ref().and_then(|cause| cause.row);
        let (leaves, maker, evidence, source) = match cause {
            Some(StallCause { leaves, maker, evidence, source, .. }) => (leaves, Some(maker), evidence, source),
            None => (Vec::new(), None, empty_row_evidence(status), StallEvidenceSource::LeafEngine),
        };
        let prior = held.filter(|prior| prior.leaves == leaves && prior.maker == maker);
        let nudge_history = obligations
            .iter()
            .find(|obligation| Some(&obligation.maker) == maker.as_ref() && obligation.leaves == leaves)
            .map_or_else(Vec::new, |obligation| obligation.history.clone());
        let mut condition = StalledCondition {
            leaves,
            maker,
            evidence,
            source,
            cause: None,
            began_at: prior.map_or(now, |stalled| stalled.began_at),
            rung: StallRung::Operator,
            supervisor: None,
            supervision_message: None,
            supervision_index: None,
            supervision_exhausted: false,
            reason: None,
            proposed_disposition: None,
            nudge_history,
        };
        let declared = held.filter(|stalled| stalled.source == StallEvidenceSource::Crew && stalled.leaves == condition.leaves);
        if let Some(declared) = declared {
            // Keep the crew's evidence, rather than accumulating prior delivery diagnostics.
            condition.evidence = stalled_source_actor(declared)
                .and_then(|(vessel, role)| status.crew_work.get(vessel)?.get(role)?.message.clone())
                .unwrap_or_else(|| declared.evidence.clone());
            condition.source = StallEvidenceSource::Crew;
            condition.reason = declared.reason;
            condition.proposed_disposition = declared.proposed_disposition;
            condition.began_at = declared.began_at;
        }
        if matches!(condition.maker, Some(LeafMaker::Supervisor { .. })) {
            if let Some(prior) = prior {
                condition.evidence = prior.evidence.clone();
                condition.source = prior.source.clone();
                condition.reason = prior.reason;
                condition.proposed_disposition = prior.proposed_disposition;
                condition.began_at = prior.began_at;
            }
        }
        let mut awaiting_resumed_turn = false;
        if let (Some(LeafMaker::Actor { vessel, role }), Some(row)) = (&condition.maker, cause_row) {
            let session = selected_sessions.values().find(|session| {
                session.metadata.labels.get(VESSEL_LABEL) == Some(vessel) && session.metadata.labels.get(ROLE_LABEL) == Some(role)
            });
            let idle_at = session
                .filter(|session| matches!(session.spec.source, TerminalSessionSource::Agent { .. }))
                .and_then(|session| self.agent_view(tick, session))
                .filter(|agent| matches!(agent.live_activity(), Some(AgentActivity::Idle | AgentActivity::Interrupted)))
                .and_then(|agent| agent.reading.activity_since);
            // A crew that declared a stall has answered its resumed turn: a seat reads Working on
            // the engine until its supervision opens a stall record, so the declaration says so.
            let resume_grace = status
                .crew_work
                .get(vessel)
                .and_then(|crew| crew.get(role))
                .filter(|work| work.phase == CrewWorkPhase::Working && declared.is_none())
                .and_then(|work| work.resumed_at.zip(work.resume_brief_id.as_deref()))
                .is_some_and(|(resumed_at, brief_id)| {
                    // A missing or recreated session must not suppress
                    // supervision indefinitely while its brief is unconfirmed.
                    if now.signed_duration_since(resumed_at) >= RESUMED_TURN_GRACE {
                        return false;
                    }
                    tick.seat_due(session, resumed_at + RESUMED_TURN_GRACE);
                    let operator_brief_delivered =
                        messages.iter().any(|message| message.name == brief_id && message.phase.has_delivery_evidence());
                    !operator_brief_delivered || idle_at.is_none_or(|idle_at| idle_at <= resumed_at)
                });
            awaiting_resumed_turn = resume_grace;
            if idle_at.is_some() && declared.is_none() && !resume_grace {
                let limit = nudge_policy(status, vessel, role).map_or(2, |policy| policy.max_per_episode) as usize;
                let grace_seconds = idle_grace_seconds(status, vessel, role);
                let backoff = nudge_backoff(grace_seconds, condition.nudge_history.len());
                let due = condition.nudge_history.last().is_none_or(|nudge| now.signed_duration_since(nudge.at) >= backoff);
                if prior
                    .is_some_and(|stalled| stalled.rung == StallRung::Operator && stalled.evidence.starts_with("nudge delivery failed:"))
                {
                    condition.rung = StallRung::Operator;
                    condition.evidence = prior.expect("checked above").evidence.clone();
                } else if condition.nudge_history.len() < limit {
                    condition.rung = StallRung::Nudge;
                    if due {
                        let leaf = row.leaves.first().ok_or_else(|| "actor row has no leaf".to_string())?;
                        let delivered =
                            off_frame(|| self.deliver_stall_nudge(tick, convoy, vessel, role, leaf, condition.nudge_history.len())).await?;
                        match delivered {
                            Ok(_) => {
                                condition.nudge_history.push(StallNudge { at: now, row: leaf.clone() });
                                if let Some(obligation) = obligations
                                    .iter_mut()
                                    .find(|obligation| obligation.maker == row.maker && obligation.leaves == row.leaves)
                                {
                                    obligation.history = condition.nudge_history.clone();
                                    obligation.reply_after = Some(now);
                                }
                            }
                            Err(error) => {
                                condition.rung = StallRung::Operator;
                                condition.evidence = format!("nudge delivery failed: {error}");
                                self.tell_operator(namespace, convoy, &condition).await;
                            }
                        }
                    }
                } else if !due {
                    condition.rung = StallRung::Nudge;
                }
                // The next nudge, or the rung past the last one, is due a backoff after it.
                if let Some(last) = condition.nudge_history.last().filter(|_| condition.rung == StallRung::Nudge) {
                    tick.seat_due(session, last.at + nudge_backoff(grace_seconds, condition.nudge_history.len()));
                }
            }
        }
        let needs_supervisor = matches!(condition.maker, Some(LeafMaker::Supervisor { .. }))
            || condition.source == StallEvidenceSource::Crew
            || condition.source == StallEvidenceSource::Session
            || condition.evidence == "NeedsInput"
            || (condition.rung == StallRung::Operator && !condition.nudge_history.is_empty())
            || (condition.rung == StallRung::Operator
                && matches!(condition.maker, Some(LeafMaker::Actor { .. }))
                && condition.evidence == "idle"
                && condition.nudge_history.is_empty());
        if needs_supervisor && !awaiting_resumed_turn && !condition.evidence.starts_with("nudge delivery failed:") {
            let start = prior.and_then(|stalled| stalled.supervision_index.map(|index| index + 1)).unwrap_or(0);
            let keep_current =
                prior.is_some_and(|stalled| stalled.supervisor.is_some()) && !matches!(condition.maker, Some(LeafMaker::Supervisor { .. }));
            let context = format!(
                "{:?}",
                (
                    &policy,
                    status.crew_work.iter().map(|(vessel, crew)| (vessel, crew.keys().collect::<Vec<_>>())).collect::<Vec<_>>(),
                    available_convoys
                        .items
                        .iter()
                        .filter(|candidate| candidate.object.metadata.name != convoy.metadata.name
                            && candidate.object.spec.project_ref == convoy.spec.project_ref)
                        .map(|candidate| {
                            let candidate = &candidate.object;
                            (
                                &candidate.metadata.name,
                                (
                                    &candidate.spec.role,
                                    candidate.spec.generation,
                                    candidate.status.as_ref().map(|status| {
                                        (
                                            &status.phase,
                                            status
                                                .crew_work
                                                .iter()
                                                .map(|(vessel, crew)| (vessel, crew.keys().collect::<Vec<_>>()))
                                                .collect::<Vec<_>>(),
                                        )
                                    }),
                                ),
                            )
                        })
                        .collect::<BTreeMap<_, _>>(),
                    // Every standing role's holder: an address target may name another project's role.
                    flotilla_store::newest_live_generations(available_convoys.items.iter().map(|source| &source.object))
                        .into_iter()
                        .map(|(ensure, holder)| (ensure, &holder.metadata.name))
                        .collect::<BTreeMap<_, _>>(),
                    projects
                        .items
                        .iter()
                        .map(|project| (&project.object.metadata.name, &project.object.metadata.resource_version))
                        .collect::<BTreeMap<_, _>>()
                )
            );
            let context_key = (namespace.to_string(), convoy.metadata.name.clone(), owned_seat(condition.seat()));
            let unchanged_context = self.subscriptions.inner.supervisor_context.lock().await.get(&context_key) == Some(&context);
            let cached_lookup = unchanged_context
                && prior
                    .is_some_and(|prior| prior.rung == StallRung::Operator && prior.supervisor.is_none() && !prior.supervision_exhausted);
            if keep_current || cached_lookup {
                condition = prior.expect("checked above").clone();
            } else {
                off_frame(|| self.escalate_stall(tick, convoy, &mut condition, prior, policy, start, context_key, context)).await?;
            }
        }
        Ok(condition)
    }

    /// Delivers a stall nudge to an idle actor; the inner error is a failed delivery.
    async fn deliver_stall_nudge(
        &self,
        tick: StallTick<'_>,
        convoy: &ResourceObject<Convoy>,
        vessel: &str,
        role: &str,
        leaf: &Leaf,
        nudges: usize,
    ) -> Result<Result<(), String>, String> {
        let StallTick { namespace, now, .. } = tick;
        let obligation = actor_obligation(leaf)?;
        let brief =
            format!("For {role}@{vessel} in {} (resource ref: {}):\n{obligation}", convoy_message_address(convoy), convoy.metadata.name);
        // A condition or reference that names nothing fails this delivery like any other.
        let parts = flotilla_resources::address::leaf_condition(leaf, Some(&convoy.spec.project_ref), &[])
            .and_then(|condition| Ok((condition, convoy_reference(convoy)?)));
        let (condition, reference) = match parts {
            Ok(parts) => parts,
            Err(error) => return Ok(Err(error)),
        };
        let request = CrewTurnIntent::builder()
            .namespace(namespace.to_string())
            .convoy(convoy.metadata.name.clone())
            .source(format!("stall-nudge-{}", nudges + 1))
            .vessel(vessel.to_owned())
            .role(role.to_owned())
            .brief(brief)
            .subject_revision(now.timestamp_micros().to_string())
            .sender(Party::fleet("nudge"))
            .expectation(flotilla_messaging::IntentExpectation::Outcome { condition })
            .references(vec![reference])
            .build();
        Ok(self.subscriptions.inner.turn_delivery.lock().await.clone().deliver(&request).await.map(|_| ()))
    }

    /// Escalates a stall along its supervision policy from rung `start`, recording the rung
    /// reached on `condition`.
    #[allow(clippy::too_many_arguments)]
    async fn escalate_stall(
        &self,
        tick: StallTick<'_>,
        convoy: &ResourceObject<Convoy>,
        condition: &mut StalledCondition,
        prior: Option<&StalledCondition>,
        policy: &[SupervisionTarget],
        start: usize,
        context_key: SupervisorContextKey,
        context: String,
    ) -> Result<(), String> {
        let StallTick { namespace, .. } = tick;
        let available_convoys = &tick.inputs.available_convoys;
        self.subscriptions.inner.supervisor_context.lock().await.remove(&context_key);
        condition.rung = StallRung::Operator;
        condition.supervisor = None;
        // The cursor records consumed rungs, not failed delivery attempts.
        // Retain it so retrying a higher rung cannot route back to a lower one.
        // Persist the last consumed rung, not the next attempt.
        condition.supervision_index = prior.and_then(|stalled| stalled.supervision_index);
        condition.maker = condition.leaves.first().and_then(|leaf| {
            let LeafAddress::Work { work, .. } = &leaf.address else { return None };
            let role = leaf.field_path.strip_prefix(".crew.")?.strip_suffix(".phase")?;
            Some(LeafMaker::Actor { vessel: work.clone(), role: role.to_string() })
        });
        condition.supervision_exhausted = true;
        let mut unavailable_target = None;
        let mut delivery_failed = false;
        let mut operator_delivered = false;
        for (index, target) in policy.iter().enumerate().skip(start) {
            let candidate =
                super::supervision::resolve_target(&tick.inputs.receivers, convoy, &mut *condition, target, &available_convoys.items)?;
            if let Some((target_convoy, target_vessel, target_role, rung)) = candidate {
                if target_convoy == convoy.metadata.name
                    && stalled_source_actor(&*condition).is_some_and(|(vessel, role)| vessel == target_vessel && role == target_role)
                {
                    unavailable_target = Some(target);
                    continue;
                }
                let from = stalled_source_actor(&*condition)
                    .map(|(vessel, role)| format!("{role}@{vessel} in {}", convoy_message_address(convoy)))
                    .unwrap_or_else(|| convoy_message_address(convoy));
                let brief = format!("Escalated from {from}:\n\n{}", stall_supervision_brief(convoy, &*condition));
                let receiver = match target {
                    SupervisionTarget::Address { address } => Party::of_policy_address(address).map(Some),
                    _ => Ok(None),
                };
                // An address or reference that names nothing fails this delivery like any other.
                let delivery = receiver.and_then(|receiver| Ok((receiver, convoy_reference(convoy)?)));
                let delivery = delivery.map(|(receiver, convoy_reference)| {
                    CrewTurnIntent::builder()
                        .maybe_receiver(receiver)
                        .namespace(namespace.to_string())
                        .convoy(target_convoy.clone())
                        .source(supervision_message_source(&convoy.metadata.name, condition.seat(), index))
                        .vessel(target_vessel.clone())
                        .role(target_role.clone())
                        .brief(brief)
                        .subject_revision(condition.began_at.timestamp_micros().to_string())
                        .sender(
                            stalled_source_actor(&*condition)
                                .map(|(_, role)| Party::Seat {
                                    project: convoy.spec.project_ref.clone(),
                                    convoy: convoy.metadata.name.clone(),
                                    role: role.to_owned(),
                                })
                                .unwrap_or_else(|| Party::fleet("stall-judge")),
                        )
                        .relation(flotilla_resources::MessageRelation::Supervisor)
                        .expectation(supervision_expectation(rung))
                        .references(vec![convoy_reference])
                        .maybe_stall_of((rung != StallRung::Operator).then(|| stall_of(convoy, &*condition)))
                        .build()
                });
                let actuator = self.subscriptions.inner.turn_delivery.lock().await.clone();
                let delivered = match delivery {
                    Ok(delivery) => actuator.deliver(&delivery).await,
                    Err(error) => Err(error),
                };
                let admission = match delivered {
                    Ok(admission) => admission,
                    Err(error) => {
                        let fell_back = prior.is_none_or(|prior| {
                            prior.rung != StallRung::Operator || !prior.evidence.ends_with(&format!("supervisor delivery failed: {error}"))
                        });
                        condition.evidence.push_str(&format!("; supervisor delivery failed: {error}"));
                        condition.supervision_exhausted = false;
                        if fell_back {
                            self.tell_operator(namespace, convoy, &*condition).await;
                        }
                        delivery_failed = true;
                        break;
                    }
                };
                condition.supervision_message = Some(admission.message);
                condition.rung = rung;
                condition.supervision_exhausted = false;
                condition.supervision_index = Some(index);
                if rung != StallRung::Operator {
                    condition.supervisor =
                        Some(StallSupervisor { convoy: target_convoy.clone(), vessel: target_vessel.clone(), role: target_role.clone() });
                    condition.maker = Some(LeafMaker::Supervisor { convoy: target_convoy, vessel: target_vessel, role: target_role });
                } else {
                    // A named operator is the final receiver: its durable
                    // obligation consumes the ladder, so subsequent judgments
                    // must neither redeliver nor fall back to an unnamed rung.
                    condition.supervision_exhausted = true;
                    operator_delivered = true;
                }
                break;
            }
            if matches!(target, SupervisionTarget::Operator) {
                if unavailable_target.is_none() {
                    condition.supervision_index = Some(index);
                }
                break;
            }
            unavailable_target = Some(target);
        }
        if condition.supervisor.is_none() && !delivery_failed && !operator_delivered {
            if prior.is_none_or(|prior| {
                prior.rung != condition.rung || prior.supervisor != condition.supervisor || prior.evidence != condition.evidence
            }) {
                self.tell_operator(namespace, convoy, &*condition).await;
            }
            if unavailable_target.is_some() {
                self.subscriptions.inner.supervisor_context.lock().await.insert(context_key, context);
                // Operator attention is a backstop while supervisors reconnect,
                // not a consumed ladder. Try the same unconsumed rungs next pass.
                condition.supervision_exhausted = false;
            } else {
                // An empty or fully consumed policy ends at the operator.
                condition.supervision_index = Some(policy.len());
            }
        }
        Ok(())
    }

    /// Tells a person that `condition`'s seat stalled with no one named to supervise it: one
    /// attention per stalled convoy role, sent to the convoy's dispatching principal and withdrawn
    /// when the stall ends. A stall about no seat, or one no person can be told of, stays the
    /// home's note.
    async fn tell_operator(&self, namespace: &str, convoy: &ResourceObject<Convoy>, condition: &StalledCondition) {
        let Some((_, role)) = stalled_source_actor(condition) else {
            tracing::warn!(convoy = %convoy.metadata.name, evidence = %condition.evidence, "a stall about no seat reached the operator");
            return;
        };
        let fact = Fact::Stalled { convoy: convoy.metadata.name.clone(), role: role.to_owned() };
        let attention = Attention::new(fact, stall_supervision_brief(convoy, condition), condition.began_at);
        match self.subscriptions.inner.backend.attentions().raise(namespace, &attention).await {
            Ok(()) => {
                let key = (namespace.to_owned(), convoy.metadata.name.clone());
                self.subscriptions.inner.operator_told.lock().await.seats.entry(key).or_default().insert(role.to_owned());
            }
            Err(error) => {
                tracing::warn!(convoy = %convoy.metadata.name, %role, %error, "a stall at the operator could not be told to a person")
            }
        }
    }

    /// Notes again, once per namespace after a start, each seat an open stall-judge attention
    /// names: the home's notes are memory, and the attention is the stall's durable record. The
    /// seat reads Stalled at the operator rung with the attention's text as its report, its ladder
    /// consumed, until a new turn closes it. Returns the launches noted, whose snapshot this tick
    /// predates the note.
    async fn reseed_operator_stalls(&self, tick: StallTick<'_>, convoys: &HashMap<String, impl ConvoyRead>) -> HashSet<String> {
        let StallTick { namespace, .. } = tick;
        let mut reseeded = HashSet::new();
        if self.subscriptions.inner.operator_told.lock().await.reseeded.contains(namespace) {
            return reseeded;
        }
        let backend = &self.subscriptions.inner.backend;
        let open = match backend.attentions().open(namespace, Some(Sender::StallJudge), None).await {
            Ok(open) => open,
            Err(error) => {
                tracing::warn!(namespace, %error, "told stalls could not be read; reading them again after a while");
                tick.retry();
                return reseeded;
            }
        };
        for attention in open {
            // Only a launch judged here is this home's to note and, later, to withdraw.
            let Subject::ConvoyRole { convoy: name, role } = attention.subject else { continue };
            let local =
                tick.inputs.available_convoys.items.iter().any(|current| {
                    current.provenance == flotilla_resources::ResourceProvenance::Local && current.object.metadata.name == name
                });
            let Some(convoy) = convoys.get(&name).map(ConvoyRead::convoy).filter(|_| local) else { continue };
            self.subscriptions
                .inner
                .operator_told
                .lock()
                .await
                .seats
                .entry((namespace.to_owned(), name.clone()))
                .or_default()
                .insert(role.clone());
            let Some(status) = convoy.status.as_ref() else { continue };
            let works: Vec<_> = status.crew_work.iter().filter(|(_, roles)| roles.contains_key(&role)).map(|(work, _)| work).collect();
            let [work] = works.as_slice() else {
                tracing::warn!(namespace, convoy = %name, %role, works = works.len(), "a told stall names no single seat; not noted again");
                continue;
            };
            // A seat already noted keeps its note.
            if status.stalls.about(Some((work.as_str(), role.as_str()))).is_some() {
                continue;
            }
            let projects = &tick.inputs.projects.items;
            let policy = match super::supervision::supervision_policy(tick.inputs.routes(), convoy, projects, None) {
                Ok(policy) => policy,
                Err(error) => {
                    tracing::warn!(namespace, convoy = %name, %error, "a told stall's supervision could not be read; not noted again");
                    continue;
                }
            };
            let condition = StalledCondition {
                leaves: vec![Leaf {
                    address: LeafAddress::Work { convoy: name.clone(), work: (*work).clone() },
                    field_path: format!(".crew.{role}.phase"),
                    operator: LeafOperator::Equal,
                    literal: "Done".into(),
                }],
                maker: Some(LeafMaker::Actor { vessel: (*work).clone(), role: role.clone() }),
                evidence: attention.reason,
                source: StallEvidenceSource::Crew,
                cause: None,
                began_at: attention.raised_at,
                rung: StallRung::Operator,
                supervisor: None,
                supervision_message: None,
                supervision_index: Some(policy.len()),
                supervision_exhausted: true,
                reason: None,
                proposed_disposition: None,
                nudge_history: Vec::new(),
            };
            let noted = flotilla_store::note_convoy(
                &self.subscriptions.inner.convoy_engine,
                &backend.clone().using::<Convoy>(namespace),
                &name,
                &flotilla_resources::ConvoyStatusPatch::SetStall { condition },
            )
            .await;
            match noted {
                Ok(_) => {
                    tracing::info!(namespace, convoy = %name, %role, "a told stall noted again after a start");
                    reseeded.insert(name);
                }
                Err(error) => tracing::warn!(namespace, convoy = %name, %error, "a told stall could not be noted again"),
            }
        }
        self.subscriptions.inner.operator_told.lock().await.reseeded.insert(namespace.to_owned());
        reseeded
    }

    /// Writes the convoy's nudge obligations and stalls to its home's notes; the engine's stall
    /// records follow them. Only the seats whose stall changed are written, so a stall noted
    /// meanwhile about another seat (a crew's declaration) stays. The records are settled on the
    /// change that settles a stall, and once on a convoy's first judgement.
    async fn write_stall_status(
        &self,
        tick: StallTick<'_>,
        convoy: &ResourceObject<Convoy>,
        status: &ConvoyStatus,
        obligations: Vec<NudgeObligation>,
        next: Stalls,
    ) -> Result<(), String> {
        let StallTick { namespace, .. } = tick;
        let backend = &self.subscriptions.inner.backend;
        if status.nudge_obligations != obligations {
            flotilla_store::note_convoy(
                &self.subscriptions.inner.convoy_engine,
                &backend.clone().using::<Convoy>(namespace),
                &convoy.metadata.name,
                &flotilla_resources::ConvoyStatusPatch::SetNudgeObligations { obligations },
            )
            .await
            .map_err(|error| error.to_string())?;
        }
        // The engine's stall records follow the notes: only once they are written.
        let ended = status
            .stalls
            .iter()
            .filter(|held| next.about(held.seat()).is_none())
            .map(|held| flotilla_resources::ConvoyStatusPatch::EndStall { seat: owned_seat(held.seat()) });
        let set = next
            .iter()
            .filter(|stall| status.stalls.about(stall.seat()) != Some(*stall))
            .map(|stall| flotilla_resources::ConvoyStatusPatch::SetStall { condition: stall.clone() });
        for patch in ended.chain(set).collect::<Vec<_>>() {
            let written = flotilla_store::note_convoy(
                &self.subscriptions.inner.convoy_engine,
                &backend.clone().using::<Convoy>(namespace),
                &convoy.metadata.name,
                &patch,
            )
            .await;
            if let Err(error) = written {
                tracing::warn!(namespace, convoy = %convoy.metadata.name, %error, "write stalled condition failed");
                tick.retry();
                return Ok(());
            }
        }
        if status.stalls != next {
            self.settle_operator_attention(tick, &convoy.metadata.name, &next).await;
        }
        if status.stalls != next || tick.first {
            self.end_settled_stalls(tick, convoy, status, &next).await;
        }
        Ok(())
    }

    /// Withdraws each attention [`Self::tell_operator`] raised for launch `name`'s seats once
    /// `current` no longer holds that seat at the operator: the stall closed, a supervisor took
    /// it, or the launch ended. One that could not be withdrawn is tried again after a while.
    async fn settle_operator_attention(&self, tick: StallTick<'_>, name: &str, current: &Stalls) {
        let StallTick { namespace, .. } = tick;
        let key = (namespace.to_owned(), name.to_owned());
        let settled: Vec<String> = match self.subscriptions.inner.operator_told.lock().await.seats.get(&key) {
            Some(roles) => {
                roles.iter().filter(|role| !current.iter().any(|stall| at_operator(stall) == Some(role.as_str()))).cloned().collect()
            }
            None => return,
        };
        for role in settled {
            let subject = Subject::ConvoyRole { convoy: name.to_owned(), role: role.clone() };
            match self.subscriptions.inner.backend.attentions().clear(namespace, Sender::StallJudge, &subject).await {
                Ok(()) => {
                    let mut told = self.subscriptions.inner.operator_told.lock().await;
                    if let Some(roles) = told.seats.get_mut(&key) {
                        roles.remove(&role);
                        if roles.is_empty() {
                            told.seats.remove(&key);
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(convoy = %name, %role, %error, "a closed stall's attention could not be withdrawn");
                    tick.retry();
                }
            }
        }
    }
}
