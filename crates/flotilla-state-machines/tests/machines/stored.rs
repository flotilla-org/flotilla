//! The engine's stored kinds (#3319, #3373, #3392 ruling D5): each machine's `Resource` is its
//! stored kind, declared by its spec, with pinned kind codes; a resource survives the schema codec,
//! and an applied edge gives the stored edge persisting it.

use std::collections::BTreeSet;

use flotilla_schema::{
    Address, Cbor, Code, CodeEnum, Decoder, EdgeParts, Encoder, Frozen, Kind, KindCode, KindSpec, Labelled, RowPath, State, StateData,
    Timestamp,
};
use flotilla_state_machines::{
    kinds,
    machines::{
        agent_session::{AgentSession, MemoryAgentSessionWorld},
        awaiting_reply::{AwaitingReplyEdgeKind, AwaitingReplySpec},
        convoy::{
            Convoy, ConvoyEdge, ConvoyEdgeKind, ConvoyRequest, Deliver, Keep, MemoryConvoyWorld, Priority, Promise, PromiseId, VesselClass,
        },
        ensure::{Attempted, Ensure, EnsureSpec, MemoryEnsureWorld, Record},
        lifecycle::{self, LifecycleLabel, LifecycleState, MemoryLifecycleWorld, Observed, Realisation, Realise, Vanish},
        message::{
            Expectation, MemoryMessageWorld, Message, MessageBody, MessageEdgeKind, MessageRef, MessageReference, MessageSpec, Relation,
            RequestBody,
        },
        vessel::{live_leases, Vessel},
    },
    placement::{Dimension, Host, Lease, Ledger, LedgerError, Request, VesselId},
    AttemptPolicy, Declared, EdgePayload, Home, Label, Machine, Outcome, Ref, Resource, Version,
};

// Kind codes are stored with every identity and change, so the production registry pins them,
// each registered once (#3373 ruling 6).
#[test]
fn kind_codes_are_pinned() {
    let registry = kinds().expect("production kinds register");
    let mut pinned: Vec<_> = registry.iter().map(|set| (set.kind.clone(), set.code)).collect();
    pinned.sort();
    let declared = [
        (Resource::<AgentSession>::KIND, Resource::<AgentSession>::CODE),
        (Resource::<Convoy>::KIND, Resource::<Convoy>::CODE),
        (Resource::<Ensure>::KIND, Resource::<Ensure>::CODE),
        (Resource::<Message>::KIND, Resource::<Message>::CODE),
        (Resource::<Vessel>::KIND, Resource::<Vessel>::CODE),
    ];
    let expected = [
        ("AgentSession", KindCode::new("as")),
        ("AwaitingReply", KindCode::new("ar")),
        ("Convoy", KindCode::new("cv")),
        ("Ensure", KindCode::new("en")),
        ("Message", KindCode::new("ms")),
        ("Vessel", KindCode::new("ve")),
    ];
    let ahead = (Declared::<AwaitingReplySpec, AwaitingReplyEdgeKind>::KIND, Declared::<AwaitingReplySpec, AwaitingReplyEdgeKind>::CODE);
    assert_eq!([&declared[..1], &[ahead], &declared[1..]].concat(), expected);
    assert_eq!(pinned, expected.map(|(kind, code)| (kind.to_owned(), code)));
}

fn project() -> Address {
    Address::project("p").expect("project")
}

fn promise() -> Promise {
    Promise { kind: "k".into(), target: Address::project("t").expect("target") }
}

/// `name`'s resource at `home`, encoded and decoded through the schema codec (the name and
/// version are the store's, given back as a store gives them), and the same resource restored at
/// another home. Its edge events are log entries, not rows (#3392 G8), so the decoded resource has
/// none.
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
    assert_eq!(restored.get(name), Some(&resource), "restore keeps spec and state");
    resource
}

// A resource as stored holds its spec, state (with its since) and edge log as they are: it
// survives the schema codec, and a home restoring it holds the same resource.
#[test]
fn resources_survive_the_codec() {
    let mut messages = Home::<Message>::new("home").expect("valid");
    let request = ConvoyRequest {
        project: project(),
        promises: [(PromiseId(0), promise())].into(),
        quota: [(VesselClass(1), 2)].into(),
        priority: Priority(4),
    };
    let spec = MessageSpec {
        references: vec![
            MessageReference { address: Address::project("cr").expect("subject"), revision: "abc".into(), subject: true },
            MessageReference { address: Address::project("doc").expect("reference"), revision: "def".into(), subject: false },
        ],
        in_reply_to: Some(MessageRef::new("earlier", "elsewhere")),
        supersedes: Some(MessageRef::new("older", "home")),
        expectation: Expectation::Reply,
        deadline: Some(Timestamp(9)),
        unbound_after: 3,
        delivery_condition: Some("project:still-wanted .phase == Wanted".parse().expect("condition")),
        ..MessageSpec::new(
            Address::project("sender").expect("sender"),
            Address::project("i").expect("inbox"),
            Relation::Supervisor,
            Frozen::new(MessageBody::Request(RequestBody::Convoy(request))).expect("a request body freezes"),
        )
    };
    let world =
        MemoryMessageWorld::default().with_sent(&MessageRef::new("earlier", "elsewhere")).with_sent(&MessageRef::new("older", "home"));
    messages.create("m", spec.clone(), &world, Timestamp(1)).expect("created");
    let message = round_trip(&messages, "m");
    assert_eq!((message.spec(), message.since()), (&spec, Timestamp(1)));

    let mut sessions = Home::<AgentSession>::new("home").expect("valid");
    sessions.create("s", (), &MemoryAgentSessionWorld::default(), Timestamp(2)).expect("created");
    round_trip(&sessions, "s");

    let mut convoys = Home::<Convoy>::new("home").expect("valid");
    let promises = [(PromiseId(0), promise())].into();
    let request = ConvoyRequest { project: project(), promises, quota: [(VesselClass(1), 2)].into(), priority: Priority(4) };
    convoys.create("c", request, &MemoryConvoyWorld::default().with_project(&project()), Timestamp(2)).expect("created");
    convoys.apply("c", Keep { promise: PromiseId(0) }, &MemoryConvoyWorld::default(), Timestamp(3)).expect("kept");
    let convoy = round_trip(&convoys, "c");
    assert_eq!((convoy.version(), convoy.since()), (Version(2), Timestamp(2)), "a data-only edge keeps since");

    let mut ensures = Home::<Ensure>::new("home").expect("valid");
    ensures
        .create(
            "e",
            EnsureSpec { inbox: Address::project("i").expect("inbox"), policy: AttemptPolicy::immediate(2) },
            &MemoryEnsureWorld::default(),
            Timestamp(0),
        )
        .expect("created");
    ensures.apply("e", Record { fulfiller: Ref::new("f") }, &MemoryEnsureWorld::default(), Timestamp(4)).expect("recorded");
    let attempt = Attempted { fulfiller: Ref::new("f"), outcome: Outcome::Succeeded };
    ensures.apply("e", attempt, &MemoryEnsureWorld::default(), Timestamp(5)).expect("attempted");
    let ensure = round_trip(&ensures, "e");
    assert_eq!(ensures.log().history("e", Version(0), usize::MAX).len(), 2, "the attempt is an edge: a log entry, not a row");
    assert_eq!(ensure.state().recorded(), &BTreeSet::from([Ref::new("f")]));

    // A vessel's convoy is homed elsewhere: its reference keeps the convoy's home (#3392 G3).
    let mut vessels = Home::<Vessel>::new("capacity-host").expect("valid");
    let vessel = lease_of("c", 4);
    vessels.create("v", vessel.clone().into(), &MemoryLifecycleWorld::default(), Timestamp(0)).expect("created");
    let attempted = lifecycle::Attempted { outcome: Outcome::Succeeded };
    vessels.apply("v", attempted, &MemoryLifecycleWorld::default(), Timestamp(5)).expect("attempt while requested");
    let mut world = MemoryLifecycleWorld::default();
    let observed = Ref::<Realisation>::new("probe");
    world.realisations.put(&observed, Observed { identity: "vm-1".into() });
    vessels.apply("v", Realise { identity: "vm-1".into(), observed }, &world, Timestamp(6)).expect("realised");
    let stored = round_trip(&vessels, "v");
    let spec = stored.spec();
    assert_eq!((spec.lease(), spec.convoy.home.as_str()), (vessel, "admitting-host"));
    assert_eq!(stored.state(), &LifecycleState::Realised { identity: "vm-1".into() });
}

/// A lease of `units` of dimension 0 on host 3, for convoy `convoy` homed on another host.
fn lease_of(convoy: &str, units: u32) -> Lease {
    let convoy = flotilla_schema::Ref::new(convoy, "admitting-host");
    Lease {
        request: Request { convoy, class: VesselClass(1), shape: [(Dimension(0), units)].into(), priority: Priority(2) },
        host: Host(3),
    }
}

// The host ledger is a view over the live vessels' leases (#3392 ruling D5): a ledger rebuilt from
// stored vessels after a restart holds exactly the live leases, so capacity and quota counts come
// back unchanged, and a Gone vessel's lease is not restored.
#[test]
fn the_host_ledger_is_rebuilt_from_live_vessels() {
    let mut before = Ledger::new();
    before.add_host(Host(3), [(Dimension(0), 10)].into()).expect("host");
    let mut vessels = Home::<Vessel>::new("capacity-host").expect("valid");
    for (name, units) in [("a", 4), ("b", 3), ("gone", 2)] {
        let lease = lease_of("c", units);
        before.set_caps(lease.request.convoy.clone(), [(VesselClass(1), 5)].into());
        before.request(VesselId::new(name), lease.request.clone()).expect("requested");
        let placed = before.place(&VesselId::new(name), Host(3)).expect("placed").clone();
        vessels.create(name, placed.into(), &MemoryLifecycleWorld::default(), Timestamp(0)).expect("created");
    }
    let mut world = MemoryLifecycleWorld::default();
    let (observed, absent) = (Ref::new("probe"), Ref::new("probe"));
    world.realisations.put(&observed, Observed { identity: "vm-1".into() });
    world.absences.put(&absent, true);
    vessels.apply("gone", Realise { identity: "vm-1".into(), observed }, &world, Timestamp(1)).expect("realised");
    vessels.apply("gone", Vanish { observed: absent }, &world, Timestamp(2)).expect("gone");
    before.release(&VesselId::new("gone")).expect("released when Gone");

    let stored: Vec<Resource<Vessel>> = vessels
        .resources()
        .map(|vessel| {
            let decoded: Resource<Vessel> = Decoder::decode(Encoder::encode(vessel).expect("encodes")).expect("decodes");
            decoded.with_identity(vessel.name(), vessel.version().0)
        })
        .collect();
    let mut after = Ledger::new();
    after.add_host(Host(3), [(Dimension(0), 10)].into()).expect("host");
    for (id, lease) in live_leases(&stored) {
        after.restore(id, lease).expect("restored");
    }
    assert_eq!(after.leases().collect::<Vec<_>>(), before.leases().collect::<Vec<_>>());
    assert_eq!(after.available(Host(3)), Ok([(Dimension(0), 3)].into()));
    let convoy = lease_of("c", 0).request.convoy;
    assert_eq!(after.live(&convoy, VesselClass(1)), before.live(&convoy, VesselClass(1)));
    assert_eq!(after.restore(VesselId::new("a"), lease_of("c", 1)), Err(LedgerError::DuplicateRequest(VesselId::new("a"))));
    assert_eq!(
        after.restore(VesselId::new("big"), lease_of("c", 4)),
        Err(LedgerError::CapacityUnderflow { host: Host(3), dimension: Dimension(0) })
    );
    assert_eq!(after.leases().count(), 2, "a refused restore leaves the ledger as it was");
}

// Attempts and receipts are edges (R4), and every edge is a log entry (#3392 G8): no kind has a
// history table. Each kind declares its machine's edge codes, the only ones its entries may carry.
#[test]
fn the_log_is_the_only_history() {
    let tables = |set: flotilla_schema::TableSet| set.tables.iter().map(|table| table.name.clone()).collect::<Vec<_>>();
    assert_eq!(tables(Resource::<Message>::table_set()), ["message", "message_references"]);
    assert_eq!(tables(Resource::<AgentSession>::table_set()), ["agent_session"]);
    assert_eq!(tables(Resource::<Ensure>::table_set()), ["ensure", "ensure_recorded", "ensure_created"]);
    assert_eq!(tables(Resource::<Vessel>::table_set()), ["vessel", "vessel_shape"]);
    assert_eq!(Resource::<Message>::table_set().edges, <MessageEdgeKind as CodeEnum>::CODES);
    assert_eq!(Resource::<Convoy>::table_set().edges, <ConvoyEdgeKind as CodeEnum>::CODES);
}

fn root_sets(parts: &EdgeParts) -> Vec<&'static str> {
    parts.sets.iter().filter(|set| set.row == RowPath::Root).map(|set| set.field).collect()
}

// The stored edge for an applied edge writes the state as one unit (#3392 A3): its label, its
// since (the event's time when the label changes, the stored since otherwise) and its column
// data; the event is the entry's header, the home's, and it leaves the stored label.
#[test]
fn a_resources_edge_moves_since_only_with_the_label() {
    let mut home = Home::<Convoy>::new("home").expect("valid");
    let promises = [(PromiseId(0), promise())].into();
    let world = MemoryConvoyWorld::default().with_project(&project());
    home.create("c", ConvoyRequest { project: project(), promises, quota: [].into(), priority: Priority(1) }, &world, Timestamp(1))
        .expect("created");
    let created = home.get("c").expect("exists").stored_state().clone();

    let event = home.apply("c", Keep { promise: PromiseId(0) }, &world, Timestamp(5)).expect("kept");
    let resource = home.get("c").expect("exists");
    let edge = resource.stored_edge(&created, &event).expect("edge");
    assert_eq!(resource.since(), Timestamp(1), "a data-only edge keeps since");
    let kept = resource.stored_state().clone();
    let parts = edge.into_parts();
    assert_eq!(
        (root_sets(&parts), parts.rows.len(), parts.from.code),
        (vec!["state", "reason"], 1, Code::new("op")),
        "the whole state, one resolution row"
    );
    assert_eq!(parts.sets[0].values[1], flotilla_schema::Value::Int(1), "the stored since");
    assert_eq!(parts.appends.len(), 0, "the event is the entry's header, not a row");
    let open = Code::new("op");
    assert_eq!(
        (parts.event.edge, parts.event.from, parts.event.to, parts.event.at),
        (CodeEnum::code(&event.edge), open, open, Timestamp(5))
    );
    assert_eq!(parts.event.payload::<EdgePayload<ConvoyEdge>>().expect("payload"), event.payload);

    let event = home.apply("c", Deliver, &world, Timestamp(8)).expect("delivered");
    let resource = home.get("c").expect("exists");
    let edge = resource.stored_edge(&kept, &event).expect("edge");
    assert_eq!(resource.stored_state(), &State::new(resource.state().clone(), Timestamp(8)), "a label change moves since");
    let parts = edge.into_parts();
    assert_eq!((root_sets(&parts), parts.rows.len()), (vec!["state", "reason"], 0), "the whole state, no row changed");
}

// `live_leases` and the `live_by_host` index agree on which vessels are live:
// `LifecycleState::live` holds for exactly the labels the index's predicate
// names, so the ledger's view and the index read the same vessels.
#[test]
fn live_leases_and_the_live_by_host_index_agree() {
    let set = Resource::<Vessel>::table_set();
    let index = set.indexes.iter().find(|index| index.name == "live_by_host").expect("declared");
    let [flotilla_schema::Condition::In { column, codes }] = index.predicate.as_slice() else {
        panic!("live_by_host is partial on the state: {:?}", index.predicate);
    };
    assert_eq!(column, "state");
    let cause = flotilla_state_machines::Cause::new(flotilla_state_machines::CauseCategory::Transient, "lost");
    let states = [
        LifecycleState::requested(),
        LifecycleState::Realised { identity: "vm".into() },
        LifecycleState::Gone,
        LifecycleState::GaveUp { cause },
    ];
    for state in states {
        assert_eq!(state.live(), codes.contains(&<LifecycleLabel as CodeEnum>::code(state.label())), "{state:?}");
    }
}
