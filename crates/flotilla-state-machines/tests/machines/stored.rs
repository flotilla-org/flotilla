//! The engine's stored kinds (#3319, #3373, #3392 ruling D5): each machine's `Resource` is its
//! stored kind, declared by its spec, with pinned kind codes; a resource survives the schema codec,
//! and an applied edge gives the stored edge persisting it.

use std::collections::BTreeSet;

use flotilla_schema::{Cbor, Code, Decoder, EdgeParts, Encoder, Kind, KindCode, KindSpec, Labelled, RowPath, State, StateData, Timestamp};
use flotilla_state_machines::{
    machines::{
        agent_session::AgentSession,
        convoy::{Convoy, ConvoyRequest, Deliver, Keep, MemoryConvoyWorld, Priority, Promise, PromiseId, VesselClass},
        ensure::{Attempted, Ensure, EnsureSpec, MemoryEnsureWorld, Record},
        message::{Expectation, Message, MessageSpec},
    },
    AttemptPolicy, EdgePayload, Home, Label, Machine, Outcome, Ref, Resource,
};

// Kind codes are stored with every identity and change, so they are pinned.
#[test]
fn kind_codes_are_pinned() {
    let kinds = [
        (Resource::<Message>::KIND, Resource::<Message>::CODE),
        (Resource::<AgentSession>::KIND, Resource::<AgentSession>::CODE),
        (Resource::<Convoy>::KIND, Resource::<Convoy>::CODE),
        (Resource::<Ensure>::KIND, Resource::<Ensure>::CODE),
    ];
    assert_eq!(
        kinds,
        [
            ("Message", KindCode::new("ms")),
            ("AgentSession", KindCode::new("as")),
            ("Convoy", KindCode::new("cv")),
            ("Ensure", KindCode::new("en"))
        ]
    );
}

/// `name`'s resource at `home`, encoded and decoded through the schema codec (the name and
/// version are the store's, given back as a store gives them), and the same resource restored at
/// another home.
fn round_trip<M>(home: &Home<M>, name: &str) -> Resource<M>
where
    M: Machine,
    M::Spec: KindSpec<State = M::State> + Clone + PartialEq + std::fmt::Debug,
    M::State: StateData + Labelled<Label = Label<M>>,
    EdgePayload<M::Edge>: Cbor + PartialEq,
{
    let resource = home.get(name).expect("exists").clone();
    let decoded: Resource<M> = Decoder::decode(Encoder::encode(&resource).expect("encodes")).expect("decodes");
    assert_eq!(decoded.name(), "", "the name is the store's");
    let decoded = decoded.with_identity(name, resource.version().0);
    assert_eq!(decoded, resource);
    let mut restored = Home::<M>::new("other").expect("valid");
    restored.restore(decoded).unwrap_or_else(|_| panic!("an empty home"));
    assert_eq!(restored.get(name), Some(&resource), "restore keeps spec, state and edge log");
    resource
}

// A resource as stored holds its spec, state (with its since) and edge log as they are: it
// survives the schema codec, and a home restoring it holds the same resource.
#[test]
fn resources_survive_the_codec() {
    let mut messages = Home::<Message>::new("home").expect("valid");
    let spec = MessageSpec { inbox: "i".into(), expectation: Expectation::Reply, deadline: Some(Timestamp(9)), unbound_after: 3 };
    messages.create("m", spec.clone(), Timestamp(1)).expect("created");
    let message = round_trip(&messages, "m");
    assert_eq!((message.spec(), message.since()), (&spec, Timestamp(1)));

    let mut sessions = Home::<AgentSession>::new("home").expect("valid");
    sessions.create("s", (), Timestamp(2)).expect("created");
    round_trip(&sessions, "s");

    let mut convoys = Home::<Convoy>::new("home").expect("valid");
    let promises = [(PromiseId(0), Promise { kind: "k".into(), target: "t".into() })].into();
    let request = ConvoyRequest { project: "p".into(), promises, quota: [(VesselClass(1), 2)].into(), priority: Priority(4) };
    convoys.create("c", request, Timestamp(2)).expect("created");
    convoys.apply("c", Keep { promise: PromiseId(0) }, &MemoryConvoyWorld::default(), Timestamp(3)).expect("kept");
    let convoy = round_trip(&convoys, "c");
    assert_eq!((convoy.events().len(), convoy.since()), (1, Timestamp(2)), "a data-only edge keeps since");

    let mut ensures = Home::<Ensure>::new("home").expect("valid");
    ensures.create("e", EnsureSpec { inbox: "i".into(), policy: AttemptPolicy::immediate(2) }, Timestamp(0)).expect("created");
    ensures.apply("e", Record { fulfiller: Ref::new("f") }, &MemoryEnsureWorld::default(), Timestamp(4)).expect("recorded");
    let attempt = Attempted { fulfiller: Ref::new("f"), outcome: Outcome::Succeeded };
    ensures.apply("e", attempt, &MemoryEnsureWorld::default(), Timestamp(5)).expect("attempted");
    let ensure = round_trip(&ensures, "e");
    assert_eq!(ensure.events().len(), 2, "the attempt is an edge in the log");
    assert_eq!(ensure.state().recorded(), &BTreeSet::from([Ref::new("f")]));
}

// Attempts and receipts are edges (R4): every kind's history is its edge log alone.
#[test]
fn the_edge_log_is_the_only_history() {
    let tables = |set: flotilla_schema::TableSet| set.tables.iter().map(|table| table.name.clone()).collect::<Vec<_>>();
    assert_eq!(tables(Resource::<Message>::table_set()), ["message", "message_edges"]);
    assert_eq!(tables(Resource::<AgentSession>::table_set()), ["agent_session", "agent_session_edges"]);
    assert_eq!(tables(Resource::<Ensure>::table_set()), ["ensure", "ensure_recorded", "ensure_created", "ensure_edges"]);
}

fn root_sets(parts: &EdgeParts) -> Vec<&'static str> {
    parts.sets.iter().filter(|set| set.row == RowPath::Root).map(|set| set.field).collect()
}

// The stored edge for an applied edge writes the label and its since only when the label
// changes, since then being the event's time, and otherwise keeps the stored since; the event is
// the home's, and it leaves the stored label.
#[test]
fn a_resources_edge_moves_since_only_with_the_label() {
    let mut home = Home::<Convoy>::new("home").expect("valid");
    let promises = [(PromiseId(0), Promise { kind: "k".into(), target: "t".into() })].into();
    home.create("c", ConvoyRequest { project: "p".into(), promises, quota: [].into(), priority: Priority(1) }, Timestamp(1))
        .expect("created");
    let world = MemoryConvoyWorld::default();
    let created = home.get("c").expect("exists").stored_state().clone();

    let event = home.apply("c", Keep { promise: PromiseId(0) }, &world, Timestamp(5)).expect("kept").clone();
    let resource = home.get("c").expect("exists");
    let edge = resource.stored_edge(&created, &event).expect("edge");
    assert_eq!(resource.since(), Timestamp(1), "a data-only edge keeps since");
    let kept = resource.stored_state().clone();
    let parts = edge.into_parts();
    assert_eq!((root_sets(&parts), parts.rows.len(), parts.from.code), (vec![], 1, Code::new("op")), "one resolution row, no root field");
    assert_eq!(parts.appends.len(), 1, "the event");

    let event = home.apply("c", Deliver, &world, Timestamp(8)).expect("delivered").clone();
    let resource = home.get("c").expect("exists");
    let edge = resource.stored_edge(&kept, &event).expect("edge");
    assert_eq!(resource.stored_state(), &State::new(resource.state().clone(), Timestamp(8)), "a label change moves since");
    let parts = edge.into_parts();
    assert_eq!((root_sets(&parts), parts.rows.len()), (vec!["state"], 0), "the label and since, no data changed");
}
