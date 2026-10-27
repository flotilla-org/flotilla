//! The access layer's per-access status (#3964 contract 4): one [`AccessDelivery`] per
//! (access, place), holding the home, each need's mechanism and digest, and the retry
//! state of a delivery that failed. `convoy explain` reads it; launch honours its retry.

use std::{collections::BTreeMap, time::Duration};

use flotilla_resources::{
    access_delivery::{DeliveredNeed, DeliveryMechanism, DeliveryRetry},
    AccessDelivery, AccessDeliverySpec, Convoy, Environment, InputMeta, ResourceError, ResourceObject, CONVOY_LABEL,
};
use flotilla_schema::Timestamp;
use flotilla_store::controller::{ReconcileErrorExhaustion, ReconcileErrorPolicy, ReconcileOutcome, Reconciler, SecondaryWatch};
use flotilla_vessel_delivery::{Access, Action, FetchBy, Liveness, Mechanism, Plan};

use super::super::{account_access::AccountLaunch, state::ControllerRuntimeState};

/// The longest diagnostic a retry keeps; the full error is in the launch's own log.
const ERROR_LIMIT: usize = 1024;
const FIRST_RETRY: Duration = Duration::from_secs(1);
const LAST_RETRY: Duration = Duration::from_secs(60);

/// One access's status at one place, as launch read it before delivering.
pub(in crate::runtime) struct AccessStatus {
    namespace: String,
    meta: InputMeta,
    identity: AccessDeliverySpec,
    existing: Option<ResourceObject<AccessDelivery>>,
}

impl AccessStatus {
    pub(in crate::runtime) async fn read(
        state: &ControllerRuntimeState,
        launch: &AccountLaunch<'_>,
        access: &str,
        harness: &str,
    ) -> Result<Self, String> {
        let name = record_name(access, launch.env_ref);
        let existing = match state.daemon.resource_backend().using::<AccessDelivery>(launch.namespace).get(&name).await {
            Ok(existing) => Some(existing),
            Err(ResourceError::NotFound { .. }) => None,
            Err(error) => return Err(format!("read access delivery `{name}`: {error}")),
        };
        let meta = InputMeta::builder().name(name).labels(BTreeMap::from([(CONVOY_LABEL.to_string(), launch.convoy.to_string())])).build();
        let identity = AccessDeliverySpec {
            access: access.to_string(),
            place: launch.env_ref.to_string(),
            harness: harness.to_string(),
            home: existing.as_ref().map(|existing| existing.spec.home.clone()).unwrap_or_default(),
            needs: existing.as_ref().map(|existing| existing.spec.needs.clone()).unwrap_or_default(),
            retry: None,
        };
        Ok(Self { namespace: launch.namespace.to_string(), meta, identity, existing })
    }

    /// Refuse a delivery before its recorded retry is due.
    pub(in crate::runtime) fn due(&self, state: &ControllerRuntimeState) -> Result<(), String> {
        let now = Timestamp::from_datetime(&state.daemon.clock().now());
        match self.existing.as_ref().and_then(|existing| existing.spec.retry.as_ref()) {
            Some(retry) if retry.next_at > now => Err(format!(
                "access `{}` delivery retries after attempt {} failed at `{}`: {}",
                self.identity.access, retry.attempts, retry.need, retry.error
            )),
            _ => Ok(()),
        }
    }

    /// Record what landed: each need's mechanism and digest, keeping when an unchanged need
    /// first landed, and clear any retry.
    pub(in crate::runtime) async fn delivered(&self, state: &ControllerRuntimeState, plan: &Plan, home: &str) -> Result<(), String> {
        let now = Timestamp::from_datetime(&state.daemon.clock().now());
        let needs = plan
            .steps
            .iter()
            .map(|step| {
                let digest = step.digest.as_str().to_string();
                let previous = self.identity.needs.get(step.need.as_str()).filter(|previous| previous.digest == digest);
                let at = match (step.action, previous) {
                    (Action::Keep, Some(previous)) => previous.at,
                    _ => now,
                };
                (step.need.as_str().to_string(), DeliveredNeed { mechanism: delivery_mechanism(step.mechanism), digest, at })
            })
            .collect();
        self.write(state, AccessDeliverySpec { home: home.to_string(), needs, retry: None, ..self.identity.clone() }).await
    }

    /// Record a failed delivery: the need it failed at, a bounded diagnostic and when the
    /// next attempt is due. What landed before stays recorded.
    pub(in crate::runtime) async fn failed(&self, state: &ControllerRuntimeState, need: String, error: &str) -> Result<(), String> {
        let now = state.daemon.clock().now();
        let attempts = self.existing.as_ref().and_then(|existing| existing.spec.retry.as_ref()).map_or(0, |retry| retry.attempts) + 1;
        let next_at = Timestamp::from_datetime(&(now + retry_delay(attempts)));
        let retry = DeliveryRetry { need, attempts, error: bounded(error), next_at };
        self.write(state, AccessDeliverySpec { retry: Some(retry), ..self.identity.clone() }).await
    }

    async fn write(&self, state: &ControllerRuntimeState, spec: AccessDeliverySpec) -> Result<(), String> {
        let records = state.daemon.resource_backend().using::<AccessDelivery>(&self.namespace);
        let name = &self.meta.name;
        let written = match &self.existing {
            Some(existing) if existing.spec == spec => return Ok(()),
            Some(existing) => records.update(&self.meta, &existing.metadata.resource_version, &spec).await.map(drop),
            None => records.create(&self.meta, &spec).await.map(drop),
        };
        written.map_err(|error| format!("record access delivery `{name}`: {error}"))
    }
}

/// One record per (access, place). The access is opaque and may hold any character, so
/// the name is a digest of the pair.
pub(in crate::runtime) fn record_name(access: &str, place: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::new().chain_update(access.as_bytes()).chain_update([0]).chain_update(place.as_bytes()).finalize();
    format!("access-{}", &format!("{digest:x}")[..16])
}

fn delivery_mechanism(mechanism: Mechanism) -> DeliveryMechanism {
    match mechanism {
        Mechanism::HostPath { access: Access::ReadOnly, liveness: Liveness::Live } => DeliveryMechanism::HostPathReadOnlyLive,
        Mechanism::HostPath { access: Access::ReadOnly, liveness: Liveness::Snapshot } => DeliveryMechanism::HostPathReadOnlySnapshot,
        Mechanism::HostPath { access: Access::ReadWrite, liveness: Liveness::Live } => DeliveryMechanism::HostPathReadWriteLive,
        Mechanism::HostPath { access: Access::ReadWrite, liveness: Liveness::Snapshot } => DeliveryMechanism::HostPathReadWriteSnapshot,
        Mechanism::AgentFetch { by: FetchBy::Url } => DeliveryMechanism::AgentFetchUrl,
        Mechanism::AgentPush => DeliveryMechanism::AgentPush,
    }
}

/// Doubling from a second, at most a minute apart.
fn retry_delay(attempts: u32) -> Duration {
    FIRST_RETRY.saturating_mul(1 << attempts.saturating_sub(1).min(6)).min(LAST_RETRY)
}

fn bounded(error: &str) -> String {
    match error.char_indices().nth(ERROR_LIMIT) {
        Some((end, _)) => format!("{}…", &error[..end]),
        None => error.to_string(),
    }
}

/// Removes a record once its place or its convoy is gone; the home went with the place,
/// and the access ended with the convoy.
pub(in crate::runtime) fn status_controller(
    state: std::sync::Arc<ControllerRuntimeState>,
    namespace: String,
) -> (Vec<Box<dyn SecondaryWatch<Primary = AccessDelivery>>>, AccessStatusReconciler) {
    (Vec::new(), AccessStatusReconciler { state, namespace })
}

pub(in crate::runtime) struct AccessStatusReconciler {
    state: std::sync::Arc<ControllerRuntimeState>,
    namespace: String,
}

impl AccessStatusReconciler {
    pub(in crate::runtime) async fn retire(&self, record: &ResourceObject<AccessDelivery>) -> Result<(), String> {
        let backend = self.state.daemon.resource_backend();
        let gone = |found: Result<_, ResourceError>| -> Result<bool, String> {
            match found {
                Ok(_) => Ok(false),
                Err(ResourceError::NotFound { .. }) => Ok(true),
                Err(error) => Err(error.to_string()),
            }
        };
        let place = &record.spec.place;
        let place_gone = *place != self.state.host_direct_environment_name
            && gone(backend.including_replicas::<Environment>(&self.namespace).get(place).await.map(drop))?;
        let convoy_gone = match record.metadata.labels.get(CONVOY_LABEL) {
            Some(convoy) => gone(backend.including_replicas::<Convoy>(&self.namespace).get(convoy).await.map(drop))?,
            None => false,
        };
        if !(place_gone || convoy_gone) {
            return Ok(());
        }
        match backend.using::<AccessDelivery>(&self.namespace).delete(&record.metadata.name).await {
            Ok(_) | Err(ResourceError::NotFound { .. }) => Ok(()),
            Err(error) => Err(format!("remove access delivery `{}`: {error}", record.metadata.name)),
        }
    }
}

impl Reconciler for AccessStatusReconciler {
    type Resource = AccessDelivery;
    type Prepared = ();

    async fn prepare(&self, record: &ResourceObject<AccessDelivery>) -> Result<(), ResourceError> {
        self.retire(record).await.map_err(ResourceError::other)
    }

    fn reconcile(
        &self,
        _obj: &ResourceObject<AccessDelivery>,
        _prepared: &(),
        _now: chrono::DateTime<chrono::Utc>,
    ) -> ReconcileOutcome<AccessDelivery> {
        ReconcileOutcome::new(None)
    }

    async fn run_finalizer(&self, _obj: &ResourceObject<AccessDelivery>) -> Result<(), ResourceError> {
        Ok(())
    }

    fn finalizer_name(&self) -> Option<&'static str> {
        None
    }

    fn reconcile_error_policy(&self) -> Option<ReconcileErrorPolicy> {
        Some(ReconcileErrorPolicy {
            max_consecutive_failures: 8,
            initial_backoff: FIRST_RETRY,
            max_backoff: LAST_RETRY,
            exhaustion: ReconcileErrorExhaustion::Retry,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Behaviour (#3964): every mechanism the planner can choose is stored as one whose
    // `convoy explain` spelling is the vocabulary's own, so the record names what ran.
    #[test]
    fn stored_mechanisms_read_as_the_vocabulary_names_them() {
        let mut mechanisms = vec![Mechanism::AgentFetch { by: FetchBy::Url }, Mechanism::AgentPush];
        for access in [Access::ReadOnly, Access::ReadWrite] {
            for liveness in [Liveness::Live, Liveness::Snapshot] {
                mechanisms.push(Mechanism::HostPath { access, liveness });
            }
        }
        for mechanism in mechanisms {
            assert_eq!(delivery_mechanism(mechanism).label(), mechanism.to_string());
        }
    }
}
