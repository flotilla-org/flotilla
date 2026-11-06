//! Environments as the engine holds them (#3108): the port every environment reader and
//! requester uses, served over the decided store.
//!
//! An environment is an engine record: what was requested (shown here in the
//! [`EnvironmentSpec`] vocabulary its providers take), who holds it, its lifecycle reading, and
//! the facts fixed when its current launch was realised. Nothing here is stored in this shape.

use flotilla_protocol::{ConfiguredResourceLimits, LifecycleReading};

use crate::{EnvironmentProviderDescriptor, EnvironmentSpec};

/// One environment as a reader sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvironmentRecord {
    pub name: String,
    /// The home that holds it: the host whose provider realises it.
    pub home: String,
    /// What was requested. Env entries derived at launch (credential grants, skills) are never
    /// stored, so they are absent here.
    pub spec: EnvironmentSpec,
    /// The vessel it is held for, if any.
    pub vessel: Option<String>,
    pub reading: LifecycleReading,
    /// The facts fixed when its launch was realised; none before.
    pub launch: Option<LaunchFacts>,
}

impl EnvironmentRecord {
    pub fn is_ready(&self) -> bool {
        matches!(self.reading, LifecycleReading::Ready { .. })
    }

    /// Whether it has ended: released, lost, failed or given up.
    pub fn ended(&self) -> bool {
        matches!(
            self.reading,
            LifecycleReading::Released | LifecycleReading::Lost { .. } | LifecycleReading::Failed { .. } | LifecycleReading::GaveUp { .. }
        )
    }

    /// The identity it is realised under: its container, cloud backing or host-direct root.
    pub fn identity(&self) -> Option<&str> {
        match &self.reading {
            LifecycleReading::Ready { identity } => Some(identity),
            _ => None,
        }
    }
}

/// What one launch of an environment was given, fixed when it was realised.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LaunchFacts {
    pub image_ref: Option<String>,
    pub local_image_id: Option<String>,
    pub registry_digest: Option<String>,
    pub configured_limits: Option<ConfiguredResourceLimits>,
    pub provider_descriptor: Option<EnvironmentProviderDescriptor>,
}

/// What changed among the environments, at any home.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnvironmentChange {
    /// The record of the environment of this name changed.
    Changed(String),
    /// Any of them may have changed: a home's log was read afresh.
    Rescanned,
}

/// The environment port, served by the engine's [`crate::lifecycle_reads::LifecycleReads`]. Each
/// read is keyed or indexed; [`EnvironmentRecords::request`] is the only write, and only for this
/// home: the Environment controller writes every edge after it.
pub trait EnvironmentRecords: Send + Sync {
    /// `name` held at `home`, or at this home when `None`.
    fn get(&self, name: &str, home: Option<&str>) -> Result<Option<EnvironmentRecord>, String>;
    /// `name` at whichever home holds it. Environment names are fleet-unique by construction
    /// (`host-direct-<host>`, `env-<vessel>`); when two homes hold one, this home's is preferred.
    fn find(&self, name: &str) -> Result<Option<EnvironmentRecord>, String>;
    /// This home's environments that have not ended.
    fn live(&self) -> Result<Vec<EnvironmentRecord>, String>;
    /// The environments held for `vessel`, across homes, every one, ended or not.
    fn held(&self, vessel: &str) -> Result<Vec<EnvironmentRecord>, String>;
    /// Requests `name` at this home with `spec`, held for `vessel`. An environment already held
    /// under the name is left as it is: `Ok(false)`.
    fn request(&self, name: &str, spec: &EnvironmentSpec, vessel: Option<&str>) -> Result<bool, String>;
    /// The environments' changes from now on, as their records are written or replicated here.
    /// Where none are followed, the channel is closed.
    fn changes(&self) -> tokio::sync::broadcast::Receiver<EnvironmentChange> {
        tokio::sync::broadcast::channel(1).1
    }
}
