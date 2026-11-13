//! Clones as the engine holds them (#3108): the port every clone reader and requester uses,
//! served over the decided store.
//!
//! A clone is one repository cloned once into one environment, shared by the checkouts made from
//! it. Each try at it is a launch of its own (`clone-<key>#<n>`): a launch that ended (given up,
//! lost) is never revived. A new request, or `reconcile-now clone`, on an ended clone starts its
//! next launch, and each checkout names the launch it was made from. Nothing here is stored in
//! this shape.

use flotilla_protocol::LifecycleReading;

use crate::{CloneSpec, RepositoryKey, RepositorySpec};

/// One launch of a clone as a reader sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloneRecord {
    /// The launch: `<clone>#<n>`.
    pub name: String,
    /// The home that holds it: the host of its environment.
    pub home: String,
    pub spec: CloneSpec,
    /// Found already at its path by startup discovery, not cloned there.
    pub discovered: bool,
    /// When this launch was requested.
    pub requested_at: chrono::DateTime<chrono::Utc>,
    pub reading: LifecycleReading,
    /// The remote's default branch, as the launch found it when it was realised.
    pub default_branch: Option<String>,
}

impl CloneRecord {
    /// The clone this launch is of: its name without the launch number.
    pub fn clone_name(&self) -> &str {
        launched(&self.name)
    }

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
}

/// The clone a launch (`<clone>#<n>`) is of; a name with no launch number is a clone's own.
pub fn launched(name: &str) -> &str {
    match name.rsplit_once('#') {
        Some((clone, n)) if n.parse::<u32>().is_ok() => clone,
        _ => name,
    }
}

/// The clone of `repository` in environment `environment`: `clone-<key>`, keyed by the
/// repository's identity (its forge repository, or its canonical remote), not by a transport.
pub fn clone_name(repository: &RepositorySpec, environment: &str) -> Result<String, String> {
    Ok(format!("clone-{}", repository.clone_key(environment)?))
}

/// What changed among the clones, at any home.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CloneChange {
    /// The launch of this name changed.
    Changed(String),
    /// Any of them may have changed: a home's log was read afresh.
    Rescanned,
}

/// What a `reconcile-now` of a clone did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CloneRenewal {
    /// Its latest launch had ended; this launch was requested after it.
    Started(String),
    /// Its latest launch has not ended: it is tried again when what it waits on changes.
    Live(CloneRecord),
}

/// The clone port, served by the engine's [`crate::lifecycle_reads::LifecycleReads`]. Each read
/// is keyed or indexed. [`CloneRecords::request`] and [`CloneRecords::renew`] are the only
/// writes, and only for this home: the Clone controller writes every edge after them.
pub trait CloneRecords: Send + Sync {
    /// `name` held at `home`, or at this home when `None`. A launch name (`<clone>#<n>`) reads
    /// that launch; a clone's name reads its latest launch.
    fn get(&self, name: &str, home: Option<&str>) -> Result<Option<CloneRecord>, String>;
    /// Every launch of every clone of `repository`, across homes.
    fn of_repository(&self, repository: &RepositoryKey) -> Result<Vec<CloneRecord>, String>;
    /// Every launch held at this home, ended or not.
    fn here(&self) -> Result<Vec<CloneRecord>, String>;
    /// Every launch at every home this one follows.
    fn every(&self) -> Result<Vec<CloneRecord>, String>;
    /// The launch that serves a request for `spec`, a clone of `repository`, at this home: the
    /// latest launch of [`clone_name`] when that has not ended, else its next launch, requested
    /// now. The clone is named for the repository's identity, never by the caller, and a spec
    /// for another repository is refused.
    fn request(&self, repository: &RepositorySpec, spec: &CloneSpec, discovered: bool) -> Result<String, String>;
    /// Starts the next launch of clone `clone`, with its latest launch's request, if that launch
    /// has ended.
    fn renew(&self, clone: &str) -> Result<CloneRenewal, String>;
    /// The clones' changes from now on, as their records are written or replicated here. Where
    /// none are followed, the channel is closed.
    fn changes(&self) -> tokio::sync::broadcast::Receiver<CloneChange> {
        tokio::sync::broadcast::channel(1).1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_launch_names_its_clone() {
        assert_eq!(launched("clone-abc#3"), "clone-abc");
        assert_eq!(launched("clone-abc"), "clone-abc");
        assert_eq!(launched("clone-a#b"), "clone-a#b");
    }
}
