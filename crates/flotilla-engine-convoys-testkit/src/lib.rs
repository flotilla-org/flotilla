//! The engine Convoy path for tests (#3108): a home's [`Convoys`] over a decided store, its
//! readers' records served on the resource backend as Convoy and its old-shape writer as the
//! [`EngineConvoyPath`], the way the composition root installs them; and launches admitted on it
//! the way admission admits them.

use std::sync::{Arc, OnceLock};

use flotilla_engine_convoys::{convoy_engine::ConvoyEngine, convoy_records::EngineConvoyRecords, convoys::Convoys};
use flotilla_protocol::LifecycleReading;
use flotilla_resources::{
    convoy::engine::{engine_name, set_notes, EngineConvoyPath, EngineConvoys, VerdictScope},
    convoy_records::ConvoyRecords,
    lifecycle_reads::{LifecycleReads, ProvisioningKind},
    terminal_records::TerminalRecords,
    vessel_records::VesselRecords,
    Clock, Convoy, ConvoyPhase, ConvoySpec, ConvoyStatus, CrewSource, CrewSpec, ObjectMeta, ResourceObject, Selector, VesselRequirement,
    WorkflowSnapshot,
};
use flotilla_schema_sqlite::stream::Service;
use flotilla_store::{ConvoyViews, ResourceBackend};

/// One home's engine Convoy path; a decided store it opened lives as long as this value.
pub struct EngineConvoysHome {
    convoys: Arc<Convoys>,
    path: EngineConvoyPath,
    _dir: Option<tempfile::TempDir>,
}

impl EngineConvoysHome {
    /// Opens the Convoy path over decided store `service` (the one a test's other engine paths
    /// write, so launches, vessels and terminals share a log) on `clock`, and serves its launches
    /// as `backend`'s Convoy under `namespace`, composed with the vessels and terminals `reads`
    /// serves.
    pub fn over(
        service: &Arc<Service>,
        namespace: &str,
        backend: &ResourceBackend,
        clock: Arc<dyn Clock>,
        reads: Arc<dyn LifecycleReads>,
    ) -> Self {
        let home = service.home().to_owned();
        let convoys = Arc::new(Convoys::open(&home, service).expect("engine Convoy home"));
        let records = EngineConvoyRecords::new(&home, Arc::clone(service), Arc::clone(convoys.memory()));
        // The views hear their changes from the store's log, on the test's runtime.
        drop(records.follow().expect("follow the convoy log"));
        backend.install_engine_records(Arc::new(ConvoyViews::new(namespace, Arc::new(WithConvoys { reads, records }))));
        let path: EngineConvoyPath = Arc::new(OnceLock::new());
        let engine: Arc<dyn EngineConvoys> = Arc::new(ConvoyEngine::new(Arc::clone(&convoys), backend.clone(), clock));
        assert!(path.set(engine).is_ok(), "a new path is empty");
        Self { convoys, path, _dir: None }
    }

    /// [`Self::over`] a new decided store at `home`, which lives as long as the value.
    pub fn open(home: &str, namespace: &str, backend: &ResourceBackend, clock: Arc<dyn Clock>, reads: Arc<dyn LifecycleReads>) -> Self {
        let dir = tempfile::tempdir().expect("decided store directory");
        let opened = Self::open_in(dir.path(), home, namespace, backend, clock, reads);
        Self { _dir: Some(dir), ..opened }
    }

    /// [`Self::over`] a new decided store at `home` in `dir`, which the caller keeps: the
    /// [`path`](Self::path) keeps the store open after this value is dropped.
    pub fn open_in(
        dir: &std::path::Path,
        home: &str,
        namespace: &str,
        backend: &ResourceBackend,
        clock: Arc<dyn Clock>,
        reads: Arc<dyn LifecycleReads>,
    ) -> Self {
        let registry = flotilla_engine_store::registry().expect("engine kind registry");
        let service = Arc::new(Service::open(&dir.join("decided.sqlite"), home, registry).expect("decided store"));
        Self::over(&service, namespace, backend, clock, reads)
    }

    /// The path, as the crew and admission take it.
    pub fn path(&self) -> EngineConvoyPath {
        Arc::clone(&self.path)
    }

    /// The engine's convoys at this home.
    pub fn convoys(&self) -> &Arc<Convoys> {
        &self.convoys
    }

    /// Admits `convoy` on this home's path; see [`launch`].
    pub async fn launch(&self, convoy: ResourceObject<Convoy>) -> String {
        launch(&self.path, convoy).await
    }
}

/// Admits `convoy` on `path` as the next launch of what it is a launch of, the way admission
/// does: its spec, labels and annotations as they are and its status's workflow snapshot frozen.
/// The status's other fields this home notes are kept as its notes, and a status that ended
/// Failed or Abandoned ends the launch so. Returns the launch's name.
pub async fn launch(path: &EngineConvoyPath, mut convoy: ResourceObject<Convoy>) -> String {
    let engine = path.get().expect("the Convoy path is installed");
    let of = engine_name(&convoy).expect("a convoy names what it is a launch of");
    let launch = engine.reserve(&of).await.expect("reserve a launch");
    convoy.metadata.name = launch.clone();
    engine.admit_reserved(&convoy).await.expect("admit the launch");
    if let Some(status) = &convoy.status {
        set_notes(path, &launch, status).expect("note the launch's status");
        match status.phase {
            ConvoyPhase::Failed => engine.fail(&convoy, "operator", "failed").await.expect("fail the launch"),
            ConvoyPhase::Abandoned => engine.abandon(&convoy, "operator", "abandoned").await.expect("abandon the launch"),
            _ => {}
        }
    }
    launch
}

/// Keeps the promises of every work of launch `convoy` at `path`'s home, as an operator's verdict
/// on each: it reads Landing.
pub async fn keep_all(path: &EngineConvoyPath, convoy: &ResourceObject<Convoy>) {
    let engine = path.get().expect("the Convoy path is installed");
    let snapshot = convoy.status.as_ref().and_then(|status| status.workflow_snapshot.as_ref()).expect("a launch's frozen workflow");
    for vessel in &snapshot.vessels {
        let scope = VerdictScope { vessel: vessel.name.clone(), role: None };
        engine.keep_by_verdict(convoy, &scope, "operator", "kept").await.expect("keep the work");
    }
}

/// [`keep_all`], then delivers launch `convoy`: it reads Landed.
pub async fn land(path: &EngineConvoyPath, convoy: &ResourceObject<Convoy>) {
    keep_all(path, convoy).await;
    path.get().expect("the Convoy path is installed").deliver(convoy, "claim").await.expect("deliver the launch");
}

/// Convoy `name` in `namespace` as admission hands it to the engine: `spec`, and `status` with
/// its frozen workflow snapshot.
pub fn convoy(namespace: &str, name: &str, spec: ConvoySpec, status: ConvoyStatus) -> ResourceObject<Convoy> {
    ResourceObject {
        metadata: ObjectMeta {
            name: name.to_owned(),
            namespace: namespace.to_owned(),
            resource_version: String::new(),
            labels: Default::default(),
            annotations: Default::default(),
            owner_references: Vec::new(),
            finalizers: Vec::new(),
            deletion_timestamp: None,
            creation_timestamp: chrono::Utc::now(),
            merge: None,
        },
        spec,
        status: Some(status),
    }
}

/// An agent seat for `role`, as a workflow declares one: on the engine a convoy owes its agent
/// seats' work, so a convoy needs one to be admitted.
pub fn agent(role: &str) -> CrewSpec {
    CrewSpec::builder()
        .role(role.to_owned())
        .source(CrewSource::Agent { selector: Selector::for_capability("code"), prompt: None, brief_template: None })
        .build()
}

/// A status whose frozen workflow has one vessel, `work`, crewed by an [`agent`] seat for `role`:
/// the least a convoy owes to be admitted.
pub fn single_agent(role: &str) -> ConvoyStatus {
    ConvoyStatus {
        workflow_snapshot: Some(WorkflowSnapshot {
            cascade: None,
            vessels: vec![VesselRequirement::builder().name("work".to_owned()).crew(vec![agent(role)]).build()],
            exit: None,
            turn_delivery: Default::default(),
            stall_nudges: Default::default(),
            supervision: None,
        }),
        ..ConvoyStatus::default()
    }
}

/// A test's lifecycle reads, with the engine's convoys added.
struct WithConvoys {
    reads: Arc<dyn LifecycleReads>,
    records: EngineConvoyRecords,
}

impl LifecycleReads for WithConvoys {
    fn try_reading(&self, kind: ProvisioningKind, name: &str, origin: Option<&str>) -> Result<Option<LifecycleReading>, String> {
        self.reads.try_reading(kind, name, origin)
    }

    fn vessels(&self) -> Result<&dyn VesselRecords, String> {
        self.reads.vessels()
    }

    fn terminals(&self) -> Result<&dyn TerminalRecords, String> {
        self.reads.terminals()
    }

    fn convoys(&self) -> Result<&dyn ConvoyRecords, String> {
        Ok(&self.records)
    }
}
