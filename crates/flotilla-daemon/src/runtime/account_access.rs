//! Launch-time delivery of a requester's bound account (#3907).
//!
//! A crew whose item declares account access starts only in the explicit home its binding's
//! plan delivers: the vessel agent (or this daemon, on its own host) writes the bound
//! credential's material there, and the provision record notes what was delivered at this
//! place. A crew with no open binding is refused with the reason; nothing falls through to an
//! ambient login (#3907 contract 8).

use std::sync::Arc;

use flotilla_agent::account_access::{environment_kind, AccountProvisioners};
use flotilla_provider_runtime::CommandRunner;
use flotilla_resources::{
    account_access::{binding_name, material_generation, AccessName, AccountBinding, AccountProvision, AccountProvisionSpec, Delivered},
    Environment, InputMeta, ResourceError, ResourceProvenance,
};
use flotilla_schema::{Ref, Timestamp};

use super::state::ControllerRuntimeState;

/// What a launch needs to know about the requester and its place.
pub(super) struct AccountLaunch<'a> {
    pub namespace: &'a str,
    pub convoy: &'a str,
    pub role: &'a str,
    pub env_ref: &'a str,
    pub agentless: bool,
}

/// The candidate environment kind a launch's place is, in the vocabulary plans resolve against.
pub(super) async fn place_environment_kind(state: &ControllerRuntimeState, launch: &AccountLaunch<'_>) -> Result<&'static str, String> {
    if launch.agentless {
        return Ok(environment_kind::AGENTLESS_SSH);
    }
    if launch.env_ref == state.host_direct_environment_name {
        return Ok(environment_kind::HOST_DIRECT);
    }
    let environment = state.daemon.resource_backend().including_replicas::<Environment>(launch.namespace).get(launch.env_ref).await;
    match environment {
        Ok(environment) if environment.object.spec.cloud_sandbox.is_some() => Ok(environment_kind::CLOUD_SANDBOX),
        Ok(environment) if environment.object.spec.host_direct.is_some() => Ok(environment_kind::HOST_DIRECT),
        Ok(environment) if environment.object.spec.docker.is_some() => Ok(environment_kind::DOCKER),
        // A shape this code does not know gets no plan: it never plans as Docker by default.
        Ok(_) => Err(format!("environment `{}` has a shape no account plan knows", launch.env_ref)),
        // Agentless and host-direct places are recognised above; a provisioned vessel whose
        // Environment record is not readable here is treated as a container vessel.
        Err(ResourceError::NotFound { .. }) => Ok(environment_kind::DOCKER),
        Err(error) => Err(error.to_string()),
    }
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
    let credential = &binding.object.spec.credential.0;
    let (declaration, material) = store.account_material(credential).await?;
    let home = provisioner.home(&store.writable_base(&*runner).await?, launch.convoy, launch.role);
    let record = provision_name(&name, launch.env_ref);
    let mut provision = AccountProvisionSpec {
        binding: Ref::new(name.clone(), binding_home),
        place: launch.env_ref.to_string(),
        environment_kind: kind.to_string(),
        plan,
        home: home.clone(),
        delivered: None,
    };
    // Record the place before writing material there, so a copy is never left without a
    // record the place pass would withdraw.
    record_provision(state, launch.namespace, &record, &provision).await?;
    let env = provisioner.deliver(&home, &material, &*runner).await?;
    provision.delivered = Some(Delivered {
        epoch: declaration.authorization_epoch.to_string(),
        generation: material_generation(&material),
        at: delivered_at(&binding.object.spec),
    });
    record_provision(state, launch.namespace, &record, &provision).await?;
    Ok(env)
}

/// When a delivery under `binding` happened, for the place pass's "delivered before the binding
/// opened" check. Never earlier than the binding's own opening, so clock skew between the
/// binding's home and this place can never make a fresh copy look stale. A rebind is noticed
/// once its opening, on the home's clock, passes the last delivery's recorded time; a home
/// clock behind this place's delays that, and a new account's material is still pushed as a
/// new generation meanwhile.
fn delivered_at(binding: &flotilla_resources::AccountBindingSpec) -> Timestamp {
    Timestamp::from_datetime(&chrono::Utc::now()).max(binding.opened_at)
}

/// One provision record per (binding, place).
pub(super) fn provision_name(binding: &str, place: &str) -> String {
    format!("{binding}.{place}")
}

async fn record_provision(state: &ControllerRuntimeState, namespace: &str, name: &str, spec: &AccountProvisionSpec) -> Result<(), String> {
    let provisions = state.daemon.resource_backend().using::<AccountProvision>(namespace);
    match provisions.get(name).await {
        Ok(existing) if &existing.spec == spec => Ok(()),
        Ok(existing) => provisions
            .update(&InputMeta::builder().name(name.to_string()).build(), &existing.metadata.resource_version, spec)
            .await
            .map(|_| ())
            .map_err(|error| format!("record account provision `{name}`: {error}")),
        Err(ResourceError::NotFound { .. }) => provisions
            .create(&InputMeta::builder().name(name.to_string()).build(), spec)
            .await
            .map(|_| ())
            .map_err(|error| format!("record account provision `{name}`: {error}")),
        Err(error) => Err(format!("read account provision `{name}`: {error}")),
    }
}

/// One pass over account bindings and this daemon's provisions: the home pass (#3882
/// lifecycle) on bindings this daemon holds, then the place pass on provisions it delivered.
pub(super) async fn reconcile_account_access(state: &Arc<ControllerRuntimeState>, namespace: &str) -> Result<(), String> {
    for (binding, change) in state.daemon.reconcile_account_bindings(namespace).await? {
        tracing::info!(%binding, ?change, "account binding changed");
    }
    reconcile_place_provisions(state, namespace).await
}

/// The place pass: every provision this daemon delivered is checked against its binding.
/// - A binding revoked, closed or reopened by a rebind since the delivery: the delivered copy
///   is deleted, the requester's harness session is stopped (its TerminalSession is deleted, so
///   a rebound requester starts afresh and a revoked one waits), and the record is removed.
/// - A place whose environment is gone: the record is removed; its copies went with it.
/// - Otherwise a new material generation or epoch is pushed into the home (update mode push).
pub(super) async fn reconcile_place_provisions(state: &Arc<ControllerRuntimeState>, namespace: &str) -> Result<(), String> {
    let backend = state.daemon.resource_backend();
    let provisions = backend.clone().using::<AccountProvision>(namespace);
    let provisioners = AccountProvisioners::seeded();
    for provision in provisions.list().await.map_err(|error| format!("list account provisions: {error}"))?.items {
        let name = provision.metadata.name.clone();
        let spec = &provision.spec;
        if spec.place != state.host_direct_environment_name
            && matches!(backend.including_replicas::<Environment>(namespace).get(&spec.place).await, Err(ResourceError::NotFound { .. }))
        {
            delete_provision(&provisions, &name).await?;
            continue;
        }
        let binding = match backend.including_replicas::<AccountBinding>(namespace).get(&spec.binding.name).await {
            Ok(binding) => Some(binding.object),
            Err(ResourceError::NotFound { .. }) => None,
            Err(error) => return Err(format!("read account binding `{}`: {error}", spec.binding.name)),
        };
        let current = binding.as_ref().filter(|binding| {
            binding.spec.state.value.is_open() && spec.delivered.as_ref().is_none_or(|delivered| delivered.at >= binding.spec.opened_at)
        });
        let Some(binding) = current else {
            if let Err(error) = withdraw(state, namespace, spec, &provisioners).await {
                tracing::warn!(%error, provision = %name, "failed to withdraw a delivered account copy; will retry");
                continue;
            }
            delete_provision(&provisions, &name).await?;
            continue;
        };
        if let Err(error) = push_generation(state, namespace, &provision, binding, &provisioners).await {
            tracing::warn!(%error, provision = %name, "failed to push a new account material generation; will retry");
        }
    }
    Ok(())
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
) -> Result<Vec<flotilla_resources::ResourceObject<flotilla_resources::TerminalSession>>, String> {
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
    sessions: &[flotilla_resources::ResourceObject<flotilla_resources::TerminalSession>],
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
    provision: &flotilla_resources::ResourceObject<AccountProvision>,
    binding: &flotilla_resources::ResourceObject<AccountBinding>,
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
    updated.delivered = Some(Delivered { epoch, generation, at: delivered_at(&binding.spec) });
    state
        .daemon
        .resource_backend()
        .using::<AccountProvision>(namespace)
        .update(&InputMeta::builder().name(provision.metadata.name.clone()).build(), &provision.metadata.resource_version, &updated)
        .await
        .map(|_| ())
        .map_err(|error| format!("record pushed generation for `{}`: {error}", provision.metadata.name))
}

/// Run both passes on every change to grants, declarations or bindings, and on a resync
/// (which also catches requester teardown): withdrawing a grant revokes at once.
pub(super) fn spawn_account_access_task(
    state: Arc<ControllerRuntimeState>,
    namespace: String,
    interval: std::time::Duration,
) -> tokio::task::JoinHandle<()> {
    use futures::{FutureExt, StreamExt};
    tokio::spawn(async move {
        let backend = state.daemon.resource_backend();
        let watches = || async {
            let mut streams = Vec::new();
            for kind in ["CredentialGrant", "CredentialSpec", "AccountBinding"] {
                match flotilla_store::watch_resource_kind_including_replicas(&backend, &namespace, kind).await {
                    Ok(watch) => streams.push(watch.stream),
                    Err(error) => tracing::warn!(%kind, %error, "could not watch an account binding dependency"),
                }
            }
            futures::stream::select_all(streams)
        };
        let mut events = watches().await;
        let mut resync = tokio::time::interval(interval);
        resync.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let mut restart = events.is_empty();
            tokio::select! {
                _ = resync.tick() => {}
                event = events.next(), if !events.is_empty() => {
                    restart |= !matches!(event, Some(Ok(_)));
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    while let Some(Some(event)) = events.next().now_or_never() {
                        restart |= event.is_err();
                    }
                }
            }
            if restart {
                events = watches().await;
            }
            if let Err(error) = reconcile_account_access(&state, &namespace).await {
                tracing::warn!(%error, "failed to reconcile account bindings");
            }
        }
    })
}
