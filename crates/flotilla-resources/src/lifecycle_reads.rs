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
/// Every adapter explicitly implements checked reads, including infallible adapters.
///
/// ```compile_fail
/// use flotilla_resources::lifecycle_reads::LifecycleReads;
/// struct Incomplete;
/// impl LifecycleReads for Incomplete {}
/// ```
pub trait LifecycleReads: Send + Sync {
    /// A read failure is distinct from an unheld resource and must not select legacy status.
    fn try_reading(
        &self,
        kind: ProvisioningKind,
        name: &str,
        origin: Option<&str>,
    ) -> Result<Option<flotilla_protocol::LifecycleReading>, String>;

    /// Current launch identity alongside its checked judgement. Adapters serving terminal
    /// lifecycles must provide the key; synthesising one could restart the wrong launch.
    fn try_terminal_launch(&self, name: &str, origin: Option<&str>) -> Result<Option<crate::terminal_launches::TerminalLaunch>, String> {
        match self.try_reading(ProvisioningKind::TerminalSession, name, origin)? {
            None => Ok(None),
            Some(_) => Err("terminal lifecycle reader does not provide launch identity".into()),
        }
    }

    /// The reading of `name` homed at `origin` (a replica's origin root; `None` for this home), or
    /// `None` when the engine holds no such resource: the switch is off, or it was written before.
    ///
    /// Lossy compatibility adapter. Authoritative lifecycle consumers use `try_reading`
    /// so a read failure cannot select stale status.
    fn reading(&self, kind: ProvisioningKind, name: &str, origin: Option<&str>) -> Option<flotilla_protocol::LifecycleReading> {
        match self.try_reading(kind, name, origin) {
            Ok(reading) => reading,
            Err(error) => {
                tracing::warn!(?kind, resource = name, %error, "engine reading failed");
                None
            }
        }
    }
}

/// No engine: every view reads the old status.
pub struct NoLifecycleReads;

impl LifecycleReads for NoLifecycleReads {
    fn try_reading(&self, _: ProvisioningKind, _: &str, _: Option<&str>) -> Result<Option<flotilla_protocol::LifecycleReading>, String> {
        Ok(None)
    }
}

/// Readings installed once, after composition opens the decided store; none before.
/// With the engine enabled, runtime composition installs these before starting lifecycle controllers.
/// An uninstalled port intentionally selects the no-engine path when the switch is off.
#[derive(Default)]
pub struct LateLifecycleReads {
    installed: std::sync::OnceLock<std::sync::Arc<dyn LifecycleReads>>,
}

impl LateLifecycleReads {
    /// Refuse engine-enabled startup before the read port is installed.
    pub fn require_installed(&self) -> Result<(), String> {
        self.installed.get().map(|_| ()).ok_or_else(|| "provisioning lifecycle reads are not installed".to_owned())
    }

    /// Installs `reads`; refused if some are installed already.
    pub fn install(&self, reads: std::sync::Arc<dyn LifecycleReads>) -> Result<(), String> {
        self.installed.set(reads).map_err(|_| "provisioning lifecycle reads are already installed".to_owned())
    }
}

impl LifecycleReads for LateLifecycleReads {
    fn try_terminal_launch(&self, name: &str, origin: Option<&str>) -> Result<Option<crate::terminal_launches::TerminalLaunch>, String> {
        self.installed.get().map_or(Ok(None), |reads| reads.try_terminal_launch(name, origin))
    }

    fn try_reading(
        &self,
        kind: ProvisioningKind,
        name: &str,
        origin: Option<&str>,
    ) -> Result<Option<flotilla_protocol::LifecycleReading>, String> {
        self.installed.get().map_or(Ok(None), |reads| reads.try_reading(kind, name, origin))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // No-engine operation remains available before installation; engine-enabled startup requires it.
    // Once installed, the port remains installed and a duplicate install is refused.
    #[test]
    fn engine_startup_requires_the_late_read_port() {
        let reads = LateLifecycleReads::default();
        assert!(reads.require_installed().is_err());
        assert_eq!(reads.try_reading(ProvisioningKind::Checkout, "checkout", None), Ok(None));
        reads.install(std::sync::Arc::new(NoLifecycleReads)).expect("install");
        reads.require_installed().expect("installed");
        assert_eq!(reads.try_reading(ProvisioningKind::Checkout, "checkout", None), Ok(None));
        assert!(reads.install(std::sync::Arc::new(NoLifecycleReads)).is_err());
        reads.require_installed().expect("still installed");
    }
}
