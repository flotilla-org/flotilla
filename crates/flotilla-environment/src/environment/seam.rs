//! Runtime-only provider vocabulary. Persisted Docker inputs remain at the
//! legacy boundary until the generated Environment table move (ADR 0058).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub use flotilla_protocol::provider::{
    CheckoutStrategy, EgressEnforcement, EgressRoute, EnvironmentClass, MaterialMode, ProviderAdvertisement, ProviderCapabilities,
    ProviderKind, ProviderReadiness,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoppedCause {
    SuspendedByRequest,
    PausedByProvider,
    GuestHalted,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoneCause {
    LifetimeCapReached,
    EvictedByProvider,
    HostOutOfMemory,
    /// The environment exceeded its own memory allocation.
    MemoryLimitReached,
    /// The backing process ended. Docker reports signal exits as 128 + signal;
    /// the runtime termination retains the signal separately for diagnostics.
    BackingExited {
        code: i32,
    },
    GuestPoweredOff,
    MissingOnList,
    DestroyedByRequest,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnknownReason {
    HostUnreachable,
    ProviderApiError { message: String },
    GuestUnresponsive,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ObservationState {
    Running,
    Stopped { cause: StoppedCause },
    Gone { cause: GoneCause },
    Unknown { reason: UnknownReason },
}

/// Runtime-only observation payload; not a wire or stored resource shape.
/// Keep all existing Docker memory/exit evidence without changing stored facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderObservation {
    pub state: ObservationState,
    pub runtime: Option<flotilla_protocol::EnvironmentRuntimeObservation>,
}
impl ProviderObservation {
    pub fn new(state: ObservationState) -> Self {
        Self { state, runtime: None }
    }
}

/// Read-only tool detection, separate from required lifecycle operations.
/// The observed command inherits only the prepared base layer in `context`.
/// Providers isolate HOME, XDG configuration/cache/state and each family's
/// account home, clear ambient authentication, and clean observation state
/// after cancellation. `scratch` identifies the caller workspace, never the
/// harness home or CWD.
#[async_trait]
pub trait EnvironmentDetection: Send + Sync {
    async fn detect(
        &self,
        image: Option<&str>,
        context: &flotilla_provider_runtime::execution_context::ObservationContext,
        binary: &str,
        args: &[&str],
        scratch: &std::path::Path,
    ) -> Result<flotilla_provider_runtime::CommandOutput, String>;
}
