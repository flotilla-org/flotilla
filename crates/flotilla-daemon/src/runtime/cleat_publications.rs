//! Each vessel's Cleat daemon, published through the Tender instance of the account this daemon
//! runs as (#4401). A pass compares the daemons this home's vessels run with this instance's own
//! `cleat` publications as Tender lists them, so a Tender restart (which withdraws every API
//! publication under a new epoch) and a vessel that ended or left here are settled alike.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use flotilla_protocol::result_set::{CleatEndpoint, CLEAT_PUBLICATION_LABEL};
use flotilla_provider_runtime::{http_channel_label, HttpClient, ReqwestHttpClient};
use flotilla_resources::{terminal_records::TerminalRecord, ConditionValue, HostCondition};
use reqwest::{header, Method, Request, StatusCode, Url};
use rigging_http_wire::Encoding;
use serde::{de::DeserializeOwned, Serialize};
use tender_types::{
    api::{ErrorBody, PublicationEntry, PublicationList, PublishEndpoint, PUBLICATIONS, PUBLISH, WITHDRAW},
    paths::control_socket,
    Availability, Label, Publication, PublicationId,
};
use tokio::{sync::Mutex, task::JoinHandle};
use tracing::{debug, info, warn};

use super::{
    health::RuntimeHealth,
    state::ControllerRuntimeState,
    tasks::{spawn_periodic_task, PeriodicTaskStart},
};

/// The host condition saying why nothing is published, present only while Tender is unreachable.
pub(super) const TENDER_CONDITION_TYPE: &str = "Tender";
const PUBLISH_INTERVAL: Duration = Duration::from_secs(5);
/// A wedged Tender must not hold the pass.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);

/// One Cleat daemon a vessel carried here runs, and the generation that holds its sessions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CarriedDaemon {
    pub id: PublicationId,
    pub display: String,
    pub endpoint: CleatEndpoint,
    pub environment: String,
    pub vessel: String,
}

impl CarriedDaemon {
    fn source(&self) -> (String, String, String) {
        (self.environment.clone(), self.endpoint.runtime_root.clone(), self.endpoint.daemon.clone())
    }
}

/// The daemons the running launches in `records` name, one per vessel and logical daemon: the
/// newest launch's generation, which is the one a handover moved its sessions to.
pub(super) fn carried(records: impl IntoIterator<Item = TerminalRecord>) -> Vec<CarriedDaemon> {
    let mut newest = BTreeMap::<PublicationId, (u64, CarriedDaemon)>::new();
    for record in records {
        let (Some(vessel), Some(endpoint)) = (record.vessel.clone(), record.cleat_endpoint()) else { continue };
        let publication = endpoint.publication(&vessel);
        let id = PublicationId::new(publication.scope, publication.name);
        let launch = record.launch_number();
        if newest.get(&id).is_some_and(|(held, _)| *held >= launch) {
            continue;
        }
        let display = format!("Cleat daemon {} of vessel {vessel}", endpoint.logical_daemon());
        newest.insert(id.clone(), (launch, CarriedDaemon { id, display, endpoint, environment: record.environment.clone(), vessel }));
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
        let access = match self.ssh_host.get(&daemon.environment) {
            Some(profile) => Some(self.vessel_access(profile, &daemon.vessel).await?),
            None => None,
        };
        let agent = self.vessel_agent(&daemon.environment, access.as_deref()).await?;
        agent.cleat_socket(daemon.endpoint.logical_daemon(), &guest).await
    }
}

pub(super) struct CleatPublisher {
    /// The Tender runtime directory of this daemon's account, or why there is none.
    runtime: Result<PathBuf, String>,
    health: RuntimeHealth,
    state: Mutex<PublisherState>,
}

#[derive(Default)]
struct PublisherState {
    epoch: Option<String>,
    /// Each daemon generation's local socket, resolved once: drydock's exposures and the vessel
    /// agents' forwards stand on their own, whatever Tender does.
    sockets: BTreeMap<(String, String, String), PathBuf>,
}

impl CleatPublisher {
    pub(super) fn new(runtime: Result<PathBuf, String>, health: RuntimeHealth) -> Self {
        Self { runtime, health, state: Mutex::default() }
    }

    /// Publish every daemon in `carried` and withdraw this instance's other Cleat publications.
    pub(super) async fn pass(&self, carried: &[CarriedDaemon], sockets: &dyn CleatSockets) {
        let (control, listing) = match self.listing().await {
            Ok(found) => found,
            Err(reason) => return self.unreachable(reason),
        };
        self.reachable();
        let mut state = self.state.lock().await;
        if state.epoch.as_deref() != Some(listing.epoch.as_str()) {
            if state.epoch.is_some() {
                info!(epoch = %listing.epoch, "Tender restarted; publishing the vessels' Cleat daemons again");
            }
            state.epoch = Some(listing.epoch.clone());
        }
        let held = own_cleat_publications(listing.publications);
        let wanted = carried.iter().map(|daemon| &daemon.id).collect::<BTreeSet<_>>();
        for id in held.keys().filter(|id| !wanted.contains(id)) {
            match control.withdraw(id).await {
                Ok(()) => info!(publication = %id, "withdrew a Cleat daemon no vessel here runs"),
                Err(ControlError::Refused { code, .. }) if matches!(code.as_str(), "denied" | "unknown_publication" | "withdrawn") => {
                    debug!(publication = %id, %code, "a Cleat publication was already withdrawn or taken over");
                }
                Err(error) => warn!(publication = %id, %error, "withdrawing a Cleat publication failed; retrying next pass"),
            }
        }
        for daemon in carried {
            let socket = match state.sockets.get(&daemon.source()) {
                Some(socket) => socket.clone(),
                None => match sockets.socket(daemon).await {
                    Ok(socket) => state.sockets.entry(daemon.source()).or_insert(socket).clone(),
                    Err(error) => {
                        warn!(publication = %daemon.id, %error, "a vessel's Cleat daemon has no local socket to publish yet");
                        continue;
                    }
                },
            };
            match held.get(&daemon.id) {
                Some(published) if *published == socket => continue,
                // Tender keeps a live publication's socket: a new one replaces it by withdrawal.
                Some(_) => {
                    if let Err(error) = control.withdraw(&daemon.id).await {
                        warn!(publication = %daemon.id, %error, "withdrawing a Cleat daemon's earlier socket failed; retrying next pass");
                        continue;
                    }
                }
                None => {}
            }
            let request = PublishEndpoint {
                id: daemon.id.clone(),
                label: Label(CLEAT_PUBLICATION_LABEL.to_owned()),
                display: daemon.display.clone(),
                endpoint: socket,
                audience: None,
            };
            match control.publish(&request).await {
                Ok(publication) => {
                    info!(publication = %publication.id, availability = ?publication.availability, "published a vessel's Cleat daemon")
                }
                Err(error) => warn!(publication = %daemon.id, %error, "publishing a vessel's Cleat daemon failed; retrying next pass"),
            }
        }
        let sources = carried.iter().map(CarriedDaemon::source).collect::<BTreeSet<_>>();
        state.sockets.retain(|source, _| sources.contains(source));
    }

    async fn listing(&self) -> Result<(TenderControl, PublicationList), String> {
        let runtime = self.runtime.as_ref().map_err(|reason| format!("no Tender runtime directory: {reason}"))?;
        let control = TenderControl::open(runtime)?;
        let listing = control.publications().await.map_err(|error| format!("{}: {error}", control_socket(runtime).display()))?;
        Ok((control, listing))
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

/// This instance's own live publications of Cleat daemons in vessel scopes, with their sockets.
fn own_cleat_publications(entries: Vec<PublicationEntry>) -> BTreeMap<PublicationId, PathBuf> {
    entries
        .into_iter()
        .filter_map(|PublicationEntry { publication, endpoint }| {
            let Publication { id, label, availability, .. } = publication;
            let ours = label.0 == CLEAT_PUBLICATION_LABEL
                && id.scope.is_vessel()
                && id.name.starts_with("cleat/")
                && availability != Availability::Withdrawn;
            ours.then_some((id, endpoint?))
        })
        .collect()
}

/// Publishes this home's vessels' Cleat daemons every [`PUBLISH_INTERVAL`].
pub(super) fn spawn_cleat_publisher(state: Arc<ControllerRuntimeState>, publisher: Arc<CleatPublisher>) -> JoinHandle<()> {
    spawn_periodic_task("cleat publications", PUBLISH_INTERVAL, PeriodicTaskStart::Immediate, move || {
        let state = Arc::clone(&state);
        let publisher = Arc::clone(&publisher);
        async move {
            let reads = state.daemon.lifecycle_reads();
            let records = reads.terminals().and_then(|terminals| terminals.at(terminals.home()));
            match records {
                Ok(records) => publisher.pass(&carried(records.into_iter().filter(TerminalRecord::is_running)), &state).await,
                Err(error) => warn!(%error, "the terminal launches here could not be read; Cleat publications wait for the next pass"),
            }
        }
    })
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
        *request.timeout_mut() = Some(CONTROL_TIMEOUT);
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
