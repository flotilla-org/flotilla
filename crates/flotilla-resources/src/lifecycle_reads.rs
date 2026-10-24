//! The shared read boundary for lifecycle consumers. It belongs with resource-facing
//! interfaces so controllers, admission and views can share it without depending on a surface.

/// A provisioning kind whose lifecycle the engine can hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProvisioningKind {
    Environment,
    ImageBuild,
    Clone,
    Checkout,
    /// A terminal session's current launch, read by its record name.
    TerminalSession,
    /// A vessel's lease on its pinned host.
    Vessel,
}

/// Lifecycle judgements served by keyed reads of the decided store, judged at read time.
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
