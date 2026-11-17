//! What holds an ended convoy (#3108 retirement): each dependent that names the convoy on its own
//! record holds it until its own writer releases it.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use flotilla_engine_convoys::{
    convoy_records::EngineConvoyRecords,
    convoys::{Convoys, Retirement},
    holds::holds,
    retire,
};
use flotilla_engine_messages::awaiting::{AwaitingReplies, OpenAwaiting, STALL};
use flotilla_engine_store::{watch::EngineWatch, StoreLog};
use flotilla_protocol::PrincipalRef;
use flotilla_provisioning::{
    engine::{account::binding_spec, LifecycleHome, Lifecycles, Step},
    stored::{
        self,
        account::{AccountBinding, BindingState},
        checkout::{CheckoutKind, CheckoutRequest, Source},
        forge_observation::{ChangeRequestRecord, ChangeRequestState},
        Holder,
    },
};
use flotilla_resources::{
    access_delivery::access_key,
    account_access::{requester_address, AccessName, AccountBindingSpec, CredentialName, EpochEntry, Selection},
    address::{stored_change_request_address, subject_for},
    convoy::{admission::AdmissionDocument, engine::seat_address},
    convoy_records::ConvoyRecords,
    vessel_resource_name,
    workflow_template::single_agent_workflow_spec,
    AccessDeliverySpec, SystemClock, WorkflowSnapshot,
};
use flotilla_schema::{Address, Frozen, Kind, NewChange, Records, State, Timestamp};
use flotilla_schema_sqlite::stream::Service;
use flotilla_state_machines::{
    kinds::{stock, Catalog},
    machines::{
        awaiting_reply::{Awaited, AwaitingReplySpec},
        convoy::{
            Abandon, AbandonCause, AdmissionBytes, AdmittedPromise, Convoy, ConvoyId, ConvoyLabel, ConvoyRequest, Link, LinkSource,
            Priority, PromiseId, PromiseSource, Seat, SeatName, VesselClass,
        },
        message::{Message, MessageBody, MessageRef, MessageSpec, MessageState, Relation},
        vessel::{VesselKind, VesselSpec},
    },
    placement::{Host, Lease, Request},
    Home, Ref, Resource, Version,
};

const HOME: &str = "home-a";

fn project() -> Address {
    Address::project("fleet").expect("project")
}

fn operator() -> Address {
    project().child(flotilla_address::Kind::Role, "operator").expect("operator")
}

fn document() -> AdmissionDocument {
    let workflow = single_agent_workflow_spec();
    AdmissionDocument {
        workflow_snapshot: WorkflowSnapshot {
            cascade: None,
            exit: workflow.exit,
            turn_delivery: workflow.turn_delivery,
            vessels: workflow.vessels,
            stall_nudges: workflow.stall_nudges,
            supervision: workflow.supervision,
        },
        placement_decision: None,
        placement_policy: None,
        role: "fixer".into(),
        ensure_admission: None,
        naming: None,
        dispatching_principal_ref: PrincipalRef { namespace: "flotilla".into(), name: "robert".into() },
        continuation: None,
        inputs: BTreeMap::new(),
        repositories: Vec::new(),
        r#ref: None,
        adopted_checkout_refs: BTreeMap::new(),
        subjects: Vec::new(),
        instruction: None,
        workflow_template: None,
        placement_snapshot: None,
        vessel_placements: BTreeMap::new(),
        brief_artifacts: false,
        adopted_checkout: None,
        provenance: BTreeMap::new(),
    }
}

fn request() -> ConvoyRequest {
    ConvoyRequest {
        launch_of: "fix-it".into(),
        workflow_ref: "single".into(),
        admission: Frozen::new(AdmissionBytes::new(document().encode().expect("encode"))).expect("freeze"),
        project: project(),
        promises: [(
            PromiseId(0),
            AdmittedPromise {
                label: "template/coder/decision-ledger".into(),
                kind: "decision-ledger".into(),
                target: project(),
                source: PromiseSource::Template,
                owner: SeatName::new("coder", ""),
            },
        )]
        .into(),
        duties: Default::default(),
        seats: vec![Seat {
            failed: None,
            retired: false,
            replacements: 0,
            role: "coder".into(),
            seat: String::new(),
            parent: None,
            supervised_by: operator(),
        }],
        quota: [(VesselClass::new("standard"), 1)].into(),
        priority: Priority(1),
        kinds: Catalog::resolve(&stock(), &[]).expect("stock").freeze(["decision-ledger"]).expect("freeze"),
    }
}

struct Fixture {
    service: Arc<Service>,
    convoys: Arc<Convoys>,
    launch: String,
}

impl Fixture {
    fn new(dir: &tempfile::TempDir) -> Self {
        let registry = stored::decided_registry().expect("registry");
        let service = Arc::new(Service::open(&dir.path().join("decided.sqlite"), HOME, registry).expect("store"));
        let convoys = Arc::new(Convoys::open(HOME, &service).expect("open"));
        convoys.world(|world| {
            world.projects.put(&Ref::new(project().as_str()), ());
            world.authority.put(&Ref::new("granted"), true);
        });
        let reserved = convoys.reserve("fix-it", Timestamp(1_000_000)).expect("reserved");
        convoys.create_reserved(&reserved, request(), Timestamp(1_000_000)).expect("admitted");
        let launch = convoys.get("fix-it").expect("launch").name().to_owned();
        Fixture { service, convoys, launch }
    }

    fn holds(&self) -> BTreeSet<String> {
        let convoy = self.convoys.get("fix-it").expect("launch");
        self.service.with_store(|store| holds(&mut store.read()?, &convoy, HOME)).expect("holds")
    }

    fn insert<K: flotilla_schema::Kind>(&self, name: &str, value: &K) {
        self.service
            .with_store(|store| {
                let mut write = store.write()?;
                write.insert(name, value, &NewChange::new("test", Timestamp(2_000_000)))?;
                write.commit()
            })
            .expect("inserted");
    }

    fn seat(&self) -> Address {
        seat_address(&project(), &self.launch, "coder").expect("seat")
    }

    fn link(&self, number: u64) -> Address {
        let change = stored_change_request_address("github.com", "flotilla-org/flotilla", number, &[]).expect("change request");
        let link =
            Link { subject: change.clone(), source: LinkSource::Claim, by: SeatName::new("coder", ""), authority: Ref::new("granted") };
        self.convoys.apply("fix-it", link, Timestamp(3_000_000)).expect("linked");
        change
    }

    fn observe(&self, change: &Address, state: ChangeRequestState) {
        let subject = subject_for(change).expect("subject");
        let record = ChangeRequestRecord {
            service: subject.source.service,
            scope: subject.source.scope,
            number: subject.id.parse().expect("number"),
            title: None,
            author: None,
            state: Some(state),
            review_decision: None,
            review_requested_from_owner: None,
            head_sha: None,
            head_branch: None,
            checks: None,
            actionable_at_head: None,
            mergeable: None,
            failing_since: None,
        };
        self.insert(&format!("cr-{}", record.number), &record);
    }

    fn message(&self, name: &str, state: MessageState) {
        let spec = MessageSpec::new(operator(), self.seat(), Relation::Supervisor, MessageBody::text("hello").expect("body"));
        self.insert(name, &Resource::<Message>::stored(String::new(), spec, State::new(state, Timestamp(2_000_000)), Version(0)));
    }

    fn binding(&self, name: &str, role: &str, state: BindingState) {
        let mut epochs = Records::new();
        epochs.append(1, EpochEntry { epoch: "1".into(), observed_at: Timestamp(2_000_000) }).expect("epoch");
        let old = AccountBindingSpec {
            requester: requester_address(Some("fleet"), &self.launch, role).expect("requester"),
            item: "claude-code".into(),
            requirement: AccessName("claude-subscription".into()),
            credential: CredentialName("cred".into()),
            account: None,
            grants: BTreeMap::new(),
            candidates: BTreeSet::from([CredentialName("cred".into())]),
            exclusions: BTreeMap::new(),
            selection: Selection::Pin { pinned_by: "test".into() },
            epochs,
            opened_at: Timestamp(2_000_000),
            state: State::new(flotilla_resources::account_access::BindingState::Open, Timestamp(2_000_000)),
        };
        let binding =
            Resource::<AccountBinding>::stored(String::new(), binding_spec(&old), State::new(state, Timestamp(2_000_000)), Version(0));
        self.insert(name, &binding);
    }
}

// Behaviour: every dependent that names the convoy on its own record holds it while its writer has
// not released it, found through its own index, and each is named; a released one holds nothing.
#[test]
fn each_unreleased_dependent_holds_the_convoy() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fixture = Fixture::new(&dir);
    assert_eq!(fixture.holds(), BTreeSet::new(), "a convoy no dependent names is held by nothing");

    // Messages to a seat: pending holds, settled does not.
    fixture.message("m-pending", MessageState::Pending);
    fixture.message("m-settled", MessageState::Settled { receiver: None, delivered_at: None });

    // A stall about the seat.
    let awaiting = AwaitingReplies::open(HOME, &fixture.service).expect("awaiting");
    let stall = AwaitingReplySpec {
        message: MessageRef::new(format!("{}/message:m1", operator()), HOME),
        sender: fixture.seat(),
        recipient: operator(),
        subject: Some(fixture.seat()),
        kind: STALL.into(),
        awaited: Awaited::Reply,
        deadline: None,
        report: None,
        stall_reason: None,
        proposed_disposition: None,
    };
    assert!(awaiting.record(stall, Timestamp(2_000_000)).expect("stall recorded"));

    // Linked change requests: open and unobserved hold; merged and closed do not.
    let open = fixture.link(1);
    fixture.observe(&open, ChangeRequestState::Open);
    let unobserved = fixture.link(2);
    for (number, state) in [(3, ChangeRequestState::Merged), (4, ChangeRequestState::Closed)] {
        let change = fixture.link(number);
        fixture.observe(&change, state);
    }

    // Checkouts it holds: a live one holds, a released one does not.
    let checkouts = LifecycleHome::new(HOME, Home::with_log(HOME, StoreLog::new(fixture.service.store())).expect("home"));
    let checkout = |name: &str| CheckoutRequest {
        launch_of: name.into(),
        launch: 0,
        repository: "repo".into(),
        environment: flotilla_schema::Ref::new("env".to_owned(), HOME),
        branch: "feature".into(),
        base: None,
        path: "/w".into(),
        source: Source::Worktree,
        clone: Some(flotilla_schema::Ref::new("clone", HOME)),
        holder: Some(Holder::convoy(&fixture.launch)),
    };
    let held: Arc<dyn Lifecycles<CheckoutKind>> = Arc::new(checkouts);
    let live = held.request("ck-live", checkout("ck-live"), Timestamp(2_000_000)).expect("requested").name().to_owned();
    let released = held.request("ck-done", checkout("ck-done"), Timestamp(2_000_000)).expect("requested").name().to_owned();
    held.step(&released, &Step::Released, Timestamp(2_500_000)).expect("released");

    // A vessel of its work.
    let work = &document().workflow_snapshot.vessels[0].name;
    let vessels: Arc<dyn Lifecycles<VesselKind>> =
        Arc::new(LifecycleHome::new(HOME, Home::with_log(HOME, StoreLog::new(fixture.service.store())).expect("home")));
    let lease = Lease {
        request: Request {
            convoy: ConvoyId::new(&fixture.launch, HOME),
            class: VesselClass::new("standard"),
            shape: BTreeMap::new(),
            priority: Priority(1),
        },
        host: Host::new("host-a"),
    };
    let launch_of = vessel_resource_name(flotilla_resources::convoy::launch_of(&fixture.launch), work);
    let vessel = vessels.request(&launch_of, VesselSpec::of(launch_of.clone(), lease), Timestamp(2_000_000)).expect("vessel");

    // Access delivered to its crew member.
    let delivery = AccessDeliverySpec {
        access: access_key("fleet", &fixture.launch, work, "coder"),
        place: "env".into(),
        harness: "claude-code".into(),
        home: "/home/coder".into(),
        needs: BTreeMap::new(),
        retry: None,
    };
    fixture.insert("access-1", &delivery);

    // Account bindings of its crew: open holds, closed does not.
    fixture.binding("binding-open", "coder", BindingState::Open);
    fixture.binding("binding-closed", "coder", BindingState::Closed { reason: "rebind".into(), delivered_until: None });

    let expected: BTreeSet<String> = [
        "message:m-pending".to_owned(),
        format!("awaiting-reply:{}", fixture.service.with_store(first_awaiting).expect("stall")),
        format!("change-request:{open}"),
        format!("change-request:{unobserved}"),
        format!("checkout:{live}"),
        format!("vessel:{}", vessel.name()),
        "access-delivery:access-1".to_owned(),
        "account-binding:binding-open".to_owned(),
    ]
    .into();
    assert_eq!(fixture.holds(), expected);
}

impl Fixture {
    fn checkouts(&self) -> Arc<dyn Lifecycles<CheckoutKind>> {
        Arc::new(LifecycleHome::new(HOME, Home::with_log(HOME, StoreLog::new(self.service.store())).expect("home")))
    }

    /// A worktree checkout held by the convoy.
    fn checkout(&self, checkouts: &Arc<dyn Lifecycles<CheckoutKind>>, name: &str) -> String {
        let request = CheckoutRequest {
            launch_of: name.into(),
            launch: 0,
            repository: "repo".into(),
            environment: flotilla_schema::Ref::new("env".to_owned(), HOME),
            branch: "feature".into(),
            base: None,
            path: "/w".into(),
            source: Source::Worktree,
            clone: Some(flotilla_schema::Ref::new("clone", HOME)),
            holder: Some(Holder::convoy(&self.launch)),
        };
        checkouts.request(name, request, Timestamp(2_000_000)).expect("requested").name().to_owned()
    }

    fn abandon(&self) {
        let abandon = Abandon {
            by: SeatName::new("coder", ""),
            cause: AbandonCause::Operator,
            reason: "stopped".into(),
            authority: Ref::new("granted"),
        };
        self.convoys.apply("fix-it", abandon, Timestamp(4_000_000)).expect("abandoned");
    }

    fn label(&self) -> ConvoyLabel {
        self.convoys.launch(&self.launch).expect("launch").label()
    }
}

// Behaviour: an open convoy does not retire; an ended one waits for its holds, then retires, and
// reads as absent to its readers.
#[test]
fn an_ended_convoy_retires_once_its_holds_release() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fixture = Fixture::new(&dir);
    let checkouts = fixture.checkouts();
    let checkout = fixture.checkout(&checkouts, "ck");
    let records =
        EngineConvoyRecords::new(HOME, Arc::clone(&fixture.service), Arc::default(), OpenAwaiting::open(&fixture.service).expect("open"));

    assert_eq!(fixture.convoys.retire(&fixture.launch, Timestamp(3_000_000)), Ok(Retirement::NotEnded));
    fixture.abandon();
    assert_eq!(
        fixture.convoys.retire(&fixture.launch, Timestamp(5_000_000)),
        Ok(Retirement::Held([format!("checkout:{checkout}")].into()))
    );
    assert_eq!(fixture.label(), ConvoyLabel::Abandoned);
    assert!(records.get(&fixture.launch, None).expect("read").is_some(), "an ended, held convoy still reads");

    checkouts.step(&checkout, &Step::Released, Timestamp(6_000_000)).expect("released");
    assert_eq!(fixture.convoys.retire(&fixture.launch, Timestamp(7_000_000)), Ok(Retirement::Retired));
    assert_eq!(fixture.label(), ConvoyLabel::Retired);
    assert_eq!(records.get(&fixture.launch, None).expect("read"), None, "a retired convoy reads as absent");
    assert_eq!(records.at(HOME).expect("listing").records.len(), 0);
}

// Behaviour: the convoy's home retires it on its own when its last hold releases, woken by the
// release's write; nothing is called and nothing polls.
#[tokio::test]
async fn the_home_retires_a_convoy_when_its_last_hold_releases() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fixture = Fixture::new(&dir);
    let checkouts = fixture.checkouts();
    let checkout = fixture.checkout(&checkouts, "ck");
    fixture.abandon();
    let mut convoy_writes = EngineWatch::start(Arc::clone(&fixture.service), [<Resource<Convoy> as Kind>::CODE]).expect("watch");
    // It starts by trying the ended convoy, which its checkout holds.
    let follower = retire::follow(Arc::clone(&fixture.convoys), Arc::new(SystemClock)).expect("follow");
    assert_eq!(fixture.label(), ConvoyLabel::Abandoned);

    checkouts.step(&checkout, &Step::Released, Timestamp(6_000_000)).expect("released");
    let retired = async {
        while fixture.label() != ConvoyLabel::Retired {
            convoy_writes.next(|_, _| Ok(())).await.expect("a convoy write");
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(30), retired).await.expect("retired once its checkout released");
    follower.abort();
}

/// The one stall record's name.
fn first_awaiting(store: &mut flotilla_schema_sqlite::Store) -> Result<String, flotilla_schema_sqlite::StoreError> {
    let mut read = store.read()?;
    let page = read.scan::<Resource<flotilla_state_machines::machines::awaiting_reply::AwaitingReply>>(HOME, None, 1)?;
    let id = page.ids[0];
    Ok(read.identity(id)?.expect("identity").0.name)
}
