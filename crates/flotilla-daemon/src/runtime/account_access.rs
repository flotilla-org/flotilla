//! Launch-time delivery of a requester's bound account (#3907), and the two controllers that
//! follow bindings and provisions through their lifecycle (#3929).
//!
//! A crew whose item declares account access starts only in the explicit home its binding's
//! plan delivers: the vessel agent (or this daemon, on its own host) writes the bound
//! credential's material there, and the provision record notes what was delivered at this
//! place. A crew with no open binding is refused with the reason; nothing falls through to an
//! ambient login (#3907 contract 8).
//!
//! - The binding controller reconciles one convoy's requesters on the convoy's home: a convoy
//!   event reaches only that convoy, a binding event the convoy it names, and a grant or
//!   declaration event the convoys whose bindings use that credential and those still waiting
//!   for a binding. Only binding a requester is serialised against admission.
//! - The provision controller reconciles one provision at its place, on its binding's events.
//!   A withdrawal that fails is an ordinary reconcile failure, retried with backoff.

use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    marker::PhantomData,
    pin::Pin,
    sync::{atomic::AtomicU64, Arc},
    time::Duration,
};

use chrono::{DateTime, Utc};
use flotilla_agent::account_access::{AccountProvisioners, EnvironmentKind};
use flotilla_core::in_process::InProcessDaemon;
use flotilla_provider_runtime::CommandRunner;
use flotilla_resources::{
    account_access::{
        binding_name, material_generation, AccessName, AccountBinding, AccountBindingSpec, AccountProvision, AccountProvisionSpec,
        Delivered, ACCOUNT_BINDING_LABEL, ACCOUNT_CREDENTIAL_LABEL,
    },
    Convoy, CredentialGrant, CredentialSpec, Environment, InputMeta, ReadWatchEvent, ResourceError, ResourceObject, ResourceProvenance,
    CONVOY_LABEL,
};
use flotilla_schema::{Ref, Timestamp};
use flotilla_store::{
    controller::{
        ControllerLoop, LabelMappedWatch, ReconcileErrorExhaustion, ReconcileErrorPolicy, ReconcileOutcome, Reconciler, SecondaryWatch,
        WorkQueueSender,
    },
    ResourceBackend,
};
use futures::StreamExt;
use tokio::task::JoinHandle;

use super::health::{spawn_controller_loop_watchdog, RuntimeHealth};
use super::state::ControllerRuntimeState;
use super::tasks::supervise_controller;
use crate::supervisor::ControllerSupervision;

/// What a launch needs to know about the requester and its place.
pub(super) struct AccountLaunch<'a> {
    pub namespace: &'a str,
    pub convoy: &'a str,
    pub role: &'a str,
    pub env_ref: &'a str,
    pub agentless: bool,
}

/// The environment kind a launch's place is.
async fn place_environment_kind(state: &ControllerRuntimeState, launch: &AccountLaunch<'_>) -> Result<EnvironmentKind, String> {
    if launch.agentless {
        return Ok(EnvironmentKind::AgentlessSsh);
    }
    if launch.env_ref == state.host_direct_environment_name {
        return Ok(EnvironmentKind::HostDirect);
    }
    let environment = match state.daemon.resource_backend().including_replicas::<Environment>(launch.namespace).get(launch.env_ref).await {
        Ok(environment) => Some(environment.object.spec),
        Err(ResourceError::NotFound { .. }) => None,
        Err(error) => return Err(error.to_string()),
    };
    EnvironmentKind::of_environment(environment.as_ref()).map_err(|reason| format!("environment `{}`: {reason}", launch.env_ref))
}

/// Deliver the requester's bound account into its home at this place and record the provision.
/// Returns the session variables the plan sets.
pub(super) async fn deliver_bound_account(
    state: &ControllerRuntimeState,
    launch: &AccountLaunch<'_>,
    access: &AccessName,
    runner: Arc<dyn CommandRunner>,
) -> Result<Vec<(String, String)>, String> {
    let backend = state.daemon.resource_backend();
    let name = binding_name(launch.convoy, launch.role, access);
    let binding = match backend.including_replicas::<AccountBinding>(launch.namespace).get(&name).await {
        Ok(binding) => binding,
        Err(ResourceError::NotFound { .. }) => {
            return Err(format!(
                "crew `{}` has no account binding for `{access}`: its harness home is never left to an ambient login",
                launch.role
            ))
        }
        Err(error) => return Err(format!("read account binding `{name}`: {error}")),
    };
    if let Some(reason) = binding.object.spec.state.value.reason() {
        return Err(format!("crew `{}` waits: {reason}; `flotilla convoy rebind` opens a new binding", launch.role));
    }
    let binding_home = match &binding.provenance {
        ResourceProvenance::Local => backend.local_root().map_err(|error| error.to_string())?.to_string(),
        ResourceProvenance::Replica { origin_root, .. } => origin_root.to_string(),
    };
    let provisioners = AccountProvisioners::seeded();
    let provisioner = provisioners.for_access(access).ok_or_else(|| format!("no registered implementation supplies `{access}`"))?;
    let kind = place_environment_kind(state, launch).await?;
    // A replaced vessel on a kind with no plan refuses the requester there; the binding never
    // switches account to fit (#3882).
    let plan = provisioner
        .plan(kind)
        .map_err(|reason| format!("account binding `{name}` cannot be provisioned at `{}`: {reason}", launch.env_ref))?;
    let store = state.credential_store.as_ref().ok_or("host-local credential store unavailable for account delivery")?;
    let (declaration, material) = store.account_material(&binding.object.spec.credential.0).await?;
    let home = provisioner.home(&store.writable_base(&*runner).await?, launch.convoy, launch.role);
    let record = provision_name(&name, launch.env_ref);
    let mut provision = AccountProvisionSpec {
        binding: Ref::new(name.clone(), binding_home),
        place: launch.env_ref.to_string(),
        environment_kind: kind.as_str().to_string(),
        plan,
        home: home.clone(),
        delivered: None,
    };
    let provisions = backend.using::<AccountProvision>(launch.namespace);
    let meta = provision_meta(&record, &name, launch.convoy);
    // Record the place before writing material there, so a copy is never left without a
    // record the place pass would withdraw. A relaunch keeps its existing record, and with it
    // the previous delivery, until this one succeeds.
    match provisions.get(&record).await {
        Ok(_) => {}
        Err(ResourceError::NotFound { .. }) => {
            provisions.create(&meta, &provision).await.map_err(|error| format!("record account provision `{record}`: {error}"))?;
        }
        Err(error) => return Err(format!("read account provision `{record}`: {error}")),
    }
    let env = provisioner.deliver(&home, &material, &*runner).await?;
    provision.delivered = Some(Delivered {
        epoch: declaration.authorization_epoch.to_string(),
        generation: material_generation(&material),
        at: delivered_at(state, &binding.object.spec),
    });
    match provisions.get(&record).await {
        Ok(existing) if existing.spec == provision => {}
        Ok(existing) => {
            provisions
                .update(&meta, &existing.metadata.resource_version, &provision)
                .await
                .map_err(|error| format!("record account provision `{record}`: {error}"))?;
        }
        Err(ResourceError::NotFound { .. }) => {
            provisions.create(&meta, &provision).await.map_err(|error| format!("record account provision `{record}`: {error}"))?;
        }
        Err(error) => return Err(format!("read account provision `{record}`: {error}")),
    }
    Ok(env)
}

/// When a delivery under `binding` happened, on the runtime's clock, for the provision
/// controller's "delivered before the binding opened" check. Never earlier than the binding's
/// own opening, so clock skew between the binding's home and this place can never make a fresh
/// copy look stale. A rebind is noticed once its opening, on the home's clock, passes the last
/// delivery's recorded time; a home clock behind this place's delays that, and a new account's
/// material is still pushed as a new generation meanwhile.
fn delivered_at(state: &ControllerRuntimeState, binding: &AccountBindingSpec) -> Timestamp {
    Timestamp::from_datetime(&state.daemon.clock().now()).max(binding.opened_at)
}

/// One provision record per (binding, place).
fn provision_name(binding: &str, place: &str) -> String {
    format!("{binding}.{place}")
}

/// A provision's metadata: labelled with its binding (binding events reach it) and convoy.
fn provision_meta(name: &str, binding: &str, convoy: &str) -> InputMeta {
    InputMeta::builder()
        .name(name.to_string())
        .labels(BTreeMap::from([(ACCOUNT_BINDING_LABEL.to_string(), binding.to_string()), (CONVOY_LABEL.to_string(), convoy.to_string())]))
        .build()
}

/// Reconciles one convoy's account bindings on the convoy's home.
struct AccountBindingReconciler {
    daemon: Arc<InProcessDaemon>,
    namespace: String,
}

impl Reconciler for AccountBindingReconciler {
    type Resource = Convoy;
    type Prepared = ();

    async fn prepare(&self, obj: &ResourceObject<Convoy>) -> Result<(), ResourceError> {
        let changes = self.daemon.reconcile_convoy_accounts(&self.namespace, &obj.metadata.name).await.map_err(ResourceError::other)?;
        for (binding, change) in changes {
            tracing::info!(%binding, ?change, "account binding changed");
        }
        Ok(())
    }

    fn reconcile(&self, _obj: &ResourceObject<Convoy>, _prepared: &(), _now: DateTime<Utc>) -> ReconcileOutcome<Convoy> {
        ReconcileOutcome::new(None)
    }

    async fn run_finalizer(&self, _obj: &ResourceObject<Convoy>) -> Result<(), ResourceError> {
        Ok(())
    }

    fn finalizer_name(&self) -> Option<&'static str> {
        None
    }
}

/// Reconciles one provision at its place against its binding.
/// - A binding revoked, closed or gone: the delivered copy is deleted, the requester's harness
///   session is stopped (its TerminalSession is deleted, so it waits), and the record removed.
///   No launch can record a delivery meanwhile: launch refuses a binding that is not open.
/// - A binding reopened by a rebind since the delivery: the copy is deleted, the session
///   stopped so a fresh one starts, and the record's delivery cleared only at the version
///   withdrawn; a relaunch that recorded a new delivery first wins.
/// - A place whose environment is gone: the record is removed; its copies went with it.
/// - Otherwise a new material generation or epoch is pushed into the home (update mode push).
struct AccountProvisionReconciler {
    state: Arc<ControllerRuntimeState>,
    namespace: String,
}

impl Reconciler for AccountProvisionReconciler {
    type Resource = AccountProvision;
    type Prepared = ();

    async fn prepare(&self, provision: &ResourceObject<AccountProvision>) -> Result<(), ResourceError> {
        reconcile_provision(&self.state, &self.namespace, provision).await.map_err(ResourceError::other)
    }

    fn reconcile(
        &self,
        _obj: &ResourceObject<AccountProvision>,
        _prepared: &(),
        _now: DateTime<Utc>,
    ) -> ReconcileOutcome<AccountProvision> {
        ReconcileOutcome::new(None)
    }

    async fn run_finalizer(&self, _obj: &ResourceObject<AccountProvision>) -> Result<(), ResourceError> {
        Ok(())
    }

    fn finalizer_name(&self) -> Option<&'static str> {
        None
    }

    fn reconcile_error_policy(&self) -> Option<ReconcileErrorPolicy> {
        Some(ReconcileErrorPolicy {
            max_consecutive_failures: 8,
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(60),
            exhaustion: ReconcileErrorExhaustion::Retry,
        })
    }
}

async fn reconcile_provision(
    state: &Arc<ControllerRuntimeState>,
    namespace: &str,
    provision: &ResourceObject<AccountProvision>,
) -> Result<(), String> {
    let backend = state.daemon.resource_backend();
    let provisions = backend.clone().using::<AccountProvision>(namespace);
    let name = &provision.metadata.name;
    let spec = &provision.spec;
    if spec.place != state.host_direct_environment_name
        && matches!(backend.including_replicas::<Environment>(namespace).get(&spec.place).await, Err(ResourceError::NotFound { .. }))
    {
        return delete_provision(&provisions, name).await;
    }
    let binding = match backend.including_replicas::<AccountBinding>(namespace).get(&spec.binding.name).await {
        Ok(binding) => Some(binding.object),
        Err(ResourceError::NotFound { .. }) => None,
        Err(error) => return Err(format!("read account binding `{}`: {error}", spec.binding.name)),
    };
    let provisioners = AccountProvisioners::seeded();
    match binding {
        Some(binding) if binding.spec.state.value.is_open() => {
            let superseded = spec.delivered.as_ref().is_some_and(|delivered| delivered.at < binding.spec.opened_at);
            if !superseded {
                return push_generation(state, namespace, provision, &binding, &provisioners).await;
            }
            withdraw(state, namespace, spec, &provisioners).await?;
            let mut cleared = spec.clone();
            cleared.delivered = None;
            let meta = InputMeta::builder().name(name.clone()).labels(provision.metadata.labels.clone()).build();
            match provisions.update(&meta, &provision.metadata.resource_version, &cleared).await {
                Ok(_) | Err(ResourceError::Conflict { .. }) | Err(ResourceError::NotFound { .. }) => Ok(()),
                Err(error) => Err(format!("clear withdrawn provision `{name}`: {error}")),
            }
        }
        _ => {
            withdraw(state, namespace, spec, &provisioners).await?;
            delete_provision(&provisions, name).await
        }
    }
}

async fn delete_provision(provisions: &flotilla_store::TypedResolver<AccountProvision>, name: &str) -> Result<(), String> {
    match provisions.delete(name).await {
        Ok(_) | Err(ResourceError::NotFound { .. }) => Ok(()),
        Err(error) => Err(format!("remove account provision `{name}`: {error}")),
    }
}

/// The requester's crew sessions at a place: those whose convoy, role and access name this
/// binding. Matching by the derived name, not by parsing it, holds whatever the names contain.
async fn requester_sessions(
    state: &ControllerRuntimeState,
    namespace: &str,
    binding: &str,
    access: &AccessName,
    place: &str,
) -> Result<Vec<ResourceObject<flotilla_resources::TerminalSession>>, String> {
    let sessions = state
        .daemon
        .resource_backend()
        .using::<flotilla_resources::TerminalSession>(namespace)
        .list()
        .await
        .map_err(|error| format!("list terminal sessions: {error}"))?;
    Ok(sessions
        .items
        .into_iter()
        .filter(|session| {
            session.spec.env_ref == place
                && matches!(
                    &session.spec.source,
                    flotilla_resources::TerminalSessionSource::Agent { context, .. }
                        if binding_name(&context.convoy, &session.spec.role, access) == binding
                )
        })
        .collect())
}

/// A runner that reaches the requester's home at a place.
async fn place_runner(
    state: &Arc<ControllerRuntimeState>,
    sessions: &[ResourceObject<flotilla_resources::TerminalSession>],
    place: &str,
) -> Result<Arc<dyn CommandRunner>, String> {
    let runtime = super::terminal::TerminalControllerRuntime { state: Arc::clone(state) };
    match sessions.first() {
        Some(session) => runtime.runner_for_spec((&session.spec).into()).await,
        None => runtime.runner_for_env(place),
    }
}

/// Delete the delivered copy and stop the requester's harness session at a place.
async fn withdraw(
    state: &Arc<ControllerRuntimeState>,
    namespace: &str,
    spec: &AccountProvisionSpec,
    provisioners: &AccountProvisioners,
) -> Result<(), String> {
    let implementation = provisioners
        .for_implementation(&spec.plan.implementation)
        .ok_or_else(|| format!("no registered implementation `{}` to remove its copy", spec.plan.implementation))?;
    let access = AccessName(implementation.access().to_string());
    let sessions = requester_sessions(state, namespace, &spec.binding.name, &access, &spec.place).await?;
    let runner = place_runner(state, &sessions, &spec.place).await?;
    implementation.remove(&spec.home, &*runner).await?;
    let terminal_sessions = state.daemon.resource_backend().using::<flotilla_resources::TerminalSession>(namespace);
    for session in sessions {
        match terminal_sessions.delete(&session.metadata.name).await {
            Ok(_) | Err(ResourceError::NotFound { .. }) => {}
            Err(error) => return Err(format!("stop harness session `{}`: {error}", session.metadata.name)),
        }
    }
    Ok(())
}

/// Push a new material generation or epoch into the home (#3881 update mode push).
async fn push_generation(
    state: &Arc<ControllerRuntimeState>,
    namespace: &str,
    provision: &ResourceObject<AccountProvision>,
    binding: &ResourceObject<AccountBinding>,
    provisioners: &AccountProvisioners,
) -> Result<(), String> {
    let spec = &provision.spec;
    let Some(delivered) = &spec.delivered else { return Ok(()) };
    let store = state.credential_store.as_ref().ok_or("host-local credential store unavailable for account delivery")?;
    let (declaration, material) = store.account_material(&binding.spec.credential.0).await?;
    let generation = material_generation(&material);
    let epoch = declaration.authorization_epoch.to_string();
    if generation == delivered.generation && epoch == delivered.epoch {
        return Ok(());
    }
    let implementation = provisioners
        .for_implementation(&spec.plan.implementation)
        .ok_or_else(|| format!("no registered implementation `{}` to push its generation", spec.plan.implementation))?;
    let access = AccessName(implementation.access().to_string());
    let sessions = requester_sessions(state, namespace, &spec.binding.name, &access, &spec.place).await?;
    let runner = place_runner(state, &sessions, &spec.place).await?;
    implementation.deliver(&spec.home, &material, &*runner).await?;
    let mut updated = spec.clone();
    updated.delivered = Some(Delivered { epoch, generation, at: delivered_at(state, &binding.spec) });
    let meta = InputMeta::builder().name(provision.metadata.name.clone()).labels(provision.metadata.labels.clone()).build();
    state
        .daemon
        .resource_backend()
        .using::<AccountProvision>(namespace)
        .update(&meta, &provision.metadata.resource_version, &updated)
        .await
        .map(|_| ())
        .map_err(|error| format!("record pushed generation for `{}`: {error}", provision.metadata.name))
}

/// Grant and declaration events reach the convoys whose bindings use the credentials they name,
/// and the live convoys here still waiting for a binding.
struct CredentialConvoysWatch<K: flotilla_resources::Resource> {
    credentials: fn(&ResourceObject<K>) -> Vec<String>,
}

impl<K: flotilla_resources::Resource> Clone for CredentialConvoysWatch<K> {
    fn clone(&self) -> Self {
        Self { credentials: self.credentials }
    }
}

impl<K: flotilla_resources::Resource> CredentialConvoysWatch<K> {
    async fn enqueue(
        backend: &ResourceBackend,
        namespace: &str,
        sender: &WorkQueueSender,
        credentials: Vec<String>,
    ) -> Result<(), ResourceError> {
        let bindings = backend.clone().using::<AccountBinding>(namespace);
        let mut convoys = BTreeSet::new();
        for credential in credentials {
            let selector = BTreeMap::from([(ACCOUNT_CREDENTIAL_LABEL.to_string(), credential)]);
            for binding in bindings.list_matching_labels(&selector).await?.items {
                convoys.extend(flotilla_resources::label_value(&binding.metadata.labels, CONVOY_LABEL).cloned());
            }
        }
        let bound = bindings
            .list()
            .await?
            .items
            .into_iter()
            .filter_map(|binding| flotilla_resources::label_value(&binding.metadata.labels, CONVOY_LABEL).cloned())
            .collect::<BTreeSet<_>>();
        for convoy in backend.query_declared::<Convoy>(namespace, &flotilla_store::live_convoys_query()).await?.items {
            if matches!(convoy.provenance, ResourceProvenance::Local) && !bound.contains(&convoy.object.metadata.name) {
                convoys.insert(convoy.object.metadata.name);
            }
        }
        for convoy in convoys {
            sender.send(convoy).await.map_err(|_| ResourceError::other("controller queue closed while forwarding a credential event"))?;
        }
        Ok(())
    }
}

impl<K: flotilla_resources::Resource> SecondaryWatch for CredentialConvoysWatch<K> {
    type Primary = Convoy;

    fn clone_box(&self) -> Box<dyn SecondaryWatch<Primary = Convoy>> {
        Box::new(self.clone())
    }

    fn spawn(
        self: Box<Self>,
        backend: ResourceBackend,
        namespace: String,
        sender: WorkQueueSender,
    ) -> Pin<Box<dyn Future<Output = Result<(), ResourceError>> + Send>> {
        Box::pin(async move {
            let mut watch = backend.including_replicas::<K>(&namespace).watch().await?;
            while let Some(event) = watch.next().await {
                let source = match event? {
                    ReadWatchEvent::Added(source) | ReadWatchEvent::Modified(source) | ReadWatchEvent::Deleted(source) => source,
                    ReadWatchEvent::DeletedByName { .. } => continue,
                };
                Self::enqueue(&backend, &namespace, &sender, (self.credentials)(&source.object)).await?;
            }
            Ok(())
        })
    }
}

/// A binding's events, its home's or a replica's, reach this daemon's provisions of it.
#[derive(Clone)]
struct BindingProvisionsWatch;

impl SecondaryWatch for BindingProvisionsWatch {
    type Primary = AccountProvision;

    fn clone_box(&self) -> Box<dyn SecondaryWatch<Primary = AccountProvision>> {
        Box::new(self.clone())
    }

    fn spawn(
        self: Box<Self>,
        backend: ResourceBackend,
        namespace: String,
        sender: WorkQueueSender,
    ) -> Pin<Box<dyn Future<Output = Result<(), ResourceError>> + Send>> {
        Box::pin(async move {
            let provisions = backend.clone().using::<AccountProvision>(&namespace);
            let mut watch = backend.including_replicas::<AccountBinding>(&namespace).watch().await?;
            while let Some(event) = watch.next().await {
                let source = match event? {
                    ReadWatchEvent::Added(source) | ReadWatchEvent::Modified(source) | ReadWatchEvent::Deleted(source) => source,
                    ReadWatchEvent::DeletedByName { .. } => continue,
                };
                let selector = BTreeMap::from([(ACCOUNT_BINDING_LABEL.to_string(), source.object.metadata.name.clone())]);
                for provision in provisions.list_matching_labels(&selector).await?.items {
                    sender
                        .send(provision.metadata.name)
                        .await
                        .map_err(|_| ResourceError::other("controller queue closed while forwarding a binding event"))?;
                }
            }
            Ok(())
        })
    }
}

/// Spawn the binding controller (keyed by convoy) and the provision controller (keyed by
/// provision), each supervised like the resource controllers.
pub(super) fn spawn_account_controllers(
    state: Arc<ControllerRuntimeState>,
    namespace: String,
    resync_interval: Duration,
    supervision: ControllerSupervision,
    runtime_health: RuntimeHealth,
) -> Vec<JoinHandle<()>> {
    let bindings = {
        let state = Arc::clone(&state);
        let namespace = namespace.clone();
        let supervision = supervision.clone();
        let health = runtime_health.clone();
        tokio::spawn(async move {
            supervise_controller("AccountBinding", supervision, health.clone(), move || {
                let backend = state.daemon.resource_backend();
                let controller = ControllerLoop {
                    primary: backend.clone().using::<Convoy>(&namespace),
                    secondaries: vec![
                        Box::new(LabelMappedWatch::<AccountBinding, Convoy> { label_key: CONVOY_LABEL, _marker: PhantomData }),
                        Box::new(CredentialConvoysWatch::<CredentialGrant> {
                            credentials: |grant| grant.spec.credentials.iter().cloned().collect(),
                        }),
                        Box::new(CredentialConvoysWatch::<CredentialSpec> { credentials: |spec| vec![spec.metadata.name.clone()] }),
                    ],
                    reconciler: AccountBindingReconciler { daemon: Arc::clone(&state.daemon), namespace: namespace.clone() },
                    resync_interval,
                    backend,
                };
                run_observed(&state, "AccountBinding", controller, resync_interval, health.clone())
            })
            .await;
        })
    };
    let provisions = tokio::spawn(async move {
        let health = runtime_health.clone();
        supervise_controller("AccountProvision", supervision, runtime_health, move || {
            let backend = state.daemon.resource_backend();
            let controller = ControllerLoop {
                primary: backend.clone().using::<AccountProvision>(&namespace),
                secondaries: vec![Box::new(BindingProvisionsWatch)],
                reconciler: AccountProvisionReconciler { state: Arc::clone(&state), namespace: namespace.clone() },
                resync_interval,
                backend,
            };
            run_observed(&state, "AccountProvision", controller, resync_interval, health.clone())
        })
        .await;
    });
    vec![bindings, provisions]
}

fn run_observed<R: Reconciler>(
    state: &ControllerRuntimeState,
    name: &'static str,
    controller: ControllerLoop<R>,
    resync_interval: Duration,
    health: RuntimeHealth,
) -> impl Future<Output = Result<(), ResourceError>>
where
    <R::Resource as flotilla_resources::Resource>::Status: Default,
{
    let heartbeat = Arc::new(AtomicU64::new(0));
    let metrics = state.controller_metrics.for_kind(name);
    async move {
        let watchdog = spawn_controller_loop_watchdog(name, Arc::clone(&heartbeat), resync_interval, health.clone());
        let result = controller.run_observed(heartbeat, metrics).await;
        watchdog.abort();
        health.clear_controller_loop_stall(name);
        result
    }
}

/// Reconcile every provision here, as the provision controller would; tests drive it directly.
#[cfg(test)]
pub(super) async fn reconcile_place_provisions(state: &Arc<ControllerRuntimeState>, namespace: &str) -> Result<(), String> {
    let provisions = state.daemon.resource_backend().using::<AccountProvision>(namespace);
    for provision in provisions.list().await.map_err(|error| error.to_string())?.items {
        reconcile_provision(state, namespace, &provision).await?;
    }
    Ok(())
}
