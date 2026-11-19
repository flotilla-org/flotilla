//! What holds an ended convoy (#3108 retirement): each dependent that names the convoy on its own
//! record holds it until its own writer releases it.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use flotilla_address::record_name;
use flotilla_engine_convoys::{
    convoy_records::EngineConvoyRecords,
    convoys::{Convoys, Retirement},
    holds::holds,
    retire,
};
use flotilla_engine_messages::{
    awaiting::{AwaitingReplies, OpenAwaiting, STALL},
    sweep::OpenMessages,
    MessagePrimitives,
};
use flotilla_engine_messages_testkit::{designate_fleet, intent, EngineMessagesHome};
use flotilla_engine_store::{watch::EngineWatch, StoreLog};
use flotilla_protocol::PrincipalRef;
use flotilla_provisioning::{
    engine::{
        access_deliveries::EngineAccessDeliveries, account::EngineAccounts, observations::EngineObservations,
        standing_roles::EngineStandingRoles, LifecycleHome, Lifecycles, Step,
    },
    stored::{
        self,
        checkout::{CheckoutKind, CheckoutRequest, Source},
        Holder,
    },
};
use flotilla_resources::{
    access_delivery::access_key,
    account_access::{
        binding_name, requester_address, AccessName, AccountBindingSpec, AccountRecords, BindingState as OldBindingState, CredentialName,
        EpochEntry, PendingBinding, Selection,
    },
    address::{stored_change_request_address, subject_for},
    convoy::{
        admission::AdmissionDocument,
        engine::{launch_address, seat_address},
    },
    convoy_records::ConvoyEnd,
    convoy_records::ConvoyRecords,
    engine_records::KindRecords,
    standing_roles::{RunningValues, StandingRoles},
    vessel_resource_name,
    workflow_template::single_agent_workflow_spec,
    AccessDelivery, AccessDeliverySpec, ChangeRequest, ChangeRequestReviewObservation, ChangeRequestSpec, ChangeRequestStatus,
    ConvoyEnsureSpec, MessageRelation, Observation, ObservedChangeRequestState, SystemClock, WorkflowSnapshot, ENSURED_FROM_ANNOTATION,
};
use flotilla_schema::{Address, Frozen, Kind, Records, State, Timestamp};
use flotilla_schema_sqlite::stream::Service;
use flotilla_state_machines::{
    kinds::{stock, Catalog},
    machines::{
        awaiting_reply::{Awaited, AwaitingReplySpec},
        convoy::{
            Abandon, AbandonCause, AdmissionBytes, AdmittedPromise, Authority, Convoy, ConvoyId, ConvoyLabel, ConvoyRequest, Deliver, Keep,
            Link, LinkSource, Priority, Progress, PromiseId, PromiseSource, Reject, Seat, SeatName, Submit, VesselClass,
        },
        message::{DropReason, MessageRef, MessageState},
        vessel::{VesselKind, VesselSpec},
    },
    placement::{Host, Lease, Request},
    Home, Ref, Resource,
};
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;

const HOME: &str = "home-a";
const NAMESPACE: &str = "flotilla";

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

/// The pr promises a convoy is admitted with, by id, beside its decision ledger (id 0).
const PRS: [u32; 4] = [1, 2, 3, 4];

fn request(document: AdmissionDocument) -> ConvoyRequest {
    let promise = |label: String, kind: &str| AdmittedPromise {
        label,
        kind: kind.into(),
        target: project(),
        source: PromiseSource::Template,
        owner: SeatName::new("coder", ""),
    };
    let ledger = (PromiseId(0), promise("template/coder/decision-ledger".into(), "decision-ledger"));
    let prs = PRS.map(|id| (PromiseId(id), promise(format!("pr-{id}"), "pr")));
    ConvoyRequest {
        launch_of: "fix-it".into(),
        workflow_ref: "single".into(),
        admission: Frozen::new(AdmissionBytes::new(document.encode().expect("encode"))).expect("freeze"),
        project: project(),
        promises: [ledger].into_iter().chain(prs).collect(),
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
        priority: Priority(1),
        kinds: Catalog::resolve(&stock(), &[]).expect("stock").freeze(["decision-ledger", "pr"]).expect("freeze"),
    }
}

struct Fixture {
    service: Arc<Service>,
    convoys: Arc<Convoys>,
    launch: String,
}

impl Fixture {
    fn new(dir: &tempfile::TempDir) -> Self {
        Self::admitted(dir, document())
    }

    /// A generation of standing role `ensure`, as the ensure pass admits one.
    fn standing(dir: &tempfile::TempDir, ensure: &str) -> Self {
        let spec = ConvoyEnsureSpec::builder()
            .project_ref("fleet".into())
            .role("coder".into())
            .workflow_ref("single".into())
            .repositories(Vec::new())
            .build();
        let provenance = BTreeMap::from([(ENSURED_FROM_ANNOTATION.to_owned(), ensure.to_owned())]);
        Self::admitted(dir, AdmissionDocument { ensure_admission: Some(spec), provenance, ..document() })
    }

    fn admitted(dir: &tempfile::TempDir, document: AdmissionDocument) -> Self {
        let registry = stored::decided_registry().expect("registry");
        let service = Arc::new(Service::open(&dir.path().join("decided.sqlite"), HOME, registry).expect("store"));
        let convoys = Arc::new(Convoys::open(HOME, &service).expect("open"));
        convoys.world(|world| {
            world.projects.put(&Ref::new(project().as_str()), ());
            world.authority.put(&Ref::new("granted"), true);
        });
        let reserved = convoys.reserve("fix-it", Timestamp(1_000_000)).expect("reserved");
        convoys.create_reserved(&reserved, request(document), Timestamp(1_000_000)).expect("admitted");
        let launch = convoys.get("fix-it").expect("launch").name().to_owned();
        Fixture { service, convoys, launch }
    }

    fn holds(&self) -> BTreeSet<String> {
        let convoy = self.convoys.get("fix-it").expect("launch");
        self.service.with_store(|store| holds(&mut store.read()?, &convoy, HOME)).expect("holds")
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

    /// Submits change request `number` to pr promise `id`, as its owner does; returns its address.
    fn submit(&self, id: u32, number: u64) -> Address {
        let change = stored_change_request_address("github.com", "flotilla-org/flotilla", number, &[]).expect("change request");
        let durable = format!("{change}@durable");
        self.convoys.world(|world| world.durable.put(&Ref::new(durable.as_str()), true));
        let submit = Submit {
            promise: PromiseId(id),
            by: SeatName::new("coder", ""),
            address: change.clone(),
            revision: "c1".into(),
            durable: Ref::new(durable),
        };
        self.convoys.apply("fix-it", submit, Timestamp(3_000_000)).expect("submitted");
        change
    }

    /// Records `change` observed in `state`, as its source's observer writes it.
    async fn observe(&self, change: &Address, state: ObservedChangeRequestState) {
        let observations = EngineObservations::open(Arc::clone(&self.service), Arc::new(SystemClock)).expect("observations");
        let subject = subject_for(change).expect("subject");
        let name = record_name(change).expect("record name");
        let spec = ChangeRequestSpec::builder()
            .service(subject.source.service)
            .scope(subject.source.scope)
            .number(subject.id.parse().expect("number"))
            .observing_authority("test".into())
            .build();
        let observed = KindRecords::<ChangeRequest>::apply(&observations, &name, None, &BTreeMap::new(), &BTreeMap::new(), &spec)
            .await
            .expect("observed");
        let status = ChangeRequestStatus {
            title: Default::default(),
            author: Default::default(),
            review_decision: Default::default(),
            review_requested_from_owner: Default::default(),
            state: Observation::known(state, chrono::Utc::now()),
            head_sha: Default::default(),
            checks: Default::default(),
            review: ChangeRequestReviewObservation { actionable_at_head: Default::default() },
            mergeable: Default::default(),
        };
        KindRecords::<ChangeRequest>::set_status(&observations, &name, observed.version, &status).await.expect("observed status");
    }

    /// Opens crew member `role`'s binding at `opened_at` as admission does, and ends it when
    /// `closed`; returns its record name.
    async fn binding(&self, accounts: &EngineAccounts, role: &str, opened_at: Timestamp, closed: bool) -> String {
        let mut epochs = Records::new();
        epochs.append(1, EpochEntry { epoch: "1".into(), observed_at: opened_at }).expect("epoch");
        let requirement = AccessName("claude-subscription".into());
        let spec = AccountBindingSpec {
            requester: requester_address(Some("fleet"), &self.launch, role).expect("requester"),
            item: "claude-code".into(),
            requirement: requirement.clone(),
            credential: CredentialName("cred".into()),
            account: None,
            grants: BTreeMap::new(),
            candidates: BTreeSet::from([CredentialName("cred".into())]),
            exclusions: BTreeMap::new(),
            selection: Selection::Pin { pinned_by: "test".into() },
            epochs,
            opened_at,
            state: State::new(OldBindingState::Open, opened_at),
        };
        let name = binding_name(&self.launch, role, &requirement);
        accounts.open(NAMESPACE, vec![PendingBinding { name, spec }]).await.expect("opened");
        let bindings = accounts.convoy_bindings(NAMESPACE, &self.launch, true).await.expect("bindings");
        let opened = bindings.into_iter().find(|binding| binding.spec.opened_at == opened_at).expect("the binding opened");
        if closed {
            let state = OldBindingState::Closed { reason: "rebind".into(), delivered_until: None };
            accounts.end(NAMESPACE, &opened, state, Timestamp(opened_at.0 + 1)).await.expect("closed");
        }
        opened.name
    }
}

// Behaviour: every dependent that names the convoy on its own record holds it while its writer has
// not released it, found through its own index, and each is named; a released one holds nothing.
#[tokio::test]
async fn each_unreleased_dependent_holds_the_convoy() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fixture = Fixture::new(&dir);
    assert_eq!(fixture.holds(), BTreeSet::new(), "a convoy no dependent names is held by nothing");

    // Messages to a seat, admitted as the seat's sender's are: pending holds; one delivered and
    // settled does not.
    let backend = ResourceBackend::new(InMemoryBackend::default());
    designate_fleet(&backend, NAMESPACE).await;
    let messages = EngineMessagesHome::over(Arc::clone(&fixture.service), NAMESPACE, &backend);
    let receiver = format!("fleet/{}/work/coder", fixture.launch);
    assert_eq!(messages.primitives().inbox_for(NAMESPACE, &receiver).await.expect("inbox"), fixture.seat());
    for record in ["m-pending", "m-settled"] {
        let intent = intent("fleet/operator", &receiver, MessageRelation::Supervisor, "hello");
        messages.primitives().admit(record, &intent).await.expect("admitted");
    }
    messages.deliver_turn(&receiver, "m-settled", "session").await;

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

    // Its pr promises: a Submitted one holds, as its change request is open; one never submitted,
    // one whose change request closed (back to Pending) and one kept at its merge do not.
    let [submitted, rejected, kept, _] = PRS;
    let open = fixture.submit(submitted, 1);
    fixture.submit(rejected, 2);
    fixture.convoys.apply("fix-it", Reject { promise: PromiseId(rejected) }, Timestamp(3_100_000)).expect("rejected");
    fixture.submit(kept, 3);
    fixture.convoys.apply("fix-it", Keep::Observed { promise: PromiseId(kept), submission: 1 }, Timestamp(3_100_000)).expect("kept");
    // A change request linked with no promise holds nothing, open or with no record.
    let linked = fixture.link(5);
    fixture.observe(&linked, ObservedChangeRequestState::Open).await;
    fixture.link(6);

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
    let deliveries = EngineAccessDeliveries::open(Arc::clone(&fixture.service), Arc::new(SystemClock)).expect("deliveries");
    KindRecords::<AccessDelivery>::apply(&deliveries, "access-1", None, &BTreeMap::new(), &BTreeMap::new(), &delivery)
        .await
        .expect("delivered");

    // Account bindings of its crew: open holds, closed does not.
    let accounts = EngineAccounts::open(Arc::clone(&fixture.service), Arc::new(SystemClock)).expect("accounts");
    fixture.binding(&accounts, "coder", Timestamp(2_000_000), true).await;
    let binding = fixture.binding(&accounts, "coder", Timestamp(2_100_000), false).await;

    let expected: BTreeSet<String> = [
        format!("message:{}/message:m-pending", fixture.seat()),
        format!("awaiting-reply:{}", fixture.service.with_store(first_awaiting).expect("stall")),
        format!("change-request:{open}"),
        format!("checkout:{live}"),
        format!("vessel:{}", vessel.name()),
        "access-delivery:access-1".to_owned(),
        format!("account-binding:{binding}"),
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
    // Read by name, a retired convoy still says how and when it ended, from its end's log entry;
    // a listing leaves it out.
    let retired = records.get(&fixture.launch, None).expect("read").expect("a retired convoy reads by name");
    let abandoned_at = Timestamp(4_000_000).to_datetime().expect("a time");
    assert_eq!(retired.ended, Some((ConvoyEnd::Abandoned { reason: "stopped".into() }, abandoned_at)));
    assert!(retired.retired);
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

// Behaviour (#3108 d8): a convoy that delivered at its change request's merge retires when its
// checkout releases later, though the observer collected the change request's record in between:
// the pr promise its merge kept holds nothing, and the record is not read.
#[tokio::test]
async fn a_convoy_retires_after_its_merge_once_its_change_request_record_is_collected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fixture = Fixture::new(&dir);
    let checkouts = fixture.checkouts();
    let checkout = fixture.checkout(&checkouts, "ck");
    let change = fixture.submit(PRS[0], 7);
    assert!(fixture.holds().contains(&format!("change-request:{change}")), "an open change request holds it");

    fixture.observe(&change, ObservedChangeRequestState::Merged).await;
    fixture.convoys.apply("fix-it", Keep::Observed { promise: PromiseId(PRS[0]), submission: 1 }, Timestamp(3_100_000)).expect("kept");
    for id in [0, PRS[1], PRS[2], PRS[3]] {
        let verdict = Keep::Verdict {
            promise: PromiseId(id),
            by: SeatName::new("operator", ""),
            authority: Authority::Operator,
            reason: "done".into(),
        };
        fixture.convoys.apply("fix-it", verdict, Timestamp(3_100_000)).expect("kept by verdict");
    }
    fixture.convoys.apply("fix-it", Deliver { disposition: "merged".into() }, Timestamp(3_200_000)).expect("delivered");
    let observations = EngineObservations::open(Arc::clone(&fixture.service), Arc::new(SystemClock)).expect("observations");
    let name = record_name(&change).expect("record name");
    KindRecords::<ChangeRequest>::delete(&observations, &name).await.expect("collected");
    assert_eq!(fixture.holds(), BTreeSet::from([format!("checkout:{checkout}")]));

    let mut convoy_writes = EngineWatch::start(Arc::clone(&fixture.service), [<Resource<Convoy> as Kind>::CODE]).expect("watch");
    let follower = retire::follow(Arc::clone(&fixture.convoys), Arc::new(SystemClock)).expect("follow");
    assert_eq!(fixture.label(), ConvoyLabel::Delivered);
    checkouts.step(&checkout, &Step::Released, Timestamp(6_000_000)).expect("released");
    let retired = async {
        while fixture.label() != ConvoyLabel::Retired {
            convoy_writes.next(|_, _| Ok(())).await.expect("a convoy write");
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(30), retired).await.expect("retired once its checkout released");
    follower.abort();
}

// Behaviour (#3108 ruling 1): an abandoned convoy's Submitted pr promise holds it while its PR is
// open. The world still resolves the promise once the convoy ended, a close rejecting it and a
// merge keeping it with the convoy left Abandoned, and the convoy's home then retires it.
#[tokio::test]
async fn an_abandoned_convoys_submitted_pr_holds_it_until_the_pr_closes_or_merges() {
    for (state, resolved) in [(ObservedChangeRequestState::Closed, Progress::Pending), (ObservedChangeRequestState::Merged, Progress::Kept)]
    {
        let dir = tempfile::tempdir().expect("tempdir");
        let fixture = Fixture::new(&dir);
        let change = fixture.submit(PRS[0], 7);
        fixture.abandon();
        let mut convoy_writes = EngineWatch::start(Arc::clone(&fixture.service), [<Resource<Convoy> as Kind>::CODE]).expect("watch");
        let follower = retire::follow(Arc::clone(&fixture.convoys), Arc::new(SystemClock)).expect("follow");
        assert_eq!(fixture.holds(), BTreeSet::from([format!("change-request:{change}")]), "{state:?}");
        assert_eq!(fixture.label(), ConvoyLabel::Abandoned);

        fixture.observe(&change, state).await;
        let promise = PromiseId(PRS[0]);
        let applied = match state {
            ObservedChangeRequestState::Closed => fixture.convoys.apply("fix-it", Reject { promise }, Timestamp(5_000_000)),
            _ => fixture.convoys.apply("fix-it", Keep::Observed { promise, submission: 1 }, Timestamp(5_000_000)),
        };
        applied.expect("the world resolves the abandoned convoy's submission");
        let retired = async {
            while fixture.label() != ConvoyLabel::Retired {
                convoy_writes.next(|_, _| Ok(())).await.expect("a convoy write");
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(30), retired).await.expect("retired once its PR resolved");
        let launch = fixture.convoys.launch(&fixture.launch).expect("launch");
        assert_eq!(launch.spec().promise(promise).expect("pr promise").1.progress.value, resolved, "{state:?}");
        follower.abort();
    }
}

// Behaviour (#3108 ruling 2): a convoy's end withdraws every message still pending to its own seats,
// on its launch's authority, so they stop holding it; a message to anyone else stays pending. A
// message to a seat admitted after the end is admitted, and the next sweep withdraws it.
#[tokio::test]
async fn a_convoys_end_withdraws_the_messages_pending_to_its_seats() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fixture = Fixture::new(&dir);
    let backend = ResourceBackend::new(InMemoryBackend::default());
    designate_fleet(&backend, NAMESPACE).await;
    let messages = EngineMessagesHome::over(Arc::clone(&fixture.service), NAMESPACE, &backend);
    let primitives = messages.primitives();
    let seat = format!("fleet/{}/work/coder", fixture.launch);
    let admit = |record: &'static str, sender: String, receiver: String| async move {
        let intent = intent(&sender, &receiver, MessageRelation::Supervisor, "hello");
        primitives.admit(record, &intent).await.expect("admitted");
    };
    let state = |inbox: Address, record: &'static str| async move {
        let (id, _) = primitives.locate_in(&inbox, record).expect("locate").expect("the message");
        primitives.inbox().get_async(&id).await.expect("read").expect("the message").state().clone()
    };
    let operator_inbox = primitives.inbox_for(NAMESPACE, "fleet/operator").await.expect("inbox");
    admit("m-seat", "fleet/operator".into(), seat.clone()).await;
    admit("m-operator", seat.clone(), "fleet/operator".into()).await;
    let mut open = OpenMessages::default();
    assert_eq!(sweep(primitives, &mut open).await, 0, "an open convoy's seat keeps its messages");

    fixture.abandon();
    assert_eq!(sweep(primitives, &mut open).await, 1);
    let ended = launch_address(&project(), &fixture.launch).expect("launch address");
    let withdrawn = DropReason::Withdrawn { by: ended.clone(), authority: ended };
    assert!(matches!(state(fixture.seat(), "m-seat").await, MessageState::Dropped { reason, .. } if reason == withdrawn));
    assert_eq!(state(operator_inbox, "m-operator").await, MessageState::Pending, "a message to a person is left");
    assert!(!fixture.holds().iter().any(|hold| hold.starts_with("message:")), "{:?}", fixture.holds());

    // Admission reads no convoy: a message to the ended seat is admitted, then withdrawn.
    admit("m-late", "fleet/operator".into(), seat.clone()).await;
    assert_eq!(sweep(primitives, &mut open).await, 1);
    assert!(matches!(state(fixture.seat(), "m-late").await, MessageState::Dropped { reason, .. } if reason == withdrawn));
}

/// Sweeps the home's open messages now; how many an ended convoy withdrew.
async fn sweep(primitives: &MessagePrimitives, open: &mut OpenMessages) -> usize {
    let now = Timestamp::from_datetime(&chrono::Utc::now());
    primitives.sweep(NAMESPACE, now, open).await.expect("sweep").convoy_ended
}

// Behaviour: a standing generation's end holds it until its role's record has counted it; the
// home retires it when the role's pass writes the count.
#[tokio::test]
async fn the_home_retires_a_standing_generation_once_its_role_counts_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fixture = Fixture::standing(&dir, "ensure-coder");
    fixture.abandon();
    assert_eq!(fixture.holds(), BTreeSet::from(["standing-role:ensure-coder".to_owned()]));
    let mut convoy_writes = EngineWatch::start(Arc::clone(&fixture.service), [<Resource<Convoy> as Kind>::CODE]).expect("watch");
    let follower = retire::follow(Arc::clone(&fixture.convoys), Arc::new(SystemClock)).expect("follow");
    assert_eq!(fixture.label(), ConvoyLabel::Abandoned);

    let launch = fixture.convoys.launch(&fixture.launch).expect("launch").spec().launch;
    let roles = EngineStandingRoles::open(Arc::clone(&fixture.service), Arc::new(SystemClock));
    roles.put("ensure-coder", &RunningValues { counted: Some(launch), ..Default::default() }).await.expect("counted");
    let retired = async {
        while fixture.label() != ConvoyLabel::Retired {
            convoy_writes.next(|_, _| Ok(())).await.expect("a convoy write");
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(30), retired).await.expect("retired once its role counted it");
    follower.abort();
}

/// The one stall record's name.
fn first_awaiting(store: &mut flotilla_schema_sqlite::Store) -> Result<String, flotilla_schema_sqlite::StoreError> {
    let mut read = store.read()?;
    let page = read.scan::<Resource<flotilla_state_machines::machines::awaiting_reply::AwaitingReply>>(HOME, None, 1)?;
    let id = page.ids[0];
    Ok(read.identity(id)?.expect("identity").0.name)
}
