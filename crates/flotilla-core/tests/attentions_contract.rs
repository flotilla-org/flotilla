//! The Attentions contract, run against the old store and the engine: raise is idempotent per
//! sender and subject, a changed reason replaces the open one, clear settles it, a person's
//! acknowledgement settles it, and open for a subject sees only the open ones about it or under it.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use flotilla_core::primitives::{view::record_name, MessagePrimitives};
use flotilla_resources::{
    attentions::{Attention, Attentions, Fact, Sender, Subject},
    Convoy, ConvoyEnsure, Demand, DemandStatusPatch, FleetDesignation, FleetDesignationSpec, InputMeta, Project, ProjectSpec,
    ResourceObject, SystemClock, TerminalSession, Vessel, CONVOY_LABEL, FLEET_DESIGNATION_NAME, ROLE_LABEL,
};
use flotilla_schema_sqlite::stream::Service;
use flotilla_state_machines::machines::message::MessageState;
use flotilla_store::{OldStoreAttentions, ResourceBackend};
use flotilla_store_memory::InMemoryBackend;
use hegel::{generators as gs, TestCase};
use serde_json::json;

const NAMESPACE: &str = "flotilla";

fn spec<R: flotilla_resources::Resource>(spec: serde_json::Value) -> R::Spec {
    serde_json::from_value(spec).expect("spec")
}

async fn records() -> ResourceBackend {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let meta = |name: &str| InputMeta::builder().name(name.to_owned()).build();
    backend
        .using::<Project>(NAMESPACE)
        .create(
            &meta("fleet"),
            &ProjectSpec::builder().display_name("Fleet".into()).default_workflow_ref("w".into()).repositories(Vec::new()).build(),
        )
        .await
        .expect("fleet project");
    backend
        .definitions::<FleetDesignation>(NAMESPACE)
        .create(
            &meta(FLEET_DESIGNATION_NAME),
            &FleetDesignationSpec { fleet: Some("lab".into()), project: "fleet".into(), image_cache: None },
        )
        .await
        .expect("designation");
    backend
        .using::<Convoy>(NAMESPACE)
        .create(&meta("demo"), &spec::<Convoy>(json!({"workflow_ref": "w", "project_ref": NAMESPACE})))
        .await
        .expect("convoy");
    backend
        .definitions::<ConvoyEnsure>(NAMESPACE)
        .create(&meta("standing-gui"), &spec::<ConvoyEnsure>(json!({"project_ref": NAMESPACE, "role": "gui", "repositories": []})))
        .await
        .expect("ensure");
    backend
        .using::<Vessel>(NAMESPACE)
        .create(&meta("demo-work"), &spec::<Vessel>(json!({"convoy_ref": "demo", "vessel_name": "work", "fulfilment_kind_ref": "p"})))
        .await
        .expect("vessel");
    let mut session = meta("session-1");
    session.labels = BTreeMap::from([(CONVOY_LABEL.to_owned(), "demo".to_owned()), (ROLE_LABEL.to_owned(), "coder".to_owned())]);
    backend
        .using::<TerminalSession>(NAMESPACE)
        .create(
            &session,
            &spec::<TerminalSession>(
                json!({"env_ref": "env", "role": "coder", "source": {"kind": "tool", "command": "true"}, "cwd": "/repo", "pool": "cleat"}),
            ),
        )
        .await
        .expect("session");
    backend
}

/// One implementation under the contract, and how a person acknowledges an attention there.
enum Implementation {
    Old { attentions: OldStoreAttentions, backend: ResourceBackend },
    Engine { primitives: Arc<MessagePrimitives>, _dir: tempfile::TempDir },
}

impl Implementation {
    async fn old() -> Self {
        let backend = records().await;
        Implementation::Old { attentions: OldStoreAttentions::new(backend.clone()), backend }
    }

    async fn engine() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let service = Arc::new(
            Service::open(&dir.path().join("decided.sqlite"), "home-a", flotilla_engine_store::registry().expect("registry"))
                .expect("store"),
        );
        let primitives = MessagePrimitives::open("home-a", service, records().await, Arc::new(SystemClock)).expect("primitives");
        Implementation::Engine { primitives: Arc::new(primitives), _dir: dir }
    }

    fn attentions(&self) -> &dyn Attentions {
        match self {
            Implementation::Old { attentions, .. } => attentions,
            Implementation::Engine { primitives, .. } => primitives.as_ref(),
        }
    }

    async fn acknowledge(&self, fact: &Fact) {
        match self {
            Implementation::Old { backend, .. } => {
                let prefix = match fact {
                    Fact::Hold { .. } => "ensure-attention-",
                    Fact::Drift { .. } => "ensure-config-drift-",
                    Fact::ReclaimRefused { .. } => "reclaim-refusal-",
                    Fact::CredentialRefresh { .. } => "credential-refresh-",
                    Fact::DeliveryHeld { .. } => "terminal-delivery-",
                };
                let demands = backend.using::<Demand>(NAMESPACE);
                let listed: Vec<ResourceObject<Demand>> = demands.list().await.expect("demands").items;
                for demand in listed.iter().filter(|demand| demand.metadata.name.starts_with(prefix)) {
                    let patch = DemandStatusPatch::Acknowledge { as_of: chrono::Utc::now(), authority: "person".into() };
                    flotilla_store::apply_status_patch(&demands, &demand.metadata.name, &patch).await.expect("acknowledge");
                }
            }
            Implementation::Engine { primitives, .. } => {
                let sender = format!("fleet:lab/role:{}", fact.sender().role());
                let pending: Vec<_> = primitives
                    .messages()
                    .expect("messages")
                    .into_iter()
                    .filter(|(_, message)| *message.state() == MessageState::Pending && message.spec().sender.as_str() == sender)
                    .collect();
                for (_, message) in pending {
                    let about = message.spec().subject().expect("subject").address.clone();
                    if about.as_str().ends_with(subject_suffix(&fact.subject()).as_str()) {
                        primitives.acknowledge(NAMESPACE, &record_name(message.name()).expect("record")).await.expect("acknowledge");
                    }
                }
            }
        }
    }
}

fn subject_suffix(subject: &Subject) -> String {
    match subject {
        Subject::Convoy { convoy } => format!("convoy:{convoy}"),
        Subject::StandingRole { project, role } => format!("project:{project}/role:{role}"),
        Subject::Vessel { convoy, vessel } => format!("convoy:{convoy}/vessel:{vessel}"),
        Subject::ConvoyRole { convoy, role } => format!("convoy:{convoy}/role:{role}"),
    }
}

/// Every fact one writer raises, each from its sender about its subject; two share the convoy.
fn attention(fact: usize, reason: &str) -> Attention {
    let convoy = || "demo".to_owned();
    match fact {
        0 => Attention::new(Fact::Hold { ensure: "standing-gui".into(), convoy: convoy() }, reason),
        1 => Attention::new(Fact::ReclaimRefused { convoy: convoy() }, reason),
        2 => Attention::new(
            Fact::Drift { ensure: "standing-gui".into(), project: NAMESPACE.into(), role: "gui".into(), convoy: convoy() },
            reason,
        ),
        // The second reason adds a credential, so the first drops one: the old store deletes its demand.
        3 => {
            let credentials: &[&str] = if reason == "still held" { &["github", "gitlab"] } else { &["github"] };
            Attention::credential_refresh(
                "demo",
                "work",
                credentials.iter().map(|credential| (credential.to_string(), reason.to_owned())).collect(),
            )
        }
        _ => Attention::new(Fact::DeliveryHeld { session: "session-1".into(), convoy: convoy(), role: "coder".into() }, reason),
    }
}

const FACTS: usize = 5;
const DELIVERY: usize = 4;

#[derive(Debug, Clone, Copy)]
enum Step {
    Raise { fact: usize, reason: usize },
    Clear { fact: usize },
    Acknowledge { fact: usize },
}

/// What a person sees, per sender and subject: the open reason, and whether the last was acknowledged.
#[derive(Default, Clone)]
struct Model {
    open: BTreeMap<(Sender, Subject), String>,
    acknowledged: BTreeSet<(Sender, Subject)>,
    /// When each open attention was first seen raised; a repeated raise keeps it.
    raised_at: BTreeMap<(Sender, Subject), chrono::DateTime<chrono::Utc>>,
}

async fn check(implementation: &Implementation, model: &mut Model, after: &[Step]) {
    let attentions = implementation.attentions();
    let seen = |open: Vec<flotilla_resources::attentions::OpenAttention>| {
        open.into_iter().map(|open| ((open.sender, open.subject), open.reason)).collect::<BTreeMap<_, _>>()
    };
    let open = attentions.open(NAMESPACE, None, None).await.expect("open");
    for attention in &open {
        let key = (attention.sender, attention.subject.clone());
        let first = *model.raised_at.entry(key.clone()).or_insert(attention.raised_at);
        assert_eq!(attention.raised_at, first, "{key:?} was raised again after {after:?}");
    }
    let all = seen(open);
    assert_eq!(all, model.open, "every open attention after {after:?}");
    let convoy = Subject::convoy("demo");
    let under = seen(attentions.open(NAMESPACE, None, Some(&convoy)).await.expect("open"));
    let expected: BTreeMap<_, _> = model
        .open
        .iter()
        .filter(|((_, subject), _)| !matches!(subject, Subject::StandingRole { .. }))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    assert_eq!(under, expected, "open about the convoy after {after:?}");
    let vessel = Subject::Vessel { convoy: "demo".into(), vessel: "work".into() };
    let about_vessel = seen(attentions.open(NAMESPACE, None, Some(&vessel)).await.expect("open"));
    let expected: BTreeMap<_, _> =
        model.open.iter().filter(|((_, subject), _)| *subject == vessel).map(|(k, v)| (k.clone(), v.clone())).collect();
    assert_eq!(about_vessel, expected, "open about the vessel after {after:?}");
    let standing = seen(attentions.open(NAMESPACE, Some(Sender::Standing), None).await.expect("open"));
    let expected: BTreeMap<_, _> =
        model.open.iter().filter(|((sender, _), _)| *sender == Sender::Standing).map(|(k, v)| (k.clone(), v.clone())).collect();
    assert_eq!(standing, expected, "open from standing after {after:?}");
    for fact in 0..FACTS {
        let key = {
            let fact = attention(fact, "").fact;
            (fact.sender(), fact.subject())
        };
        let acknowledged = attentions.acknowledged(NAMESPACE, key.0, &key.1).await.expect("acknowledged");
        assert_eq!(acknowledged, model.acknowledged.contains(&key), "{key:?} acknowledged after {after:?}");
    }
}

fn contract(tc: TestCase, engine: bool) {
    let count = tc.draw(gs::integers::<usize>().min_value(1).max_value(12));
    let steps: Vec<Step> = (0..count)
        .map(|_| {
            let fact = tc.draw(gs::integers::<usize>().min_value(0).max_value(FACTS - 1));
            match tc.draw(gs::integers::<u8>().min_value(0).max_value(3)) {
                0 | 1 => Step::Raise { fact, reason: tc.draw(gs::integers::<usize>().min_value(0).max_value(1)) },
                2 => Step::Clear { fact },
                _ => Step::Acknowledge { fact },
            }
        })
        .collect();
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    runtime.block_on(async {
        let implementation = if engine { Implementation::engine().await } else { Implementation::old().await };
        let mut model = Model::default();
        for (index, step) in steps.iter().enumerate() {
            match *step {
                Step::Raise { fact, reason } => {
                    // On the old store the old Message controller owns a held delivery's reason.
                    let reason = if fact == DELIVERY && !engine { 0 } else { reason };
                    let attention = attention(fact, ["held", "still held"][reason]);
                    implementation.attentions().raise(NAMESPACE, &attention).await.expect("raise");
                    let key = (attention.fact.sender(), attention.fact.subject());
                    // An acknowledged attention stays acknowledged until its writer clears it.
                    if model.acknowledged.contains(&key) {
                        check(&implementation, &mut model, &steps[..=index]).await;
                        continue;
                    }
                    if model.open.insert(key.clone(), attention.reason.clone()).is_none_or(|before| before != attention.reason) {
                        model.raised_at.remove(&key);
                    }
                }
                Step::Clear { fact } => {
                    let fact = attention(fact, "").fact;
                    implementation.attentions().clear(NAMESPACE, fact.sender(), &fact.subject()).await.expect("clear");
                    let key = (fact.sender(), fact.subject());
                    model.open.remove(&key);
                    model.acknowledged.remove(&key);
                    model.raised_at.remove(&key);
                }
                Step::Acknowledge { fact } => {
                    let fact = attention(fact, "").fact;
                    implementation.acknowledge(&fact).await;
                    let key = (fact.sender(), fact.subject());
                    if model.open.remove(&key).is_some() {
                        model.raised_at.remove(&key);
                        model.acknowledged.insert(key);
                    }
                }
            }
            check(&implementation, &mut model, &steps[..=index]).await;
        }
    });
}

#[hegel::test(test_cases = 100)]
fn the_old_store_keeps_the_attentions_contract(tc: TestCase) {
    contract(tc, false);
}

#[hegel::test(test_cases = 100)]
fn the_engine_keeps_the_attentions_contract(tc: TestCase) {
    contract(tc, true);
}

async fn acknowledged_until_cleared(implementation: Implementation) {
    let attentions = implementation.attentions();
    let hold = attention(0, "held");
    let (sender, subject) = (hold.fact.sender(), hold.fact.subject());
    let open = || async { attentions.open(NAMESPACE, Some(sender), Some(&subject)).await.expect("open") };
    attentions.raise(NAMESPACE, &hold).await.expect("raise");
    implementation.acknowledge(&hold.fact).await;
    attentions.raise(NAMESPACE, &hold).await.expect("raise again");
    attentions.raise(NAMESPACE, &attention(0, "still held")).await.expect("raise for another reason");
    assert!(open().await.is_empty(), "raising again leaves an acknowledged attention acknowledged");
    assert!(attentions.acknowledged(NAMESPACE, sender, &subject).await.expect("acknowledged"));
    attentions.clear(NAMESPACE, sender, &subject).await.expect("clear");
    assert!(!attentions.acknowledged(NAMESPACE, sender, &subject).await.expect("acknowledged"));
    attentions.raise(NAMESPACE, &hold).await.expect("raise after clear");
    assert_eq!(open().await.len(), 1, "cleared, the next raise opens a new attention");
}

// Behaviour: raise, acknowledge, raise again stays acknowledged; clear, then raise opens a new one.
#[tokio::test]
async fn the_old_store_keeps_an_acknowledgement_until_cleared() {
    acknowledged_until_cleared(Implementation::old().await).await;
}

#[tokio::test]
async fn the_engine_keeps_an_acknowledgement_until_cleared() {
    acknowledged_until_cleared(Implementation::engine().await).await;
}

/// A home's engine over its own decided store.
fn engine_at(home: &str, backend: ResourceBackend, dir: &tempfile::TempDir) -> (Arc<Service>, Arc<MessagePrimitives>) {
    let service = Arc::new(
        Service::open(&dir.path().join(format!("{home}.sqlite")), home, flotilla_engine_store::registry().expect("registry"))
            .expect("store"),
    );
    let primitives = MessagePrimitives::open(home, Arc::clone(&service), backend, Arc::new(SystemClock)).expect("primitives");
    (service, Arc::new(primitives))
}

/// One round of `from`'s stream applied at `to`, as a follower does.
fn follow(from: &Service, from_home: &str, to: &Service, cursor: &mut Option<u64>) {
    let mut frames = Vec::new();
    let next = from
        .serve(*cursor, flotilla_schema::Timestamp::from_datetime(&chrono::Utc::now()), |frame| {
            frames.push(frame.to_vec());
            Ok(())
        })
        .expect("serve");
    to.with_store(|store| {
        flotilla_schema_sqlite::stream::Follower::new(store).receive(from_home, |sink| {
            for frame in &frames {
                sink(frame)?;
            }
            Ok(())
        })
    })
    .expect("follow");
    *cursor = Some(next);
}

// Behaviour: an attention raised at one home, and its acknowledgement there, reach another home's
// readers with the replicated entries: the signal fires when they are applied, with no local write.
#[tokio::test]
async fn another_homes_attentions_signal_their_readers_when_they_arrive() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (a_store, a) = engine_at("home-a", records().await, &dir);
    let (b_store, b) = engine_at("home-b", records().await, &dir);
    let mut cursor = None;
    follow(&a_store, "home-a", &b_store, &mut cursor);
    let mut signal = b.changes().expect("the engine signals its readers");
    let hold = attention(0, "held");
    let (sender, subject) = (hold.fact.sender(), hold.fact.subject());
    a.raise(NAMESPACE, &hold).await.expect("raise at home a");
    assert!(signal.try_recv().is_err(), "nothing has reached home b yet");
    follow(&a_store, "home-a", &b_store, &mut cursor);
    signal.try_recv().expect("applying home a's entries signals home b's readers");
    let seen = b.open(NAMESPACE, Some(sender), Some(&subject)).await.expect("open at home b");
    assert_eq!(seen.len(), 1, "home b reads home a's hold");

    let message = a
        .messages()
        .expect("messages")
        .into_iter()
        .map(|(_, message)| message)
        .find(|message| *message.state() == MessageState::Pending)
        .expect("the hold");
    a.acknowledge(NAMESPACE, &record_name(message.name()).expect("record")).await.expect("acknowledge at home a");
    follow(&a_store, "home-a", &b_store, &mut cursor);
    signal.try_recv().expect("the acknowledgement's entries signal home b's readers");
    assert!(b.open(NAMESPACE, Some(sender), Some(&subject)).await.expect("open at home b").is_empty());
    assert!(b.acknowledged(NAMESPACE, sender, &subject).await.expect("acknowledged at home b"));
}
