//! The engine's stored kinds (#3319, #3373): each machine's row beside it, with pinned kind codes,
//! the conversion between a resource and its row, and the stored edge for an applied edge.

use std::collections::BTreeSet;

use flotilla_schema::{Decoder, EdgeParts, Encoder, Kind, KindCode, RowPath, State, Timestamp};
use flotilla_state_machines::{
    machines::{
        agent_session::{AgentSession, AgentSessionRow},
        convoy::{Convoy, ConvoyRequest, ConvoyRow, Deliver, Keep, MemoryConvoyWorld, Priority, Promise, PromiseId, VesselClass},
        ensure::{Ensure, EnsureRow, EnsureSpec, MemoryEnsureWorld, Record},
        message::{Expectation, Message, MessageRow, MessageSpec},
    },
    AttemptPolicy, Home, MachineRow, Ref, Version,
};

// Kind codes are stored with every identity and change, so they are pinned: Message 1,
// AgentSession 3, Convoy 5 and Ensure 9, the next code after the schema stack's test kinds.
#[test]
fn kind_codes_are_pinned() {
    let kinds = [
        (MessageRow::KIND, MessageRow::CODE),
        (AgentSessionRow::KIND, AgentSessionRow::CODE),
        (ConvoyRow::KIND, ConvoyRow::CODE),
        (EnsureRow::KIND, EnsureRow::CODE),
    ];
    assert_eq!(kinds, [("Message", KindCode(1)), ("AgentSession", KindCode(3)), ("Convoy", KindCode(5)), ("Ensure", KindCode(9))]);
}

/// `name`'s row at `home`, encoded and decoded through the schema codec, and the row a home
/// restored from it gives back.
fn round_trip<R: MachineRow + PartialEq + std::fmt::Debug>(home: &Home<R::Machine>, name: &str, since: Timestamp) -> R {
    let row = R::of(home.get(name).expect("exists"), since);
    let decoded: R = Decoder::decode(Encoder::encode(&row).expect("encodes")).expect("decodes");
    assert_eq!(decoded, row);
    let mut restored = Home::<R::Machine>::new("other").expect("valid");
    restored.restore(name, Version(7), row.parts()).unwrap_or_else(|_| panic!("an empty home"));
    assert_eq!(R::of(restored.get(name).expect("restored"), since), row, "restore keeps spec, state and records");
    row
}

// A resource's row holds its spec, state and records as they are: it survives the schema codec,
// and a home restoring it holds the same resource.
#[test]
fn rows_hold_their_resource() {
    let mut messages = Home::<Message>::new("home").expect("valid");
    let spec = MessageSpec { inbox: "i".into(), expectation: Expectation::Reply, deadline: Some(Timestamp(9)), unbound_after: 3 };
    messages.create("m", spec.clone()).expect("created");
    let row: MessageRow = round_trip(&messages, "m", Timestamp(1));
    assert_eq!((row.spec(), row.state.since), (spec, Timestamp(1)));

    let mut sessions = Home::<AgentSession>::new("home").expect("valid");
    sessions.create("s", ()).expect("created");
    round_trip::<AgentSessionRow>(&sessions, "s", Timestamp(2));

    let mut convoys = Home::<Convoy>::new("home").expect("valid");
    let promises = [(PromiseId(0), Promise { kind: "k".into(), target: "t".into() })].into();
    let request = ConvoyRequest { project: "p".into(), promises, quota: [(VesselClass(1), 2)].into(), priority: Priority(4) };
    convoys.create("c", request).expect("created");
    convoys.apply("c", Keep { promise: PromiseId(0) }, &MemoryConvoyWorld::default(), Timestamp(3)).expect("kept");
    let row: ConvoyRow = round_trip(&convoys, "c", Timestamp(2));
    assert_eq!(row.edges.len(), 1);

    let mut ensures = Home::<Ensure>::new("home").expect("valid");
    ensures.create("e", EnsureSpec { inbox: "i".into(), policy: AttemptPolicy::immediate(2) }).expect("created");
    ensures.apply("e", Record { fulfiller: Ref::new("f") }, &MemoryEnsureWorld::default(), Timestamp(4)).expect("recorded");
    ensures.record_attempt("e", flotilla_state_machines::Attempt::succeeded(Timestamp(5))).expect("attempt");
    let row: EnsureRow = round_trip(&ensures, "e", Timestamp(0));
    assert_eq!((row.edges.len(), row.attempts.len()), (1, 1));
    assert_eq!(row.state.value.recorded(), &BTreeSet::from([Ref::new("f")]));
}

fn root_sets(parts: &EdgeParts) -> Vec<&'static str> {
    parts.sets.iter().filter(|set| set.row == RowPath::Root).map(|set| set.field).collect()
}

// The stored edge for an applied edge writes the label and its since only when the label
// changes, since then being the event's time, and otherwise keeps the stored since; the event is
// the home's, and it leaves the stored label.
#[test]
fn a_rows_edge_moves_since_only_with_the_label() {
    let mut home = Home::<Convoy>::new("home").expect("valid");
    let promises = [(PromiseId(0), Promise { kind: "k".into(), target: "t".into() })].into();
    home.create("c", ConvoyRequest { project: "p".into(), promises, quota: [].into(), priority: Priority(1) }).expect("created");
    let world = MemoryConvoyWorld::default();
    let row = ConvoyRow::of(home.get("c").expect("exists"), Timestamp(1));

    let event = home.apply("c", Keep { promise: PromiseId(0) }, &world, Timestamp(5)).expect("kept").clone();
    let (edge, kept) = row.edge(&event, home.get("c").expect("exists").state().clone()).expect("edge");
    assert_eq!(kept.since, Timestamp(1), "a data-only edge keeps since");
    let parts = edge.into_parts();
    assert_eq!((root_sets(&parts), parts.rows.len(), parts.from.code), (vec![], 1, 1), "one resolution row, no root field");
    assert_eq!(parts.appends.len(), 1, "the event");

    let row = ConvoyRow { state: kept, ..row };
    let event = home.apply("c", Deliver, &world, Timestamp(8)).expect("delivered").clone();
    let (edge, delivered) = row.edge(&event, home.get("c").expect("exists").state().clone()).expect("edge");
    assert_eq!(delivered, State::new(home.get("c").expect("exists").state().clone(), Timestamp(8)), "a label change moves since");
    let parts = edge.into_parts();
    assert_eq!((root_sets(&parts), parts.rows.len()), (vec!["state"], 0), "the label and since, no data changed");
}
