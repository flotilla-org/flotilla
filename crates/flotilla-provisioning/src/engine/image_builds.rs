//! Image builds on the engine (#3108, ADR 0062): the demand an image build spec makes, the
//! launch a builder makes for the demands naming it, and the [`ImageBuildRecords`] port over the
//! decided store.
//!
//! Admission demands a build at its own home ([`ImageBuildDemand`], written only there). The
//! builder follows every home's demands, and its [`ImageBuildLauncher`] launches each build
//! demanded of it once (`<build>#<n>`): one launch serves the demands of every home for the same
//! build. A launch that gave up is launched again only on the operator's retry, at the builder; a
//! launch released once nothing demanded it is launched again by the next demand. The ImageBuild
//! controller writes every edge after the request.

use std::{collections::BTreeMap, sync::Arc};

use flotilla_engine_store::plain::{PendingChange, PlainError, PlainLog};
use flotilla_protocol::LifecycleReading;
use flotilla_resources::{
    image_build_records::{ImageBuildRecord, ImageBuildRecords},
    ImageBuildReservation, ImageBuildSpec, ImageDelivery,
};
use flotilla_schema::{Frozen, Kind, NewChange, Ref, Timestamp};
use flotilla_schema_sqlite::{stream::Service, Read, StoreError, StoreHandle};
use flotilla_state_machines::{machines::lifecycle::LifecycleState, Resource};

use super::{
    reads::{latest, ReadError, StoreReads, View},
    watch::EngineWatch,
    EngineError, Lifecycles,
};
use crate::stored::{
    host::{Connection, HostRecord},
    image_build::{Architecture, Delivery, ImageBuild, ImageBuildDemand, ImageBuildKind, ImageBuildRequest, Reservation},
};

/// The writer recorded on a demand's writes.
const DEMAND_WRITER: &str = "admission";

fn architecture(spec: &ImageBuildSpec) -> Result<Architecture, String> {
    match spec.architecture.as_str() {
        "amd64" => Ok(Architecture::Amd64),
        "arm64" => Ok(Architecture::Arm64),
        other => Err(format!("image build architecture `{other}` must be amd64 or arm64")),
    }
}

fn delivery(delivery: &ImageDelivery) -> Delivery {
    match delivery {
        ImageDelivery::Registry { repository } => Delivery::Registry { repository: repository.clone() },
        ImageDelivery::Local => Delivery::Local,
    }
}

fn image_delivery(delivery: &Delivery) -> ImageDelivery {
    match delivery {
        Delivery::Registry { repository } => ImageDelivery::Registry { repository: repository.clone() },
        Delivery::Local => ImageDelivery::Local,
    }
}

fn architecture_name(architecture: Architecture) -> String {
    match architecture {
        Architecture::Amd64 => "amd64",
        Architecture::Arm64 => "arm64",
    }
    .to_owned()
}

/// The demand `spec` makes of the builder whose Host record is `builder`.
pub fn demand_of(spec: &ImageBuildSpec, builder: Ref<HostRecord>) -> Result<ImageBuildDemand, String> {
    Ok(ImageBuildDemand {
        composition: spec.identity.clone(),
        architecture: architecture(spec)?,
        layers: Frozen::new(spec.layers.clone()).map_err(|error| format!("freeze the layers: {error}"))?,
        builder,
        delivery: delivery(&spec.delivery),
        reservation: Reservation { cpu: spec.reservation.cpu, disk_bytes: spec.reservation.disk_bytes },
    })
}

/// The spec `demand` shows its readers.
pub fn demand_spec(demand: &ImageBuildDemand) -> ImageBuildSpec {
    ImageBuildSpec {
        identity: demand.composition.clone(),
        architecture: architecture_name(demand.architecture),
        layers: demand.layers.value().clone(),
        host_ref: demand.builder.name.clone(),
        delivery: image_delivery(&demand.delivery),
        reservation: ImageBuildReservation { cpu: demand.reservation.cpu, disk_bytes: demand.reservation.disk_bytes },
    }
}

/// The launch of build `name` that `demand` asks its builder for.
pub fn launch_of(name: &str, demand: &ImageBuildDemand) -> ImageBuildRequest {
    ImageBuildRequest {
        launch_of: name.to_owned(),
        launch: 0,
        composition: demand.composition.clone(),
        architecture: demand.architecture,
        layers: demand.layers.clone(),
        delivery: demand.delivery.clone(),
        reservation: demand.reservation.clone(),
    }
}

/// The spec `request` shows its readers, built by `builder`.
pub fn spec_of(request: &ImageBuildRequest, builder: &str) -> ImageBuildSpec {
    ImageBuildSpec {
        identity: request.composition.clone(),
        architecture: architecture_name(request.architecture),
        layers: request.layers.value().clone(),
        host_ref: builder.to_owned(),
        delivery: image_delivery(&request.delivery),
        reservation: ImageBuildReservation { cpu: request.reservation.cpu, disk_bytes: request.reservation.disk_bytes },
    }
}

/// The home of the builder build `name` is demanded of, as the first home's demand names it.
pub(crate) fn builder_home(read: &mut Read<'_>, homes: &[String], name: &str) -> Result<Option<String>, StoreError> {
    for home in homes {
        let Some(id) = read.resolve(ImageBuildDemand::CODE, name, home)? else { continue };
        if let Some(loaded) = read.load::<ImageBuildDemand>(id)? {
            return Ok(Some(loaded.value.builder.home));
        }
    }
    Ok(None)
}

/// The host whose daemon is `home`: the one Host record homed there that the daemon connects as.
fn host_at(read: &mut Read<'_>, homes: &[String], host: &str) -> Result<Option<String>, StoreError> {
    for home in homes {
        let Some(id) = read.resolve(HostRecord::CODE, host, home)? else { continue };
        if read.load::<HostRecord>(id)?.is_some() {
            return Ok(Some(home.clone()));
        }
    }
    Ok(None)
}

/// Whether any home demands build `name`, and the demands found, for the controller to follow.
pub(crate) fn demands_of(read: &mut Read<'_>, homes: &[String], name: &str) -> Result<Vec<flotilla_schema::Identity>, StoreError> {
    let mut found = Vec::new();
    for home in homes {
        // A withdrawn demand keeps its identity; only a held one demands.
        let Some(id) = read.resolve(ImageBuildDemand::CODE, name, home)? else { continue };
        if read.load::<ImageBuildDemand>(id)?.is_some() {
            found.push(flotilla_schema::Identity { kind: ImageBuildDemand::CODE, name: name.to_owned(), home: home.clone() });
        }
    }
    Ok(found)
}

/// How far `reading` got, for choosing between two homes' launches of one build.
fn rank(reading: &LifecycleReading) -> u8 {
    match reading {
        LifecycleReading::Ready { .. } => 3,
        LifecycleReading::InProgress => 2,
        LifecycleReading::Retrying { .. } => 1,
        _ => 0,
    }
}

fn record(view: View<ImageBuildKind>, builder: &str) -> ImageBuildRecord {
    let failures = match &view.state {
        LifecycleState::Requested { failures, .. } => *failures,
        _ => 0,
    };
    ImageBuildRecord {
        name: view.request.launch_of.clone(),
        launch: view.name,
        spec: spec_of(&view.request, builder),
        home: view.home,
        reading: view.judgement.reading(),
        failures,
    }
}

/// The builder host of the launches at `home`: the Host record homed there that its demands
/// name, read from any demand of `name`.
fn builder_host(read: &mut Read<'_>, homes: &[String], name: &str, home: &str) -> Result<String, StoreError> {
    for demand_home in homes {
        let Some(id) = read.resolve(ImageBuildDemand::CODE, name, demand_home)? else { continue };
        if let Some(loaded) = read.load::<ImageBuildDemand>(id)? {
            if loaded.value.builder.home == home {
                return Ok(loaded.value.builder.name);
            }
        }
    }
    // Every demand withdrawn: the builder is the daemon of its home.
    let mut after = None;
    loop {
        let page = read.scan::<HostRecord>(home, after, 16)?;
        for id in page.ids {
            if let Some(loaded) = read.load::<HostRecord>(id)? {
                if loaded.value.connection == Connection::Daemon {
                    return Ok(loaded.name);
                }
            }
        }
        after = page.next;
        if after.is_none() {
            return Ok(String::new());
        }
    }
}

/// The [`ImageBuildRecords`] port over one decided store: this home's demands and, at a builder,
/// its launches, and every home's through keyed and indexed reads.
pub struct EngineImageBuilds {
    lifecycles: Arc<dyn Lifecycles<ImageBuildKind>>,
    reads: StoreReads,
    log: PlainLog,
    homes: Box<dyn Fn() -> Vec<String> + Send + Sync>,
}

impl EngineImageBuilds {
    /// The port over `store`, whose homes `homes` names.
    pub fn new(
        lifecycles: Arc<dyn Lifecycles<ImageBuildKind>>,
        store: Arc<StoreHandle>,
        homes: impl Fn() -> Vec<String> + Send + Sync + 'static,
    ) -> Self {
        Self { lifecycles, reads: StoreReads::new(Arc::clone(&store)), log: PlainLog::new(store), homes: Box::new(homes) }
    }

    /// The port over `service`'s store and homes.
    pub fn over(lifecycles: Arc<dyn Lifecycles<ImageBuildKind>>, service: Arc<Service>) -> Self {
        let store = service.store();
        Self::new(lifecycles, store, move || service.homes())
    }

    fn home(&self) -> &str {
        self.lifecycles.home()
    }

    fn change(&self) -> PendingChange {
        PendingChange::new(NewChange::new(DEMAND_WRITER, Timestamp::from_datetime(&chrono::Utc::now())))
    }

    fn held_demand(&self, name: &str) -> Result<Option<flotilla_engine_store::plain::Plain<ImageBuildDemand>>, String> {
        self.log.get::<ImageBuildDemand>(name, self.home()).map_err(|error| format!("read ImageBuildDemand {name}: {error}"))
    }
}

impl ImageBuildRecords for EngineImageBuilds {
    fn get(&self, name: &str) -> Result<Option<ImageBuildRecord>, String> {
        let homes = (self.homes)();
        self.reads
            .read(|read| {
                let mut chosen: Option<ImageBuildRecord> = None;
                for home in &homes {
                    let Some(view) = latest::<ImageBuildKind>(read, name, home)? else { continue };
                    let builder = builder_host(read, &homes, name, home)?;
                    let found = record(view, &builder);
                    if chosen.as_ref().is_none_or(|held| rank(&found.reading) > rank(&held.reading)) {
                        chosen = Some(found);
                    }
                }
                Ok(chosen)
            })
            .map_err(|error: ReadError| format!("engine ImageBuild/{name}: {error}"))
    }

    fn demanded(&self, name: &str) -> Result<Option<ImageBuildSpec>, String> {
        Ok(self.held_demand(name)?.map(|held| demand_spec(&held.spec)))
    }

    fn demand(&self, name: &str, spec: &ImageBuildSpec) -> Result<bool, String> {
        spec.validate()?;
        if spec.resource_name() != name {
            return Err(format!("image build for this identity, architecture and delivery is named `{}`", spec.resource_name()));
        }
        let homes = (self.homes)();
        let builder = self
            .reads
            .read(|read| Ok(host_at(read, &homes, &spec.host_ref)?))
            .map_err(|error| format!("find builder host {}: {error}", spec.host_ref))?
            .ok_or_else(|| format!("builder host `{}` has no Host record here", spec.host_ref))?;
        let demand = demand_of(spec, Ref::new(spec.host_ref.clone(), builder))?;
        let held = |held: flotilla_engine_store::plain::Plain<ImageBuildDemand>| {
            if held.spec == demand {
                Ok(false)
            } else {
                Err(format!("ImageBuildDemand `{name}` already demands another build"))
            }
        };
        if let Some(current) = self.held_demand(name)? {
            return held(current);
        }
        match self.log.create(name, &demand, &self.change()) {
            Ok(_) => Ok(true),
            // A concurrent admission demanded it first.
            Err(PlainError::Exists { .. }) => match self.held_demand(name)? {
                Some(current) => held(current),
                None => Err(format!("ImageBuildDemand `{name}` was withdrawn while it was demanded")),
            },
            Err(error) => Err(format!("demand image build {name}: {error}")),
        }
    }

    fn withdraw(&self, name: &str) -> Result<(), String> {
        let Some(current) = self.held_demand(name)? else { return Ok(()) };
        match self.log.delete::<ImageBuildDemand>(name, current.version, &self.change()) {
            Ok(()) | Err(PlainError::Missing { .. }) => Ok(()),
            Err(error) => Err(format!("withdraw image build demand {name}: {error}")),
        }
    }

    fn builder_home(&self, name: &str) -> Result<Option<String>, String> {
        let homes = (self.homes)();
        self.reads.read(|read| Ok(builder_home(read, &homes, name)?)).map_err(|error| format!("find the builder of {name}: {error}"))
    }

    fn retry(&self, name: &str) -> Result<String, String> {
        let held = self.lifecycles.newest_launch(name).ok_or_else(|| format!("image build {name} is not launched here"))?;
        if !matches!(held.state(), LifecycleState::GaveUp { .. }) {
            return Err(format!("image build {} has not given up", held.name()));
        }
        self.lifecycles
            .replace_launch(name, held.name(), held.spec().clone(), Timestamp::from_datetime(&chrono::Utc::now()))
            .map_err(|error| format!("retry image build {name}: {error}"))
    }
}

/// The builder's launcher: launches each build demanded of this host, at any home, once. It
/// passes over the demands at its start and again at each commit of a demand or a launch.
pub struct ImageBuildLauncher {
    lifecycles: Arc<dyn Lifecycles<ImageBuildKind>>,
    watch: EngineWatch,
    builder: Ref<HostRecord>,
}

impl ImageBuildLauncher {
    /// The launcher of `host`, whose daemon is this home.
    pub fn start(lifecycles: Arc<dyn Lifecycles<ImageBuildKind>>, service: Arc<Service>, host: &str) -> Result<Self, StoreError> {
        let builder = Ref::new(host.to_owned(), lifecycles.home().to_owned());
        let watch = EngineWatch::start(service, [ImageBuildDemand::CODE, <Resource<ImageBuild> as Kind>::CODE])?;
        Ok(Self { lifecycles, watch, builder })
    }

    /// The demands naming this builder, by build, each home's in home order.
    fn demanded(read: &mut Read<'_>, builder: &Ref<HostRecord>) -> Result<BTreeMap<String, ImageBuildDemand>, StoreError> {
        let mut demanded = BTreeMap::new();
        let query = ImageBuildDemand::by_builder(builder);
        let mut after = None;
        loop {
            let page = read.page(&query, after.as_ref(), 256)?;
            for id in page.ids {
                let Some(loaded) = read.load::<ImageBuildDemand>(id)? else { continue };
                match demanded.get(&loaded.name) {
                    Some(held) if held != &loaded.value => {
                        // Two homes' demands of one name differ only under a changed reservation
                        // or a moved builder: the first one read is launched.
                        tracing::warn!(image_build = %loaded.name, home = %loaded.home, "image build demands disagree; launching the first");
                    }
                    Some(_) => {}
                    None => {
                        demanded.insert(loaded.name, loaded.value);
                    }
                }
            }
            match page.next {
                Some(next) => after = Some(next),
                None => return Ok(demanded),
            }
        }
    }

    /// Launches each demanded build with no live or given-up launch, on a blocking thread: the
    /// home's writes take the store's lock.
    async fn launch(&self, demanded: BTreeMap<String, ImageBuildDemand>) {
        let lifecycles = Arc::clone(&self.lifecycles);
        if let Err(error) = tokio::task::spawn_blocking(move || launch(lifecycles.as_ref(), demanded)).await {
            tracing::warn!(%error, "image build launches did not finish");
        }
    }

    /// Runs for as long as the store does.
    pub async fn run(mut self) {
        let builder = self.builder.clone();
        match self.watch.read(move |read| Self::demanded(read, &builder)).await {
            Ok(demanded) => self.launch(demanded).await,
            Err(error) => tracing::warn!(%error, "image build demands could not be read; reading again at the next commit"),
        }
        loop {
            let builder = self.builder.clone();
            match self.watch.next(move |read, _| Self::demanded(read, &builder)).await {
                Ok(demanded) => self.launch(demanded).await,
                Err(error) => tracing::warn!(%error, "image build demands could not be read; reading again at the next commit"),
            }
        }
    }
}

fn launch(lifecycles: &dyn Lifecycles<ImageBuildKind>, demanded: BTreeMap<String, ImageBuildDemand>) {
    let at = Timestamp::from_datetime(&chrono::Utc::now());
    for (name, demand) in demanded {
        let launched = match lifecycles.newest_launch(&name) {
            None => lifecycles.request(&name, launch_of(&name, &demand), at).map(|created| created.name().to_owned()),
            // Released once nothing demanded it: demanded again, it is launched again.
            Some(held) if matches!(held.state(), LifecycleState::Gone { .. }) => {
                lifecycles.replace_launch(&name, held.name(), launch_of(&name, &demand), at)
            }
            // Live, or given up until the operator's retry.
            Some(_) => continue,
        };
        match launched {
            Ok(launch) => tracing::info!(image_build = %name, %launch, "image build launched"),
            Err(EngineError::Raced { .. }) => {}
            Err(error) => tracing::warn!(image_build = %name, %error, "image build not launched"),
        }
    }
}
