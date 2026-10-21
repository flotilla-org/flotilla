//! AgentSession against `specs/primitives/machines/agent_session.qnt`.

use flotilla_schema::{Code, CodeEnum, Timestamp};
use flotilla_state_machines::{
    machines::agent_session::{
        Activity, ActivityObservation, AgentSession, AgentSessionEdgeKind, AgentSessionLabel, AgentSessionRefusal, AgentSessionState,
        Attach, Channel, ChannelPresence, Detach, End, EndReason, MemoryAgentSessionWorld, Observe, Receive, SessionEnd,
    },
    Coded, Diagram, Home, InputRead, Machine, Ref, Transport, Version, WriteError,
};

fn at(micros: i64) -> Timestamp {
    Timestamp::from_micros(micros)
}

fn home() -> Home<AgentSession> {
    let mut home = Home::<AgentSession>::new("kiwi").expect("valid declaration");
    home.create("s", (), &MemoryAgentSessionWorld::default(), Timestamp(0)).expect("created");
    home
}

fn end(reason: EndReason) -> End {
    End { reason, observed: Ref::new("s") }
}

fn attach() -> Attach {
    Attach { observed: Ref::new("s") }
}

fn detach() -> Detach {
    Detach { observed: Ref::new("s") }
}

fn channel(present: Option<bool>) -> MemoryAgentSessionWorld {
    let mut world = MemoryAgentSessionWorld::default();
    if let Some(present) = present {
        world.channels.put(&Ref::<ChannelPresence>::new("s"), present);
    }
    world
}

fn ended_with(reason: Option<EndReason>) -> MemoryAgentSessionWorld {
    let mut world = MemoryAgentSessionWorld::default();
    if let Some(reason) = reason {
        world.ends.put(&Ref::<SessionEnd>::new("s"), reason);
    }
    world
}

fn refused(refusal: AgentSessionRefusal) -> Option<WriteError<AgentSession>> {
    Some(WriteError::Refused(refusal))
}

fn state(home: &Home<AgentSession>) -> &AgentSessionState {
    home.get("s").expect("exists").state()
}

fn live(channel: Channel) -> AgentSessionState {
    AgentSessionState::live(channel)
}

// The declaration obeys the engine's rules.
#[test]
fn the_declaration_validates() {
    assert_eq!(AgentSession::definition().validate(), Ok(()));
}

// Codes are stable identities: a rename must never change them.
#[test]
fn codes_and_names_are_pinned() {
    let labels: Vec<_> = AgentSessionLabel::ALL.iter().map(|s| (s.name(), s.code())).collect();
    assert_eq!(labels, [("Live", Code::new("li")), ("Ended", Code::new("en"))]);
    let edges: Vec<_> = AgentSessionEdgeKind::ALL.iter().map(|k| (k.name(), k.code())).collect();
    let codes = [("attach", "at"), ("detach", "de"), ("end", "en"), ("receive", "rv"), ("observe", "ob")];
    assert_eq!(edges, codes.map(|(name, code)| (name, Code::new(code))));
}

// The diagram shows the ADR's table: the channel moving within Live, a
// receipt keeping Live, and Live → Ended with its reason; the labels view
// keeps only the end.
#[test]
fn mermaid_is_pinned() {
    let head = "---\ntitle: AgentSession\n---\nstateDiagram-v2\n  direction LR\n  [*] --> Live\n";
    let channel = "  Live --> Live: attach [channel]\n  Live --> Live: detach [channel]\n  Live --> Live: receive\n  Live --> Live: observe [activity, activity_since, activity_cause]\n";
    let rest = "  Live --> Ended: end [reason]\n  Ended --> [*]\n  note right of Ended: the session has ended\n";
    let definition = AgentSession::definition();
    assert_eq!(definition.mermaid(Diagram::Full), format!("{head}{channel}{rest}"));
    assert_eq!(definition.mermaid(Diagram::Labels), format!("{head}{rest}"));
}

// A receipt is a Receive edge (R4, G11): Live to the same Live on either
// channel, reading nothing, its event carrying the message and transport,
// and its master the message, so the message's gates find the entries about it.
#[test]
fn a_receipt_is_a_receive_edge_about_its_message() {
    let mut h = home();
    let receive = || Receive { message: "m".into(), transport: Transport::Notify };
    let event = h.apply("s", receive(), &channel(None), at(1)).expect("received while detached");
    assert_eq!((event.from, event.to), (AgentSessionLabel::Live, AgentSessionLabel::Live));
    assert!(event.payload.reads.is_empty(), "an ungated receipt reads nothing");
    assert_eq!(h.definition().master(&event.payload.edge).as_deref(), Some("m"));
    assert_eq!(h.definition().master(&attach().into()), None);
    h.apply("s", attach(), &channel(Some(true)), at(2)).expect("attached");
    h.apply("s", receive(), &channel(None), at(3)).expect("received while attached");
    assert_eq!(*state(&h), live(Channel::Attached), "a receipt keeps the channel");
    assert_eq!(h.log().history("s", Version(0), usize::MAX).len(), 3);
}

// A new session is Live and Detached; a name is created once.
#[test]
fn create_makes_a_live_detached_session_once() {
    let mut h = home();
    assert_eq!(*state(&h), live(Channel::Detached));
    assert_eq!(h.create("s", (), &MemoryAgentSessionWorld::default(), Timestamp(0)).err(), Some(WriteError::Exists("s".into())));
    assert_eq!(h.apply("other", attach(), &channel(Some(true)), at(1)).err(), Some(WriteError::Unknown("other".into())));
}

// End needs the session's end observed with the edge's reason; the Ended
// state then carries that reason.
#[test]
fn end_needs_an_observed_end_with_that_reason() {
    for reason in [EndReason::Completed, EndReason::Lost] {
        let other = if reason == EndReason::Completed { EndReason::Lost } else { EndReason::Completed };
        let mut h = home();
        assert_eq!(h.apply("s", end(reason), &ended_with(None), at(1)).err(), refused(AgentSessionRefusal::MissingEvidence));
        assert_eq!(h.apply("s", end(reason), &ended_with(Some(other)), at(1)).err(), refused(AgentSessionRefusal::MissingEvidence));
        let event = h.apply("s", end(reason), &ended_with(Some(reason)), at(2)).expect("ended");
        assert_eq!(event.payload.reads, [InputRead { kind: "session_end".into(), name: "s".into(), version: Some(Version(1)) }]);
        assert_eq!(*state(&h), AgentSessionState::ended(reason));
    }
}

// Attach needs a detached channel, then one observed present; detach needs an
// attached channel, then one observed absent (no observation is not an
// absence). Both change only the channel and keep the session Live.
#[test]
fn channel_edges_need_the_right_channel_then_an_observation() {
    let mut h = home();
    assert_eq!(h.apply("s", detach(), &channel(Some(false)), at(1)).err(), refused(AgentSessionRefusal::WrongChannel));
    assert_eq!(h.apply("s", attach(), &channel(None), at(1)).err(), refused(AgentSessionRefusal::MissingEvidence));
    assert_eq!(h.apply("s", attach(), &channel(Some(false)), at(1)).err(), refused(AgentSessionRefusal::MissingEvidence));
    let event = h.apply("s", attach(), &channel(Some(true)), at(2)).expect("attached");
    assert_eq!((event.from, event.to), (AgentSessionLabel::Live, AgentSessionLabel::Live));
    assert_eq!(*state(&h), live(Channel::Attached));
    // The wrong channel refuses before the observation is read.
    assert_eq!(h.apply("s", attach(), &channel(None), at(3)).err(), refused(AgentSessionRefusal::WrongChannel));
    assert_eq!(h.apply("s", detach(), &channel(None), at(3)).err(), refused(AgentSessionRefusal::MissingEvidence));
    assert_eq!(h.apply("s", detach(), &channel(Some(true)), at(3)).err(), refused(AgentSessionRefusal::MissingEvidence));
    h.apply("s", detach(), &channel(Some(false)), at(4)).expect("detached");
    assert_eq!(*state(&h), live(Channel::Detached));
}

// Once ended, every edge refuses AlreadyEnded before any channel check,
// receipts too.
#[test]
fn an_ended_session_refuses_everything_with_already_ended() {
    let mut h = home();
    h.apply("s", attach(), &channel(Some(true)), at(1)).expect("attached");
    let receipt = Receive { message: "m".into(), transport: Transport::Turn };
    h.apply("s", receipt.clone(), &channel(None), at(1)).expect("received while live");
    h.apply("s", end(EndReason::Lost), &ended_with(Some(EndReason::Lost)), at(2)).expect("ended");
    let already = refused(AgentSessionRefusal::AlreadyEnded);
    // Attach would be WrongChannel and detach would pass on a live session.
    assert_eq!(h.apply("s", attach(), &channel(Some(true)), at(3)).err(), already);
    assert_eq!(h.apply("s", detach(), &channel(Some(false)), at(3)).err(), already);
    assert_eq!(h.apply("s", end(EndReason::Lost), &ended_with(Some(EndReason::Lost)), at(3)).err(), already);
    assert_eq!(h.apply("s", receipt, &channel(None), at(3)).err(), already);
    assert_eq!(*state(&h), AgentSessionState::ended(EndReason::Lost));
}

// Owner ruling #3625: only changes write the observation and the home log;
// all lifecycle edges preserve it, and an ended session refuses Observe.
// Generate all three activities, repeated causes, both channel states and both ends.
#[hegel::test]
fn activity_observations_change_only_on_a_real_change(tc: hegel::TestCase) {
    use hegel::generators as gs;
    let mut h = home();
    assert_eq!(state(&h).observation(), None);
    let world = MemoryAgentSessionWorld::default();
    for tick in 1..=tc.draw(gs::integers::<i64>().min_value(4).max_value(30)) {
        let activity =
            [Activity::Working, Activity::Idle, Activity::Interrupted][tc.draw(gs::integers::<usize>().min_value(0).max_value(2))];
        let cause = format!("event-{}", tc.draw(gs::integers::<u8>().min_value(0).max_value(3)));
        let before = h.get("s").unwrap().clone();
        let result = h.apply("s", Observe { activity, cause: cause.clone() }, &world, at(tick));
        if before.state().observation().is_some_and(|old| old.activity == activity) {
            assert_eq!(result.err(), refused(AgentSessionRefusal::UnchangedActivity));
            assert_eq!(h.get("s").unwrap(), &before);
        } else {
            result.expect("activity changed");
            assert_eq!(state(&h).observation(), Some(ActivityObservation { activity, activity_since: at(tick), activity_cause: cause }));
            assert!(h.get("s").unwrap().version() > before.version());
        }
        assert_eq!(h.get("s").unwrap().since(), at(0));
    }
    let observation = state(&h).observation();
    h.apply("s", attach(), &channel(Some(true)), at(40)).unwrap();
    assert_eq!(state(&h).observation(), observation);
    h.apply("s", detach(), &channel(Some(false)), at(41)).unwrap();
    assert_eq!(state(&h).observation(), observation);
    let reason = if tc.draw(gs::booleans()) { EndReason::Lost } else { EndReason::Completed };
    h.apply("s", end(reason), &ended_with(Some(reason)), at(42)).unwrap();
    assert_eq!(state(&h).observation(), observation);
    let before = h.get("s").unwrap().clone();
    assert_eq!(
        h.apply("s", Observe { activity: Activity::Working, cause: "late".into() }, &world, at(43)).err(),
        refused(AgentSessionRefusal::AlreadyEnded)
    );
    assert_eq!(h.get("s").unwrap(), &before);
}
