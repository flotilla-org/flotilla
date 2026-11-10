//! The Checkout controller's host work (#3108): the daemon's [`CheckoutProvider`], making,
//! inspecting and removing checkouts in this host's environments through the routing checkout
//! runtime; its [`HolderInputs`], read from the holder Convoy as the old checkout reconciler
//! read them at a checkout's creation; and this host's checkout inspector, which answers gates'
//! inspection requests.

use std::sync::Arc;

use async_trait::async_trait;
use flotilla_address::{Address, Kind as AddressKind};
use flotilla_core::in_process::InProcessDaemon;
use flotilla_engine_messages::roles;
use flotilla_inbox::Pending;
use flotilla_projections::ports::LifecycleReads;
use flotilla_protocol::RepositoryKey;
use flotilla_provisioning::{
    checkout_runtime::{CheckoutRemoval, CheckoutRemovalOutcome, PreparedCheckout},
    engine::checkout_actuator::{Against, BranchValidation, CheckoutActuator, CheckoutInputs, CheckoutProvider, FreshClone, HolderInputs},
};
use flotilla_resources::{
    checkout_integration::convoy_change_request_id,
    checkout_records::{CheckoutHolder, Inspection},
    CheckoutSpec, CloneCredentials, Convoy, Forge, LandedEvidence, ResourceError, ResourceObject, Vessel,
};
use flotilla_state_machines::machines::message::{MessageBody, RequestBody};
use flotilla_store::ResourceBackend;
use flotilla_vcs::{
    checkout_integration::RecordedAssociation,
    vcs::{CheckoutMaterialisationError, LocalCloneRequest},
};

use futures::StreamExt;
use tracing::{debug, error, info, warn};

use super::{
    ports::{finish_clone_delivery, unauthorised_clone, RoutingCheckoutRuntime},
    role_consumer::{Handled, RoleRequests},
};

/// This host's checkout work for the engine's Checkout controller.
pub(super) struct EngineCheckoutProvider {
    pub(super) routing: RoutingCheckoutRuntime,
}

#[async_trait]
impl CheckoutProvider for EngineCheckoutProvider {
    async fn path_exists(&self, environment: &str, path: &str) -> Result<Option<bool>, String> {
        self.routing.checkout_path_exists_in(environment, path).await
    }

    async fn create_worktree(
        &self,
        environment: &str,
        clone_path: &str,
        branch: &str,
        base: Option<&str>,
        target: &str,
        reason: &str,
    ) -> Result<PreparedCheckout, CheckoutMaterialisationError> {
        self.routing.create_worktree_in(environment, clone_path, branch, base, target, reason).await
    }

    async fn continue_worktree(
        &self,
        environment: &str,
        clone_path: &str,
        branch: &str,
        target: &str,
        reason: &str,
    ) -> Result<PreparedCheckout, CheckoutMaterialisationError> {
        let vcs = self.routing.runtime_for(environment, clone_path).await?.vcs(clone_path)?;
        let continued = vcs.continue_checkout(branch, target, reason).await?;
        Ok(PreparedCheckout { commit: continued.commit, branch_provenance: continued.provenance })
    }

    async fn create_fresh_clone(&self, environment: &str, clone: FreshClone<'_>) -> Result<PreparedCheckout, String> {
        if !clone.continuing {
            return self
                .routing
                .create_fresh_clone_in(environment, clone.url, clone.branch, clone.base, clone.target, clone.credentials)
                .await;
        }
        let (vcs, delivery) = self.routing.fresh_clone_vcs(environment, clone.url, clone.target, clone.credentials).await?;
        let continued = vcs.continue_fresh_clone(clone.url, clone.branch, clone.target).await;
        finish_clone_delivery(delivery, clone.target).await;
        let continued = continued.map_err(|error| unauthorised_clone(clone.url, clone.credentials, error))?;
        Ok(PreparedCheckout { commit: continued.commit, branch_provenance: continued.provenance })
    }

    async fn create_local_clone(
        &self,
        environment: &str,
        request: LocalCloneRequest<'_>,
    ) -> Result<(PreparedCheckout, Option<String>), String> {
        self.routing.create_local_clone_in(environment, request).await
    }

    async fn inspect(&self, environment: &str, path: &str, spec: &CheckoutSpec, against: Against<'_>) -> Result<Inspection, String> {
        let association = RecordedAssociation {
            change_request_id: against.change_request,
            complete: against.change_request_searched || against.change_request.is_some(),
            observe_remote_ref: against.remote,
        };
        let held = against.held.and_then(|held| held.landed_evidence.as_ref()).map(|landed| LandedEvidence {
            change_request_id: landed.change_request.to_string(),
            merged_at: landed.merged_at.map(|at| at.to_rfc3339()),
            target_ref: landed.target_ref.clone(),
            checkout_head_in_merged_head: landed.head_in_merged_head,
        });
        let runtime = self.routing.runtime_for(environment, path).await?;
        let status = runtime.inspect_at(path, spec, against.repository_url, association, held.as_ref()).await?;
        Ok(Inspection::of_status(&status))
    }

    async fn remove(&self, environment: &str, removal: &CheckoutRemoval) -> Result<CheckoutRemovalOutcome, String> {
        self.routing.remove_checkout_in(environment, removal).await
    }

    async fn protect(&self, environment: &str, clone_path: &str, target: &str, reason: &str) -> Result<(), String> {
        self.routing.protect_worktree_in(environment, clone_path, target, reason).await
    }
}

/// A new branch is validated against the change requests this host's forge observer finds from
/// the repository's own branch of that name (#3108); a fork's branch of the same name never counts.
pub(super) struct ForgeBranchValidation {
    pub(super) daemon: Arc<InProcessDaemon>,
}

#[async_trait]
impl BranchValidation for ForgeBranchValidation {
    async fn validate_new_branch(&self, repository: &str, branch: &str, continuing: bool) -> Result<Option<String>, String> {
        self.daemon.validate_new_branch(&RepositoryKey(repository.to_owned()), branch, continuing).await.map_err(|error| error.to_string())
    }
}

/// A checkout's inputs from its holder Convoy's old record: whether it continues the
/// repository's branch, the repository's URL, the change request bound to or found for its
/// branch, and, for a fresh clone, the credential grants of the vessel whose environment it is
/// cloned into, from the convoy's frozen requirement.
pub(super) struct ConvoyHolderInputs {
    pub(super) backend: ResourceBackend,
    pub(super) namespace: String,
    pub(super) lifecycle_reads: Arc<dyn LifecycleReads>,
}

impl ConvoyHolderInputs {
    /// The grants of the vessel whose environment `environment` is, that cover `spec`'s repository.
    async fn credentials(
        &self,
        convoy: &ResourceObject<Convoy>,
        environment: &str,
        spec: &CheckoutSpec,
    ) -> Result<CloneCredentials, String> {
        let reads = Arc::clone(&self.lifecycle_reads);
        let name = environment.to_owned();
        let record = tokio::task::spawn_blocking(move || reads.environments()?.get(&name, None))
            .await
            .map_err(|error| format!("read environment {environment}: {error}"))??;
        let Some(vessel) = record.and_then(|record| record.vessel) else { return Ok(CloneCredentials::default()) };
        let vessel =
            self.backend.using::<Vessel>(&self.namespace).get(&vessel).await.map_err(|error| format!("read vessel {vessel}: {error}"))?;
        let Some(requirement) = convoy
            .status
            .as_ref()
            .and_then(|status| status.workflow_snapshot.as_ref())
            .and_then(|snapshot| snapshot.vessels.iter().find(|requirement| requirement.name == vessel.spec.vessel_name))
        else {
            return Err(format!("vessel {} missing from convoy {}'s snapshot", vessel.spec.vessel_name, convoy.metadata.name));
        };
        Ok(CloneCredentials::for_repository(
            spec.repo_ref(),
            &requirement.credential_refs,
            &requirement.credential_scopes,
            &requirement.credential_permissions,
        ))
    }
}

#[async_trait]
impl HolderInputs for ConvoyHolderInputs {
    async fn inputs(&self, holder: Option<&CheckoutHolder>, spec: &CheckoutSpec) -> Result<CheckoutInputs, String> {
        let Some(CheckoutHolder::Convoy(launch)) = holder else { return Ok(CheckoutInputs::default()) };
        // The holder is a launch (`<launch_of>#<n>`); a finite convoy's launch_of is its record's name.
        let name = flotilla_resources::checkout_records::launched(launch);
        let owner = Some(name.to_owned());
        let convoy = match self.backend.including_replicas::<Convoy>(&self.namespace).get(name).await {
            Ok(convoy) => convoy.object,
            // A holder with no record supplies nothing but its name.
            Err(ResourceError::NotFound { .. }) => return Ok(CheckoutInputs { owner, ..CheckoutInputs::default() }),
            Err(error) => return Err(format!("read convoy {name}: {error}")),
        };
        let forges: Vec<_> = self
            .backend
            .definitions::<Forge>(&self.namespace)
            .list()
            .await
            .map_err(|error| format!("read forges: {error}"))?
            .into_iter()
            .map(|forge| forge.spec)
            .collect();
        let repo_ref = spec.repo_ref();
        let credentials = match spec {
            CheckoutSpec::FreshClone(fresh) => self.credentials(&convoy, &fresh.env_ref, spec).await?,
            CheckoutSpec::Worktree(_) | CheckoutSpec::Observed(_) => CloneCredentials::default(),
        };
        Ok(CheckoutInputs {
            continuing: convoy.spec.continuation.as_ref() == Some(repo_ref),
            credentials,
            owner,
            repository_url: convoy
                .spec
                .repositories
                .iter()
                .find(|repository| repository.repo_ref == *repo_ref)
                .map(|repository| repository.url.clone()),
            change_request: convoy_change_request_id(&convoy, repo_ref, &forges),
            change_request_searched: convoy.status.as_ref().is_some_and(|status| status.branch_subject_scan_at.is_some()),
        })
    }
}

/// Rechecks this home's checkout launches as the observed-checkout refresh sees their paths change
/// (#3108): a checkout observed at a launch's path, or no longer observed there, names that launch
/// to the Checkout controller, which observes it again. Its wake is the observed store's watch;
/// a watch opened again after a failure rechecks every launch, since changes in the gap are unseen.
pub(super) async fn recheck_on_observed_refresh(state: Arc<super::state::ControllerRuntimeState>) {
    let namespace = state.namespace.clone();
    let observed = state.daemon.observed_resource_backend().using::<flotilla_resources::Checkout>(&namespace);
    let mut reopened = false;
    loop {
        let mut watch = match observed.watch(flotilla_resources::WatchStart::Now).await {
            Ok(watch) => watch,
            Err(error) => {
                error!(%error, "checkout rechecks stopped: the observed checkouts cannot be watched");
                return;
            }
        };
        if reopened && !recheck_launches(&state, None).await {
            return;
        }
        reopened = true;
        while let Some(event) = watch.next().await {
            let path = match event {
                Ok(
                    flotilla_resources::WatchEvent::Added(checkout)
                    | flotilla_resources::WatchEvent::Modified(checkout)
                    | flotilla_resources::WatchEvent::Deleted(checkout),
                ) => flotilla_resources::checkout_path::checkout_path(&checkout).map(str::to_owned),
                // A deletion by name carries no path: every launch here is observed again.
                Ok(flotilla_resources::WatchEvent::DeletedByName(_)) => None,
                Err(error) => {
                    warn!(%error, "the observed checkout watch failed; it is opened again");
                    break;
                }
            };
            if !recheck_launches(&state, path.as_deref()).await {
                return;
            }
        }
    }
}

/// Names this home's ready launches at `path` (every ready launch when `None`) to the Checkout
/// controller; false once the controller has stopped.
async fn recheck_launches(state: &super::state::ControllerRuntimeState, path: Option<&str>) -> bool {
    let reads = state.daemon.lifecycle_reads();
    let here =
        match tokio::task::spawn_blocking(move || reads.checkouts()?.here()).await.map_err(|error| error.to_string()).and_then(|here| here)
        {
            Ok(here) => here,
            Err(error) => {
                warn!(%error, "checkouts here could not be read for a recheck");
                return true;
            }
        };
    let launches = flotilla_resources::checkout_records::latest_launches(here)
        .into_values()
        .filter(|checkout| checkout.is_ready() && path.is_none_or(|path| checkout.ready_path() == Some(path)))
        .map(|checkout| checkout.name);
    for launch in launches {
        if state.checkout_rechecks.send(launch).await.is_err() {
            return false;
        }
    }
    true
}

/// What a gate's inspection request asks of this host: checkout `checkout` inspected afresh, its
/// branch's remote ref read when `remote`, and the inspection recorded on its latest launch. The
/// launch inspected, or `None` when no launch of it is realised here.
#[async_trait]
pub(super) trait GateInspection: Send + Sync {
    async fn inspect(&self, checkout: &str, remote: bool) -> Result<Option<String>, String>;
}

#[async_trait]
impl<P: CheckoutProvider> GateInspection for CheckoutActuator<P> {
    async fn inspect(&self, checkout: &str, remote: bool) -> Result<Option<String>, String> {
        self.inspect_for_gate(checkout, remote).await
    }
}

/// This host's checkout inspector (#3108), served on its role's inbox
/// (`host:<h>/role:checkout-inspector`): the subject checkout (`host:<h>/checkout:<name>`) is
/// inspected and recorded, then the request takes the inspector's receipt, which the gate reads as
/// its answer.
pub(super) struct CheckoutInspector {
    pub(super) inspection: Arc<dyn GateInspection>,
}

#[async_trait]
impl RoleRequests for CheckoutInspector {
    fn role(&self) -> &'static str {
        roles::CHECKOUT_INSPECTOR
    }

    fn inbox(&self, home: &str) -> Result<Address, String> {
        roles::checkout_inspector(home)
    }

    /// A request for no checkout realised here is answered too: there is nothing newer to record,
    /// and the gate reads the checkout's record as it stands. A failed inspection is answered as
    /// well, since it is recorded as failing on the launch where it could be.
    async fn handle(&self, item: &Pending, home: &str) -> Handled {
        let subject = item.spec.references.iter().find(|reference| reference.subject).map(|reference| &reference.address);
        let asked = match (item.spec.body.value(), subject) {
            (MessageBody::Request(RequestBody::CheckoutInspection { remote }), Some(subject))
                if subject.get(AddressKind::Host) == Some(home) =>
            {
                subject.get(AddressKind::Checkout).map(|checkout| (checkout.to_owned(), *remote))
            }
            _ => None,
        };
        let inspected = match &asked {
            Some((checkout, remote)) => self.inspection.inspect(checkout, *remote).await,
            None => Ok(None),
        };
        match &inspected {
            Ok(Some(launch)) => debug!(message = %item.message, %launch, "checkout inspected for a gate"),
            Ok(None) => info!(
                target: "flotilla::checkout",
                event = "CheckoutInspectionUnheld",
                message = %item.message,
                host = home,
                subject = ?subject.map(Address::as_str),
                "an inspection request names no checkout realised here; it is answered with nothing recorded"
            ),
            Err(error) => warn!(message = %item.message, %error, "a gate's checkout inspection failed"),
        }
        Handled::Answered
    }
}

/// This host's forge observer (#3108), served on its role's inbox
/// (`host:<h>/role:forge-observer`): a ChangeRequestBranch request about a repository is looked
/// up with that repository's change request provider and what is found is recorded on the change
/// request's record, then the request takes the observer's receipt. A lookup that fails drops the
/// request, so branch validation fails rather than reading an absence nobody observed.
pub(super) struct ForgeObserver {
    pub(super) daemon: Arc<InProcessDaemon>,
}

#[async_trait]
impl RoleRequests for ForgeObserver {
    fn role(&self) -> &'static str {
        roles::FORGE_OBSERVER
    }

    fn inbox(&self, home: &str) -> Result<Address, String> {
        roles::forge_observer(home)
    }

    async fn handle(&self, item: &Pending, _home: &str) -> Handled {
        let subject = item.spec.references.iter().find(|reference| reference.subject).map(|reference| &reference.address);
        let (MessageBody::Request(RequestBody::ChangeRequestBranch { branch }), Some(repository)) = (item.spec.body.value(), subject)
        else {
            return Handled::Failed("the forge observer answers only a branch request about a repository".into());
        };
        match self.daemon.observe_change_request_branch(repository, branch).await {
            Ok(()) => {
                debug!(message = %item.message, %repository, %branch, "branch looked up for validation");
                Handled::Answered
            }
            Err(error) => Handled::Failed(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use flotilla_inbox::InboxBackend;
    use flotilla_resources::{
        FleetDesignation, FleetDesignationSpec, InputMeta, Project, ProjectSpec, SystemClock, FLEET_DESIGNATION_NAME,
    };
    use flotilla_schema::Frozen;
    use flotilla_schema_sqlite::stream::Service;
    use flotilla_state_machines::machines::message::{MessageReference, MessageSpec, Relation};
    use flotilla_store_memory::InMemoryBackend;

    use super::*;
    use flotilla_engine_messages::{MessagePrimitives, RequestAnswer};

    const NAMESPACE: &str = "flotilla";
    const HOME: &str = "home-a";

    async fn primitives(dir: &tempfile::TempDir) -> Arc<MessagePrimitives> {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        backend
            .clone()
            .using::<Project>(NAMESPACE)
            .create(
                &InputMeta::builder().name("fleet".to_string()).build(),
                &ProjectSpec::builder()
                    .display_name("Fleet".to_string())
                    .default_workflow_ref("w".to_string())
                    .repositories(Vec::new())
                    .build(),
            )
            .await
            .expect("fleet project");
        backend
            .definitions::<FleetDesignation>(NAMESPACE)
            .create(
                &InputMeta::builder().name(FLEET_DESIGNATION_NAME.to_string()).build(),
                &FleetDesignationSpec { fleet: Some("lab".into()), project: "fleet".into(), image_cache: None },
            )
            .await
            .expect("designation");
        let service = Arc::new(
            Service::open(&dir.path().join("decided.sqlite"), HOME, flotilla_engine_store::registry().expect("registry")).expect("store"),
        );
        Arc::new(MessagePrimitives::open(HOME, service, backend, Arc::new(SystemClock)).expect("primitives"))
    }

    /// The checkouts asked about, and the launch each is realised under.
    #[derive(Default)]
    struct Realised {
        asked: Mutex<Vec<(String, bool)>>,
    }

    #[async_trait]
    impl GateInspection for Realised {
        async fn inspect(&self, checkout: &str, remote: bool) -> Result<Option<String>, String> {
            self.asked.lock().expect("asked").push((checkout.to_owned(), remote));
            Ok((checkout == "feature").then(|| format!("{checkout}#1")))
        }
    }

    async fn request(primitives: &MessagePrimitives, checkout: &str) -> String {
        let roles = primitives.roles(NAMESPACE).await.expect("roles");
        let mut spec = MessageSpec::new(
            roles.address("teardown").expect("declared"),
            roles::checkout_inspector(HOME).expect("inspector inbox"),
            Relation::System,
            Frozen::new(MessageBody::Request(RequestBody::CheckoutInspection { remote: true })).expect("body"),
        );
        let subject = format!("host:{HOME}/checkout:{checkout}").parse::<Address>().expect("subject");
        spec.references.push(MessageReference { address: subject, revision: "1".into(), subject: true });
        primitives.inbox().enqueue(spec).await.expect("enqueued")
    }

    async fn answered(primitives: &MessagePrimitives, message: &str) {
        for _ in 0..200 {
            if matches!(primitives.request_answer(message).await.expect("answer"), RequestAnswer::Answered { .. }) {
                return;
            }
            tokio::task::yield_now().await;
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("{message} was not answered");
    }

    // Behaviour: the inspector answers each request in its inbox as it arrives, after inspecting
    // the subject checkout with the remote read the request asks for; a request for a checkout
    // not realised here is answered too.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_inspector_inspects_each_requests_subject_then_answers_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let primitives = primitives(&dir).await;
        let realised = Arc::new(Realised::default());
        let inspector = Arc::new(CheckoutInspector { inspection: Arc::clone(&realised) as _ });
        let before = request(&primitives, "feature").await;
        tokio::spawn(super::super::role_consumer::serve_role(Arc::clone(&primitives), inspector));
        answered(&primitives, &before).await;
        let after = request(&primitives, "gone").await;
        answered(&primitives, &after).await;
        assert_eq!(*realised.asked.lock().expect("asked"), vec![("feature".to_owned(), true), ("gone".to_owned(), true)]);
    }
}
