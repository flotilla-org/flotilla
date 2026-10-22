use async_trait::async_trait;
use flotilla_protocol::{EnvironmentInfo, ToolInventory};
#[async_trait]
pub trait EnvironmentSummaries: Send + Sync {
    fn local_inventory(&self) -> ToolInventory;
    async fn host_summary_environments(&self) -> Vec<EnvironmentInfo>;
    async fn visible_environments(&self) -> Vec<EnvironmentInfo>;
}

use flotilla_protocol::{AttachBinding, ResolvedAttachPlan};
/// An attach resolution: the plan the CLI should execute, plus the
/// structured binding it stamps onto its enclosing PM pane (#708).
#[derive(Debug, Clone)]
pub struct ResolvedAttach {
    pub plan: ResolvedAttachPlan,
    pub binding: Option<AttachBinding>,
}

/// A provisioning kind the engine holds behind `[primitives]` (#3629).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvisioningKind {
    Environment,
    ImageBuild,
    Clone,
    Checkout,
}

/// The engine's readings of provisioning resources behind `[primitives]` (#3629 layer 4). Each
/// reading is one keyed read of the decided store, judged at read time.
pub trait LifecycleReads: Send + Sync {
    /// The reading of `name` homed at `origin` (a replica's origin root; `None` for this home), or
    /// `None` when the engine holds no such resource: the switch is off, or it was written before.
    fn reading(&self, kind: ProvisioningKind, name: &str, origin: Option<&str>) -> Option<flotilla_protocol::LifecycleReading>;
}

/// No engine: every view reads the old status.
pub struct NoLifecycleReads;

impl LifecycleReads for NoLifecycleReads {
    fn reading(&self, _: ProvisioningKind, _: &str, _: Option<&str>) -> Option<flotilla_protocol::LifecycleReading> {
        None
    }
}

/// Readings installed once, after composition opens the decided store; none before.
#[derive(Default)]
pub struct LateLifecycleReads {
    installed: std::sync::OnceLock<std::sync::Arc<dyn LifecycleReads>>,
}

impl LateLifecycleReads {
    /// Installs `reads`; refused if some are installed already.
    pub fn install(&self, reads: std::sync::Arc<dyn LifecycleReads>) -> Result<(), String> {
        self.installed.set(reads).map_err(|_| "provisioning lifecycle reads are already installed".to_owned())
    }
}

impl LifecycleReads for LateLifecycleReads {
    fn reading(&self, kind: ProvisioningKind, name: &str, origin: Option<&str>) -> Option<flotilla_protocol::LifecycleReading> {
        self.installed.get()?.reading(kind, name, origin)
    }
}
