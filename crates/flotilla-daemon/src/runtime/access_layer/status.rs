//! The access layer's per-access status (#3964 contract 4): one [`AccessDelivery`] per
//! (access, place), holding the home, each need's mechanism and digest, and the retry
//! state of a delivery that failed. `convoy explain` reads it; launch honours its retry.

use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use async_trait::async_trait;
use flotilla_provider_runtime::{ChannelLabel, CommandRunner};
use flotilla_provisioning::{
    engine::{
        access_deliveries::{AccessDeliveries, HeldDelivery},
        controller::{Actuator, Backing, Holding, ProviderFailure, Usability},
        plain_controlled::PlainLifecycle,
    },
    stored::environment::Environment,
};
use flotilla_resources::{
    access_delivery::{convoy_of_access, record_name, DeliveredNeed, DeliveryMechanism, DeliveryRetry},
    account_access::{requester_convoy_dir, REQUESTER_HOME_DIR, REQUESTER_MATERIAL_DIR},
    AccessDeliverySpec,
};
use flotilla_schema::{Identity, Kind, KindCode, Timestamp};
use flotilla_schema_sqlite::{stream::Service, Read, ResourceId, StoreError};
use flotilla_state_machines::{
    machines::convoy::{Convoy as EngineConvoy, ConvoyLabel},
    Cause, CauseCategory, Resource as EngineResource, StateValue,
};
use flotilla_vessel_delivery::{Access, Action, FetchBy, Liveness, Mechanism, Plan};

use super::super::{account_access::AccountLaunch, state::ControllerRuntimeState};

/// The longest diagnostic a retry keeps; the full error is in the launch's own log.
const ERROR_LIMIT: usize = 1024;
const FIRST_RETRY: Duration = Duration::from_secs(1);
const LAST_RETRY: Duration = Duration::from_secs(60);
/// How a permanent failure's diagnostic begins, so `convoy explain` and launch say why
/// no retry is due.
const PERMANENT: &str = "permanent, not retried: ";

/// One access's status at one place, as launch read it before delivering.
pub(in crate::runtime) struct AccessStatus {
    deliveries: AccessDeliveries,
    name: String,
    identity: AccessDeliverySpec,
    existing: Option<HeldDelivery>,
}

impl AccessStatus {
    pub(in crate::runtime) async fn read(
        state: &ControllerRuntimeState,
        launch: &AccountLaunch<'_>,
        access: &str,
        harness: &str,
    ) -> Result<Self, String> {
        let deliveries = state.access_deliveries();
        let name = record_name(access, launch.env_ref);
        let existing = deliveries.get(&name)?;
        let identity = AccessDeliverySpec {
            access: access.to_string(),
            place: launch.env_ref.to_string(),
            harness: harness.to_string(),
            home: existing.as_ref().map(|existing| existing.spec.home.clone()).unwrap_or_default(),
            needs: existing.as_ref().map(|existing| existing.spec.needs.clone()).unwrap_or_default(),
            retry: None,
        };
        Ok(Self { deliveries, name, identity, existing })
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
    /// next attempt is due. A permanent failure, such as an authority refusal or a policy
    /// violation, is never due again: launch stops retrying until the record is removed.
    /// What landed before stays recorded.
    pub(in crate::runtime) async fn failed(
        &self,
        state: &ControllerRuntimeState,
        need: String,
        error: &str,
        permanent: bool,
    ) -> Result<(), String> {
        let now = state.daemon.clock().now();
        let attempts = self.existing.as_ref().and_then(|existing| existing.spec.retry.as_ref()).map_or(0, |retry| retry.attempts) + 1;
        let (next_at, error) = if permanent {
            (Timestamp::MAX, bounded(&format!("{PERMANENT}{error}")))
        } else {
            (Timestamp::from_datetime(&(now + retry_delay(attempts))), bounded(error))
        };
        let retry = DeliveryRetry { need, attempts, error, next_at };
        self.write(state, AccessDeliverySpec { retry: Some(retry), ..self.identity.clone() }).await
    }

    async fn write(&self, state: &ControllerRuntimeState, spec: AccessDeliverySpec) -> Result<(), String> {
        if self.existing.as_ref().is_some_and(|existing| existing.spec == spec) {
            return Ok(());
        }
        let now = Timestamp::from_datetime(&state.daemon.clock().now());
        self.deliveries.write(&self.name, self.existing.as_ref().map(|existing| existing.version), &spec, now).map(drop)
    }
}

/// Record a push of `need` outside a launch into the home `record` names (#4037): the
/// digest that landed, or the retry a failure leaves. Answers when that retry falls due, unless
/// the push has failed `limit` times in a row.
pub(in crate::runtime) async fn record_push(
    state: &ControllerRuntimeState,
    record: &HeldDelivery,
    need: &str,
    digest: &str,
    pushed: Result<(), String>,
    limit: u32,
) -> Result<Option<Timestamp>, String> {
    let now = state.daemon.clock().now();
    let mut spec = record.spec.clone();
    spec.retry = match pushed {
        Ok(()) => {
            if let Some(landed) = spec.needs.get_mut(need) {
                landed.digest = digest.to_string();
                landed.at = Timestamp::from_datetime(&now);
            }
            // A launch's retry for another need stays recorded.
            record.spec.retry.clone().filter(|retry| retry.need != need)
        }
        Err(error) => {
            let attempts = record.spec.retry.as_ref().filter(|retry| retry.need == need).map_or(0, |retry| retry.attempts) + 1;
            let next_at = Timestamp::from_datetime(&(now + retry_delay(attempts)));
            Some(DeliveryRetry { need: need.to_string(), attempts, error: bounded(&error), next_at })
        }
    };
    state.access_deliveries().write(&record.name, Some(record.version), &spec, Timestamp::from_datetime(&now))?;
    Ok(spec.retry.filter(|retry| retry.need == need && retry.attempts < limit).map(|retry| retry.next_at))
}

fn delivery_mechanism(mechanism: Mechanism) -> DeliveryMechanism {
    match mechanism {
        Mechanism::HostPath { access: Access::ReadOnly, liveness: Liveness::Live } => DeliveryMechanism::HostPathReadOnlyLive,
        Mechanism::HostPath { access: Access::ReadOnly, liveness: Liveness::Snapshot } => DeliveryMechanism::HostPathReadOnlySnapshot,
        Mechanism::HostPath { access: Access::ReadWrite, liveness: Liveness::Live } => DeliveryMechanism::HostPathReadWriteLive,
        Mechanism::HostPath { access: Access::ReadWrite, liveness: Liveness::Snapshot } => DeliveryMechanism::HostPathReadWriteSnapshot,
        Mechanism::AgentFetch { by: FetchBy::Url } => DeliveryMechanism::AgentFetchUrl,
        Mechanism::AgentFetch { by: FetchBy::Digest } => DeliveryMechanism::AgentFetchDigest,
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

/// What ends a delivery, as the store holds its place and its convoy now.
#[derive(Debug, Default)]
struct Ending {
    place_gone: bool,
    convoy_gone: bool,
    /// Why it is held: its place or its convoy is not found here.
    unresolved: Vec<String>,
    /// The records of its place and its convoy, whose commits look at it again.
    followed: Vec<Identity>,
}

impl Ending {
    fn ended(&self) -> bool {
        self.place_gone || self.convoy_gone
    }
}

/// The record of kind `code` named `name`: this home's, else the first other home's.
fn find(read: &mut Read<'_>, code: KindCode, name: &str, home: &str) -> Result<Option<(ResourceId, Identity)>, StoreError> {
    let mut found = None;
    for id in read.resolve_named(code, name)? {
        let Some((identity, _)) = read.identity(id)? else { continue };
        let here = identity.home == home;
        if found.is_none() || here {
            found = Some((id, identity));
        }
        if here {
            break;
        }
    }
    Ok(found)
}

/// Judges `delivery` against its place and its convoy, read in `read`. A place that ended is gone:
/// its environment is not launched again. A convoy that ended is gone too: a launch is ended,
/// never deleted, and holds no access. Not finding either is no record that it ended: the delivery
/// is held, and its requester state stays. Imported ended convoys rely on the start pass (#3108
/// ruling (x)): one held only by an access delivery the old fleet had not yet removed retires once
/// this removes it.
fn ending(read: &mut Read<'_>, delivery: &AccessDeliverySpec, home: &str, host_direct: &str) -> Result<Ending, StoreError> {
    let environment = <EngineResource<Environment> as Kind>::CODE;
    let convoy_code = <EngineResource<EngineConvoy> as Kind>::CODE;
    let mut ending = Ending::default();
    let place = &delivery.place;
    // The host-direct place never ends.
    if place != host_direct {
        match find(read, environment, place, home)? {
            Some((id, identity)) => {
                match read.load::<EngineResource<Environment>>(id)? {
                    Some(loaded) => ending.place_gone = !loaded.value.state().live(),
                    None => ending.unresolved.push(format!("its place `{place}` is not found")),
                }
                ending.followed.push(identity);
            }
            None => {
                ending.unresolved.push(format!("its place `{place}` is not found"));
                ending.followed.push(Identity { kind: environment, name: place.clone(), home: home.to_owned() });
            }
        }
    }
    if let Some(convoy) = convoy_of_access(&delivery.access) {
        match find(read, convoy_code, &convoy, home)? {
            Some((id, identity)) => {
                match read.load::<EngineResource<EngineConvoy>>(id)? {
                    Some(loaded) => ending.convoy_gone = loaded.value.state().label() != ConvoyLabel::Open,
                    None => ending.unresolved.push(format!("its convoy `{convoy}` is not found")),
                }
                ending.followed.push(identity);
            }
            None => {
                ending.unresolved.push(format!("its convoy `{convoy}` is not found"));
                ending.followed.push(Identity { kind: convoy_code, name: convoy, home: home.to_owned() });
            }
        }
    }
    Ok(ending)
}

/// AccessDelivery's actuator on the engine [`Controller`] (#3964, #4415): a record is released,
/// and so removed, once its place or its convoy is recorded as ended; the home went with the place,
/// and the access ended with the convoy. Its holders are its place's Environment and its convoy,
/// whose commits look at it again, as its own do. An SSH host's requester state goes with a
/// convoy's end; a failure keeps the record, retried at the injected clock's backoff.
///
/// [`Controller`]: flotilla_provisioning::engine::controller::Controller
pub(in crate::runtime) struct AccessDeliveryActuator {
    state: Arc<ControllerRuntimeState>,
    service: Arc<Service>,
    home: String,
    /// Each record held because its place or convoy cannot be found, by name, with the reason
    /// last logged: a hold is logged when it begins or its reason changes, not at every pass.
    held: Mutex<BTreeMap<String, String>>,
    /// The failed releases of each record in a row, which its next try's backoff doubles from.
    failures: Mutex<BTreeMap<String, u32>>,
}

impl AccessDeliveryActuator {
    pub(in crate::runtime) fn new(state: Arc<ControllerRuntimeState>) -> Self {
        let service = state.daemon.decided_store();
        let home = service.home().to_owned();
        Self { state, service, home, held: Mutex::default(), failures: Mutex::default() }
    }

    /// How `delivery` stands against its place and its convoy now.
    async fn ending(&self, delivery: &AccessDeliverySpec) -> Result<Ending, String> {
        let (service, delivery) = (Arc::clone(&self.service), delivery.clone());
        let (home, host_direct) = (self.home.clone(), self.state.host_direct_environment_name.clone());
        tokio::task::spawn_blocking(flotilla_schema::reads::carry(move || {
            service.with_store(|store| ending(&mut store.read()?, &delivery, &home, &host_direct))
        }))
        .await
        .map_err(|error| format!("read access delivery ending: {error}"))?
        .map_err(|error| error.to_string())
    }

    /// Removes the requester state of `delivery`'s convoy from its SSH host once the convoy has
    /// ended. With the place gone too the host is no longer ours to drive, so nothing is removed.
    async fn release_requester_state(&self, delivery: &AccessDeliverySpec) -> Result<(), String> {
        let ending = self.ending(delivery).await?;
        if !ending.convoy_gone || ending.place_gone {
            return Ok(());
        }
        let place = &delivery.place;
        match (convoy_of_access(&delivery.access), self.state.ssh_host.get(place)) {
            (Some(convoy), Some(profile)) => remove_requester_state(profile.runner.as_ref(), &convoy).await,
            (convoy, None) => {
                tracing::debug!(%place, ?convoy, "no SSH host profile for the place; requester state not removed");
                Ok(())
            }
            (None, Some(_)) => Ok(()),
        }
    }

    /// Notes `name` held for `reason`, or no longer held with none; a hold is logged when it
    /// begins or its reason changes.
    fn hold(&self, name: &str, reason: Option<String>) {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        match reason {
            Some(reason) if held.get(name) != Some(&reason) => {
                tracing::warn!(delivery = %name, %reason, "access delivery held: not retired, requester state kept");
                held.insert(name.to_owned(), reason);
            }
            Some(_) => {}
            None => {
                held.remove(name);
            }
        }
    }
}

#[async_trait]
impl Actuator<PlainLifecycle<AccessDeliverySpec>> for AccessDeliveryActuator {
    /// A record held is its own backing.
    async fn observe(&self, name: &str, _: &AccessDeliverySpec) -> Result<Backing, Cause> {
        Ok(Backing::Present { identity: name.to_owned() })
    }

    /// Never asked: a record is realised as soon as launch writes it.
    async fn realise(&self, name: &str, _: &AccessDeliverySpec) -> Result<String, ProviderFailure> {
        Ok(name.to_owned())
    }

    async fn release(&self, name: &str, delivery: &AccessDeliverySpec, _: Option<&str>) -> Result<(), ProviderFailure> {
        match self.release_requester_state(delivery).await {
            Ok(()) => {
                self.failures.lock().unwrap_or_else(PoisonError::into_inner).remove(name);
                self.hold(name, None);
                Ok(())
            }
            Err(error) => {
                let failures = {
                    let mut failures = self.failures.lock().unwrap_or_else(PoisonError::into_inner);
                    let failures = failures.entry(name.to_owned()).or_default();
                    *failures = failures.saturating_add(1);
                    *failures
                };
                let retry_at = Timestamp::from_datetime(&(self.state.daemon.clock().now() + retry_delay(failures)));
                Err(ProviderFailure { retry_at: Some(retry_at), ..ProviderFailure::new(Cause::new(CauseCategory::External, error), true) })
            }
        }
    }

    fn followed(&self) -> Vec<KindCode> {
        vec![<EngineResource<Environment> as Kind>::CODE, <EngineResource<EngineConvoy> as Kind>::CODE]
    }

    /// Its holders are its only inputs.
    fn dependencies(&self, _: &AccessDeliverySpec) -> Vec<Identity> {
        Vec::new()
    }

    fn holder(&self, read: &mut Read<'_>, name: &str, delivery: &AccessDeliverySpec) -> Result<Holding, StoreError> {
        let ending = ending(read, delivery, &self.home, &self.state.host_direct_environment_name)?;
        let ended = ending.ended();
        self.hold(name, (!ended && !ending.unresolved.is_empty()).then(|| ending.unresolved.join("; ")));
        Ok(Holding { ended, records: ending.followed })
    }

    /// Nothing waits: a record is never tried.
    fn usability(&self, _: &mut Read<'_>, _: &AccessDeliverySpec) -> Result<Vec<Usability>, StoreError> {
        Ok(Vec::new())
    }
}

/// Remove every requester directory of `convoy` from a host: its material (credential-bearing,
/// under the runtime base), its home (under the durable base), and the homes a runtime base held
/// before #4012. Missing paths are not errors; read-only content is made writable first (#4070).
async fn remove_requester_state(runner: &dyn CommandRunner, convoy: &str) -> Result<(), String> {
    let fallback = Path::new(flotilla_environment::environment::runner::CONTAINED_WRITABLE_CONFIG_BASE);
    let runtime = runner.writable_config_base(None, fallback).await?;
    let durable = flotilla_environment::environment::runner::home_base(runner).await?;
    let paths = [
        requester_convoy_dir(&runtime, REQUESTER_MATERIAL_DIR, convoy),
        requester_convoy_dir(&durable, REQUESTER_HOME_DIR, convoy),
        requester_convoy_dir(&runtime, REQUESTER_HOME_DIR, convoy),
    ];
    let mut args = vec!["-c", REMOVE_TREES, "flotilla-requester-remove"];
    let paths = paths.iter().map(|path| path.display().to_string()).collect::<Vec<_>>();
    args.extend(paths.iter().map(String::as_str));
    runner
        .run("sh", &args, Path::new("/"), &ChannelLabel::Default)
        .await
        .map(drop)
        .map_err(|error| format!("remove requester state of convoy `{convoy}`: {error}"))
}

/// Each argument is a directory to remove; a read-only tree is made writable first.
const REMOVE_TREES: &str = r#"for path in "$@"; do
  case "$path" in /?*) ;; *) echo "refusing $path" >&2; exit 1;; esac
  [ -e "$path" ] || [ -L "$path" ] || continue
  chmod -R u+rwX -- "$path" 2>/dev/null || true
  rm -rf -- "$path" || exit 1
done"#;

#[cfg(test)]
mod tests {
    use super::*;

    // Behaviour (#3964): every mechanism the planner can choose is stored as one whose
    // `convoy explain` spelling is the vocabulary's own, so the record names what ran.
    #[test]
    fn stored_mechanisms_read_as_the_vocabulary_names_them() {
        let mut mechanisms =
            vec![Mechanism::AgentFetch { by: FetchBy::Url }, Mechanism::AgentFetch { by: FetchBy::Digest }, Mechanism::AgentPush];
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
