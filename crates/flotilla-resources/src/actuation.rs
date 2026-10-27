use crate::{CheckoutSpec, CloneSpec, EnvironmentSpec, InputMeta, TerminalSessionSpec, VesselSpec};
use chrono::{DateTime, Utc};
#[derive(Debug, Clone)]
pub enum Actuation {
    CreateRepository { key: crate::RepositoryKey, spec: crate::RepositorySpec },
    CreateEnvironment { meta: InputMeta, spec: EnvironmentSpec },
    CreateClone { meta: InputMeta, spec: CloneSpec },
    RetryClone { name: String, failed_at: DateTime<Utc> },
    CreateCheckout { meta: InputMeta, spec: CheckoutSpec },
    CreateTerminalSession { meta: InputMeta, spec: TerminalSessionSpec },
    CreateDemand { meta: InputMeta, spec: crate::DemandSpec },
    DeleteDemand { name: String },
    RestartTerminalSession { name: String },
    PruneTerminalMessages { name: String },
    DeleteTerminalSession { name: String },
    CreateVessel { meta: InputMeta, spec: VesselSpec },
    DeleteVessel { name: String },
    DeleteCheckout { name: String },
}
