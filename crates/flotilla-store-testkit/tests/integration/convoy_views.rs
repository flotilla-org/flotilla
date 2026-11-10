//! Convoys read from the engine's launches in the old Convoy shape (#3108): each launch joined
//! with its works' Vessels and its seats' sessions, its home's notes at that home only, and every
//! write refused.

use std::{collections::BTreeMap, sync::Arc};

use chrono::{DateTime, TimeZone, Utc};
use flotilla_protocol::{LifecycleReading, PrincipalRef};
use flotilla_resources::{
    convoy::admission::AdmissionDocument,
    convoy_records::{ConvoyMemory, ConvoyNotes, ConvoyRecord, ConvoyRecords, CrewNotes},
    engine_records::EngineRecords,
    lifecycle_reads::{LifecycleReads, ProvisioningKind},
    promises::{Promise, PromiseKind, PromiseSource, PromiseState},
    terminal_records::{
        TerminalLaunchRequest, TerminalLaunchSource, TerminalObservations, TerminalRecord, TerminalRecords, TerminalRenewal,
    },
    workflow_template::{CrewSource, CrewSpec, Selector, VesselRequirement},
    ConvoyAttention, ConvoyPhase, CrewWorkPhase, Vessel, VesselPhase, VesselSpec, VesselStatus, WorkPhase, WorkflowSnapshot,
};
use flotilla_store::{ConvoyViews, ResourceBackend};
use flotilla_store_memory::InMemoryBackend;
use flotilla_store_testkit::fixtures::resource_meta;

const HERE: &str = "home-a";

fn at(seconds: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(seconds, 0).single().expect("time")
}

fn agent(role: &str) -> CrewSpec {
    CrewSpec::builder()
        .role(role.to_owned())
        .source(CrewSource::Agent { selector: Selector::for_capability("code"), prompt: None, brief_template: None })
        .build()
}

fn launch(name: &str, home: &str) -> ConvoyRecord {
    let snapshot = WorkflowSnapshot {
        cascade: None,
        exit: None,
        turn_delivery: Default::default(),
        stall_nudges: Default::default(),
        supervision: None,
        vessels: vec![VesselRequirement::builder().name("work".to_owned()).crew(vec![agent("coder"), agent("reviewer")]).build()],
    };
    let promise = Promise {
        id: "template/pr".into(),
        vessel: "work".into(),
        role: "coder".into(),
        source: PromiseSource::Template,
        kind: PromiseKind::Pr,
        state: PromiseState::Open,
        retraction_reason: None,
        submissions: Vec::new(),
    };
    ConvoyRecord {
        name: name.into(),
        home: home.into(),
        version: 3,
        launch_of: name.split('#').next().expect("launch_of").into(),
        launch: 1,
        project: "flotilla".into(),
        workflow_ref: "review".into(),
        admission: AdmissionDocument {
            workflow_snapshot: snapshot,
            placement_decision: None,
            placement_policy: None,
            role: String::new(),
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
            admitted_from: None,
        },
        admitted_at: at(100),
        ended: None,
        seats: BTreeMap::new(),
        promises: BTreeMap::from([("work".into(), BTreeMap::from([("coder".into(), vec![promise])]))]),
        subjects: Vec::new(),
        unlinked: Vec::new(),
    }
}

fn session(vessel: &str, role: &str, reading: LifecycleReading) -> TerminalRecord {
    TerminalRecord {
        name: format!("terminal-{vessel}-{role}#1"),
        home: HERE.into(),
        environment: "env".into(),
        environment_home: HERE.into(),
        vessel: Some(vessel.into()),
        role: role.into(),
        pool: "cleat".into(),
        cwd: "/work".into(),
        env: BTreeMap::new(),
        source: TerminalLaunchSource::Tool { command: "true".into() },
        requested_at: at(110),
        since: at(111),
        reading,
        facts: None,
    }
}

struct Convoys {
    records: Vec<ConvoyRecord>,
    memory: ConvoyMemory,
}

impl ConvoyRecords for Convoys {
    fn home(&self) -> &str {
        HERE
    }
    fn get(&self, name: &str, home: Option<&str>) -> Result<Option<ConvoyRecord>, String> {
        let home = home.unwrap_or(HERE);
        Ok(self.records.iter().find(|record| record.name == name && record.home == home).cloned())
    }
    fn newest(&self, of: &str, _project: &str, home: Option<&str>) -> Result<Option<ConvoyRecord>, String> {
        let home = home.unwrap_or(HERE);
        Ok(self.records.iter().filter(|record| record.launch_of == of && record.home == home).max_by_key(|record| record.launch).cloned())
    }
    fn at(&self, home: &str) -> Result<Vec<ConvoyRecord>, String> {
        Ok(self.records.iter().filter(|record| record.home == home).cloned().collect())
    }
    fn every(&self) -> Result<Vec<ConvoyRecord>, String> {
        Ok(self.records.clone())
    }
    fn memory(&self) -> &ConvoyMemory {
        &self.memory
    }
}

struct Terminals {
    records: Vec<TerminalRecord>,
    observations: TerminalObservations,
}

impl TerminalRecords for Terminals {
    fn home(&self) -> &str {
        HERE
    }
    fn get(&self, name: &str, _home: Option<&str>) -> Result<Option<TerminalRecord>, String> {
        Ok(self.records.iter().find(|record| record.name == name).cloned())
    }
    fn at(&self, home: &str) -> Result<Vec<TerminalRecord>, String> {
        Ok(self.records.iter().filter(|record| record.home == home).cloned().collect())
    }
    fn every(&self) -> Result<Vec<TerminalRecord>, String> {
        Ok(self.records.clone())
    }
    fn of_vessel(&self, vessel: &str) -> Result<Vec<TerminalRecord>, String> {
        Ok(self.records.iter().filter(|record| record.vessel.as_deref() == Some(vessel)).cloned().collect())
    }
    fn of_environment(&self, _environment: &str, _home: &str) -> Result<Vec<TerminalRecord>, String> {
        Ok(Vec::new())
    }
    fn request(&self, _request: TerminalLaunchRequest) -> Result<String, String> {
        Err("not requested here".into())
    }
    fn renew(&self, _session: &str) -> Result<TerminalRenewal, String> {
        Err("not renewed here".into())
    }
    fn observations(&self) -> &TerminalObservations {
        &self.observations
    }
}

struct Reads {
    convoys: Convoys,
    terminals: Terminals,
}

impl LifecycleReads for Reads {
    fn try_reading(&self, _kind: ProvisioningKind, _name: &str, _origin: Option<&str>) -> Result<Option<LifecycleReading>, String> {
        Ok(None)
    }
    fn terminals(&self) -> Result<&dyn TerminalRecords, String> {
        Ok(&self.terminals)
    }
    fn convoys(&self) -> Result<&dyn ConvoyRecords, String> {
        Ok(&self.convoys)
    }
}

async fn ready_vessel(backend: &ResourceBackend, name: &str) {
    let vessels = backend.using::<Vessel>("flotilla");
    let spec = VesselSpec {
        convoy_ref: "fix-it#1".into(),
        vessel_name: "work".into(),
        fulfilment_kind_ref: "docker".into(),
        adopted_checkout_refs: Default::default(),
    };
    let created = vessels.create(&resource_meta().name(name).call(), &spec).await.expect("vessel");
    let status = VesselStatus { phase: VesselPhase::Ready, started_at: Some(at(110)), ready_at: Some(at(115)), ..VesselStatus::default() };
    vessels.update_status(name, &created.metadata.resource_version, &status).await.expect("status");
}

// Behaviour: a launch reads as a Convoy named by its launch, its work Running on its ready
// Vessel, its eager seat working on its live session and its latent one once its home delivered
// its task. Its home's notes show at that home; another home's launch reads without them.
#[tokio::test]
async fn a_launch_reads_with_its_vessels_sessions_and_home_notes() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    ready_vessel(&backend, "fix-it#1-work").await;
    let reads = Arc::new(Reads {
        convoys: Convoys { records: vec![launch("fix-it#1", HERE), launch("other#1", "home-b")], memory: ConvoyMemory::default() },
        terminals: Terminals {
            records: vec![
                session("fix-it#1-work", "coder", LifecycleReading::Ready { identity: "s1".into() }),
                session("fix-it#1-work", "reviewer", LifecycleReading::Ready { identity: "s2".into() }),
            ],
            observations: TerminalObservations::default(),
        },
    });
    let views = ConvoyViews::new(backend.clone(), "flotilla", reads.clone());

    let convoy = views.get("fix-it#1").await.expect("read");
    let status = convoy.status.expect("status");
    assert_eq!(status.phase, ConvoyPhase::Active);
    assert_eq!(status.work["work"].phase, WorkPhase::Running);
    assert_eq!((status.work["work"].started_at, status.work["work"].ready_at), (Some(at(110)), Some(at(115))));
    assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Working);
    assert_eq!(status.crew_work["work"]["reviewer"].phase, CrewWorkPhase::Pending, "a latent seat waits for its task");

    let (_, changed) = reads.convoys.memory.update("fix-it#1", |notes: &mut ConvoyNotes| {
        notes.attention = Some(ConvoyAttention { source: "turn".into(), reason: "queued".into(), raised_at: at(120) });
        notes.crew.entry("work".into()).or_default().insert("reviewer".into(), CrewNotes { task_assigned: true, ..CrewNotes::default() });
    });
    assert!(changed);
    let status = views.get("fix-it#1").await.expect("read").status.expect("status");
    assert_eq!(status.crew_work["work"]["reviewer"].phase, CrewWorkPhase::Working);
    assert_eq!(status.attention.map(|attention| attention.reason).as_deref(), Some("queued"));

    reads.convoys.memory.update("other#1", |notes: &mut ConvoyNotes| {
        notes.attention = Some(ConvoyAttention { source: "turn".into(), reason: "not here".into(), raised_at: at(120) });
    });
    let other = views.find("other#1", Some("home-b")).await.expect("read").expect("held");
    assert!(other.object.status.expect("status").attention.is_none(), "another home's launch reads without this home's notes");
    assert_eq!(views.sources().await.expect("sources").len(), 2);
    assert_eq!(views.here().await.expect("here").len(), 1);
}

// Behaviour: the views write nothing; a convoy is admitted, edged and ended on the engine.
#[tokio::test]
async fn the_views_refuse_writes() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let reads = Arc::new(Reads {
        convoys: Convoys { records: vec![launch("fix-it#1", HERE)], memory: ConvoyMemory::default() },
        terminals: Terminals { records: Vec::new(), observations: TerminalObservations::default() },
    });
    let views = ConvoyViews::new(backend, "flotilla", reads);
    let none = BTreeMap::new();
    assert!(views.apply("Convoy", "fix-it#1", &none, &none, serde_json::Value::Null).await.is_err());
    assert!(views.set_status("Convoy", "fix-it#1", serde_json::Value::Null).await.is_err());
    assert!(views.delete("Convoy", "fix-it#1").await.is_err());
    assert_eq!(EngineRecords::list(&views, "Convoy").await.expect("list").len(), 1);
}
