//! Real decided-store fixture shared by lifecycle reader contract tests.
pub mod agent_sessions;
pub mod checkouts;
pub mod clones;
pub mod environments;
pub mod image_build;
mod memory_store;
pub mod other_home;
pub mod terminals;
pub mod vessels;
pub use agent_sessions::AgentSessionsFixture;
pub use checkouts::CheckoutFixture;
pub use clones::CloneFixture;
pub use environments::EnvironmentFixture;
pub use memory_store::memory_store;
pub use other_home::{other_home_for, put_host_at_its_home, put_host_at_its_home_heard, replicate_home, replicate_home_at, OtherHome};
use std::sync::{Arc, Mutex};
pub use terminals::TerminalsFixture;
pub use vessels::VesselsFixture;

use flotilla_provisioning::engine::reads::{LifecycleReadPort, StoreReads};
use flotilla_resources::{environment_records::EnvironmentRecords, lifecycle_reads::LifecycleReads, ResourceObject};
use flotilla_schema_sqlite::Store;

/// The engine's environments, image builds, clones, checkouts, terminal sessions and vessels homed
/// at one home on one store, each fixture's `reads` serving them all, as a daemon's lifecycle reads do.
pub struct EngineFixture {
    pub environments: EnvironmentFixture,
    pub image_builds: image_build::ImageBuildFixture,
    pub clones: CloneFixture,
    pub checkouts: CheckoutFixture,
    pub terminals: TerminalsFixture,
    pub vessels: VesselsFixture,
}

impl Default for EngineFixture {
    fn default() -> Self {
        Self::at("home")
    }
}

impl EngineFixture {
    /// An engine whose records are homed at `home`.
    pub fn at(home: &str) -> Self {
        Self::over(home, CheckoutFixture::at(home).store)
    }

    /// An engine over `store`, whose home is `home`: a daemon's decided store, say.
    pub fn over(home: &str, store: Arc<Mutex<Store>>) -> Self {
        Self::with_terminals(
            home,
            Arc::clone(&store),
            image_build::ImageBuildFixture::over(home, Arc::clone(&store)),
            TerminalsFixture::over(home, Arc::clone(&store)),
            VesselsFixture::over(home, store),
        )
    }

    /// An engine over a daemon's decided store `service`, whose terminal sessions and builder hosts
    /// are read at every home it holds: another daemon's arrive as its log is replicated in.
    pub fn over_service(service: &Arc<flotilla_schema_sqlite::stream::Service>) -> Self {
        Self::with_terminals(
            service.home(),
            service.store(),
            image_build::ImageBuildFixture::over_service(service),
            TerminalsFixture::over_service(service),
            VesselsFixture::over_service(service),
        )
    }

    fn with_terminals(
        home: &str,
        store: Arc<Mutex<Store>>,
        mut image_builds: image_build::ImageBuildFixture,
        mut terminals: TerminalsFixture,
        mut vessels: VesselsFixture,
    ) -> Self {
        let mut environments = EnvironmentFixture::over(home, Arc::clone(&store));
        let mut clones = CloneFixture::over(home, Arc::clone(&store));
        let mut checkouts = CheckoutFixture::over(home, Arc::clone(&store));
        let reads: Arc<dyn LifecycleReads> = Arc::new(
            LifecycleReadPort::new(StoreReads::new(store), home.to_owned())
                .with_environments(Arc::clone(&environments.environments) as Arc<dyn EnvironmentRecords>)
                .with_image_builds(Arc::clone(&image_builds.builds) as Arc<dyn flotilla_resources::image_build_records::ImageBuildRecords>)
                .with_clones(Arc::clone(&clones.clones) as Arc<dyn flotilla_resources::clone_records::CloneRecords>)
                .with_checkouts(Arc::clone(&checkouts.checkouts) as Arc<dyn flotilla_resources::checkout_records::CheckoutRecords>)
                .with_terminals(Arc::clone(&terminals.terminals) as Arc<dyn flotilla_resources::terminal_records::TerminalRecords>)
                .with_vessels(Arc::clone(&vessels.vessels) as Arc<dyn flotilla_resources::vessel_records::VesselRecords>),
        );
        environments.reads = reads.clone();
        clones.reads = reads.clone();
        checkouts.reads = reads.clone();
        image_builds.reads = reads.clone();
        terminals.reads = reads.clone();
        vessels.reads = reads;
        Self { environments, image_builds, clones, checkouts, terminals, vessels }
    }

    /// The readings serving them all.
    pub fn reads(&self) -> Arc<dyn LifecycleReads> {
        self.checkouts.reads.clone()
    }
}

/// Endings shared by every resource reader; the identity belongs to that kind's realisation.
#[derive(Clone, Copy)]
pub enum Ending<'a> {
    Ready(&'a str),
    Lost(&'a str),
    Released,
    Failed(&'a str),
    GaveUp(&'a str),
}

/// A stale resource snapshot for pure reader seams; no provider or persistence is replaced.
pub fn object<T: flotilla_resources::Resource>(name: &str, spec: T::Spec, status: T::Status) -> ResourceObject<T> {
    ResourceObject {
        metadata: flotilla_resources::ObjectMeta {
            name: name.into(),
            namespace: "flotilla".into(),
            resource_version: "1".into(),
            labels: Default::default(),
            annotations: Default::default(),
            owner_references: Vec::new(),
            finalizers: Vec::new(),
            deletion_timestamp: None,
            creation_timestamp: chrono::DateTime::UNIX_EPOCH,
            merge: None,
        },
        spec,
        status: Some(status),
    }
}

/// Store a stale snapshot through the real backend's validation and versioning.
pub async fn store_object<T: flotilla_resources::Resource>(
    backend: &flotilla_store::ResourceBackend,
    object: ResourceObject<T>,
) -> ResourceObject<T> {
    let records = backend.using::<T>(&object.metadata.namespace);
    let created = records.create(&flotilla_resources::InputMeta::from(&object.metadata), &object.spec).await.expect("resource");
    match object.status {
        Some(status) => {
            records.update_status(&created.metadata.name, &created.metadata.resource_version, &status).await.expect("stale status")
        }
        None => created,
    }
}

/// A one-layer image build on host `builder`, whose layer provides `tool:verified`.
pub fn image_build_spec() -> flotilla_resources::ImageBuildSpec {
    use flotilla_resources::{ImageBuildReservation, ImageBuildSpec, ImageDelivery, ImageLayerSource, ImageLayerSpec, ImageLayerTier};
    let layers = vec![ImageLayerSpec::builder()
        .name("base".into())
        .version("1".into())
        .tier(ImageLayerTier::Base)
        .source(ImageLayerSource::Stdlib)
        .from(format!("ubuntu@sha256:{}", "a".repeat(64)))
        .fragment("FROM ${BASE}\n".into())
        .probe("true".into())
        .provides(std::collections::BTreeSet::from(["tool:verified".into()]))
        .build()];
    ImageBuildSpec::builder()
        .identity(flotilla_resources::image_identity(&layers))
        .architecture("amd64".into())
        .layers(layers)
        .host_ref("builder".into())
        .delivery(ImageDelivery::Local)
        .reservation(ImageBuildReservation { cpu: 1, disk_bytes: 1 })
        .build()
}
