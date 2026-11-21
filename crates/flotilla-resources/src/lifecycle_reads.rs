//! The shared read boundary for lifecycle consumers. It belongs with resource-facing
//! interfaces so controllers, admission and views can share it without depending on a surface.

/// The changes of some records, written here or replicated in, since the cursor's last pass,
/// read from the decided log on the caller's thread: a pass sees every commit made before it, so
/// a reader that passes before it reads reads its own and its peers' writes.
pub trait ChangeCursor<C>: Send {
    fn pass(&mut self) -> Result<Vec<C>, String>;
    /// How far its passes have read the log: it never falls, and moves with every entry a pass
    /// reads.
    fn position(&self) -> u64;
    /// Each home's sequence its passes have read through.
    fn read_through(&self) -> std::collections::BTreeMap<String, u64>;
    /// The records written since each home's sequence in `at`, read once and leaving the cursor
    /// where it is, with each home's sequence they were read through. A home `at` does not name
    /// is read from the start of its log.
    fn since(&self, at: &std::collections::BTreeMap<String, u64>) -> Result<CursorSince, String>;
    /// How many commits that may change its records have landed since it started, counted at each
    /// commit before the writer's call returns; none where it cannot tell.
    fn told(&self) -> Option<Told> {
        None
    }
}

/// A count of commits, moved at each commit before the writer's call returns. A pass that reads
/// the count before it reads the log reads every commit counted: a reader holding what such a
/// pass read is current while the count has not moved. It keeps counting while any holder keeps
/// it, whatever became of the cursor that made it.
#[derive(Clone)]
pub struct Told {
    count: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// What moves the count, kept alive with it.
    _source: std::sync::Arc<dyn std::any::Any + Send + Sync>,
}

impl Told {
    /// A count `count`, moved by `source`, which it keeps alive.
    pub fn new(count: std::sync::Arc<std::sync::atomic::AtomicU64>, source: std::sync::Arc<dyn std::any::Any + Send + Sync>) -> Self {
        Self { count, _source: source }
    }

    /// The commits counted so far.
    pub fn get(&self) -> u64 {
        self.count.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl std::fmt::Debug for Told {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_tuple("Told").field(&self.get()).finish()
    }
}

/// What a [`ChangeCursor`] read of the records written since some sequences found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CursorSince {
    /// The records written, by name and home, and each home's sequence read through.
    Changed { written: Vec<(String, String)>, through: std::collections::BTreeMap<String, u64> },
    /// The log no longer holds what was written since: why.
    Expired(String),
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
    /// This home: the home whose records these readings resolve first. Unavailable where they
    /// read no engine.
    fn home(&self) -> Result<&str, String>;

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

    /// The engine's convoys (#3108): every convoy reader goes through them. Unavailable where no
    /// engine is installed.
    fn convoys(&self) -> Result<&dyn crate::convoy_records::ConvoyRecords, String> {
        Err("convoys are not served without the engine".into())
    }

    /// The Vessel and session launches the views join, held once for this port (#3108): every
    /// view composing from them reads them here. Unavailable where no engine is installed. Every
    /// adapter answers it, so a wrapper hands on its port's joins rather than dropping them.
    fn joins(&self) -> Result<&crate::held_joins::HeldJoins, String>;

    /// The reading of `name` homed at `origin` (a replica's origin root; `None` for this home), or
    /// `None` when the engine holds no such resource: it was written before the engine took over.
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

/// One of the record sets [`LifecycleReads`] serves, for a view naming those it reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordSet {
    Convoys,
    Vessels,
    Terminals,
    Environments,
    Checkouts,
    Clones,
}

/// The stored kinds of `sets` as `reads` serves them; a set it does not serve adds none.
pub fn record_codes(reads: &dyn LifecycleReads, sets: &[RecordSet]) -> Vec<flotilla_schema::KindCode> {
    let mut codes = Vec::new();
    for set in sets {
        let served = match set {
            RecordSet::Convoys => reads.convoys().map(|records| records.codes()),
            RecordSet::Vessels => reads.vessels().map(|records| records.codes()),
            RecordSet::Terminals => reads.terminals().map(|records| records.codes()),
            RecordSet::Environments => reads.environments().map(|records| records.codes()),
            RecordSet::Checkouts => reads.checkouts().map(|records| records.codes()),
            RecordSet::Clones => reads.clones().map(|records| records.codes()),
        };
        codes.extend(served.unwrap_or_default());
    }
    codes
}

/// Readings of an engine that holds nothing.
pub struct NoLifecycleReads;

impl LifecycleReads for NoLifecycleReads {
    fn home(&self) -> Result<&str, String> {
        Err("readings of no engine have no home".into())
    }

    fn try_reading(&self, _: ProvisioningKind, _: &str, _: Option<&str>) -> Result<Option<flotilla_protocol::LifecycleReading>, String> {
        Ok(None)
    }

    fn joins(&self) -> Result<&crate::held_joins::HeldJoins, String> {
        Err("held joins are not served without the engine".into())
    }
}

/// Readings installed once, after composition opens the decided store, before any lifecycle
/// controller starts; a read before then is refused.
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
    fn home(&self) -> Result<&str, String> {
        self.installed.get().ok_or_else(|| "provisioning lifecycle reads are not installed".to_owned())?.home()
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

    fn convoys(&self) -> Result<&dyn crate::convoy_records::ConvoyRecords, String> {
        self.installed.get().ok_or_else(|| "provisioning lifecycle reads are not installed".to_owned())?.convoys()
    }

    fn joins(&self) -> Result<&crate::held_joins::HeldJoins, String> {
        self.installed.get().ok_or_else(|| "provisioning lifecycle reads are not installed".to_owned())?.joins()
    }

    fn try_reading(
        &self,
        kind: ProvisioningKind,
        name: &str,
        origin: Option<&str>,
    ) -> Result<Option<flotilla_protocol::LifecycleReading>, String> {
        self.installed.get().ok_or_else(|| "provisioning lifecycle reads are not installed".to_owned())?.try_reading(kind, name, origin)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A read before installation is refused; startup requires the port.
    // Once installed, the port remains installed and a duplicate install is refused.
    #[test]
    fn engine_startup_requires_the_late_read_port() {
        let reads = LateLifecycleReads::default();
        assert!(reads.require_installed().is_err());
        assert!(reads.try_reading(ProvisioningKind::Checkout, "checkout", None).is_err());
        reads.install(std::sync::Arc::new(NoLifecycleReads)).expect("install");
        reads.require_installed().expect("installed");
        assert_eq!(reads.try_reading(ProvisioningKind::Checkout, "checkout", None), Ok(None));
        assert!(reads.install(std::sync::Arc::new(NoLifecycleReads)).is_err());
        reads.require_installed().expect("still installed");
    }
}
