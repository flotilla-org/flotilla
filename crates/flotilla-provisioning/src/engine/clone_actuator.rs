//! The Clone controller's provider work (#3108): the shared [`Controller`] realises a clone
//! launch by cloning its repository into its environment, or, for one startup discovery found,
//! by inspecting what is already there. A clone is shared by the checkouts made from it, so it is
//! held for no one and never released here.
//!
//! - **Realise** clones, adopting a clone of the same origin already at the path, and stages the
//!   default branch the clone found as the launch's [`CloneFacts`] for the Realise edge.
//! - **Waits**, which are not tries: it is tried only while its environment is realised, its
//!   host's disk is above the free-space floor (`HostObservation.below_free_space_floor`), and its
//!   host reaches the remote (`HostObservation.conditions["Remote/<remote host>"]` is not False).
//! - **Failures**, each a try of the kind's three: a transport failure records the remote
//!   unreachable at once, through [`RemoteReachability`], and the try after it waits for the
//!   host's reachability probe to reach the remote again; a full disk waits for the free-space
//!   fact to change; a misconfiguration (a different repository at the path, no such repository,
//!   refused credentials) gives up at once.
//!
//! [`Controller`]: super::controller::Controller

use std::sync::Arc;

use async_trait::async_trait;
use flotilla_resources::{remote_condition_type, remote_host};
use flotilla_schema::{Identity, Kind, KindCode};
use flotilla_schema_sqlite::{Read, StoreError};
use flotilla_state_machines::{
    machines::lifecycle::{Lifecycle, LifecycleState},
    Cause, CauseCategory, Resource,
};

use super::{
    controller::{Actuator, Backing, Holding, ProviderFailure, Usability},
    environment_actuator::{host_provider, state_of},
    launch_log::StagedFacts,
    reads::StoreReads,
};
use crate::{
    reconcilers::CloneRuntime,
    stored::{
        clone::{CloneFacts, CloneKind, CloneRequest},
        environment::EnvironmentKind,
        host::{HostObservation, Value},
    },
};

/// Where a clone's transport failure is recorded: the host's `Remote/<remote host>` condition,
/// False at once. The host's reachability probe records it True again.
pub trait RemoteReachability: Send + Sync {
    /// The host could not reach `remote` when cloning `url`, as `message` says.
    fn unreachable(&self, remote: &str, url: &str, message: &str);
}

/// Records nothing: tests, and hosts with no reachability probe.
pub struct NoRemoteReachability;

impl RemoteReachability for NoRemoteReachability {
    fn unreachable(&self, _: &str, _: &str, _: &str) {}
}

/// What a failed clone or inspection says about why.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloneFailure {
    /// The remote could not be reached.
    Transport,
    /// The host's disk is full.
    DiskFull,
    /// Its inputs are wrong: another try on the same inputs cannot succeed.
    Misconfigured,
    /// Anything else.
    Other,
}

/// Why `message`, a clone's or an inspection's error, failed.
pub fn classify(message: &str) -> CloneFailure {
    let message = message.to_ascii_lowercase();
    let any = |needles: &[&str]| needles.iter().any(|needle| message.contains(needle));
    if any(&["no space left on device", "disk quota exceeded"]) {
        CloneFailure::DiskFull
    } else if any(&[
        "already exists but is not a reusable clone",
        "already exists with origin",
        "repository not found",
        "does not appear to be a git repository",
        "authentication failed",
        "could not read username",
        "permission denied (publickey",
        "the requested url returned error: 403",
        "the requested url returned error: 404",
    ]) {
        CloneFailure::Misconfigured
    } else if any(&[
        "could not resolve host",
        "could not resolve hostname",
        "temporary failure in name resolution",
        "name or service not known",
        "failed to connect",
        "connection refused",
        "connection timed out",
        "operation timed out",
        "connection reset",
        "network is unreachable",
        "no route to host",
        "ssh: connect to host",
        "could not connect to server",
    ]) {
        CloneFailure::Transport
    } else {
        CloneFailure::Other
    }
}

/// The Clone kind's [`Actuator`] over the host's clone runtime.
pub struct CloneActuator<R> {
    runtime: Arc<R>,
    reads: StoreReads,
    home: String,
    staged: StagedFacts<CloneFacts>,
    remotes: Arc<dyn RemoteReachability>,
}

impl<R> CloneActuator<R> {
    pub fn new(
        runtime: Arc<R>,
        reads: StoreReads,
        home: impl Into<String>,
        staged: StagedFacts<CloneFacts>,
        remotes: Arc<dyn RemoteReachability>,
    ) -> Self {
        Self { runtime, reads, home: home.into(), staged, remotes }
    }
}

const ENVIRONMENT: KindCode = <Resource<Lifecycle<EnvironmentKind>> as Kind>::CODE;
const OBSERVATION: KindCode = <HostObservation as Kind>::CODE;

/// The host whose provider realises environment `name` at `home`.
pub(crate) fn environment_host(read: &mut Read<'_>, name: &str, home: &str) -> Result<Option<String>, StoreError> {
    let Some(id) = read.resolve(ENVIRONMENT, name, home)? else { return Ok(None) };
    let Some(loaded) = read.load::<Resource<Lifecycle<EnvironmentKind>>>(id)? else { return Ok(None) };
    Ok(host_provider(&loaded.value.spec().provider).map(|(host, _)| host.to_owned()))
}

#[async_trait]
impl<R> Actuator<CloneKind> for CloneActuator<R>
where
    R: CloneRuntime + 'static,
{
    async fn observe(&self, name: &str, _: &CloneRequest) -> Result<Backing, Cause> {
        // A realised clone is present; a requested one is realised again, which adopts a clone
        // of the same origin already at its path.
        let state = self
            .reads
            .read(|read| Ok(state_of::<CloneKind>(read, name, &self.home)?))
            .map_err(|error| Cause::new(CauseCategory::Transient, format!("read clone {name}: {error}")))?;
        Ok(match state {
            Some(LifecycleState::Realised { identity }) => Backing::Present { identity },
            _ => Backing::Absent { reason: None },
        })
    }

    async fn realise(&self, name: &str, request: &CloneRequest) -> Result<String, ProviderFailure> {
        let environment = &request.environment.name;
        let found = if request.discovered {
            self.runtime.inspect_existing_in(environment, &request.path).await
        } else {
            self.runtime.clone_and_inspect_in(environment, &request.url, &request.path).await
        };
        match found {
            Ok(default_branch) => {
                self.staged.stage(name, CloneFacts { default_branch });
                // Realised under its path, where the repository was observed.
                Ok(request.path.clone())
            }
            Err(message) => Err(match classify(&message) {
                CloneFailure::Misconfigured => ProviderFailure::new(Cause::new(CauseCategory::Misconfigured, message), false),
                CloneFailure::Transport => {
                    if let Some(remote) = remote_host(&request.url) {
                        self.remotes.unreachable(&remote, &request.url, &message);
                    }
                    ProviderFailure::new(Cause::new(CauseCategory::External, message), true)
                }
                CloneFailure::DiskFull | CloneFailure::Other => ProviderFailure::new(Cause::new(CauseCategory::Transient, message), true),
            }),
        }
    }

    async fn release(&self, _: &str, _: &CloneRequest, _: Option<&str>) -> Result<(), ProviderFailure> {
        // Held for no one, a clone is never released; its checkouts' teardown leaves it in place.
        Ok(())
    }

    fn followed(&self) -> Vec<KindCode> {
        vec![ENVIRONMENT, OBSERVATION]
    }

    fn dependencies(&self, request: &CloneRequest) -> Vec<Identity> {
        vec![Identity { kind: ENVIRONMENT, name: request.environment.name.clone(), home: request.environment.home.clone() }]
    }

    fn holder(&self, _: &mut Read<'_>, _: &str, _: &CloneRequest) -> Result<Holding, StoreError> {
        Ok(Holding::open())
    }

    fn usability(&self, read: &mut Read<'_>, request: &CloneRequest) -> Result<Vec<Usability>, StoreError> {
        let environment = &request.environment;
        let realised =
            matches!(state_of::<EnvironmentKind>(read, &environment.name, &environment.home)?, Some(LifecycleState::Realised { .. }));
        let mut facts = vec![Usability {
            fact: Identity { kind: ENVIRONMENT, name: environment.name.clone(), home: environment.home.clone() },
            usable: realised,
        }];
        let Some(host) = environment_host(read, &environment.name, &environment.home)? else { return Ok(facts) };
        let Some(id) = read.resolve(OBSERVATION, &host, &self.home)? else { return Ok(facts) };
        let Some(observation) = read.load::<HostObservation>(id)? else { return Ok(facts) };
        let unreachable = remote_host(&request.url)
            .and_then(|remote| observation.value.conditions.get(&remote_condition_type(&remote)).map(|condition| condition.value))
            == Some(Value::False);
        facts.push(Usability {
            fact: Identity { kind: OBSERVATION, name: host, home: self.home.clone() },
            usable: observation.value.below_free_space_floor == Some(false) && !unreachable,
        });
        Ok(facts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clone_error_names_why_it_failed() {
        assert_eq!(classify("fatal: unable to access 'https://h/r/': Could not resolve host: h"), CloneFailure::Transport);
        assert_eq!(classify("ssh: connect to host h port 22: Connection refused"), CloneFailure::Transport);
        assert_eq!(classify("error: could not write: No space left on device"), CloneFailure::DiskFull);
        assert_eq!(classify("remote: Repository not found."), CloneFailure::Misconfigured);
        assert_eq!(classify("clone target /w/r already exists with origin x, expected y"), CloneFailure::Misconfigured);
        assert_eq!(classify("fatal: early EOF"), CloneFailure::Other);
    }
}
