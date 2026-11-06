//! This host's reach of the remotes its clones fetch from (#3108).
//!
//! A clone whose transport fails records its remote unreachable here at once: a false
//! `Remote/<remote host>` condition on this host's heartbeat, published straight away. The Clone
//! controller holds such a clone until that condition stops being false. The probe here is the
//! only thing that turns it true again: at each pass it asks each unreached remote for its
//! `HEAD`, and on an answer records it reached and publishes.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};

use chrono::Utc;
use flotilla_provisioning::engine::clone_actuator::{classify, CloneFailure, RemoteReachability};
use flotilla_resources::{remote_condition_type, ConditionValue, HostCondition};
use flotilla_vcs::vcs::Vcs;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use super::{health::RuntimeHealth, state::ControllerRuntimeState};

/// How often an unreached remote is asked again. The probe is the edge that makes the fact;
/// nothing else polls.
pub(super) const REMOTE_PROBE_INTERVAL: Duration = Duration::from_secs(30);

/// The remotes this host could not reach, and the conditions it publishes for them.
pub(super) struct RemoteProbe {
    health: RuntimeHealth,
    /// Each unreached remote host, with a URL to ask it through.
    unreached: StdMutex<BTreeMap<String, String>>,
}

impl RemoteProbe {
    pub(super) fn new(health: RuntimeHealth) -> Self {
        Self { health, unreached: StdMutex::default() }
    }

    fn record(&self, remote: &str, value: ConditionValue, reason: &str, message: String) {
        let condition_type = remote_condition_type(remote);
        let condition = HostCondition::builder()
            .condition_type(condition_type.clone())
            .value(value)
            .reason(reason)
            .message(message)
            .observed_at(Utc::now())
            // One unreachable remote leaves the host ready for everything else.
            .blocks_readiness(false)
            .blocks_forge_observation(false)
            .build();
        self.health.failures.lock().expect("runtime health lock poisoned").insert(condition_type, condition);
        self.health.heartbeat_now.notify_one();
    }

    /// The remotes waiting to be reached, with a URL for each.
    pub(super) fn unreached(&self) -> Vec<(String, String)> {
        self.unreached.lock().unwrap_or_else(std::sync::PoisonError::into_inner).iter().map(|(h, u)| (h.clone(), u.clone())).collect()
    }

    /// Asks `remote` for its `HEAD` through `vcs`. An answer, or a refusal that is not the
    /// transport's (the remote answered), records it reached.
    pub(super) async fn probe(&self, vcs: &dyn Vcs, remote: &str, url: &str) {
        let reached = match vcs.remote_ref_digest(url, "HEAD").await {
            Ok(_) => true,
            Err(error) => classify(&error) != CloneFailure::Transport,
        };
        if reached {
            self.unreached.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(remote);
            info!(%remote, "a remote this host could not reach answers again");
            self.record(remote, ConditionValue::True, "Reached", format!("{remote} answered"));
        }
    }
}

impl RemoteReachability for RemoteProbe {
    fn unreachable(&self, remote: &str, url: &str, message: &str) {
        self.unreached.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(remote.to_owned(), url.to_owned());
        self.record(remote, ConditionValue::False, "Unreachable", message.to_owned());
    }
}

/// Probes each unreached remote every `interval`, through this host's own Git.
pub(super) fn spawn_remote_probe_task(state: Arc<ControllerRuntimeState>, interval: Duration) -> JoinHandle<()> {
    super::tasks::spawn_periodic_task(interval, super::tasks::PeriodicTaskStart::AfterInterval, move || {
        let state = Arc::clone(&state);
        async move {
            let unreached = state.remote_reachability.unreached();
            if unreached.is_empty() {
                return;
            }
            let Some(environment) = state.daemon.resolve_environment_ref(&state.host_direct_environment_name) else {
                warn!("this host's own environment is unavailable; unreached remotes are not probed");
                return;
            };
            let vcs = flotilla_vcs::vcs::FlotillaVcs::fresh_clone(environment.runner);
            for (remote, url) in unreached {
                state.remote_reachability.probe(&vcs, &remote, &url).await;
            }
        }
    })
}
