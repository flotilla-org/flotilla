//! The shared read boundary for lifecycle consumers. It belongs with resource-facing
//! interfaces so controllers, admission and views can share it without depending on a surface.

/// The changes of some records, written here or replicated in, since the cursor's last pass,
/// read from the decided log on the caller's thread: a pass sees every commit made before it, so
/// a reader that passes before it reads reads its own and its peers' writes.
pub trait ChangeCursor<C>: Send {
    fn pass(&mut self) -> Result<Vec<C>, String>;
}

/// A provisioning kind whose lifecycle the engine can hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProvisioningKind {
    Environment,
    Clone,
    Checkout,
    /// A terminal session's current launch, read by its record name.
    TerminalSession,
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
    /// No home is configured, so consumers can preserve the no-engine path without
    /// dispatching a blocking read. Active adapters default to checked reads.
    fn is_disabled(&self) -> bool {
        false
    }

    /// A read failure is distinct from an unheld resource and must not select legacy status.
    fn try_reading(
        &self,
        kind: ProvisioningKind,
        name: &str,
        origin: Option<&str>,
    ) -> Result<Option<flotilla_protocol::LifecycleReading>, String>;

    /// The engine's environments (#3108): every environment reader and requester goes through
    /// them. Unavailable where no engine is installed.
    fn environments(&self) -> Result<&dyn crate::environment_records::EnvironmentRecords, String> {
        Err("environment records are not served without the engine".into())
    }

    /// The engine's clones (#3108): every clone reader and requester goes through them.
    /// Unavailable where no engine is installed.
    fn clones(&self) -> Result<&dyn crate::clone_records::CloneRecords, String> {
        Err("clone records are not served without the engine".into())
    }

    /// The engine's checkouts (#3108): every checkout reader and requester goes through them.
    /// Unavailable where no engine is installed.
    fn checkouts(&self) -> Result<&dyn crate::checkout_records::CheckoutRecords, String> {
        Err("checkout records are not served without the engine".into())
    }

    /// The engine's image builds (#3108): every image build reader, demander and retrier goes
    /// through them. Unavailable where no engine is installed.
    fn image_builds(&self) -> Result<&dyn crate::image_build_records::ImageBuildRecords, String> {
        Err("image builds are not served without the engine".into())
    }

    /// The AgentSessions the decided store holds (#3108): every reader of what a crew's agent is
    /// doing goes through them. Unavailable where no engine is installed.
    fn agent_sessions(&self) -> Result<&dyn crate::agent_activity::AgentSessionReads, String> {
        Err("AgentSessions are not served without the engine".into())
    }

    /// The engine's terminal sessions (#3108): every terminal session reader and requester goes
    /// through them. Unavailable where no engine is installed.
    fn terminals(&self) -> Result<&dyn crate::terminal_records::TerminalRecords, String> {
        Err("terminal sessions are not served without the engine".into())
    }

    /// The engine's vessels (#3108): every vessel reader goes through them. Unavailable where no
    /// engine is installed.
    fn vessels(&self) -> Result<&dyn crate::vessel_records::VesselRecords, String> {
        Err("vessels are not served without the engine".into())
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
    fn is_disabled(&self) -> bool {
        true
    }

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
    fn is_disabled(&self) -> bool {
        self.installed.get().is_none_or(|reads| reads.is_disabled())
    }

    fn environments(&self) -> Result<&dyn crate::environment_records::EnvironmentRecords, String> {
        self.installed.get().ok_or_else(|| "provisioning lifecycle reads are not installed".to_owned())?.environments()
    }

    fn clones(&self) -> Result<&dyn crate::clone_records::CloneRecords, String> {
        self.installed.get().ok_or_else(|| "provisioning lifecycle reads are not installed".to_owned())?.clones()
    }

    fn image_builds(&self) -> Result<&dyn crate::image_build_records::ImageBuildRecords, String> {
        self.installed.get().ok_or_else(|| "provisioning lifecycle reads are not installed".to_owned())?.image_builds()
    }

    fn checkouts(&self) -> Result<&dyn crate::checkout_records::CheckoutRecords, String> {
        self.installed.get().ok_or_else(|| "provisioning lifecycle reads are not installed".to_owned())?.checkouts()
    }

    fn agent_sessions(&self) -> Result<&dyn crate::agent_activity::AgentSessionReads, String> {
        self.installed.get().ok_or_else(|| "provisioning lifecycle reads are not installed".to_owned())?.agent_sessions()
    }

    fn terminals(&self) -> Result<&dyn crate::terminal_records::TerminalRecords, String> {
        self.installed.get().ok_or_else(|| "provisioning lifecycle reads are not installed".to_owned())?.terminals()
    }

    fn vessels(&self) -> Result<&dyn crate::vessel_records::VesselRecords, String> {
        self.installed.get().ok_or_else(|| "provisioning lifecycle reads are not installed".to_owned())?.vessels()
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
