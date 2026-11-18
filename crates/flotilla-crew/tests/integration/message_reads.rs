//! What the Message path's role routing, gate conditions and attention causes read (#3108 read-cost
//! audit): each reads the records its crews, convoys and vessels name by key, so a delivery pass's
//! reads do not grow with the convoys whose crews run at other homes, nor with unrelated records.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use chrono::Utc;
use flotilla_address::{Address, Condition, Kind};
use flotilla_engine_convoys_testkit::{convoy, single_agent, EngineConvoysHome, KeyedGets};
use flotilla_engine_messages_testkit::{designate_fleet, EngineMessagesHome};
use flotilla_provisioning_testkit::{EngineFixture, OtherHome, TerminalsFixture};
use flotilla_resources::{
    artifact::blob_digest_of, attentions::Attentions, terminal_records::TerminalLaunchFacts, vessel_records::VesselLaunchRequest, Artifact,
    ArtifactSpec, Convoy, ConvoyEnsure, ConvoyEnsureSpec, ConvoySpec, InputMeta, Project, ProjectSpec, Repository, RepositorySpec,
    SystemClock, TerminalSession, Vessel, ENSURED_FROM_ANNOTATION,
};
use flotilla_store::{
    load_metrics::{attributed, CallerReads},
    ResourceBackend, TerminalViews, VesselViews,
};
use flotilla_store_memory::InMemoryBackend;
use flotilla_store_testkit::{assert_flat_reads, ReadCountsBackendExt, ReadOperation, ReadSnapshot};

const NAMESPACE: &str = "flotilla";

/// A home whose convoy `crew` has a running coder here, with other convoys admitted here whose
/// coders run at another home, and unrelated artifacts, repositories and projects.
struct Home {
    store: OtherHome,
    engine: EngineFixture,
    convoys: EngineConvoysHome,
    messages: EngineMessagesHome,
    backend: ResourceBackend,
    /// The crew's launch, and its coder's session.
    crew: String,
    session: String,
    /// Each pool session id given so far, so each realised launch has its own.
    realised: usize,
    /// The engine reads of the calls measured through [`Home::measured`].
    reads: Arc<CallerReads>,
    _third: OtherHome,
    _placed: EngineFixture,
}

/// The reads one call made: launches read by listing, index or scan, convoy views converted,
/// keyed gets through the lifecycle reads, and the backend's own reads.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cost {
    terminal_launches: u64,
    vessel_launches: u64,
    convoys_converted: u64,
    gets: KeyedGets,
    reads: ReadSnapshot,
}

impl Home {
    /// With `others` other convoys, artifacts and repositories, and `projects` other projects.
    async fn open(others: usize, projects: usize) -> Self {
        let store = OtherHome::open("home");
        let engine = EngineFixture::over_service(store.service());
        let backend = ResourceBackend::new(InMemoryBackend::default()).with_read_counts();
        designate_fleet(&backend, NAMESPACE).await;
        let convoys = EngineConvoysHome::over(store.service(), NAMESPACE, &backend, Arc::new(SystemClock), engine.reads());
        backend.install_engine_records(Arc::new(TerminalViews::new(NAMESPACE, Arc::clone(convoys.reads()))));
        backend.install_engine_records(Arc::new(VesselViews::new(backend.clone(), NAMESPACE, Arc::clone(convoys.reads()))));
        let messages = EngineMessagesHome::over(Arc::clone(store.service()), NAMESPACE, &backend);
        messages.primitives().install_lifecycle_reads(Arc::clone(convoys.reads())).expect("lifecycle reads");

        let third = OtherHome::open("third");
        let placed = EngineFixture::over_service(third.service());
        let mut home = Self {
            store,
            engine,
            convoys,
            messages,
            backend,
            crew: String::new(),
            session: String::new(),
            realised: 0,
            reads: Arc::default(),
            _third: third,
            _placed: placed,
        };
        let spec = ConvoySpec::builder().workflow_ref("workflow".into()).project_ref(NAMESPACE.into()).build();
        let crew = home.convoys.launch(convoy(NAMESPACE, "crew", spec, single_agent("coder"))).await;
        home.session = home.crew_here(&crew);
        home.crew = crew;
        for n in 0..others {
            let spec = ConvoySpec::builder().workflow_ref("workflow".into()).project_ref(NAMESPACE.into()).build();
            let launch = home.convoys.launch(convoy(NAMESPACE, &format!("other-{n}"), spec, single_agent("coder"))).await;
            home.crew_at_third(&launch);
            unrelated(&home.backend, n).await;
        }
        for n in 0..projects {
            unrelated_project(&home.backend, n).await;
        }
        home._third.replicate_to(home.store.service());
        // The views are filled once, at their first read, as a running daemon's already are.
        home.backend.clone().using::<Convoy>(NAMESPACE).list().await.expect("the convoy views");
        home.backend.clone().using::<TerminalSession>(NAMESPACE).list().await.expect("the session views");
        home.backend.clone().using::<Vessel>(NAMESPACE).list().await.expect("the vessel views");
        home
    }

    /// Places `launch`'s work here with its coder running, and returns the coder's session.
    fn crew_here(&mut self, launch: &str) -> String {
        let vessel = self.engine.vessels.request(launch, "work", self.store.home());
        self.engine.vessels.ready(&vessel, "env");
        let session = self.engine.terminals.request(&TerminalsFixture::agent(&vessel, "coder", "env"));
        self.realised += 1;
        self.engine.terminals.ready(&session, &format!("pool-{}", self.realised), &TerminalLaunchFacts::default());
        self.engine.terminals.record(&session).session_name().to_owned()
    }

    /// Places `launch`'s work at the third home with its coder running there, as that home's log
    /// arrives here.
    fn crew_at_third(&mut self, launch: &str) {
        let placed = &self._placed;
        let vessel = placed.vessels.request_for(&VesselLaunchRequest {
            convoy: launch.to_owned(),
            convoy_home: self.store.home().into(),
            work: "work".into(),
            host: "third".into(),
        });
        placed.vessels.ready(&vessel, "env-third");
        let session = placed.terminals.request(&TerminalsFixture::agent(&vessel, "coder", "env-third"));
        self.realised += 1;
        placed.terminals.ready(&session, &format!("pool-{}", self.realised), &TerminalLaunchFacts::default());
    }

    /// Admits the next generation of standing role `coder`, ensured from `ensure-coder`, and
    /// returns its launch.
    async fn standing(&mut self) -> String {
        let ensure = ConvoyEnsureSpec::builder()
            .project_ref(NAMESPACE.into())
            .role("coder".into())
            .workflow_ref("workflow".into())
            .repositories(Vec::new())
            .build();
        let spec = ConvoySpec::builder().workflow_ref("workflow".into()).project_ref(NAMESPACE.into()).role("coder".into()).build();
        let mut status = single_agent("coder");
        status.ensure_admission = Some(ensure);
        let mut generation = convoy(NAMESPACE, "standing", spec, status);
        generation.metadata.annotations.insert(ENSURED_FROM_ANNOTATION.into(), "ensure-coder".into());
        self.convoys.launch(generation).await
    }

    fn meters(&self) -> (u64, u64, u64, KeyedGets, ReadSnapshot) {
        let mut gets = self.convoys.keyed_gets();
        gets.convoys += self.engine.vessels.vessels.convoys_read();
        (
            self.engine.terminals.terminals.launches_read(),
            self.engine.vessels.vessels.launches_read(),
            self.reads.snapshot().get("Convoy").map_or(0, |counts| counts.converted),
            gets,
            self.backend.read_snapshot(),
        )
    }

    /// `work`, with its engine reads counted for this home's meters.
    async fn measured<T>(&self, work: impl std::future::Future<Output = T>) -> T {
        attributed(Arc::clone(&self.reads), work).await
    }

    fn spent(&self, before: (u64, u64, u64, KeyedGets, ReadSnapshot)) -> Cost {
        let after = self.meters();
        Cost {
            terminal_launches: after.0 - before.0,
            vessel_launches: after.1 - before.1,
            convoys_converted: after.2 - before.2,
            gets: after.3 - before.3,
            reads: after.4.since(&before.4),
        }
    }

    /// Each holder's session and the inboxes it holds.
    async fn holders(&self) -> BTreeMap<String, BTreeSet<String>> {
        let primitives = self.messages.primitives();
        let roles = primitives.roles(NAMESPACE).await.expect("fleet roles");
        let holders = primitives.bindings().holders(NAMESPACE, &roles).await.expect("holders");
        holders
            .into_iter()
            .map(|holder| (holder.terminal.metadata.name, holder.inboxes.into_iter().map(|inbox| inbox.to_string()).collect()))
            .collect()
    }
}

/// An artifact and a repository no message names.
async fn unrelated(backend: &ResourceBackend, n: usize) {
    backend
        .using::<Artifact>(NAMESPACE)
        .create(
            &InputMeta::builder().name(format!("artifact-{n}")).build(),
            &ArtifactSpec::builder()
                .convoy(format!("other-{n}"))
                .producer("coder".into())
                .kind("explainer".into())
                .subject("other".into())
                .digest(blob_digest_of(b"unrelated"))
                .size(0)
                .media_type("text/markdown".into())
                .expires_at(Utc::now() + chrono::Duration::days(1))
                .build(),
        )
        .await
        .expect("unrelated artifact");
    let repository = RepositorySpec::remote(format!("https://github.com/other/repo-{n}")).expect("repository");
    backend
        .using::<Repository>(NAMESPACE)
        .create(&InputMeta::builder().name(repository.key().0.clone()).build(), &repository)
        .await
        .expect("unrelated repository");
}

/// A project no message names.
async fn unrelated_project(backend: &ResourceBackend, n: usize) {
    backend
        .definitions::<Project>(NAMESPACE)
        .apply(
            &InputMeta::builder().name(format!("project-{n}")).build(),
            &ProjectSpec::builder().display_name("Other".into()).default_workflow_ref("work".into()).repositories(Vec::new()).build(),
        )
        .await
        .expect("unrelated project");
}

/// No kind is listed whole through the backend.
fn no_listing(cost: &Cost) {
    no_listing_but(cost, &[]);
}

/// No kind but `listed` is listed whole through the backend.
fn no_listing_but(cost: &Cost, listed: &[&str]) {
    for kind in ["Artifact", "Convoy", "ConvoyEnsure", "Forge", "Project", "Repository", "TerminalSession", "Vessel"] {
        if !listed.contains(&kind) {
            assert_eq!(cost.reads.calls(kind, ReadOperation::List), 0, "no {kind} listing: {cost:#?}");
        }
    }
}

/// The holders a delivery pass finds, and what finding them read, with `others` other convoys'
/// crews running at another home.
async fn holders_read(others: usize) -> (BTreeMap<String, BTreeSet<String>>, Cost) {
    let home = Home::open(others, others).await;
    let before = home.meters();
    let holders = home.measured(home.holders()).await;
    let crew = Address::project(NAMESPACE)
        .and_then(|project| project.child(Kind::Convoy, &home.crew))
        .and_then(|convoy| convoy.child(Kind::Role, "coder"))
        .expect("the crew's inbox");
    assert_eq!(holders.get(&home.session), Some(&BTreeSet::from([crew.to_string()])), "the crew's coder holds its seat: {holders:?}");
    (holders, home.spent(before))
}

// Read cost: a delivery pass finds this home's role holders from this home's running sessions and
// the records they name, not from every convoy, session, artifact, repository and project held.
#[tokio::test]
async fn role_holders_are_read_from_this_homes_running_sessions() {
    let (_, cost) = assert_flat_reads([0, 32], holders_read).await;
    no_listing(&cost);
    assert_eq!(cost.convoys_converted, 1, "the crew's convoy is read by its name, once: {cost:#?}");
}

/// What finding the role holders read with `ended` ended agent launches retained at this home
/// beside the crew's running coder.
async fn holders_read_beside_ended(ended: usize) -> Cost {
    let home = Home::open(0, 0).await;
    for n in 0..ended {
        let launch = home.engine.terminals.request(&TerminalsFixture::agent(&format!("ended-{n}"), "coder", "env"));
        home.engine.terminals.ready(&launch, &format!("pool-ended-{n}"), &TerminalLaunchFacts::default());
        home.engine.terminals.release(&launch);
    }
    // The session views take these launches in at their next read, as a running daemon's took
    // them in as they ended: that is not the pass's cost.
    home.backend.clone().using::<TerminalSession>(NAMESPACE).list().await.expect("the session views");
    let before = home.meters();
    let holders = home.measured(home.holders()).await;
    assert!(holders.contains_key(&home.session), "the crew's coder still holds its seat: {holders:?}");
    let cost = home.spent(before);
    let scanned = home.reads.snapshot().get("TerminalSession").map_or(0, |counts| counts.whole_kind);
    assert_eq!(scanned, 0, "no scan of this home's terminal launches: {:#?}", home.reads.snapshot());
    cost
}

// Read cost (d10): a delivery pass reads this home's live launches by key, never every launch the
// home has retained, so its reads do not grow with the ended launches beside the running ones.
#[tokio::test]
async fn role_holders_read_only_this_homes_live_launches() {
    let cost = assert_flat_reads([0, 32], holders_read_beside_ended).await;
    // The running launch, then its vessel's sessions when its seat is read.
    assert_eq!(cost.terminal_launches, 2, "only the running launch is read: {cost:#?}");
}

/// What raising a credential refresh about the crew's vessel read, with `others` other convoys'
/// vessels at another home. No other project is declared: raising reads the open attentions in
/// every person inbox, which every project's declaration names (`person_inboxes`).
async fn cause_read(others: usize) -> Cost {
    let home = Home::open(others, 0).await;
    let before = home.meters();
    let failures = BTreeMap::from([("github".to_owned(), "expired".to_owned())]);
    let attention = flotilla_resources::attentions::Attention::credential_refresh(&home.crew, "work", failures, Utc::now());
    home.measured(home.messages.primitives().raise(NAMESPACE, &attention)).await.expect("raise the attention");
    home.spent(before)
}

// Read cost: a credential refresh names its vessel by its convoy's work, read by that work's newest
// launch at each home, not by listing every vessel.
#[tokio::test]
async fn a_credential_refresh_reads_its_vessel_by_key() {
    let cost = assert_flat_reads([0, 32], cause_read).await;
    // The projects are listed for the person inboxes whose open attentions a raise reads.
    no_listing_but(&cost, &["Project"]);
    assert_eq!(cost.vessel_launches, 0, "no vessel launch is listed, scanned or read through an index: {cost:#?}");
}

/// The value of a condition on the crew's coder, and what reading it read, with `others` other
/// convoys' crews at another home.
async fn condition_read(others: usize) -> (flotilla_resources::ThreeValue, Cost) {
    let home = Home::open(others, others).await;
    let role = Address::project(NAMESPACE)
        .and_then(|project| project.child(Kind::Convoy, &home.crew))
        .and_then(|convoy| convoy.child(Kind::Role, "coder"))
        .expect("the crew's role");
    let condition: Condition = format!("{role} .phase == Running").parse().expect("condition");
    let before = home.meters();
    let value = home.measured(home.messages.primitives().conditions().value(NAMESPACE, &condition)).await.expect("the condition's value");
    (value, home.spent(before))
}

// Read cost: a condition on a crew role reads its convoy by name for the work holding the role,
// not every convoy, session, artifact, repository and project held.
#[tokio::test]
async fn a_crew_condition_reads_its_convoy_by_key() {
    let (_, cost) = assert_flat_reads([0, 32], condition_read).await;
    no_listing(&cost);
}

// Behaviour: a gate condition on a crew role is read through its convoy's snapshot, for a finite
// launch and a standing one, whose role's convoy segment is not its record's name; no session is
// searched for by its labels.
#[tokio::test]
async fn a_condition_on_a_launchs_role_reads_its_crew() {
    let mut home = Home::open(0, 0).await;
    let standing = home.standing().await;
    home.crew_here(&standing);
    let project = Address::project(NAMESPACE).expect("project");
    for launch in [home.crew.clone(), standing] {
        let role = flotilla_resources::convoy::engine::seat_address(&project, &launch, "coder").expect("the role");
        let condition: Condition = format!("{role} .phase == Working").parse().expect("condition");
        let (value, reads) =
            flotilla_store::load_metrics::measure(home.messages.primitives().conditions().value(NAMESPACE, &condition)).await;
        assert_eq!(value, Ok(flotilla_resources::ThreeValue::True), "{launch}'s coder is working");
        let searched = reads.get("TerminalSession port").map_or(0, |counts| counts.index + counts.whole_kind);
        assert_eq!(searched, 0, "no session search by label for {launch}: {reads:#?}");
    }
}

// Behaviour: a message about a crew seat is shown in the old shape with the session holding that
// seat as its subject, for a finite launch and a standing one, whose seat's convoy segment is not
// its record's name.
#[tokio::test]
async fn a_seat_subject_is_spelled_as_its_session() {
    let mut home = Home::open(0, 0).await;
    let standing = home.standing().await;
    let standing_session = home.crew_here(&standing);
    let primitives = home.messages.primitives();
    for (launch, session) in [(home.crew.clone(), home.session.clone()), (standing, standing_session)] {
        let seat = flotilla_resources::convoy::engine::seat_address(&Address::project(NAMESPACE).expect("project"), &launch, "coder")
            .expect("the seat");
        let receiver = flotilla_resources::crew_role_address(NAMESPACE, &launch, "work", "coder");
        let name = flotilla_resources::message_record_name(&receiver, "system:capabilities", &format!("{launch}:card"));
        let mut card = flotilla_engine_messages_testkit::intent(
            "system:capabilities",
            &receiver,
            flotilla_resources::MessageRelation::System,
            "## Your capabilities\n",
        );
        card.references =
            vec![flotilla_messaging::IntentReference::subject(seat, flotilla_address::Revision::new("1".to_owned()).expect("revision"))];
        primitives.admit(&name, &card).await.expect("admit");
        let (view, reads) = flotilla_store::load_metrics::measure(primitives.view_to(NAMESPACE, &receiver, &name)).await;
        let view = view.expect("view").expect("the card's view").object;
        assert_eq!(
            view.spec.subject,
            Some(flotilla_resources::MessageReference::ControlRecord {
                resource: flotilla_protocol::ResourceRef::new("flotilla.work/v1", "TerminalSession", NAMESPACE, &session),
                revision: "1".into(),
            }),
            "{launch}'s seat is spelled as its coder's session"
        );
        let searched = reads.get("TerminalSession port").map_or(0, |counts| counts.index + counts.whole_kind);
        assert_eq!(searched, 0, "no session search by label for {launch}: {reads:#?}");
    }
}

// Behaviour (#3108 R4): a launch has one address. An attention about a convoy role is about that
// role's seat, and one about the convoy is about the seat's convoy, for a finite launch and a
// standing one, whose seat's convoy segment is not its record's name; each reads back about the
// launch's record, so clearing one by its subject clears it.
#[tokio::test]
async fn an_attention_cause_and_its_seat_share_the_launchs_address() {
    use flotilla_resources::attentions::{Attention, Fact, Sender, Subject};
    let mut home = Home::open(0, 0).await;
    let standing = home.standing().await;
    let standing_session = home.crew_here(&standing);
    // The standing role is declared, as every standing launch's is.
    let ensure = ConvoyEnsureSpec::builder()
        .project_ref(NAMESPACE.into())
        .role("coder".into())
        .workflow_ref("workflow".into())
        .repositories(Vec::new())
        .build();
    home.backend
        .definitions::<ConvoyEnsure>(NAMESPACE)
        .create(&InputMeta::builder().name("ensure-coder".to_owned()).build(), &ensure)
        .await
        .expect("the standing role's ensure");
    let primitives = home.messages.primitives();
    let project = Address::project(NAMESPACE).expect("project");
    for (launch, session) in [(home.crew.clone(), home.session.clone()), (standing, standing_session)] {
        let seat = flotilla_resources::convoy::engine::seat_address(&project, &launch, "coder").expect("the seat");
        let held = Fact::DeliveryHeld { session, convoy: launch.clone(), role: "coder".into() };
        let refused = Fact::ReclaimRefused { convoy: launch.clone() };
        for fact in [held, refused] {
            primitives.raise(NAMESPACE, &Attention::new(fact, "held", Utc::now())).await.expect("raise");
        }
        let subjects: BTreeSet<String> = primitives
            .person_messages(NAMESPACE)
            .await
            .expect("person messages")
            .into_iter()
            .filter_map(|(_, message)| Some(message.spec().subject()?.address.as_str().to_owned()))
            .collect();
        let convoy = seat.parent().expect("the seat's convoy");
        assert!(subjects.contains(seat.as_str()), "{launch}: the role's attention is about its seat: {subjects:?}");
        assert!(subjects.contains(convoy.as_str()), "{launch}: the convoy's attention is about its seat's convoy: {subjects:?}");

        let about = Subject::convoy(launch.clone());
        let open: BTreeSet<_> =
            primitives.open(NAMESPACE, None, Some(&about)).await.expect("open").into_iter().map(|open| open.subject).collect();
        let role = Subject::ConvoyRole { convoy: launch.clone(), role: "coder".into() };
        assert_eq!(open, BTreeSet::from([about.clone(), role.clone()]), "{launch}: each reads back about its launch's record");
        primitives.clear(NAMESPACE, Sender::Teardown, &about).await.expect("clear");
        primitives.clear(NAMESPACE, Sender::TurnRules, &role).await.expect("clear");
        assert!(primitives.open(NAMESPACE, None, Some(&about)).await.expect("open").is_empty(), "{launch}: cleared by its subject");
    }
}

// Behaviour: a standing role's inbox is held by its newest generation's running crew; an older
// generation still running holds only its own seat.
#[tokio::test]
async fn a_standing_role_is_held_by_its_newest_generations_crew() {
    let mut home = Home::open(0, 0).await;
    home.backend
        .using::<ConvoyEnsure>(NAMESPACE)
        .create(
            &InputMeta::builder().name("ensure-coder".into()).build(),
            &ConvoyEnsureSpec::builder()
                .project_ref(NAMESPACE.into())
                .role("coder".into())
                .workflow_ref("workflow".into())
                .repositories(Vec::new())
                .build(),
        )
        .await
        .expect("the ensure");
    let standing =
        Address::project(NAMESPACE).and_then(|project| project.child(Kind::Role, "coder")).expect("the standing role").to_string();
    let holding = |holders: &BTreeMap<String, BTreeSet<String>>| {
        holders.iter().filter(|(_, inboxes)| inboxes.contains(&standing)).map(|(session, _)| session.clone()).collect::<Vec<_>>()
    };

    let first = home.standing().await;
    let first = home.crew_here(&first);
    assert_eq!(holding(&home.holders().await), std::slice::from_ref(&first), "the first generation's coder holds the role");
    // The second generation's coder runs at another home: the role is held there, not here.
    let second = home.standing().await;
    home.crew_at_third(&second);
    home._third.replicate_to(home.store.service());
    let holders = home.holders().await;
    assert_eq!(holding(&holders), Vec::<String>::new(), "the newer generation's coder elsewhere holds the role: {holders:?}");
    let third = home.standing().await;
    let third = home.crew_here(&third);
    let holders = home.holders().await;
    assert_eq!(holding(&holders), [third], "the newest generation's coder holds the role: {holders:?}");
    assert!(holders.contains_key(&home.session), "the finite crew still holds its seat: {holders:?}");
}
