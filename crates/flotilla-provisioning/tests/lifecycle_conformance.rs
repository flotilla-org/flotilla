//! Each provisioning kind's machine against `conformance/lifecycle.qnt`, through the decided store
//! (#3629 layer 2).
//!
//! The engine's own conformance checks the shared lifecycle over an in-memory log and a request of
//! `()`. Here each kind (Environment, ImageBuild, Clone, Checkout, and TerminalSession from layer 6)
//! runs the same full graph with its
//! real request, through [`StoreLog`] over an SQLite store holding its stored kind. Every accepted
//! edge is one `apply_edge` in its own transaction, and the encoded state is read back from the
//! store: the resource's row loaded and decoded, and its history from its log entries, decoded
//! with `edge_event`. A refused edge must leave the stored version unchanged.
//!
//! The policy is the spec's (`MAX_FAILURES` = 2, no backoff): [`Conforming<K>`] is the kind `K`
//! with its request, so its stored kind, tables and code are `K`'s, under the spec's policy. Each
//! kind's own policy is checked by `policies_give_up_as_the_old_retries_did` in `stored_kinds.rs`.
//!
//! Evidence and state mapping are the engine's (`flotilla-state-machines/tests/conformance/
//! lifecycle.rs`): a cause is recorded as `Transient` with the spec's string as its detail.
//!
//! ```text
//! QUINT_GRAPH_DIR=DIR cargo test -p flotilla-provisioning --test lifecycle_conformance -- --ignored
//! ```

use std::{
    marker::PhantomData,
    sync::{Arc, Mutex},
};

use flotilla_engine_store::StoreLog;
use flotilla_provisioning::stored::{
    checkout::{CheckoutKind, CheckoutRequest, Source},
    clone::{CloneKind, CloneRequest},
    environment::{EnvironmentKind, EnvironmentRequest, Recipe},
    image_build::{Architecture, Delivery, ImageBuildKind, ImageBuildRequest, Reservation},
    terminal_session::{Launch, TerminalSessionKind, TerminalSessionRequest},
    Holder,
};
use flotilla_quint_testkit::{
    graph::{self, refusal, tick, Conformance, Report, Step},
    itf::{boolean, payload, tag, text, unit, variant},
};
use flotilla_resources::{ImageLayerSource, ImageLayerSpec, ImageLayerTier};
use flotilla_schema::{Address, Frozen, Kind, KindRegistry, Ref as StoredRef, Timestamp};
use flotilla_schema_sqlite::Store;
use flotilla_state_machines::{
    edge_event,
    machines::lifecycle::{
        Attempted, GiveUp, Lifecycle, LifecycleEdge, LifecycleKind, LifecycleRefusal, LifecycleState, MemoryLifecycleWorld, Observed,
        Realise, Release, Vanish,
    },
    AttemptPolicy, Cause, CauseCategory, Home, Log, Outcome, Ref, Resource, WriteError,
};
use serde_json::{json, Value};

/// Kind `K` under the spec's policy: the same request, so the same stored kind.
struct Conforming<K>(PhantomData<K>);

impl<K: LifecycleKind> LifecycleKind for Conforming<K> {
    const NAME: &'static str = K::NAME;
    type Request = K::Request;
    fn policy() -> AttemptPolicy {
        AttemptPolicy::immediate(2)
    }
}

/// A kind's request for the probe resource.
trait Sample: LifecycleKind<Request: PartialEq + std::fmt::Debug> {
    fn request() -> Self::Request;
}

impl Sample for EnvironmentKind {
    fn request() -> EnvironmentRequest {
        EnvironmentRequest {
            provider: "host:feta/provider:host-direct".parse::<Address>().expect("provider"),
            image_build: None,
            recipe: Frozen::new(
                Recipe::of(&flotilla_provisioning::stored::environment::HostDirectRecipe { repo_default_dir: "/work".into() })
                    .expect("recipe"),
            )
            .expect("freeze"),
            holder: None,
        }
    }
}

impl Sample for ImageBuildKind {
    fn request() -> ImageBuildRequest {
        let layer = ImageLayerSpec::builder()
            .name("base".into())
            .version("1.0.0".into())
            .tier(ImageLayerTier::Base)
            .source(ImageLayerSource::Stdlib)
            .from("docker.io/library/ubuntu@sha256:00".into())
            .fragment("RUN true".into())
            .probe("true".into())
            .build();
        ImageBuildRequest {
            composition: "sha256:feed".into(),
            architecture: Architecture::Amd64,
            layers: Frozen::new(vec![layer]).expect("freeze"),
            delivery: Delivery::Local,
            reservation: Reservation { cpu: 2, disk_bytes: 1 << 30 },
        }
    }
}

impl Sample for CloneKind {
    fn request() -> CloneRequest {
        CloneRequest {
            repository: "repo".into(),
            url: "https://example/repo".into(),
            environment: StoredRef::new("env", HOME),
            path: "/c".into(),
        }
    }
}

impl Sample for CheckoutKind {
    fn request() -> CheckoutRequest {
        CheckoutRequest {
            repository: "repo".into(),
            environment: StoredRef::new("env", HOME),
            branch: "feature".into(),
            base: Some("main".into()),
            path: "/w".into(),
            source: Source::Worktree,
            clone: Some(StoredRef::new("clone", HOME)),
            holder: Some(Holder::convoy("convoy")),
        }
    }
}

impl Sample for TerminalSessionKind {
    fn request() -> TerminalSessionRequest {
        TerminalSessionRequest {
            environment: StoredRef::new("env", HOME),
            holder: Some(Holder::vessel("vessel")),
            role: "coder".into(),
            pool: "cleat".into(),
            cwd: "/workspace".into(),
            env: [("A".to_owned(), "1".to_owned())].into(),
            source: Launch::Tool { command: "bash".into() },
        }
    }
}

const NAME: &str = "r";
const HOME: &str = "home";

type Machine<K> = Lifecycle<Conforming<K>>;

/// The engine home over its store log, and the store it writes.
struct Stored<K: Sample>
where
    StoreLog: Log<Machine<K>>,
{
    home: Home<Machine<K>, StoreLog>,
    store: Arc<Mutex<Store>>,
}

impl<K: Sample> Stored<K>
where
    Resource<Machine<K>>: Kind,
    StoreLog: Log<Machine<K>>,
{
    /// The resource as the store holds it, and the outcomes of its `Attempted` log entries in order.
    fn read_back(&self) -> (Resource<Machine<K>>, u64, Vec<Outcome>) {
        let mut store = self.store.lock().expect("store");
        let mut read = store.read().expect("read");
        let id = read.resolve(Resource::<Machine<K>>::CODE, NAME, HOME).expect("resolve").expect("stored");
        let loaded = read.load::<Resource<Machine<K>>>(id).expect("load").expect("current row");
        let mut attempts = Vec::new();
        let mut after = 0;
        loop {
            let entries = read.changes(id, after, 64).expect("changes");
            let Some(last) = entries.last() else { break };
            after = last.home_seq;
            for entry in &entries {
                if let Some(event) = edge_event::<Machine<K>>(entry).expect("entry decodes") {
                    if let LifecycleEdge::Attempted(attempted) = event.payload.edge {
                        attempts.push(attempted.outcome);
                    }
                }
            }
        }
        (loaded.value.with_identity(&loaded.name, loaded.version), loaded.version, attempts)
    }
}

fn cause(detail: &str) -> Cause {
    Cause::new(CauseCategory::Transient, detail)
}

fn outcome<T, M: flotilla_state_machines::Machine<Refusal = LifecycleRefusal>>(result: Result<T, WriteError<M>>) -> Step {
    match result {
        Ok(_) => Step::Accepted,
        Err(WriteError::Refused(LifecycleRefusal::Ended(_))) => refusal("Ended"),
        Err(WriteError::Refused(LifecycleRefusal::WrongState)) => refusal("WrongState"),
        Err(WriteError::Refused(LifecycleRefusal::MissingEvidence)) => refusal("MissingEvidence"),
        Err(other) => refusal(&format!("{other:?}")),
    }
}

fn outcome_value(outcome: &Outcome) -> Value {
    match outcome {
        Outcome::Succeeded => variant("Succeeded", unit()),
        Outcome::Failed { cause, retryable } => variant("AttemptFailed", json!({ "retryable": retryable, "cause": cause.detail })),
    }
}

/// The stored state in the spec's shape, checking the Requested tally against the logged attempts.
fn encode_state(state: &LifecycleState, attempts: &[Outcome]) -> Value {
    match state {
        LifecycleState::Requested { failures, last_failure } => {
            let since_success = attempts.iter().rev().take_while(|o| matches!(o, Outcome::Failed { .. })).count();
            assert_eq!(*failures as usize, since_success, "the stored tally counts the failures since the last success");
            let last = attempts.last().and_then(|o| match o {
                Outcome::Failed { cause, retryable } => Some((cause, *retryable)),
                Outcome::Succeeded => None,
            });
            assert_eq!(last_failure.as_ref().map(|f| (&f.cause, f.retryable)), last, "the stored tally keeps the last failure");
            variant("Requested", Value::Array(attempts.iter().map(outcome_value).collect()))
        }
        LifecycleState::Realised { identity } => variant("Realised", Value::from(identity.as_str())),
        LifecycleState::Gone { ending } => variant("Gone", flotilla_quint_testkit::lifecycle::ending_value(ending)),
        LifecycleState::GaveUp { cause } => variant("GaveUp", Value::from(cause.detail.as_str())),
    }
}

struct StoredLifecycle<K>(PhantomData<K>);

impl<K: Sample> Conformance for StoredLifecycle<K>
where
    Resource<Machine<K>>: Kind,
    StoreLog: Log<Machine<K>>,
{
    const MACHINE: &'static str = "lifecycle";
    type Subject = Stored<K>;

    fn create(_: &Value) -> Stored<K> {
        // Only the kind under test: every probe replays its path into a fresh store, and the
        // other provisioning kinds' tables and views would only lengthen each store's creation.
        let mut registry = KindRegistry::new();
        registry.register::<Resource<Machine<K>>>().expect("registry");
        let connection = rusqlite::Connection::open_in_memory().expect("memory");
        let store = Arc::new(Mutex::new(Store::open(connection, HOME, registry).expect("store")));
        let mut home = Home::<Machine<K>, StoreLog>::with_log(HOME, StoreLog::new(store.clone())).expect("valid declaration");
        home.create(NAME, K::request(), &MemoryLifecycleWorld::default(), Timestamp(0)).expect("created");
        Stored { home, store }
    }

    fn apply(subject: &mut Stored<K>, event: &Value) -> Step {
        let o = payload(event);
        let mut world = MemoryLifecycleWorld::default();
        let at = tick(1);
        let home = &mut subject.home;
        match tag(event) {
            "Realise" => {
                let identity = text(&o["identity"]);
                if boolean(&o["present"]) {
                    let seen = if boolean(&o["matches"]) { identity.to_string() } else { format!("not-{identity}") };
                    world.realisations.put(&Ref::new(NAME), Observed { identity: seen });
                }
                outcome(home.apply(NAME, Realise { identity: identity.into(), observed: Ref::new(NAME) }, &world, at))
            }
            "Release" => {
                world.teardowns.put(&Ref::new(NAME), boolean(&o["teardownComplete"]));
                outcome(home.apply(NAME, Release { teardown: Ref::new(NAME) }, &world, at))
            }
            "Vanish" => {
                world.absences.put(&Ref::new(NAME), boolean(&o["absent"]));
                outcome(home.apply(
                    NAME,
                    Vanish { cause: flotilla_quint_testkit::lifecycle::vanish_cause(&o["cause"]), observed: Ref::new(NAME) },
                    &world,
                    at,
                ))
            }
            "Attempted" => {
                let attempted = match tag(o) {
                    "Succeeded" => Outcome::Succeeded,
                    _ => {
                        let failed = payload(o);
                        Outcome::Failed { cause: cause(text(&failed["cause"])), retryable: boolean(&failed["retryable"]) }
                    }
                };
                outcome(home.apply(NAME, Attempted { outcome: attempted }, &world, at))
            }
            "GiveUp" => outcome(home.apply(NAME, GiveUp { cause: cause(text(&o["cause"])) }, &world, at)),
            other => panic!("lifecycle event {other}"),
        }
    }

    fn encode(subject: &Stored<K>) -> Value {
        let (stored, _, attempts) = subject.read_back();
        assert_eq!(Some(&stored), subject.home.get(NAME), "the store holds the resource the home holds");
        encode_state(stored.state(), &attempts)
    }

    fn version(subject: &Stored<K>) -> u64 {
        subject.read_back().1
    }
}

fn check<K: Sample>()
where
    Resource<Machine<K>>: Kind,
    StoreLog: Log<Machine<K>>,
{
    let mut report = Report::new(K::NAME);
    graph::check::<StoredLifecycle<K>>(&graph::load("lifecycle"), &mut report);
    report.finish();
}

// Every reachable lifecycle state is reached by each kind through its stored kind, and every
// (state, event) probe moves to the same state value or refuses with the same variant, writing
// nothing to the store.
#[test]
#[ignore = "export graphs with specs/primitives/check.sh"]
fn environment_follows_the_full_graph_through_the_store() {
    check::<EnvironmentKind>();
}

#[test]
#[ignore = "export graphs with specs/primitives/check.sh"]
fn image_build_follows_the_full_graph_through_the_store() {
    check::<ImageBuildKind>();
}

#[test]
#[ignore = "export graphs with specs/primitives/check.sh"]
fn clone_follows_the_full_graph_through_the_store() {
    check::<CloneKind>();
}

#[test]
#[ignore = "export graphs with specs/primitives/check.sh"]
fn checkout_follows_the_full_graph_through_the_store() {
    check::<CheckoutKind>();
}

// TerminalSession (Track B, #3629 layer 6) shares the lifecycle.
#[test]
#[ignore = "export graphs with specs/primitives/check.sh"]
fn terminal_session_follows_the_full_graph_through_the_store() {
    check::<TerminalSessionKind>();
}

/// One path through the adaptor, with the spec's results written out: two retryable failures
/// exhaust the spec's policy, a give-up with another cause is refused and writes nothing, and the
/// matching give-up ends the request; a second resource is realised and then released.
fn walk<K: Sample>()
where
    Resource<Machine<K>>: Kind,
    StoreLog: Log<Machine<K>>,
{
    let failed = |cause: &str| variant("AttemptFailed", json!({ "retryable": true, "cause": cause }));
    let mut subject = StoredLifecycle::<K>::create(&Value::Null);
    assert_eq!(StoredLifecycle::<K>::encode(&subject), variant("Requested", json!([])));
    for _ in 0..2 {
        assert!(matches!(StoredLifecycle::<K>::apply(&mut subject, &variant("Attempted", failed("failure"))), Step::Accepted));
    }
    assert_eq!(StoredLifecycle::<K>::encode(&subject), variant("Requested", json!([failed("failure"), failed("failure")])));
    let before = StoredLifecycle::<K>::version(&subject);
    let refused = StoredLifecycle::<K>::apply(&mut subject, &variant("GiveUp", json!({ "cause": "other" })));
    assert!(matches!(refused, Step::Refused(ref r) if tag(r) == "MissingEvidence"));
    assert_eq!(StoredLifecycle::<K>::version(&subject), before, "a refusal writes nothing to the store");
    assert!(matches!(StoredLifecycle::<K>::apply(&mut subject, &variant("GiveUp", json!({ "cause": "failure" }))), Step::Accepted));
    assert_eq!(StoredLifecycle::<K>::encode(&subject), variant("GaveUp", json!("failure")));

    let mut subject = StoredLifecycle::<K>::create(&Value::Null);
    let realise = variant("Realise", json!({ "present": true, "matches": true, "identity": "model" }));
    assert!(matches!(StoredLifecycle::<K>::apply(&mut subject, &realise), Step::Accepted));
    assert_eq!(StoredLifecycle::<K>::encode(&subject), variant("Realised", json!("model")));
    let release = variant("Release", json!({ "teardownComplete": true }));
    assert!(matches!(StoredLifecycle::<K>::apply(&mut subject, &release), Step::Accepted));
    assert_eq!(StoredLifecycle::<K>::encode(&subject), variant("Gone", variant("Released", flotilla_quint_testkit::itf::unit())));
    assert!(matches!(StoredLifecycle::<K>::apply(&mut subject, &realise), Step::Refused(ref r) if tag(r) == "Ended"));
}

// Behaviour (#3629 layer 2): without the exported graph, the adaptor itself drives each kind
// through the decided store, reading the state back from its row and its attempts from its log.
#[test]
fn each_kind_walks_the_lifecycle_through_the_store() {
    walk::<EnvironmentKind>();
    walk::<ImageBuildKind>();
    walk::<CloneKind>();
    walk::<CheckoutKind>();
    walk::<TerminalSessionKind>();
}
