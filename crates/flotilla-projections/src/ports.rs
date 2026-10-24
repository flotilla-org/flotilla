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

pub use flotilla_resources::lifecycle_reads::{LateLifecycleReads, LifecycleReads, NoLifecycleReads, ProvisioningKind};
