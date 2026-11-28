//! Each vessel's Cleat daemon, published through the Tender instance of the account this daemon
//! runs as (#4401), and each daemon of the sessions here in no vessel, under that instance's own
//! scope. A pass compares the daemons this home's launches run with this instance's own `cleat`
//! publications as Tender lists them, so a Tender restart (which withdraws every API publication
//! under a new epoch) and a vessel that ended or left here are settled alike. A pass runs when a
//! launch here changes, when an environment's handle is registered, and when Tender's control
//! socket appears or goes.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use flotilla_protocol::result_set::{CleatEndpoint, CLEAT_PUBLICATION_LABEL};
use flotilla_provider_runtime::{http_channel_label, HttpClient, ReqwestHttpClient};
use flotilla_resources::{
    terminal_records::{TerminalChange, TerminalRecord},
    ConditionValue, HostCondition,
};
use flotilla_store::load_metrics::{attributed, caller};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use reqwest::{header, Method, Request, StatusCode, Url};
use rigging_http_wire::Encoding;
use serde::{de::DeserializeOwned, Serialize};
use tender_types::{
    api::{ErrorBody, Instance, PublicationEntry, PublicationList, PublishEndpoint, IDENTITY, PUBLICATIONS, PUBLISH, WITHDRAW},
    paths::control_socket,
    Availability, Label, Publication, PublicationId, Scope,
};
use tokio::{
    sync::{
        broadcast::{self, error::RecvError},
        mpsc, watch,
    },
    task::JoinHandle,
};
use tracing::{debug, info, warn};

use super::{health::RuntimeHealth, state::ControllerRuntimeState};

/// The host condition saying why nothing is published, present only while Tender is unreachable.
pub(super) const TENDER_CONDITION_TYPE: &str = "Tender";

/// One Cleat daemon a launch here runs, and the generation that holds its sessions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CarriedDaemon {
    /// The Vessel resource it runs for; none for sessions in no vessel, which this instance
    /// publishes under its own scope.
    pub vessel: Option<String>,
    pub name: String,
    pub display: String,
    pub endpoint: CleatEndpoint,
    pub environment: String,
}

impl CarriedDaemon {
    /// Its publication's identity, given this instance's own scope.
    pub fn id(&self, instance: &Scope) -> PublicationId {
        let scope = self.vessel.as_ref().map_or_else(|| instance.clone(), |vessel| Scope(self.endpoint.publication(vessel).scope));
        PublicationId { scope, name: self.name.clone() }
    }
}

/// The daemons the running launches at `host` in `records` name, one per vessel (or none) and
/// logical daemon: the newest launch's generation, which is the one a handover moved its
/// sessions to.
pub(super) fn carried(records: impl IntoIterator<Item = TerminalRecord>, host: &str) -> Vec<CarriedDaemon> {
    let mut newest = BTreeMap::<(Option<String>, String), (u64, CarriedDaemon)>::new();
    for record in records {
        let Some(endpoint) = record.cleat_endpoint() else { continue };
        let (name, display) = match &record.vessel {
            Some(vessel) => (endpoint.publication(vessel).name, format!("Cleat daemon {} of vessel {vessel}", endpoint.logical_daemon())),
            None => (endpoint.host_publication_name(host), format!("Cleat daemon {} of {host}'s sessions", endpoint.logical_daemon())),
        };
        let key = (record.vessel.clone(), name.clone());
        let launch = record.launch_number();
        if newest.get(&key).is_some_and(|(held, _)| *held >= launch) {
            continue;
        }
        let daemon = CarriedDaemon { vessel: record.vessel.clone(), name, display, endpoint, environment: record.environment.clone() };
        newest.insert(key, (launch, daemon));
    }
    newest.into_values().map(|(_, daemon)| daemon).collect()
}

/// Where each carried daemon's socket is on this machine.
#[async_trait]
pub(super) trait CleatSockets: Send + Sync {
    async fn socket(&self, daemon: &CarriedDaemon) -> Result<PathBuf, String>;
}

/// A host-direct vessel's daemon is its own socket; a provisioned or SSH host vessel's is the
/// local socket its vessel agent keeps reaching it.
#[async_trait]
impl CleatSockets for Arc<ControllerRuntimeState> {
    async fn socket(&self, daemon: &CarriedDaemon) -> Result<PathBuf, String> {
        let guest = daemon.endpoint.socket();
        if daemon.environment == self.host_direct_environment_name {
            return Ok(guest);
        }
        let access = match (self.ssh_host.get(&daemon.environment), &daemon.vessel) {
            (Some(profile), Some(vessel)) => Some(self.vessel_access(profile, vessel).await?),
            (Some(_), None) => return Err("an SSH host's sessions are reached through their vessels".to_owned()),
            (None, _) => None,
        };
        let agent = self.vessel_agent(&daemon.environment, access.as_deref()).await?;
        agent.cleat_socket(daemon.endpoint.logical_daemon(), &guest).await
    }
}

pub(super) struct CleatPublisher {
    /// The Tender runtime directory of this daemon's account, or why there is none.
    runtime: Result<PathBuf, String>,
    /// This daemon's host, which names the daemons of its sessions in no vessel.
    host: String,
    health: RuntimeHealth,
}

impl CleatPublisher {
    pub(super) fn new(runtime: Result<PathBuf, String>, host: impl Into<String>, health: RuntimeHealth) -> Self {
        Self { runtime, host: host.into(), health }
    }

    /// Publish every daemon in `carried` and withdraw this instance's other Cleat publications.
    pub(super) async fn pass(&self, carried: &[CarriedDaemon], sockets: &dyn CleatSockets) {
        let (control, instance, listing) = match self.listing().await {
            Ok(found) => found,
            Err(reason) => return self.unreachable(reason),
        };
        self.reachable();
        let held = own_cleat_publications(listing.publications, &instance);
        let carried = carried.iter().map(|daemon| (daemon.id(&instance), daemon)).collect::<Vec<_>>();
        let wanted = carried.iter().map(|(id, _)| id).collect::<BTreeSet<_>>();
        for id in held.keys().filter(|id| !wanted.contains(id)) {
            match control.withdraw(id).await {
                Ok(()) => info!(publication = %id, "withdrew a Cleat daemon no launch here runs"),
                Err(ControlError::Refused { code, .. }) if matches!(code.as_str(), "denied" | "unknown_publication" | "withdrawn") => {
                    debug!(publication = %id, %code, "a Cleat publication was already withdrawn or taken over");
                }
                Err(error) => warn!(publication = %id, %error, "withdrawing a Cleat publication failed; retrying next pass"),
            }
        }
        for (id, daemon) in carried {
            let socket = match sockets.socket(daemon).await {
                Ok(socket) => socket,
                Err(error) => {
                    warn!(publication = %id, %error, "a Cleat daemon has no local socket to publish yet");
                    continue;
                }
            };
            match held.get(&id) {
                Some(published) if *published == socket => continue,
                // Tender keeps a live publication's socket: a new one replaces it by withdrawal.
                Some(_) => {
                    if let Err(error) = control.withdraw(&id).await {
                        warn!(publication = %id, %error, "withdrawing a Cleat daemon's earlier socket failed; retrying next pass");
                        continue;
                    }
                }
                None => {}
            }
            let request = PublishEndpoint {
                id: id.clone(),
                label: Label(CLEAT_PUBLICATION_LABEL.to_owned()),
                display: daemon.display.clone(),
                endpoint: socket,
                audience: None,
            };
            match control.publish(&request).await {
                Ok(publication) => {
                    info!(publication = %publication.id, availability = ?publication.availability, "published a Cleat daemon")
                }
                Err(error) => warn!(publication = %id, %error, "publishing a Cleat daemon failed; retrying next pass"),
            }
        }
    }

    /// The control API, this instance's own scope and its publications.
    async fn listing(&self) -> Result<(TenderControl, Scope, PublicationList), String> {
        let runtime = self.runtime.as_ref().map_err(|reason| format!("no Tender runtime directory: {reason}"))?;
        let control = TenderControl::open(runtime)?;
        let unreachable = |error: ControlError| format!("{}: {error}", control_socket(runtime).display());
        let instance = control.identity().await.map_err(unreachable)?;
        let listing = control.publications().await.map_err(unreachable)?;
        Ok((control, Scope::instance(&instance.fingerprint), listing))
    }

    fn unreachable(&self, reason: String) {
        let message = format!("no Tender instance reachable, so no vessel's Cleat daemon is published: {reason}");
        let mut failures = self.health.failures.lock().expect("runtime health lock poisoned");
        if failures.get(TENDER_CONDITION_TYPE).is_some_and(|held| held.message == message) {
            return;
        }
        warn!(%reason, "no Tender instance reachable; vessels' Cleat daemons are not published");
        let condition = HostCondition::builder()
            .condition_type(TENDER_CONDITION_TYPE)
            .value(ConditionValue::False)
            .reason("TenderUnreachable")
            .message(message)
            .observed_at(flotilla_resources::clock::now())
            // Vessels run and attach through flotilla without it.
            .blocks_readiness(false)
            .blocks_forge_observation(false)
            .build();
        failures.insert(TENDER_CONDITION_TYPE.to_owned(), condition);
        drop(failures);
        self.health.heartbeat_now.notify_one();
    }

    fn reachable(&self) {
        if self.health.failures.lock().expect("runtime health lock poisoned").remove(TENDER_CONDITION_TYPE).is_some() {
            info!("a Tender instance answers again; publishing the vessels' Cleat daemons");
            self.health.heartbeat_now.notify_one();
        }
    }
}

/// This instance's own live publications of Cleat daemons, in vessel scopes and its own, with
/// their sockets.
fn own_cleat_publications(entries: Vec<PublicationEntry>, instance: &Scope) -> BTreeMap<PublicationId, PathBuf> {
    entries
        .into_iter()
        .filter_map(|PublicationEntry { publication, endpoint }| {
            let Publication { id, label, availability, .. } = publication;
            let ours = label.0 == CLEAT_PUBLICATION_LABEL
                && (id.scope.is_vessel() || id.scope == *instance)
                && id.name.starts_with("cleat/")
                && availability != Availability::Withdrawn;
            ours.then_some((id, endpoint?))
        })
        .collect()
}

/// Publishes this home's Cleat daemons as their launches change, as environments are registered
/// and as Tender comes and goes.
pub(super) fn spawn_cleat_publisher(state: Arc<ControllerRuntimeState>, publisher: Arc<CleatPublisher>) -> JoinHandle<()> {
    tokio::spawn(attributed(caller("cleat publications"), async move {
        let reads = state.daemon.lifecycle_reads();
        let changes = match reads.terminals() {
            Ok(terminals) => terminals.changes(),
            Err(error) => return warn!(%error, "the terminal launches cannot be followed; no vessel's Cleat daemon is published"),
        };
        let home = reads.terminals().map(|terminals| terminals.home().to_owned()).unwrap_or_default();
        let launches = || reads.terminals().and_then(|terminals| terminals.at(terminals.home()));
        let registered = state.environment_registered.subscribe();
        follow(&publisher, &home, launches, changes, registered, &state).await
    }))
}

/// Passes once, then again at each launch change at this home, each environment registered (a
/// pass before a provisioned environment's handle is back after a restart finds no socket for its
/// vessels) and each change to Tender's control socket, until the launches' feed closes.
pub(super) async fn follow(
    publisher: &CleatPublisher,
    home: &str,
    launches: impl Fn() -> Result<Vec<TerminalRecord>, String>,
    mut changes: broadcast::Receiver<TerminalChange>,
    mut registered: watch::Receiver<()>,
    sockets: &dyn CleatSockets,
) {
    let (wake, mut woken) = mpsc::unbounded_channel();
    let mut tender = publisher.runtime.as_ref().ok().map(|runtime| TenderWatch::new(runtime.clone(), wake.clone()));
    let forward_home = home.to_owned();
    let registrations = wake.clone();
    let forward = tokio::spawn(async move {
        loop {
            match changes.recv().await {
                Ok(TerminalChange::Changed { home: changed, .. }) if changed != forward_home => {}
                Ok(_) | Err(RecvError::Lagged(_)) => drop(wake.send(())),
                Err(RecvError::Closed) => return,
            }
        }
    });
    registered.mark_unchanged();
    let forward_registrations = tokio::spawn(async move {
        while registered.changed().await.is_ok() {
            let _ = registrations.send(());
        }
    });
    loop {
        if let Some(tender) = tender.as_mut() {
            tender.arm();
        }
        match launches() {
            Ok(records) => publisher.pass(&carried(records.into_iter().filter(TerminalRecord::is_running), &publisher.host), sockets).await,
            Err(error) => warn!(%error, "the terminal launches here could not be read; Cleat publications wait for the next change"),
        }
        if woken.recv().await.is_none() {
            break;
        }
        while woken.try_recv().is_ok() {}
    }
    forward.abort();
    forward_registrations.abort();
}

/// Tender's runtime directory, watched for its control socket appearing or going: a Tender that
/// starts binds it afresh, under a new epoch. Its parent is watched while it does not exist.
struct TenderWatch {
    runtime: PathBuf,
    watcher: Option<RecommendedWatcher>,
    watching: Option<PathBuf>,
}

impl TenderWatch {
    fn new(runtime: PathBuf, wake: mpsc::UnboundedSender<()>) -> Self {
        let (directory, control) = (runtime.clone(), control_socket(&runtime));
        let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            // An error may hide a change, so it wakes a pass too; reading the directory changes nothing.
            let changed = |event: notify::Event| {
                !matches!(event.kind, EventKind::Access(_)) && event.paths.iter().any(|path| *path == directory || *path == control)
            };
            if event.map_or(true, changed) {
                let _ = wake.send(());
            }
        });
        let watcher = watcher.inspect_err(|error| warn!(%error, "not watching for Tender; only launch changes publish")).ok();
        Self { runtime, watcher, watching: None }
    }

    fn arm(&mut self) {
        let target = if self.runtime.is_dir() { Some(self.runtime.as_path()) } else { self.runtime.parent() };
        let (Some(watcher), Some(target)) = (self.watcher.as_mut(), target) else { return };
        if self.watching.as_deref() == Some(target) {
            return;
        }
        if let Some(watching) = self.watching.take() {
            let _ = watcher.unwatch(&watching);
        }
        match watcher.watch(target, RecursiveMode::NonRecursive) {
            Ok(()) => self.watching = Some(target.to_path_buf()),
            Err(error) => warn!(directory = %target.display(), %error, "not watching for Tender; only launch changes publish"),
        }
    }
}

#[derive(Debug)]
enum ControlError {
    Unreachable(String),
    Refused { status: StatusCode, code: String, message: String },
}

impl fmt::Display for ControlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreachable(reason) => f.write_str(reason),
            Self::Refused { status, code, message } => write!(f, "{status} {code}: {message}"),
        }
    }
}

/// Tender's control API on its control socket.
struct TenderControl(ReqwestHttpClient);

impl TenderControl {
    fn open(runtime: &Path) -> Result<Self, String> {
        ReqwestHttpClient::local(&control_socket(runtime)).map(Self)
    }

    async fn identity(&self) -> Result<Instance, ControlError> {
        self.call(Method::GET, IDENTITY, None::<&()>).await
    }

    async fn publications(&self) -> Result<PublicationList, ControlError> {
        self.call(Method::GET, PUBLICATIONS, None::<&()>).await
    }

    async fn publish(&self, request: &PublishEndpoint) -> Result<Publication, ControlError> {
        self.call(Method::POST, PUBLISH, Some(request)).await
    }

    async fn withdraw(&self, id: &PublicationId) -> Result<(), ControlError> {
        self.call(Method::POST, WITHDRAW, Some(id)).await
    }

    async fn call<R: DeserializeOwned>(&self, method: Method, path: &str, body: Option<&impl Serialize>) -> Result<R, ControlError> {
        let encoding = Encoding::Json;
        let url = Url::parse(&format!("http://tender{path}")).map_err(|error| ControlError::Unreachable(error.to_string()))?;
        let mut request = Request::new(method.clone(), url);
        request.headers_mut().insert(header::ACCEPT, header::HeaderValue::from_static(encoding.content_type()));
        if let Some(body) = body {
            let bytes = encoding.encode(body).map_err(ControlError::Unreachable)?;
            request.headers_mut().insert(header::CONTENT_TYPE, header::HeaderValue::from_static(encoding.content_type()));
            *request.body_mut() = Some(bytes.into());
        }
        let response = self.0.execute(request, &http_channel_label(method.as_str(), path)).await.map_err(ControlError::Unreachable)?;
        let status = response.status();
        if status.is_success() {
            return serde_json::from_slice(response.body())
                .map_err(|error| ControlError::Unreachable(format!("unexpected answer from Tender: {error}")));
        }
        let ErrorBody { code, message } = serde_json::from_slice(response.body()).unwrap_or_else(|_| ErrorBody {
            code: status.as_str().to_owned(),
            message: String::from_utf8_lossy(response.body()).into_owned(),
        });
        Err(ControlError::Refused { status, code, message })
    }
}
