//! The stall judge runs on the changes that move a judgement and at the instants one turns on
//! time, never on a timer (#3108): driven through the leaf engine's own loop under paused time.

use super::*;
use crate::leaf_engine::stalls::TURN_INACTIVITY_BOUND;
use flotilla_messaging::{AdmittedMessage, EngineMessages, InboxChange, MessageResolution, SeatMessage, SentMessage, StallOf};
use flotilla_resources::Clock as _;
use flotilla_store::controller::WorkQueueSender;
use flotilla_store_testkit::PausedClock;

/// The Message path, counting each call into it by name: every one reads or writes the store.
struct CountedMessages {
    inner: Arc<dyn EngineMessages>,
    calls: std::sync::Mutex<BTreeMap<&'static str, usize>>,
    /// The seats each settling of stalls was asked about.
    settled: std::sync::Mutex<Vec<Vec<String>>>,
    /// The seats each read of seats' messages asked for.
    standings: std::sync::Mutex<Vec<Vec<String>>>,
}

impl CountedMessages {
    fn over(inner: Arc<dyn EngineMessages>) -> Arc<Self> {
        Arc::new(Self { inner, calls: Default::default(), settled: Default::default(), standings: Default::default() })
    }

    fn count(&self, call: &'static str) {
        *self.calls.lock().expect("calls").entry(call).or_default() += 1;
    }

    fn calls(&self) -> BTreeMap<&'static str, usize> {
        self.calls.lock().expect("calls").clone()
    }

    fn path(self: &Arc<Self>) -> crate::ports::MessagePath {
        let path = crate::ports::MessagePath::default();
        let _ = path.set(Arc::clone(self) as Arc<dyn EngineMessages>);
        path
    }
}

#[async_trait]
impl EngineMessages for CountedMessages {
    async fn holds(&self, inbox: &flotilla_address::Address, record: &str) -> Result<bool, ResourceError> {
        self.count("holds");
        self.inner.holds(inbox, record).await
    }
    async fn fleet_role(&self, namespace: &str, role: &str) -> Result<flotilla_address::Address, ResourceError> {
        self.count("fleet_role");
        self.inner.fleet_role(namespace, role).await
    }
    async fn person(&self, namespace: &str, principal: &str, project: Option<&str>) -> Result<flotilla_address::Address, ResourceError> {
        self.count("person");
        self.inner.person(namespace, principal, project).await
    }
    async fn admitted(&self, inbox: &flotilla_address::Address, record: &str) -> Result<Option<AdmittedMessage>, ResourceError> {
        self.count("admitted");
        self.inner.admitted(inbox, record).await
    }
    async fn open_superseded(&self, intent: &flotilla_messaging::Intent) -> Result<Option<String>, ResourceError> {
        self.count("open_superseded");
        self.inner.open_superseded(intent).await
    }
    async fn undelivered_from(&self, intent: &flotilla_messaging::Intent) -> Result<Option<SentMessage>, ResourceError> {
        self.count("undelivered_from");
        self.inner.undelivered_from(intent).await
    }
    async fn awaited_reply_from(
        &self,
        sender: &flotilla_address::Address,
        inbox: &flotilla_address::Address,
        about: &[flotilla_address::Address],
    ) -> Result<Option<String>, ResourceError> {
        self.count("awaited_reply_from");
        self.inner.awaited_reply_from(sender, inbox, about).await
    }
    async fn queued_briefs(&self, namespace: &str, seats: &[String]) -> Result<Vec<SentMessage>, ResourceError> {
        self.count("queued_briefs");
        self.inner.queued_briefs(namespace, seats).await
    }
    async fn seat_standings(
        &self,
        namespace: &str,
        receivers: &[String],
        named: &[(String, String)],
    ) -> Result<Vec<SeatMessage>, ResourceError> {
        self.count("seat_standings");
        self.standings.lock().expect("standings").push(receivers.to_vec());
        self.inner.seat_standings(namespace, receivers, named).await
    }
    async fn sent_messages(&self, namespace: &str, named: &[(String, String)]) -> Result<Vec<SentMessage>, ResourceError> {
        self.count("sent_messages");
        self.inner.sent_messages(namespace, named).await
    }
    async fn admit(&self, name: &str, intent: &flotilla_messaging::Intent) -> Result<flotilla_messaging::Admitted, ResourceError> {
        self.count("admit");
        self.inner.admit(name, intent).await
    }
    async fn resolve_in_reply_to(&self, intent: &flotilla_messaging::Intent) -> MessageResolution {
        self.count("resolve_in_reply_to");
        self.inner.resolve_in_reply_to(intent).await
    }
    async fn withdraw(&self, namespace: &str, receiver: &str, record: &str, convoy: &str) -> Result<(), String> {
        self.count("withdraw");
        self.inner.withdraw(namespace, receiver, record, convoy).await
    }
    fn home(&self) -> String {
        self.inner.home()
    }
    async fn await_reply(
        &self,
        namespace: &str,
        name: &str,
        intent: &flotilla_messaging::Intent,
        home: &str,
        kind: &str,
        stall: Option<&StallOf>,
    ) -> Result<bool, String> {
        self.count("await_reply");
        self.inner.await_reply(namespace, name, intent, home, kind, stall).await
    }
    async fn delivery_wait(&self, namespace: &str) -> Result<Option<String>, String> {
        self.count("delivery_wait");
        self.inner.delivery_wait(namespace).await
    }
    async fn end_stalls(&self, namespace: &str, seats: &[String], stalled: &[String], reason: &str) -> Result<Vec<String>, String> {
        self.count("end_stalls");
        self.settled.lock().expect("settled").push(seats.to_vec());
        self.inner.end_stalls(namespace, seats, stalled, reason).await
    }
    async fn delivery_held_until(&self, namespace: &str, receiver: &str) -> Result<Option<DateTime<Utc>>, String> {
        self.count("delivery_held_until");
        self.inner.delivery_held_until(namespace, receiver).await
    }
    fn changes(&self) -> Result<futures::stream::BoxStream<'static, ()>, ResourceError> {
        self.inner.changes()
    }
    fn inbox_changes(&self) -> Result<futures::stream::BoxStream<'static, InboxChange>, ResourceError> {
        self.inner.inbox_changes()
    }
    fn charter_notices(self: Arc<Self>, namespace: &str) -> Box<dyn flotilla_messaging::CharterNotices> {
        self.count("charter_notices");
        Arc::clone(&self.inner).charter_notices(namespace)
    }
}

/// A leaf table at `home` on paused time, reading messages through `messages` and delivering
/// turns to `delivery`; and its clock.
async fn paused_wake(
    backend: &ResourceBackend,
    home: &EngineHome,
    messages: crate::ports::MessagePath,
    delivery: Arc<RecordingTurnDelivery>,
) -> (ReconcilerWake, Arc<PausedClock>) {
    let clock = Arc::new(PausedClock::start());
    let (event_tx, _) = broadcast::channel(16);
    let refresher = ChangeRequestRefresher::new(
        backend.clone(),
        "test-host".into(),
        Arc::new(UnavailableChangeRequests),
        flotilla_observation::change_request_observer::ChangeRequestRefreshCadence::default(),
    );
    let table = LeafSubscriptionTable::with_episode_limit(backend.clone(), broadcast_test_sink(event_tx), refresher, home.reads(), 3)
        .with_message_path(messages)
        .with_convoy_engine(home.path())
        .with_clock(Arc::clone(&clock) as Arc<dyn flotilla_resources::Clock>);
    table.set_turn_delivery_actuator(delivery).await;
    (ReconcilerWake::new(table), clock)
}

/// Runs `wake`'s loop in namespace `flotilla`, as its Convoy controller does.
fn run(wake: &ReconcilerWake) -> tokio::task::JoinHandle<Result<(), String>> {
    let wake = wake.clone();
    let (queue, _receiver) = WorkQueueSender::channel();
    tokio::spawn(async move {
        let _receiver = _receiver;
        wake.run("flotilla".into(), queue).await
    })
}

/// Lets the loop run `seconds` of paused time, in steps of a second.
async fn elapse(seconds: u64, loop_task: &tokio::task::JoinHandle<Result<(), String>>) {
    for _ in 0..seconds {
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(!loop_task.is_finished(), "the judge's loop stopped");
    }
}

// An idle home whose launches have ended reads no messages: an ended launch is judged once, on
// the change that ended it, and never on a timer.
#[tokio::test(start_paused = true)]
async fn an_idle_judge_reads_no_messages_for_ended_launches() {
    const ENDED: usize = 4;
    let (backend, home) = engine_backend().await;
    for index in 0..ENDED {
        let launch = create_convoy(&home, &format!("ended-{index}"), single_agent("coder")).await;
        home.keep(&backend, &launch, "work", "coder").await;
        home.end(&backend, &launch, ConvoyPhase::Landed).await;
        assert_eq!(home.view(&backend, &launch).await.status.expect("status").phase, ConvoyPhase::Landed);
    }
    let messages = CountedMessages::over(home.message_path().get().cloned().expect("the Message path"));
    let (wake, _clock) = paused_wake(&backend, &home, messages.path(), Arc::default()).await;
    let loop_task = run(&wake);
    // The first pass settles each ended launch's stall records, once.
    tokio::time::timeout(Duration::from_secs(30), async {
        while messages.calls().get("end_stalls").copied().unwrap_or_default() < ENDED {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("the first pass settles every ended launch");
    elapse(2, &loop_task).await;
    let settled = messages.calls();
    assert_eq!(settled.get("end_stalls"), Some(&ENDED), "{settled:?}");
    elapse(600, &loop_task).await;
    assert_eq!(messages.calls(), settled, "an idle judge read messages over ten minutes");
}

/// The idle-nudge scenario, its coder's session running and the judge's loop running on paused
/// time, reading messages through the returned counter; the coder's agent goes idle, by its
/// hook, now.
async fn idle_coder_on_the_loop() -> (Case, Arc<CountedMessages>, tokio::task::JoinHandle<Result<(), String>>) {
    let case = idle_nudge_scenario().await;
    let messages = CountedMessages::over(case.home.message_path().get().cloned().expect("the Message path"));
    let (wake, clock) = paused_wake(&case.backend, &case.home, messages.path(), case.delivery.clone()).await;
    let loop_task = run(&wake);
    elapse(1, &loop_task).await;
    observe_agent(&case.home, "resumed-coder", TerminalAttentionState::Idle, TerminalAttentionSource::Hook, clock.now());
    elapse(1, &loop_task).await;
    (case, messages, loop_task)
}

fn nudges(case: &Case) -> usize {
    case.delivery.requests.lock().expect("deliveries").len()
}

// An idle seat is nudged at the end of its idle grace, with nothing else changing: its judgement
// was due then. Its messages, unchanged, are not read again while it waits.
#[tokio::test(start_paused = true)]
async fn a_live_seat_stalls_at_its_due_time() {
    let (case, messages, loop_task) = idle_coder_on_the_loop().await;
    let read = messages.calls().get("seat_standings").copied();
    assert!(read.is_some(), "the seat's messages were never read");
    let grace = u64::from(DEFAULT_IDLE_GRACE_SECONDS);
    elapse(grace - 5, &loop_task).await;
    assert_eq!(nudges(&case), 0, "nudged before its idle grace ran out");
    assert_eq!(messages.calls().get("seat_standings").copied(), read, "unchanged messages were read again");
    elapse(10, &loop_task).await;
    assert_eq!(nudges(&case), 1, "not nudged once its idle grace ran out");
    assert_eq!(case.status().await.nudge_obligations[0].history.len(), 1);
    assert_eq!(messages.calls().get("seat_standings").copied(), read, "the due judgement read unchanged messages: {:?}", messages.calls());
}

// A message waiting for an idle, nudged seat settles its stall as soon as it is admitted: the
// seat's messages are read again on the change, not at its next due time.
#[tokio::test(start_paused = true)]
async fn a_new_message_unstalls_a_seat_without_waiting() {
    let (case, _messages, loop_task) = idle_coder_on_the_loop().await;
    // Nudged, and still idle past its backoff, the seat stalls.
    for _ in 0..1200 {
        if case.status().await.stalled.is_some() {
            break;
        }
        elapse(1, &loop_task).await;
    }
    assert_eq!(case.status().await.stalled.map(|stall| stall.rung), Some(StallRung::Nudge), "precondition: the idle seat stalled");
    message_to_coder(&case, "operator-note", "Carry on with the migration.").await;
    // Its next judgement on time is a whole backoff away; the message is judged well before.
    for _ in 0..10 {
        if case.status().await.stalled.is_none() {
            return;
        }
        elapse(1, &loop_task).await;
    }
    panic!("a waiting message did not settle the stall: {:?}", case.status().await.stalled);
}

// A launch whose status comes to name a turn reads its seats' messages again, though no message
// changed: the read asks for that turn.
#[tokio::test(start_paused = true)]
async fn a_newly_named_turn_is_read() {
    let (case, messages, loop_task) = idle_coder_on_the_loop().await;
    let read = messages.calls().get("seat_standings").copied().expect("the seat's messages were read");
    case.note(|status| {
        status.crew_work.get_mut("work").and_then(|crew| crew.get_mut("coder")).expect("the coder's seat").resume_brief_id =
            Some("resume-brief".into());
    })
    .await;
    elapse(1, &loop_task).await;
    assert_eq!(messages.calls().get("seat_standings").copied(), Some(read + 1), "the newly named turn was not read");
}

// A change feed that lagged past a notice judges every live launch once, the ended ones passed
// by, and the judge goes back to waking on changes and due times: a missed notice cannot leave a
// seat unjudged.
#[tokio::test(start_paused = true)]
async fn a_lagged_feed_judges_every_live_launch_once() {
    let case = idle_nudge_scenario().await;
    for index in 0..2 {
        let launch = create_convoy(&case.home, &format!("ended-{index}"), single_agent("coder")).await;
        case.home.keep(&case.backend, &launch, "work", "coder").await;
        case.home.end(&case.backend, &launch, ConvoyPhase::Landed).await;
    }
    let messages = CountedMessages::over(case.home.message_path().get().cloned().expect("the Message path"));
    let (wake, clock) = paused_wake(&case.backend, &case.home, messages.path(), case.delivery.clone()).await;
    let loop_task = run(&wake);
    elapse(1, &loop_task).await;
    let ended_settled = || {
        let settled = messages.settled.lock().expect("settled");
        settled.iter().filter(|seats| seats.iter().any(|seat| seat.contains("ended-"))).count()
    };
    assert_eq!(ended_settled(), 2, "the first pass settles each ended launch");
    // The coder goes idle, and its notice is lost among more than the feed holds.
    observe_agent(&case.home, "resumed-coder", TerminalAttentionState::Idle, TerminalAttentionSource::Hook, clock.now());
    for index in 0..100 {
        agents().observe(&format!("elsewhere-{index}"), flotilla_protocol::AgentActivity::Working, "hook", clock.now());
    }
    elapse(u64::from(DEFAULT_IDLE_GRACE_SECONDS) + 5, &loop_task).await;
    assert_eq!(nudges(&case), 1, "the idle seat whose notice was lost was never judged");
    assert_eq!(ended_settled(), 2, "the fallback judged the ended launches again");
    // Back on changes and due times: nothing is judged before the next nudge is due.
    let calls = messages.calls();
    elapse(60, &loop_task).await;
    assert_eq!(messages.calls(), calls, "the judge kept judging after the fallback");
}

// A change to one crew member's session reads only that session again, and a message to one
// seat reads only that seat's messages: the launch's other seat is judged from what is held of it.
#[tokio::test(start_paused = true)]
async fn a_seat_change_reads_only_that_seat() {
    let mut workflow = supervised_workflow();
    let mut reviewer = workflow.vessels[0].crew[0].clone();
    reviewer.role = "reviewer".into();
    workflow.vessels[0].crew.push(reviewer);
    let case = supervised_case(workflow, &[]).await;
    case.note(|status| status.stalls.clear()).await;
    resume_coder(&case);
    case.home.session(&case.vessel, "reviewer", "reviewer-session", "env");
    let (coder, reviewer) = (case.launch("resumed-coder"), case.launch("reviewer-session"));
    let reviewer_seat = flotilla_resources::crew_role_address("wheelhouse", &case.work, "work", "reviewer");
    let messages = CountedMessages::over(case.home.message_path().get().cloned().expect("the Message path"));
    let (wake, clock) = paused_wake(&case.backend, &case.home, messages.path(), case.delivery.clone()).await;
    for session in ["resumed-coder", "reviewer-session"] {
        observe_agent(&case.home, session, TerminalAttentionState::Working, TerminalAttentionSource::Hook, clock.now());
    }
    let agents = Arc::clone(&case.home.agents);
    let loop_task = run(&wake);
    elapse(2, &loop_task).await;
    let reads = (agents.reads(&coder), agents.reads(&reviewer));
    assert!(reads.0 > 0 && reads.1 > 0, "the seats' sessions were never read: {reads:?}");
    let standings = || messages.standings.lock().expect("standings").clone();
    let read = standings();
    assert!(read.iter().any(|seats| seats.contains(&reviewer_seat)), "the seats' messages were never read: {read:?}");

    observe_agent(&case.home, "resumed-coder", TerminalAttentionState::Idle, TerminalAttentionSource::Hook, clock.now());
    elapse(1, &loop_task).await;
    assert!(agents.reads(&coder) > reads.0, "the changed session was not read again");
    assert_eq!(agents.reads(&reviewer), reads.1, "the unchanged session was read again");
    assert_eq!(standings(), read, "messages none of which changed were read again");

    let reads = (agents.reads(&coder), agents.reads(&reviewer));
    let inbox = flotilla_resources::convoy::engine::role_inbox(&reviewer_seat).expect("the reviewer's inbox");
    let operator = flotilla_engine_messages_testkit::party("project:wheelhouse/role:operator");
    let mut spec = flotilla_messaging::Intent::new(operator, inbox, flotilla_resources::MessageRelation::Supervisor, "Review the plan.");
    spec.principal = Some("implicit".into());
    case.home.messages.primitives().admit("note-to-reviewer", &spec).await.expect("message accepted");
    elapse(1, &loop_task).await;
    let after = standings();
    assert_eq!(after.len(), read.len() + 1, "the seat's new message was not read: {after:?}");
    assert_eq!(after.last().expect("a read"), &vec![reviewer_seat], "seats whose messages did not change were read");
    assert_eq!((agents.reads(&coder), agents.reads(&reviewer)), reads, "a message read the sessions again");
}

// A working turn's tool calls, which only this daemon's memory holds, are read when its
// inactivity bound comes due: a turn that kept calling tools is not stalled then, and is once it
// has been silent for the bound since.
#[tokio::test(start_paused = true)]
async fn a_turn_bound_reads_its_session_again_when_due() {
    let case = idle_nudge_scenario().await;
    let messages = CountedMessages::over(case.home.message_path().get().cloned().expect("the Message path"));
    let (wake, clock) = paused_wake(&case.backend, &case.home, messages.path(), case.delivery.clone()).await;
    observe_agent(&case.home, "resumed-coder", TerminalAttentionState::Working, TerminalAttentionSource::Hook, clock.now());
    let loop_task = run(&wake);
    elapse(2, &loop_task).await;
    let half = (TURN_INACTIVITY_BOUND.num_seconds() / 2) as u64;
    elapse(half, &loop_task).await;
    case.home.agents.progress.tool("resumed-coder", clock.now());
    let inactive = || async { case.status().await.stalls.iter().any(|stall| stall.evidence.starts_with("turn inactivity bound exceeded")) };
    elapse(half + 5, &loop_task).await;
    assert!(!inactive().await, "a turn that called a tool was judged silent: {:?}", case.status().await.stalls);
    elapse(half + 5, &loop_task).await;
    assert!(inactive().await, "a turn silent for the bound was not judged so: {:?}", case.status().await.stalls);
}

// A supervisor at work on another home is judged from that home's reading while the home is
// heard from. With nothing else changing, its reading is judged again the moment it stops being
// current, and again when the home is heard from once more.
#[tokio::test(start_paused = true)]
async fn another_homes_reading_is_judged_when_it_goes_stale_and_when_its_home_is_heard() {
    let case = project_supervision_case(&[]).await;
    // The governor is another daemon's, its log heard here.
    let remote_backend = ResourceBackend::new(InMemoryBackend::default());
    let remote = EngineHome::install(&remote_backend, "flotilla", "remote");
    super::stalls_supervision::governor_at(&remote, "governor", Some("wheelhouse-governor"), 1).await;
    flotilla_provisioning_testkit::replicate_home(remote.service(), "remote", case.home.service());
    let governor = remote.engine.terminals.record("governor-1-governor").name;
    let (wake, clock) = paused_wake(&case.backend, &case.home, case.home.message_path(), case.delivery.clone()).await;
    let agents = Arc::clone(&case.home.agents);
    agents.observe(&governor, flotilla_protocol::AgentActivity::Working, "hook", clock.now());
    let heard_at = clock.now();
    agents.heard("remote", heard_at, Arc::clone(&clock) as Arc<dyn flotilla_resources::Clock>);
    let loop_task = run(&wake);
    elapse(2, &loop_task).await;
    let supervisor_row = wake
        .subscriptions
        .rows()
        .await
        .into_iter()
        .find(|row| matches!(row.maker, LeafMaker::Supervisor { .. }))
        .expect("the stall is supervised by the remote governor")
        .id;
    let reported = || async { wake.subscriptions.inner.stale_attention_reported.lock().await.contains(&supervisor_row) };
    assert!(!reported().await, "a current reading was reported stale");
    let stale_in = flotilla_resources::agent_activity::HOME_HEARD_WITHIN.num_seconds() as u64;
    elapse(stale_in - 4, &loop_task).await;
    assert!(!reported().await, "the reading was reported stale before its home fell silent");
    elapse(4, &loop_task).await;
    assert!(reported().await, "the reading was not judged when its home fell silent");
    agents.heard("remote", clock.now(), Arc::clone(&clock) as Arc<dyn flotilla_resources::Clock>);
    elapse(1, &loop_task).await;
    assert!(!reported().await, "the reading was not judged again when its home was heard from");
}
