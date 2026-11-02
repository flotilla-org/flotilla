//! The Attentions contract, run against the old store and the engine: raise is idempotent per
//! sender and subject, a changed reason replaces the open one, clear withdraws it, a person's
//! acknowledgement settles it and answers its episode, and open for a subject sees only the open
//! ones about it or under it.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use chrono::{DateTime, Utc};

use flotilla_core::primitives::{view::record_name, MessagePrimitives};
use flotilla_resources::{
    attentions::{Attention, Attentions, Fact, Sender, Subject},
    Convoy, ConvoyEnsure, Demand, DemandState, DemandStatusPatch, FleetDesignation, FleetDesignationSpec, InputMeta, Project, ProjectSpec,
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
                // A person acknowledges what is open.
                let open = |demand: &&ResourceObject<Demand>| {
                    demand.status.as_ref().is_none_or(|status| matches!(status.state, DemandState::Raised | DemandState::Escalated))
                };
                for demand in listed.iter().filter(|demand| demand.metadata.name.starts_with(prefix)).filter(open) {
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
fn attention(fact: usize, reason: &str, began_at: DateTime<Utc>) -> Attention {
    let convoy = || "demo".to_owned();
    match fact {
        0 => Attention::new(Fact::Hold { ensure: "standing-gui".into(), convoy: convoy() }, reason, began_at),
        1 => Attention::new(Fact::ReclaimRefused { convoy: convoy() }, reason, began_at),
        2 => Attention::new(
            Fact::Drift { ensure: "standing-gui".into(), project: NAMESPACE.into(), role: "gui".into(), convoy: convoy() },
            reason,
            began_at,
        ),
        // The second reason adds a credential, so the first drops one: the old store deletes its demand.
        3 => {
            let credentials: &[&str] = if reason == "still held" { &["github", "gitlab"] } else { &["github"] };
            Attention::credential_refresh(
                "demo",
                "work",
                credentials.iter().map(|credential| (credential.to_string(), reason.to_owned())).collect(),
                began_at,
            )
        }
        _ => Attention::new(Fact::DeliveryHeld { session: "session-1".into(), convoy: convoy(), role: "coder".into() }, reason, began_at),
    }
}

fn key(fact: usize) -> (Sender, Subject) {
    let fact = attention(fact, "", DateTime::UNIX_EPOCH).fact;
    (fact.sender(), fact.subject())
}

/// An episode's start, on a later microsecond than anything before it, as the stores keep time.
async fn episode_start() -> DateTime<Utc> {
    tokio::time::sleep(Duration::from_millis(1)).await;
    Utc::now()
}

const FACTS: usize = 5;
const DELIVERY: usize = 4;

#[derive(Debug, Clone, Copy)]
enum Step {
    Raise {
        fact: usize,
        reason: usize,
    },
    Clear {
        fact: usize,
    },
    Acknowledge {
        fact: usize,
    },
    /// Every attention about the convoy and the records under it ends.
    ClearWithin,
}

/// One fact's episode, as its writer knows it.
#[derive(Clone, Copy)]
struct Episode {
    began_at: DateTime<Utc>,
    ended: bool,
}

/// What a person sees, per sender and subject: the open reason, and what a person answered.
#[derive(Default, Clone)]
struct Model {
    open: BTreeMap<(Sender, Subject), String>,
    episodes: BTreeMap<(Sender, Subject), Episode>,
    /// The episode a person answered, and the reason they acknowledged.
    answered: BTreeMap<(Sender, Subject), (DateTime<Utc>, String)>,
    /// When each open attention was first seen raised; a repeated raise keeps it.
    raised_at: BTreeMap<(Sender, Subject), DateTime<Utc>>,
}

impl Model {
    /// The episode a raise belongs to: the current one, or a new one once the last ended.
    async fn episode(&mut self, key: &(Sender, Subject)) -> DateTime<Utc> {
        match self.episodes.get(key) {
            Some(episode) if !episode.ended => episode.began_at,
            _ => {
                let began_at = episode_start().await;
                self.episodes.insert(key.clone(), Episode { began_at, ended: false });
                began_at
            }
        }
    }

    fn end(&mut self, key: &(Sender, Subject)) {
        self.open.remove(key);
        self.raised_at.remove(key);
        if let Some(episode) = self.episodes.get_mut(key) {
            episode.ended = true;
        }
    }
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
    let expected: BTreeMap<_, _> =
        model.open.iter().filter(|((_, subject), _)| subject.within(&convoy)).map(|(k, v)| (k.clone(), v.clone())).collect();
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
    // A writer asks about its live episode; an ended one is never raised again.
    for (key, episode) in model.episodes.iter().filter(|(_, episode)| !episode.ended) {
        let acknowledged = attentions.acknowledged(NAMESPACE, key.0, &key.1, episode.began_at).await.expect("acknowledged");
        let answered = model.answered.get(key).filter(|(began_at, _)| *began_at == episode.began_at).map(|(_, reason)| reason.clone());
        assert_eq!(acknowledged, answered, "{key:?} acknowledged after {after:?}");
    }
}

fn contract(tc: TestCase, engine: bool) {
    let count = tc.draw(gs::integers::<usize>().min_value(1).max_value(14));
    // Raises, clears and acknowledgements of every fact, and the convoy's records ending.
    let steps: Vec<Step> = (0..count)
        .map(|_| {
            let fact = tc.draw(gs::integers::<usize>().min_value(0).max_value(FACTS - 1));
            match tc.draw(gs::integers::<u8>().min_value(0).max_value(6)) {
                0..=2 => Step::Raise { fact, reason: tc.draw(gs::integers::<usize>().min_value(0).max_value(1)) },
                3 => Step::Clear { fact },
                4 | 5 => Step::Acknowledge { fact },
                _ => Step::ClearWithin,
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
                    let key = key(fact);
                    let began_at = model.episode(&key).await;
                    // On the old store the old Message controller owns a held delivery's reason.
                    let reason = if fact == DELIVERY && !engine { 0 } else { reason };
                    let attention = attention(fact, ["held", "still held"][reason], began_at);
                    implementation.attentions().raise(NAMESPACE, &attention).await.expect("raise");
                    // An acknowledgement answers every raise of its episode.
                    if model.answered.get(&key).is_none_or(|(answered, _)| *answered != began_at)
                        && model.open.insert(key.clone(), attention.reason.clone()).is_none_or(|before| before != attention.reason)
                    {
                        model.raised_at.remove(&key);
                    }
                }
                Step::Clear { fact } => {
                    let key = key(fact);
                    implementation.attentions().clear(NAMESPACE, key.0, &key.1).await.expect("clear");
                    model.end(&key);
                }
                Step::Acknowledge { fact } => {
                    let key = key(fact);
                    implementation.acknowledge(&attention(fact, "", DateTime::UNIX_EPOCH).fact).await;
                    if let Some(reason) = model.open.remove(&key) {
                        model.raised_at.remove(&key);
                        model.answered.insert(key.clone(), (model.episodes[&key].began_at, reason));
                    }
                }
                Step::ClearWithin => {
                    let convoy = Subject::convoy("demo");
                    implementation.attentions().clear_within(NAMESPACE, &convoy).await.expect("clear within");
                    for fact in 0..FACTS {
                        let key = key(fact);
                        if key.1.within(&convoy) {
                            model.end(&key);
                        }
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

impl Implementation {
    /// How many records this implementation stores, open or not.
    async fn stored(&self) -> usize {
        match self {
            Implementation::Old { backend, .. } => backend.using::<Demand>(NAMESPACE).list().await.expect("demands").items.len(),
            Implementation::Engine { primitives, .. } => primitives.messages().expect("messages").len(),
        }
    }
}

async fn an_acknowledgement_answers_its_episode(implementation: Implementation) {
    let attentions = implementation.attentions();
    let first = episode_start().await;
    let hold = attention(0, "held", first);
    let (sender, subject) = (hold.fact.sender(), hold.fact.subject());
    let open = || async { attentions.open(NAMESPACE, Some(sender), Some(&subject)).await.expect("open") };
    attentions.raise(NAMESPACE, &hold).await.expect("raise");
    implementation.acknowledge(&hold.fact).await;
    attentions.raise(NAMESPACE, &hold).await.expect("raise again");
    attentions.raise(NAMESPACE, &attention(0, "still held", first)).await.expect("raise for another reason");
    assert!(open().await.is_empty(), "the acknowledgement answers every raise of its episode");
    assert_eq!(attentions.acknowledged(NAMESPACE, sender, &subject, first).await.expect("acknowledged").as_deref(), Some("held"));
    let stored = implementation.stored().await;
    attentions.clear(NAMESPACE, sender, &subject).await.expect("clear");
    assert_eq!(implementation.stored().await, stored, "clearing an acknowledged attention writes nothing");
    let second = episode_start().await;
    assert_eq!(attentions.acknowledged(NAMESPACE, sender, &subject, second).await.expect("acknowledged"), None);
    attentions.raise(NAMESPACE, &attention(0, "held", second)).await.expect("raise a new episode");
    assert_eq!(open().await.len(), 1, "a new episode is told again");
    attentions.clear(NAMESPACE, sender, &subject).await.expect("clear");
    assert!(open().await.is_empty(), "clearing withdraws the open one");
}

// Behaviour: an acknowledgement answers its episode's raises, a clear leaves it as it is, and the
// next episode is told again.
#[tokio::test]
async fn the_old_store_answers_an_episode_once() {
    an_acknowledgement_answers_its_episode(Implementation::old().await).await;
}

#[tokio::test]
async fn the_engine_answers_an_episode_once() {
    an_acknowledgement_answers_its_episode(Implementation::engine().await).await;
}

async fn nothing_open_is_left_within(implementation: Implementation) {
    let attentions = implementation.attentions();
    let began_at = episode_start().await;
    for fact in 0..FACTS {
        attentions.raise(NAMESPACE, &attention(fact, "held", began_at)).await.expect("raise");
    }
    implementation.acknowledge(&attention(0, "", began_at).fact).await;
    attentions.clear_within(NAMESPACE, &Subject::convoy("demo")).await.expect("clear within");
    let left: Vec<_> = attentions.open(NAMESPACE, None, None).await.expect("open").into_iter().map(|open| open.subject).collect();
    assert_eq!(left, [key(2).1], "only the standing role's drift is not under the convoy");
    if let Implementation::Old { backend, .. } = &implementation {
        let names: Vec<_> =
            backend.using::<Demand>(NAMESPACE).list().await.expect("demands").items.into_iter().map(|d| d.metadata.name).collect();
        assert_eq!(names, ["ensure-config-drift-standing-gui"], "the acknowledged hold's demand is deleted too");
    }
}

// Behaviour: clearing within a convoy ends every attention about it and its records, acknowledged
// or open, and none about another record.
#[tokio::test]
async fn the_old_store_clears_every_attention_within_a_convoy() {
    nothing_open_is_left_within(Implementation::old().await).await;
}

#[tokio::test]
async fn the_engine_clears_every_attention_within_a_convoy() {
    nothing_open_is_left_within(Implementation::engine().await).await;
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
// readers with the replicated entries: the signal fires when Messages are applied, with no local
// write, and not for entries of another kind.
#[tokio::test]
async fn another_homes_attentions_signal_their_readers_when_they_arrive() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (a_store, a) = engine_at("home-a", records().await, &dir);
    let (b_store, b) = engine_at("home-b", records().await, &dir);
    let mut cursor = None;
    follow(&a_store, "home-a", &b_store, &mut cursor);
    let mut signal = b.changes().expect("the engine signals its readers");
    a.register_person(flotilla_schema::Timestamp::from_datetime(&Utc::now())).await.expect("a person at home a");
    follow(&a_store, "home-a", &b_store, &mut cursor);
    assert!(signal.try_recv().is_err(), "another kind's entries are no attention: its readers are not signalled");
    let hold = attention(0, "held", episode_start().await);
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
    assert!(b.acknowledged(NAMESPACE, sender, &subject, hold.began_at).await.expect("acknowledged at home b").is_some());
}
