//! A reference to a crew seat is spelled as the session holding it wherever that session's vessel
//! is homed (#3108): a vessel launched at a home that is neither this one nor its convoy's is read
//! at that home, as its log arrives here, through the convoy's snapshot, the vessel's launch and
//! its sessions.

use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use flotilla_engine_messages_testkit::{crewed_convoy, designate_fleet, EngineMessagesHome};
use flotilla_protocol::{LifecycleReading, ResourceRef};
use flotilla_provisioning_testkit::{EngineFixture, OtherHome, TerminalsFixture};
use flotilla_resources::{
    lifecycle_reads::{LifecycleReads, ProvisioningKind},
    message_record_name,
    terminal_records::TerminalRecords,
    vessel_records::{VesselLaunchRequest, VesselRecord, VesselRecords, VesselStep},
    ConvoySpec, InputMeta, MessageReference, MessageRelation, Selector, TerminalBrief, TerminalCrewContext, TerminalSession,
    TerminalSessionSource, TerminalSessionSpec,
};
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;

const NAMESPACE: &str = "flotilla";

// Behaviour: a seat whose vessel's record is homed at a third home is spelled as its session.
#[tokio::test]
async fn a_seat_whose_vessel_is_homed_at_a_third_home_is_spelled() {
    seat_spelled(0).await;
}

// Read cost: the seat's vessel is probed once per home the store holds, whatever the number of
// other convoys' vessels at those homes, and no home's vessels are read whole.
#[tokio::test]
async fn a_seats_vessel_probes_are_bounded_by_the_homes() {
    let few = seat_spelled(0).await;
    let many = seat_spelled(32).await;
    assert_eq!(few, many, "the probes do not grow with the other convoys' vessels");
    assert_eq!(many.whole, 0, "no home's vessels are read whole");
    assert_eq!(many.newest, 2, "the view's spelling probes each of the two homes once");
}

/// The vessel reads made while a message naming the seat is admitted and viewed.
#[derive(Debug, PartialEq, Eq)]
struct VesselReads {
    /// Newest-launch probes.
    newest: u64,
    /// Whole reads: every launch at a home, or at every home.
    whole: u64,
}

/// Admits and views a message naming a seat whose vessel and session are at `third`, with
/// `others` other convoys' vessels launched at each of the two homes.
async fn seat_spelled(others: usize) -> VesselReads {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    designate_fleet(&backend, NAMESPACE).await;
    // The convoy, held here, names vessel `work` for the role.
    let spec = ConvoySpec::builder().workflow_ref("workflow".into()).project_ref(NAMESPACE.into()).role("crew".into()).build();
    crewed_convoy(&backend, NAMESPACE, "crew", &spec, &["coder"]).await;

    // The vessel and its session are launched at `third`, whose log arrives here.
    let here = OtherHome::open("home");
    let third = OtherHome::open("third");
    let placed = EngineFixture::over_service(third.service());
    let vessel = placed.vessels.request_for(&VesselLaunchRequest {
        convoy: "crew".into(),
        convoy_home: here.home().into(),
        work: "work".into(),
        host: "third".into(),
    });
    let launch = placed.terminals.request(&TerminalsFixture::agent(&vessel, "coder", "env"));
    let session = placed.terminals.record(&launch).session_name().to_owned();
    let local = EngineFixture::over_service(here.service());
    for other in 0..others {
        for (fixture, host) in [(&placed, "third"), (&local, "home")] {
            fixture.vessels.request_for(&VesselLaunchRequest {
                convoy: format!("other-{other}"),
                convoy_home: here.home().into(),
                work: "work".into(),
                host: host.into(),
            });
        }
    }
    third.replicate_to(here.service());

    // The session's record, unlabelled: only the seat's hops reach it.
    backend
        .using::<TerminalSession>(NAMESPACE)
        .create(
            &InputMeta::builder().name(session.clone()).build(),
            &TerminalSessionSpec {
                env_ref: "env".into(),
                role: "coder".into(),
                source: TerminalSessionSource::Agent {
                    selector: Selector { capability: "code".into(), adapter: None, model: None },
                    brief: TerminalBrief { artifact_digest: None, path: "brief.md".into(), content: "brief".into(), copies: vec![] },
                    context: Box::new(TerminalCrewContext {
                        namespace: NAMESPACE.into(),
                        convoy: "crew".into(),
                        vessel_ref: vessel.clone(),
                    }),
                },
                cwd: "/repo".into(),
                env: BTreeMap::new(),
                pool: "passthrough".into(),
            },
        )
        .await
        .expect("session");

    let home = EngineMessagesHome::over(here.service().clone(), NAMESPACE, &backend);
    let primitives = home.primitives();
    let counted = Arc::new(Counted { reads: local.reads(), newest: AtomicU64::new(0), whole: AtomicU64::new(0) });
    primitives.install_lifecycle_reads(Arc::clone(&counted) as Arc<dyn LifecycleReads>).expect("lifecycle reads");

    let receiver = "flotilla/crew/work/coder";
    let sender = "system:turn-rules";
    let name = message_record_name(receiver, sender, "seat");
    let seat = MessageReference::ControlRecord {
        resource: ResourceRef::new("flotilla.work/v1", "TerminalSession", NAMESPACE, &session),
        revision: "1".into(),
    };
    // A reference to the session's seat, which is the receiver's own role.
    let mut intent = flotilla_engine_messages_testkit::intent(sender, receiver, MessageRelation::System, "about the seat");
    let revision = flotilla_address::Revision::new("1".to_owned()).expect("revision");
    intent.references = vec![flotilla_messaging::IntentReference::new(flotilla_engine_messages_testkit::party(receiver), revision)];
    primitives.admit(&name, &intent).await.expect("admit");

    let view = primitives.view_to(NAMESPACE, receiver, &name).await.expect("view").expect("the message's view").object;
    assert_eq!(view.spec.references, vec![seat], "the seat is spelled as the session its vessel at `third` holds");
    VesselReads { newest: counted.newest.load(Ordering::Relaxed), whole: counted.whole.load(Ordering::Relaxed) }
}

/// A home's lifecycle reads, counting the vessel reads made through them.
struct Counted {
    reads: Arc<dyn LifecycleReads>,
    newest: AtomicU64,
    whole: AtomicU64,
}

impl LifecycleReads for Counted {
    fn home(&self) -> Result<&str, String> {
        LifecycleReads::home(&*self.reads)
    }

    fn try_reading(&self, kind: ProvisioningKind, name: &str, origin: Option<&str>) -> Result<Option<LifecycleReading>, String> {
        self.reads.try_reading(kind, name, origin)
    }

    fn terminals(&self) -> Result<&dyn TerminalRecords, String> {
        self.reads.terminals()
    }

    fn vessels(&self) -> Result<&dyn VesselRecords, String> {
        Ok(self)
    }

    fn joins(&self) -> Result<&flotilla_resources::held_joins::HeldJoins, String> {
        self.reads.joins()
    }
}

impl VesselRecords for Counted {
    fn home(&self) -> &str {
        self.reads.vessels().expect("vessels").home()
    }

    fn codes(&self) -> Vec<flotilla_schema::KindCode> {
        self.reads.vessels().expect("vessels").codes()
    }

    fn get(&self, name: &str, home: Option<&str>) -> Result<Option<VesselRecord>, String> {
        self.reads.vessels()?.get(name, home)
    }

    fn launches(&self, convoy: &str, convoy_home: &str, launch_of: &str, home: Option<&str>) -> Result<Vec<VesselRecord>, String> {
        self.reads.vessels()?.launches(convoy, convoy_home, launch_of, home)
    }

    fn newest(&self, convoy: &str, convoy_home: &str, launch_of: &str, home: Option<&str>) -> Result<Option<VesselRecord>, String> {
        self.newest.fetch_add(1, Ordering::Relaxed);
        self.reads.vessels()?.newest(convoy, convoy_home, launch_of, home)
    }

    fn newest_at_each_home(&self, convoy: &str, convoy_home: &str, launch_of: &str) -> Result<Vec<VesselRecord>, String> {
        self.newest.fetch_add(1, Ordering::Relaxed);
        self.reads.vessels()?.newest_at_each_home(convoy, convoy_home, launch_of)
    }

    fn at(&self, home: &str) -> Result<Vec<VesselRecord>, String> {
        self.whole.fetch_add(1, Ordering::Relaxed);
        self.reads.vessels()?.at(home)
    }

    fn every(&self) -> Result<Vec<VesselRecord>, String> {
        self.whole.fetch_add(1, Ordering::Relaxed);
        self.reads.vessels()?.every()
    }

    fn find(&self, name: &str) -> Result<Option<VesselRecord>, String> {
        self.reads.vessels()?.find(name)
    }

    fn of_convoy(&self, convoy: &str) -> Result<Vec<VesselRecord>, String> {
        self.whole.fetch_add(1, Ordering::Relaxed);
        self.reads.vessels()?.of_convoy(convoy)
    }

    fn request(&self, request: &VesselLaunchRequest) -> Result<String, String> {
        self.reads.vessels()?.request(request)
    }

    fn step(&self, name: &str, step: &VesselStep) -> Result<VesselRecord, String> {
        self.reads.vessels()?.step(name, step)
    }
}
