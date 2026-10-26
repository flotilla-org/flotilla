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
//!   A withdrawal that fails is retried with backoff.
//!
//! Both read and write the account kinds through [`AccountRecords`]: the old store's with
//! `[primitives]` off, the engine's with it on (#3824 layer 1), and are woken by its changes.

use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use chrono::{DateTime, Utc};
use flotilla_agent::account_access::{AccountProvisioners, EnvironmentKind};
use flotilla_core::in_process::InProcessDaemon;
use flotilla_provider_runtime::CommandRunner;
use flotilla_resources::{
    account_access::{
        binding_name, material_generation, AccessName, AccountBindingSpec, AccountChange, AccountProvisionSpec, AccountRecords,
        BindingRecord, Delivered, ProvisionRecord,
    },
    Convoy, CredentialGrant, CredentialSpec, Environment, ReadWatchEvent, ResourceError, ResourceObject, ResourceProvenance,
    TerminalSession, TerminalSessionSource,
};
use flotilla_schema::{Ref, Timestamp};
use flotilla_store::{
    controller::{ReconcileOutcome, Reconciler, SecondaryWatch, WorkQueueSender},
    ResourceBackend,
};
use futures::StreamExt;

use super::state::ControllerRuntimeState;

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
    let accounts = state.daemon.account_records();
    let Some(binding) = accounts.requester_binding(launch.namespace, launch.convoy, launch.role, access).await? else {
        return Err(format!(
            "crew `{}` has no account binding for `{access}`: its harness home is never left to an ambient login",
            launch.role
        ));
    };
    let name = binding.name.clone();
    // The provision controller cannot withdraw this binding's copy here while it is delivered.
    let lock = state.account_delivery_lock(&name);
    let _delivering = lock.lock().await;
    if let Some(reason) = binding.spec.state.value.reason() {
        return Err(format!("crew `{}` waits: {reason}; `flotilla convoy rebind` opens a new binding", launch.role));
    }
    let provisioners = AccountProvisioners::seeded();
    let provisioner = provisioners.for_access(access).ok_or_else(|| format!("no registered implementation supplies `{access}`"))?;
    let kind = place_environment_kind(state, launch).await?;
    // A replaced vessel on a kind with no plan refuses the requester there; the binding never
    // switches account to fit (#3882).
    let plan = provisioner
        .plan(kind)
        .map_err(|reason| format!("account binding `{name}` cannot be provisioned at `{}`: {reason}", launch.env_ref))?;
    let store = state.credential_store.as_ref().ok_or("host-local credential store unavailable for account delivery")?;
    let (declaration, material) = store.account_material(&binding.spec.credential.0).await?;
    let home = provisioner.home(&store.writable_base(&*runner).await?, launch.convoy, launch.role);
    let record = provision_name(&name, launch.env_ref);
    let mut provision = AccountProvisionSpec {
        binding: Ref::new(name.clone(), binding.home.clone()),
        place: launch.env_ref.to_string(),
        environment_kind: kind.as_str().to_string(),
        plan,
        home: home.clone(),
        delivered: None,
    };
    // Record the place before writing material there, so a copy is never left without a
    // record the provision controller would withdraw. A relaunch keeps its existing record,
    // and with it the previous delivery, until this one succeeds.
    accounts.record_provision(launch.namespace, &record, launch.convoy, &provision, false).await?;
    let env = provisioner.deliver(&home, &material, &*runner).await?;
    provision.delivered = Some(Delivered {
        epoch: declaration.authorization_epoch.to_string(),
        generation: material_generation(&material),
        at: delivered_at(state, &binding.spec),
    });
    accounts.record_provision(launch.namespace, &record, launch.convoy, &provision, true).await?;
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

/// Reconciles one convoy's account bindings on the convoy's home.
pub(super) struct AccountBindingReconciler {
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
async fn reconcile_provision(state: &Arc<ControllerRuntimeState>, namespace: &str, name: &str) -> Result<(), String> {
    let accounts = state.daemon.account_records();
    // Read under the binding's lock: a launch may record a new delivery meanwhile.
    let Some(unlocked) = accounts.provision(namespace, name).await? else { return Ok(()) };
    let lock = state.account_delivery_lock(&unlocked.spec.binding.name);
    let _withdrawing = lock.lock().await;
    let Some(provision) = accounts.provision(namespace, name).await? else { return Ok(()) };
    let spec = &provision.spec;
    if spec.place != state.host_direct_environment_name
        && matches!(
            state.daemon.resource_backend().including_replicas::<Environment>(namespace).get(&spec.place).await,
            Err(ResourceError::NotFound { .. })
        )
    {
        return accounts.delete_provision(namespace, name).await;
    }
    let binding = accounts.binding(namespace, &spec.binding.name).await?;
    let provisioners = AccountProvisioners::seeded();
    match binding {
        Some(binding) if binding.spec.state.value.is_open() => {
            let superseded = spec.delivered.as_ref().is_some_and(|delivered| delivered.at < binding.spec.opened_at);
            if !superseded {
                return push_generation(state, namespace, &provision, &binding, &provisioners).await;
            }
            withdraw(state, namespace, spec, &provisioners).await?;
            let mut cleared = spec.clone();
            cleared.delivered = None;
            accounts.update_provision(namespace, &provision, &cleared).await.map(drop)
        }
        _ => {
            withdraw(state, namespace, spec, &provisioners).await?;
            accounts.delete_provision(namespace, name).await
        }
    }
}

/// The requester's crew sessions at a place: those whose convoy, role and access name this
/// binding. Matching by the derived name, not by parsing it, holds whatever the names contain.
async fn requester_sessions(
    state: &ControllerRuntimeState,
    namespace: &str,
    binding: &BindingRecord,
    access: &AccessName,
    place: &str,
) -> Result<Vec<ResourceObject<TerminalSession>>, String> {
    let Some((convoy, role)) = flotilla_resources::account_access::requester_parts(&binding.spec.requester) else { return Ok(Vec::new()) };
    let sessions = state
        .daemon
        .resource_backend()
        .using::<TerminalSession>(namespace)
        .list()
        .await
        .map_err(|error| format!("list terminal sessions: {error}"))?;
    let requester = binding_name(&convoy, &role, access);
    Ok(sessions
        .items
        .into_iter()
        .filter(|session| {
            session.spec.env_ref == place
                && matches!(
                    &session.spec.source,
                    TerminalSessionSource::Agent { context, .. }
                        if binding_name(&context.convoy, &session.spec.role, access) == requester
                )
        })
        .collect())
}

/// A runner that reaches the requester's home at a place.
async fn place_runner(
    state: &Arc<ControllerRuntimeState>,
    sessions: &[ResourceObject<TerminalSession>],
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
    let sessions = match state.daemon.account_records().binding(namespace, &spec.binding.name).await? {
        Some(binding) => requester_sessions(state, namespace, &binding, &access, &spec.place).await?,
        None => Vec::new(),
    };
    let runner = place_runner(state, &sessions, &spec.place).await?;
    implementation.remove(&spec.home, &*runner).await?;
    let terminal_sessions = state.daemon.resource_backend().using::<TerminalSession>(namespace);
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
    provision: &ProvisionRecord,
    binding: &BindingRecord,
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
    let sessions = requester_sessions(state, namespace, binding, &access, &spec.place).await?;
    let runner = place_runner(state, &sessions, &spec.place).await?;
    implementation.deliver(&spec.home, &material, &*runner).await?;
    let mut updated = spec.clone();
    updated.delivered = Some(Delivered { epoch, generation, at: delivered_at(state, &binding.spec) });
    state
        .daemon
        .account_records()
        .update_provision(namespace, provision, &updated)
        .await
        .map(drop)
        .map_err(|error| format!("record pushed generation for `{}`: {error}", provision.name))
}

/// Grant and declaration events reach the convoys whose bindings use the credentials they name,
/// and the live convoys here still waiting for a binding.
struct CredentialConvoysWatch<K: flotilla_resources::Resource> {
    accounts: Arc<dyn AccountRecords>,
    credentials: fn(&ResourceObject<K>) -> Vec<String>,
    /// The credentials a deletion known only by name names, when the name alone says.
    deleted: fn(&str) -> Option<Vec<String>>,
}

impl<K: flotilla_resources::Resource> Clone for CredentialConvoysWatch<K> {
    fn clone(&self) -> Self {
        Self { accounts: Arc::clone(&self.accounts), credentials: self.credentials, deleted: self.deleted }
    }
}

impl<K: flotilla_resources::Resource> CredentialConvoysWatch<K> {
    /// `credentials: None` (a grant deleted by name) reaches every convoy with a binding here.
    async fn enqueue(
        &self,
        backend: &ResourceBackend,
        namespace: &str,
        sender: &WorkQueueSender,
        credentials: Option<Vec<String>>,
    ) -> Result<(), ResourceError> {
        let (mut convoys, bound) =
            self.accounts.bound_convoys(namespace, credentials.as_deref().unwrap_or_default()).await.map_err(ResourceError::other)?;
        if credentials.is_none() {
            convoys.extend(bound.iter().cloned());
        }
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
                let credentials = match event? {
                    ReadWatchEvent::Added(source) | ReadWatchEvent::Modified(source) | ReadWatchEvent::Deleted(source) => {
                        Some((self.credentials)(&source.object))
                    }
                    ReadWatchEvent::DeletedByName { tombstone, .. } => (self.deleted)(&tombstone.name),
                };
                self.enqueue(&backend, &namespace, &sender, credentials).await?;
            }
            Ok(())
        })
    }
}

/// A binding's or provision's change, from its home or a replica, reaches its convoy.
#[derive(Clone)]
struct AccountConvoysWatch {
    accounts: Arc<dyn AccountRecords>,
}

impl SecondaryWatch for AccountConvoysWatch {
    type Primary = Convoy;

    fn clone_box(&self) -> Box<dyn SecondaryWatch<Primary = Convoy>> {
        Box::new(self.clone())
    }

    fn spawn(
        self: Box<Self>,
        _backend: ResourceBackend,
        namespace: String,
        sender: WorkQueueSender,
    ) -> Pin<Box<dyn Future<Output = Result<(), ResourceError>> + Send>> {
        Box::pin(async move {
            let mut changes = self.accounts.changes(&namespace).await.map_err(ResourceError::other)?;
            while let Some(change) = changes.next().await {
                let convoy = match change.map_err(ResourceError::other)? {
                    AccountChange::Binding { convoy, .. } | AccountChange::Provision { convoy, .. } => convoy,
                };
                if let Some(convoy) = convoy {
                    sender
                        .send(convoy)
                        .await
                        .map_err(|_| ResourceError::other("controller queue closed while forwarding an account event"))?;
                }
            }
            Ok(())
        })
    }
}

/// The binding controller's parts: keyed by convoy, on the convoy's home.
pub(super) fn binding_controller(
    daemon: Arc<InProcessDaemon>,
    namespace: String,
) -> (Vec<Box<dyn SecondaryWatch<Primary = Convoy>>>, AccountBindingReconciler) {
    let accounts = daemon.account_records();
    (
        vec![
            Box::new(AccountConvoysWatch { accounts: Arc::clone(&accounts) }),
            Box::new(CredentialConvoysWatch::<CredentialGrant> {
                accounts: Arc::clone(&accounts),
                credentials: |grant| grant.spec.credentials.iter().cloned().collect(),
                deleted: |_| None,
            }),
            Box::new(CredentialConvoysWatch::<CredentialSpec> {
                accounts,
                credentials: |spec| vec![spec.metadata.name.clone()],
                deleted: |name| Some(vec![name.to_string()]),
            }),
        ],
        AccountBindingReconciler { daemon, namespace },
    )
}

/// The first retry delay of a provision whose reconcile failed, doubling to [`RETRY_CAP`].
const RETRY_BASE: Duration = Duration::from_secs(1);
const RETRY_CAP: Duration = Duration::from_secs(60);

/// The provision controller: reconciles each provision at this place when it or its binding
/// changes, every provision once a `resync` and at start, and a failed one again with backoff.
/// A failure to read the changes ends the run, for the supervisor to restart it.
pub(super) async fn run_provision_controller(
    state: Arc<ControllerRuntimeState>,
    namespace: String,
    resync: Duration,
) -> Result<(), ResourceError> {
    let accounts = state.daemon.account_records();
    let mut changes = accounts.changes(&namespace).await.map_err(ResourceError::other)?;
    let mut retries: BTreeMap<String, (u32, tokio::time::Instant)> = BTreeMap::new();
    let mut due: BTreeSet<String> = BTreeSet::new();
    let mut resync_at = tokio::time::Instant::now();
    loop {
        let now = tokio::time::Instant::now();
        if now >= resync_at {
            due.extend(
                accounts.local_provisions(&namespace).await.map_err(ResourceError::other)?.into_iter().map(|provision| provision.name),
            );
            resync_at = now + resync;
        }
        due.extend(retries.iter().filter(|(_, (_, at))| *at <= now).map(|(name, _)| name.clone()));
        for name in std::mem::take(&mut due) {
            match reconcile_provision(&state, &namespace, &name).await {
                Ok(()) => {
                    retries.remove(&name);
                }
                Err(error) => {
                    let failures = retries.get(&name).map_or(0, |(failures, _)| *failures) + 1;
                    let delay = RETRY_BASE.saturating_mul(1 << failures.min(6)).min(RETRY_CAP);
                    tracing::warn!(provision = %name, %error, failures, "account provision reconcile failed; retrying");
                    retries.insert(name, (failures, tokio::time::Instant::now() + delay));
                }
            }
        }
        let wake = retries.values().map(|(_, at)| *at).chain([resync_at]).min().unwrap_or(resync_at);
        tokio::select! {
            change = changes.next() => match change {
                Some(Ok(AccountChange::Provision { name, .. })) => {
                    due.insert(name);
                }
                Some(Ok(AccountChange::Binding { name, .. })) => {
                    due.extend(
                        accounts.binding_provisions(&namespace, &[name], true).await.map_err(ResourceError::other)?.into_iter().map(|provision| provision.name),
                    );
                }
                Some(Err(error)) => return Err(ResourceError::other(error)),
                None => return Err(ResourceError::other("account changes closed")),
            },
            () = tokio::time::sleep_until(wake) => {}
        }
    }
}

/// Reconcile every provision here, as the provision controller would; tests drive it directly.
#[cfg(test)]
pub(super) async fn reconcile_place_provisions(state: &Arc<ControllerRuntimeState>, namespace: &str) -> Result<(), String> {
    for provision in state.daemon.account_records().local_provisions(namespace).await? {
        reconcile_provision(state, namespace, &provision.name).await?;
    }
    Ok(())
}
