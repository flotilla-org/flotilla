//! Flotilla's view of drydock's provider API: the operations this adapter
//! calls, and their wire shapes as far as it reads them, serialized as
//! drydock serializes them. The composition root supplies the implementation.
use std::{collections::BTreeMap, net::SocketAddr};

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};

pub use crate::platform::Platform;

pub type ApiResult<T> = Result<T, ApiError>;
pub type ObservationStream = BoxStream<'static, ApiResult<ObserveRecord>>;
pub use rigging_stream::ByteStream;

#[async_trait]
pub trait ProviderApi: Send + Sync {
    async fn hello(&self) -> ApiResult<Hello>;
    async fn provider(&self, provider: &str) -> ApiResult<ProviderDeclaration>;
    async fn prepare(&self, provider: &str, request: &PrepareRequest) -> ApiResult<Prepared>;
    async fn provision(&self, provider: &str, request: &ProvisionRequest) -> ApiResult<Provisioned>;
    async fn list(&self, provider: &str) -> ApiResult<Vec<Environment>>;
    async fn inspect(&self, provider: &str, id: &str) -> ApiResult<Environment>;
    async fn destroy(&self, provider: &str, id: &str) -> ApiResult<()>;
    /// Every provider's events after `since`, starting with a snapshot.
    async fn observe(&self, since: Option<&str>) -> ApiResult<ObservationStream>;
    async fn put_inward(&self, provider: &str, id: &str, name: &str, request: &InwardRequest) -> ApiResult<Inward>;
    async fn inward(&self, provider: &str, id: &str, name: &str) -> ApiResult<Inward>;
    async fn put_outward(&self, provider: &str, id: &str, name: &str, request: &OutwardRequest) -> ApiResult<Outward>;
    async fn outward(&self, provider: &str, id: &str, name: &str) -> ApiResult<Outward>;
    /// How drydock reaches the vessel, by kind alone.
    async fn reach_kind(&self, provider: &str, id: &str) -> ApiResult<ReachKind>;
    /// A connection to the vessel's agent that drydock's host carries (a relay reach).
    async fn relay(&self, provider: &str, id: &str) -> ApiResult<ByteStream>;
}

/// A failed call. The text is drydock's own: a provider's explanation is
/// masked for credentials before it leaves the host.
#[derive(Debug, Clone, PartialEq)]
pub enum ApiError {
    /// The host answered with a protocol error.
    Protocol {
        kind: ProtocolErrorKind,
        reason: String,
        description: String,
    },
    NotReady,
    ReadinessTimeout,
    /// The call failed before the host answered, or its answer was not the API's.
    Other(String),
}
/// The protocol errors this adapter tells apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolErrorKind {
    NotFound,
    Refused,
    Other,
}
impl ApiError {
    /// A protocol error described as drydock describes one of its `class`.
    pub fn protocol(kind: ProtocolErrorKind, class: &str, reason: impl Into<String>) -> Self {
        let reason = reason.into();
        Self::Protocol { kind, description: format!("{class}: {reason}"), reason }
    }
    pub fn not_found(reason: impl Into<String>) -> Self {
        Self::protocol(ProtocolErrorKind::NotFound, "not_found", reason)
    }
    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::Protocol { kind: ProtocolErrorKind::NotFound, .. })
    }
}
impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Protocol { description, .. } => write!(f, "drydock provider {description}"),
            Self::NotReady => f.write_str("drydock provider not ready"),
            Self::ReadinessTimeout => f.write_str("drydock provider readiness timed out"),
            Self::Other(description) => f.write_str(description),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Hello {
    /// Whether the host speaks this build's protocol version.
    pub compatible: bool,
    pub host_version: String,
    pub loadable_modules: Vec<LoadableModule>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoadableModule {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderDeclaration {
    pub module: String,
    pub capabilities: Capabilities,
    pub readiness: Readiness,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Capabilities {
    pub material_modes: Vec<MaterialMode>,
    pub checkout_strategies: Vec<CheckoutStrategy>,
    pub workspace_root: String,
    pub fallback_runner: bool,
    /// The vessels share the host's filesystem (drydock's `local` module).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub host_filesystem: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaterialMode {
    Frozen,
    Pushed,
    Shared,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckoutStrategy {
    HostWorktree,
    HostClone,
    CloneInVessel,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Readiness {
    Ready,
    MissingTool { reason: String },
    MissingCredential { reason: String },
    VersionMismatch { reason: String },
    Unknown { reason: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Size {
    pub cpus: f64,
    pub memory_bytes: u64,
    pub disk_bytes: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageRouteKind {
    ProviderPush,
    RegistryPull,
    RemoteBuild,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrepareRequest {
    pub size: Size,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<Platform>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<ImageRouteKind>,
    pub image_digest: String,
    pub tools_digest: String,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Prepared {
    pub provider: String,
    pub id: String,
    pub platform: Platform,
    pub image_digest: String,
    pub tools_digest: String,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum EgressRequest {
    Open,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProvisionRequest {
    pub label: String,
    pub prepared: Prepared,
    pub size: Size,
    pub egress: EgressRequest,
    /// A key for the vessel; without one drydock makes and holds the vessel's key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_public_key: Option<String>,
}
/// A provision's environment: created by it, or one its label already named.
#[derive(Debug, Clone, PartialEq)]
pub enum Provisioned {
    Created(Environment),
    Existing(Environment),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Environment {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub provider: String,
    pub state: Observation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<Platform>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Observation {
    Running,
    Stopped { cause: StopCause },
    Gone { cause: GoneCause },
    Transitioning {},
    Unknown { reason: UnknownReason },
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopCause {
    SuspendedByRequest,
    PausedByProvider,
    GuestHalted,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GoneCause {
    LifetimeCapReached,
    EvictedByProvider,
    HostOutOfMemory,
    VmmExited { code: i32 },
    GuestPoweredOff,
    DestroyedAtProvider,
    MissingOnList,
    DestroyedByRequest,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnknownReason {
    ProviderStateUnknown,
    HostUnreachable,
    ProviderApiError,
    GuestUnresponsive,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ObserveRecord {
    Event {
        event: Event,
    },
    Snapshot {
        cursor: String,
        environments: Vec<Environment>,
        /// Providers whose environments the snapshot could not list, with why.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        unavailable_providers: BTreeMap<String, serde_json::Value>,
    },
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub cursor: String,
    pub id: String,
    pub provider: String,
    pub event: EventKind,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventKind {
    Created,
    Running {},
    Stopped { cause: StopCause },
    Paused { cause: StopCause },
    Resumed,
    Gone { cause: GoneCause },
    Exposure { name: String },
}

/// The kind of a vessel's reach. Its endpoint and secret stay with drydock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReachKind {
    Ssh,
    Dial,
    Relay,
}

/// What an inward exposure reaches in the vessel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InwardTarget {
    Unix(String),
    Tcp(SocketAddr),
}
/// The local end of an exposure, on drydock's machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Local {
    Unix(String),
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InwardRequest {
    pub target: InwardTarget,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inward {
    pub target: InwardTarget,
    pub local: Local,
    /// Why the exposure is not working.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ExposureError>,
}
/// The endpoint an outward exposure makes in the vessel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Guest {
    Unix {
        unix: String,
    },
    /// A name the vessel resolves to an address drydock gives it.
    Name {
        name: String,
        port: u16,
    },
}
/// Where an outward exposure's connections go, on drydock's machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutwardDestination {
    Unix(String),
    Tcp(String),
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutwardRequest {
    pub guest: Guest,
    pub destination: OutwardDestination,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outward {
    pub guest: Guest,
    pub destination: OutwardDestination,
    pub ready: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ExposureError>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExposureError {
    pub code: ExposureErrorCode,
    pub reason: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExposureErrorCode {
    NotRunning,
    VesselUnreachable,
    Refused,
    Unresolved,
}
