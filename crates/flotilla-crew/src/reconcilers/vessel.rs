use flotilla_resources::account_access::AccountRecords;
use flotilla_resources::checkout_path::checkout_path_component;
use flotilla_resources::lifecycle_reads::{LifecycleReads, NoLifecycleReads, ProvisioningKind};
use flotilla_resources::Actuation;
use flotilla_store::DockerImageSourceStoreExt;
use std::{
    collections::{BTreeMap, BTreeSet},
    marker::PhantomData,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use flotilla_agent::adapter::{
    adapter_enabling_env, append_convoy_work_context, build_convoy_crew_brief_with_options, required_agent_adapters, CrewAssignment,
    CrewBriefMember, CrewBriefTemplateResolver,
};
use flotilla_environment::environment::ENVIRONMENT_PROVIDER_INSTANCE_LABEL;
use flotilla_protocol::{CanonicalHostId, ConfiguredResourceLimits, LifecycleReading, PlacementDecision};
use flotilla_resources::{
    address::artifact_slot_record_name, canonicalize_repo_url, host_direct_environment_name, is_prepared_snapshot,
    repository_workspace_slugs, Artifact, Checkout, CheckoutPhase, CheckoutSpec, CheckoutWorktreeSpec, Clone, CloneSpec, Convoy,
    CrewSource, CrewWorkPhase, DockerEnvironmentSpec, DockerImageSource, Environment, EnvironmentMount, EnvironmentMountMode,
    EnvironmentPhase, EnvironmentSpec, FreshCloneCheckoutSpec, FulfilmentKind, FulfilmentKindSpec, FulfilmentRealisation, InputMeta,
    LifecycleAuthority, OwnerReference, PlacementPolicy, Repository, RepositoryKey, RepositorySpec, Resource, ResourceError,
    ResourceObject, ResourceProvenance, Stance, TerminalSession, TerminalSessionIdentity, TerminalSessionPhase, TerminalSessionSpec,
    Vessel, VesselImage, VesselPhase, VesselStatusPatch, WorkPhase, ACTUATOR_HOST_REF_ANNOTATION, ACTUATOR_SOURCE_ROOT_ANNOTATION,
    CHANGE_REQUEST_ID_LABEL, CONVOY_LABEL, CREDENTIAL_PERMISSIONS_ANNOTATION, CREDENTIAL_PERMISSIONS_ENV, CREDENTIAL_REFS_ANNOTATION,
    CREDENTIAL_REFS_ENV, CREDENTIAL_SCOPES_ANNOTATION, CREDENTIAL_SCOPES_ENV, PLACEMENT_SNAPSHOT_KIND, VESSEL_REF_LABEL,
};
use flotilla_resources::{realisation_settings, BRIEF_ARTIFACTS_ANNOTATION};
use flotilla_store::{
    controller::{delete_lifecycle_owned_matching, LabelJoinWatch, LabelMappedWatch, ReconcileOutcome, Reconciler, SecondaryWatch},
    ReplicaReadResolver, ResourceBackend, TypedResolver,
};
use sha2::{Digest, Sha256};
use tracing::{debug, warn};

const REPO_KEY_LABEL: &str = "flotilla.work/repo-key";
const REPO_LABEL: &str = "flotilla.work/repo";
const ENV_LABEL: &str = "flotilla.work/env";
const VESSEL_PROVISIONING_REQUEUE_AFTER: Duration = Duration::from_secs(5);
const VESSEL_INTERRUPTED_REQUEUE_AFTER: Duration = Duration::from_millis(250);
/// A vessel older than this still waiting for its convoy's snapshot is stuck, not racing admission.
const VESSEL_SNAPSHOT_WAIT_WARN_AFTER: chrono::TimeDelta = chrono::TimeDelta::minutes(5);
const VESSEL_PROVISIONING_STUCK_SECONDS: i64 = 2 * 60;
const CHECKOUT_RECREATE_BACKOFF: Duration = Duration::from_secs(60);

// Compare the grants we own, independent of serialization order and unrelated
// policy mounts. Legacy config/hooks overlays identify old shared roots too, so
// removing an entire repository still detects its stale writable grants.
fn git_mounts_match(existing: &[EnvironmentMount], desired: &[EnvironmentMount]) -> bool {
    let roots = existing
        .iter()
        .chain(desired)
        .filter(|mount| {
            mount.mode == EnvironmentMountMode::Ro
                && mount.source_path == mount.target_path
                && PathBuf::from(&mount.source_path).file_name().is_some_and(|name| name == "config" || name == "hooks")
        })
        .filter_map(|mount| PathBuf::from(&mount.source_path).parent().map(PathBuf::from))
        .collect::<BTreeSet<_>>();
    let grants = |mounts: &[EnvironmentMount]| {
        mounts
            .iter()
            .filter(|mount| {
                roots
                    .iter()
                    .any(|root| PathBuf::from(&mount.source_path).starts_with(root) || PathBuf::from(&mount.target_path).starts_with(root))
            })
            .map(|mount| (mount.source_path.clone(), mount.target_path.clone(), mount.mode == EnvironmentMountMode::Rw))
            .collect::<BTreeSet<_>>()
    };
    grants(existing) == grants(desired)
}

/// Environment-scoped VCS inspection used to plan contained Git mounts.
#[async_trait::async_trait]
pub trait WorktreeMetadataResolver: Send + Sync {
    async fn worktree_metadata(&self, env_ref: &str, target: &str) -> Result<flotilla_vcs::vcs::WorktreeMetadata, String>;
}

pub struct VesselReconciler {
    backend: ResourceBackend,
    lifecycle_reads: Arc<dyn LifecycleReads>,
    /// The account kinds' records: a crew whose binding ended waits.
    accounts: Arc<dyn AccountRecords>,
    convoys: TypedResolver<Convoy>,
    repositories: TypedResolver<Repository>,
    kinds: TypedResolver<FulfilmentKind>,
    policies: TypedResolver<PlacementPolicy>,
    environments: TypedResolver<Environment>,
    clones: TypedResolver<Clone>,
    checkouts: TypedResolver<Checkout>,
    terminal_sessions: TypedResolver<TerminalSession>,
    artifacts: ReplicaReadResolver<Artifact>,
    federated_convoys: Option<ReplicaReadResolver<Convoy>>,
    federated_kinds: Option<ReplicaReadResolver<FulfilmentKind>>,
    federated_policies: Option<ReplicaReadResolver<PlacementPolicy>>,
    local_host_ref: Option<CanonicalHostId>,
    additional_host_refs: std::collections::BTreeSet<CanonicalHostId>,
    namespace: String,
    worktree_metadata: Option<Arc<dyn WorktreeMetadataResolver>>,
    brief_templates: CrewBriefTemplateResolver,
    // A local backstop for rapid create/delete fights; it resets on restart.
    recent_checkout_create_attempts: Mutex<BTreeMap<String, Instant>>,
}

struct EnvironmentDependency {
    engine_held: bool,
    ready: bool,
    waiting: Option<String>,
    unavailable: Option<VesselPrepared>,
}

struct CheckoutDependency {
    phase: CheckoutPhase,
    failure: Option<String>,
    path: Option<String>,
}

impl VesselReconciler {
    pub fn new(backend: ResourceBackend, namespace: &str) -> Self {
        Self {
            backend: backend.clone(),
            lifecycle_reads: Arc::new(NoLifecycleReads),
            accounts: Arc::new(flotilla_store::OldStoreAccounts::new(backend.clone())),
            convoys: backend.clone().using::<Convoy>(namespace),
            repositories: backend.clone().using::<Repository>(namespace),
            kinds: backend.clone().using::<FulfilmentKind>(namespace),
            policies: backend.clone().using::<PlacementPolicy>(namespace),
            environments: backend.clone().using::<Environment>(namespace),
            clones: backend.clone().using::<Clone>(namespace),
            checkouts: backend.clone().using::<Checkout>(namespace),
            terminal_sessions: backend.clone().using::<TerminalSession>(namespace),
            artifacts: backend.including_replicas::<Artifact>(namespace),
            federated_convoys: None,
            federated_kinds: None,
            federated_policies: None,
            local_host_ref: None,
            additional_host_refs: Default::default(),
            namespace: namespace.to_string(),
            worktree_metadata: None,
            brief_templates: CrewBriefTemplateResolver::default(),
            recent_checkout_create_attempts: Mutex::new(BTreeMap::new()),
        }
    }

    /// Why an agent crew must not start yet: its account binding is no longer open.
    async fn account_wait(&self, convoy: &str, process: &flotilla_resources::CrewSpec) -> Result<Option<String>, ResourceError> {
        let CrewSource::Agent { selector, .. } = &process.source else { return Ok(None) };
        let Some(access) = flotilla_agent::adapter::CapabilityTable::seeded()
            .resolve_selector(selector)
            .ok()
            .and_then(|requirement| requirement.account_access().cloned())
        else {
            return Ok(None);
        };
        let binding =
            self.accounts.requester_binding(&self.namespace, convoy, &process.role, &access).await.map_err(ResourceError::other)?;
        Ok(binding.and_then(|binding| {
            binding
                .spec
                .state
                .value
                .reason()
                .map(|reason| format!("crew `{}` waits: {reason}; `flotilla convoy rebind` opens a new binding", process.role))
        }))
    }

    /// The account records the daemon composed: the engine's behind `[primitives]`.
    pub fn with_accounts(mut self, accounts: Arc<dyn AccountRecords>) -> Self {
        self.accounts = accounts;
        self
    }

    pub fn with_lifecycle_reads(mut self, reads: Arc<dyn LifecycleReads>) -> Self {
        self.lifecycle_reads = reads;
        self
    }

    fn checkout_dependency(&self, checkout: &ResourceObject<Checkout>) -> Result<CheckoutDependency, ResourceError> {
        let reading =
            self.lifecycle_reads.try_reading(ProvisioningKind::Checkout, &checkout.metadata.name, None).map_err(ResourceError::other)?;
        Ok(match reading {
            Some(LifecycleReading::Ready { identity }) => {
                CheckoutDependency { phase: CheckoutPhase::Ready, failure: None, path: Some(identity) }
            }
            Some(
                LifecycleReading::Failed { reason: cause } | LifecycleReading::GaveUp { cause } | LifecycleReading::Exhausted { cause },
            ) => CheckoutDependency { phase: CheckoutPhase::Failed, failure: Some(cause), path: None },
            Some(LifecycleReading::Lost | LifecycleReading::Released) => {
                CheckoutDependency { phase: CheckoutPhase::Gone, failure: None, path: None }
            }
            Some(_) => CheckoutDependency { phase: CheckoutPhase::Pending, failure: None, path: None },
            None => CheckoutDependency {
                phase: checkout.status.as_ref().map(|status| status.phase).unwrap_or_default(),
                failure: checkout.status.as_ref().and_then(|status| status.message.clone()),
                path: checkout
                    .status
                    .as_ref()
                    .and_then(|status| status.path.clone())
                    .or_else(|| checkout.spec.target_path().map(str::to_string)),
            },
        })
    }

    fn environment_dependency(&self, environment: &ResourceObject<Environment>) -> Result<EnvironmentDependency, ResourceError> {
        let reading = self
            .lifecycle_reads
            .try_reading(ProvisioningKind::Environment, &environment.metadata.name, None)
            .map_err(ResourceError::other)?;
        Ok(match reading {
            Some(reading) => {
                let unavailable = match &reading {
                    LifecycleReading::Failed { reason: cause }
                    | LifecycleReading::Exhausted { cause }
                    | LifecycleReading::GaveUp { cause } => Some(VesselPrepared::failed(cause.clone())),
                    LifecycleReading::Lost | LifecycleReading::Released => Some(VesselPrepared {
                        phase: None,
                        patch: PlannedPatch::Lost {
                            message: format!("environment {} {}", environment.metadata.name, reading.reason().unwrap_or_default()),
                        },
                        actuations: Vec::new(),
                    }),
                    _ => None,
                };
                EnvironmentDependency {
                    engine_held: true,
                    ready: matches!(reading, LifecycleReading::Ready { .. }),
                    waiting: reading.reason(),
                    unavailable,
                }
            }
            None => EnvironmentDependency {
                engine_held: false,
                ready: environment.status.as_ref().map(|status| status.phase) == Some(EnvironmentPhase::Ready),
                waiting: environment.status.as_ref().and_then(|status| status.message.clone()),
                unavailable: VesselPrepared::legacy_unavailable_environment(environment),
            },
        })
    }

    pub fn new_with_config_dir(backend: ResourceBackend, namespace: &str, config_dir: impl Into<PathBuf>) -> Self {
        Self { brief_templates: CrewBriefTemplateResolver::with_config_dir(config_dir), ..Self::new(backend, namespace) }
    }

    pub fn with_worktree_metadata(mut self, resolver: Arc<dyn WorktreeMetadataResolver>) -> Self {
        self.worktree_metadata = Some(resolver);
        self
    }

    async fn contained_git_mounts(&self, checkouts: &[(String, Option<String>)]) -> Result<Vec<EnvironmentMount>, ResourceError> {
        let mut directories = BTreeMap::<PathBuf, BTreeSet<PathBuf>>::new();
        for (checkout_name, clone_ref) in checkouts {
            let clone_ref = clone_ref.as_ref().ok_or_else(|| {
                ResourceError::invalid(format!("contained worktree placement requires checkout {checkout_name} to be a managed worktree"))
            })?;
            match self.clones.get(clone_ref).await {
                Ok(_) => {}
                Err(ResourceError::NotFound { .. }) => {
                    return Err(ResourceError::invalid(format!(
                        "contained worktree checkout {checkout_name} refers to missing clone {clone_ref}"
                    )))
                }
                Err(error) => return Err(error),
            }
            let checkout = self.checkouts.get(checkout_name).await?;
            let CheckoutSpec::Worktree(spec) = &checkout.spec else {
                return Err(ResourceError::invalid(format!("checkout {checkout_name} is no longer a managed worktree")));
            };
            let resolver = self.worktree_metadata.as_ref().ok_or_else(|| ResourceError::other("worktree metadata resolver unavailable"))?;
            let metadata = resolver.worktree_metadata(&spec.env_ref, &spec.target_path).await.map_err(ResourceError::other)?;
            // Only direct registration children may override the read-only parent.
            // Reject unrelated paths rather than granting a broader writable mount.
            if !metadata.common_dir.is_absolute()
                || !metadata.admin_dir.is_absolute()
                || metadata.admin_dir.parent() != Some(metadata.common_dir.join("worktrees").as_path())
            {
                return Err(ResourceError::invalid(format!("checkout {checkout_name} has an invalid Git administration directory")));
            }
            directories.entry(metadata.common_dir).or_default().insert(metadata.admin_dir);
        }
        let mut mounts = Vec::new();
        for (common, admins) in directories {
            let mount = |path: PathBuf, mode| {
                let path = path.to_string_lossy().into_owned();
                EnvironmentMount { source_path: path.clone(), target_path: path, mode }
            };
            mounts.push(mount(common.clone(), EnvironmentMountMode::Rw));
            for protected in ["config", "hooks", "worktrees"] {
                mounts.push(mount(common.join(protected), EnvironmentMountMode::Ro));
            }
            for admin in admins {
                mounts.push(mount(admin, EnvironmentMountMode::Rw));
            }
        }
        Ok(mounts)
    }

    pub fn with_federated_dependencies(mut self, backend: &ResourceBackend, local_host_ref: CanonicalHostId) -> Self {
        self.federated_convoys = Some(backend.including_replicas::<Convoy>(&self.namespace));
        self.federated_kinds = Some(backend.including_replicas::<FulfilmentKind>(&self.namespace));
        self.federated_policies = Some(backend.including_replicas::<PlacementPolicy>(&self.namespace));
        self.local_host_ref = Some(local_host_ref);
        self
    }

    pub fn with_additional_host_refs(mut self, host_refs: impl IntoIterator<Item = CanonicalHostId>) -> Self {
        self.additional_host_refs = host_refs.into_iter().collect();
        self
    }

    pub fn secondary_watches() -> Vec<Box<dyn SecondaryWatch<Primary = Vessel>>> {
        vec![
            Box::new(LabelMappedWatch::<Environment, Vessel> { label_key: VESSEL_REF_LABEL, _marker: PhantomData }),
            Box::new(LabelJoinWatch::<Checkout, Vessel> { label_key: CONVOY_LABEL, _marker: PhantomData }),
            Box::new(LabelMappedWatch::<TerminalSession, Vessel> { label_key: VESSEL_REF_LABEL, _marker: PhantomData }),
        ]
    }

    async fn dependency<T: Resource>(
        local: &TypedResolver<T>,
        federated: Option<&ReplicaReadResolver<T>>,
        vessel: &ResourceObject<Vessel>,
        name: &str,
    ) -> Result<ResourceObject<T>, ResourceError> {
        let Some(origin) = vessel.metadata.annotations.get(ACTUATOR_SOURCE_ROOT_ANNOTATION) else {
            return local.get(name).await;
        };
        let Some(federated) = federated else {
            return Err(ResourceError::not_found(name));
        };
        federated
            .list()
            .await?
            .items
            .into_iter()
            .find(|source| {
                source.object.metadata.name == name
                    && matches!(
                        &source.provenance,
                        ResourceProvenance::Replica { origin_root, .. } if origin_root.as_str() == origin
                    )
            })
            .map(|source| source.object)
            .ok_or_else(|| ResourceError::not_found(name))
    }

    /// `name` as this vessel reads it, or, as a placement snapshot, from whichever origin wrote it.
    async fn snapshot_dependency<T: Resource>(
        local: &TypedResolver<T>,
        federated: Option<&ReplicaReadResolver<T>>,
        vessel: &ResourceObject<Vessel>,
        name: &str,
    ) -> Result<ResourceObject<T>, ResourceError> {
        match Self::dependency(local, federated, vessel, name).await {
            Err(ResourceError::NotFound { .. }) => {
                let Some(federated) = federated else {
                    return Err(ResourceError::not_found(name));
                };
                let source = federated.get(name).await?;
                if is_prepared_snapshot(name, &source.object.metadata.labels, PLACEMENT_SNAPSHOT_KIND) {
                    Ok(source.object)
                } else {
                    Err(ResourceError::not_found(name))
                }
            }
            result => result,
        }
    }

    async fn placement_dependency(
        &self,
        vessel: &ResourceObject<Vessel>,
        name: &str,
        decision: Option<&PlacementDecision>,
    ) -> Result<Result<Placement, String>, ResourceError> {
        let frozen = decision.and_then(|decision| decision.provider_instance.clone());
        match Self::snapshot_dependency(&self.kinds, self.federated_kinds.as_ref(), vessel, name).await {
            Ok(kind) => match frozen {
                Some(instance) => Ok(Ok(Placement { kind, instance: Some(instance) })),
                None if kind.spec.realisation == FulfilmentRealisation::HostDirect => Ok(Ok(Placement { kind, instance: None })),
                None => Ok(self.unfrozen_instance(&kind).await?.map(|instance| Placement { kind, instance })),
            },
            // ADR 0047: a convoy admitted before kinds froze its placement as a PlacementPolicy.
            // Remove one roll after this ships, with PlacementPolicy.
            Err(ResourceError::NotFound { .. }) => {
                let policy = Self::snapshot_dependency(&self.policies, self.federated_policies.as_ref(), vessel, name).await?;
                let instance = frozen
                    .or_else(|| policy.metadata.labels.get(ENVIRONMENT_PROVIDER_INSTANCE_LABEL).cloned())
                    .or_else(|| policy.spec.provider_requirements.as_ref().and_then(|requirements| requirements.instance.clone()));
                Ok(FulfilmentKindSpec::from_policy(&policy.spec, "linux")
                    .map(|spec| Placement { kind: ResourceObject { metadata: policy.metadata, spec, status: None }, instance }))
            }
            Err(error) => Err(error),
        }
    }

    /// A vessel no admission froze an instance for takes the one provider on its kind's
    /// host that realises the kind, as admission would have chosen; with no advertisements
    /// observed it names none.
    async fn unfrozen_instance(&self, kind: &ResourceObject<FulfilmentKind>) -> Result<Result<Option<String>, String>, ResourceError> {
        let hosts = self.backend.including_replicas::<flotilla_resources::Host>(&self.namespace).list().await?.items;
        let hosts = hosts.into_iter().map(|host| host.object).collect::<Vec<_>>();
        let host = match flotilla_resources::canonical_host_id(hosts.iter(), &kind.spec.host_ref) {
            Ok(Some(id)) => hosts.iter().find(|host| host.metadata.name == id.as_str()),
            Ok(None) => hosts.iter().find(|host| host.metadata.name == kind.spec.host_ref),
            Err(error) => return Ok(Err(error)),
        };
        let Some(advertised) = host
            .and_then(|host| host.status.as_ref())
            .and_then(|status| status.capabilities.get(flotilla_resources::ENVIRONMENT_PROVIDERS_CAPABILITY))
        else {
            return Ok(Ok(None));
        };
        let providers: Vec<flotilla_protocol::provider::ProviderAdvertisement> = match serde_json::from_value(advertised.clone()) {
            Ok(providers) => providers,
            Err(error) => return Ok(Err(format!("{}: invalid provider advertisements: {error}", kind.spec.host_ref))),
        };
        Ok(flotilla_resources::provider_placement::match_provider(&kind.spec.host_ref, &kind.spec.realisation, None, &providers)
            .map(|provider| Some(provider.instance.clone())))
    }

    fn missing_host_local_environment(&self, environment_ref: &str, placement_host_ref: &str) -> String {
        let reconciler_host_ref = self.local_host_ref.as_ref().map_or("unknown", CanonicalHostId::as_str);
        let message = format!(
            "host-local Environment {environment_ref} for placement host {placement_host_ref} was not found in the resource store for reconciler host {reconciler_host_ref}"
        );
        warn!(
            %environment_ref,
            %placement_host_ref,
            %reconciler_host_ref,
            "host-local Environment lookup failed in Vessel reconciliation"
        );
        message
    }
}

/// What a vessel is realised from: its kind, and the provider instance admission froze for it.
struct Placement {
    kind: ResourceObject<FulfilmentKind>,
    instance: Option<String>,
}

#[derive(Debug, Clone)]
enum PlacementStrategy {
    HostDirect { host_ref: String, pool: String },
    DockerHostCheckoutAndMount { host_ref: String, pool: String, image: Option<DockerImageSource> },
    CloneInVessel { host_ref: String, pool: String, image: String, clone_path: String },
}

impl PlacementStrategy {
    fn canonicalize_host_ref(&mut self, host_ref: &str) {
        match self {
            Self::HostDirect { host_ref: strategy_host_ref, .. }
            | Self::DockerHostCheckoutAndMount { host_ref: strategy_host_ref, .. }
            | Self::CloneInVessel { host_ref: strategy_host_ref, .. } => host_ref.clone_into(strategy_host_ref),
        }
    }
}

#[derive(Debug, Clone)]
enum PlannedPatch {
    None,
    /// A dependency is not ready yet; change nothing and look again.
    Waiting,
    Provisioning {
        observed_kind_ref: String,
        observed_kind_version: String,
        placement_decision: Option<PlacementDecision>,
        waiting_for: String,
        image: Option<VesselImage>,
    },
    EnvironmentRecreation {
        observed_kind_ref: String,
        observed_kind_version: String,
        placement_decision: Option<PlacementDecision>,
        message: String,
        image: Option<VesselImage>,
    },
    CheckoutChurn {
        observed_kind_ref: String,
        observed_kind_version: String,
        placement_decision: Option<PlacementDecision>,
        message: String,
        image: Option<VesselImage>,
    },
    Ready {
        configured_limits: Option<ConfiguredResourceLimits>,
        placement_decision: Option<PlacementDecision>,
        environment_ref: String,
        image: Option<ImageStamp>,
        checkout_refs: BTreeMap<RepositoryKey, String>,
        terminal_session_refs: Vec<String>,
        requested_stance: Stance,
        effective_stance: Stance,
    },
    Interrupted {
        roles: BTreeSet<String>,
        message: String,
    },
    Lost {
        message: String,
    },
    Failed {
        message: String,
    },
}

#[derive(Debug, Clone)]
struct ImageStamp {
    image_ref: String,
    local_image_id: String,
    registry_digest: Option<String>,
    /// The composed identity and the digest it provisioned from.
    composed: Option<VesselImage>,
}

/// The composed identity a placement froze at admission (ADR 0062).
fn composed_image(placement: &Placement, digest: Option<String>) -> Option<VesselImage> {
    match &placement.kind.spec.realisation {
        FulfilmentRealisation::DockerPerVessel { image: Some(DockerImageSource::Built { identity, .. }) } => {
            Some(VesselImage { identity: identity.clone(), digest })
        }
        _ => None,
    }
}

/// The Environment's image fields: a composed image names its per-identity
/// ImageBuild and receives the build's digest from the Environment reconciler;
/// a literal image resolves now.
fn environment_image(image: &Option<DockerImageSource>) -> Result<(String, Option<String>), String> {
    match image {
        None => Err("composed image was not frozen at admission; readmit the convoy".into()),
        Some(DockerImageSource::Built { image_build_ref, .. }) => Ok((String::new(), Some(image_build_ref.clone()))),
        Some(image) => image.resolve().map(|image| (image, None)),
    }
}

#[derive(Debug, Clone)]
pub struct VesselPrepared {
    phase: Option<VesselPhase>,
    patch: PlannedPatch,
    actuations: Vec<Actuation>,
}

impl VesselPrepared {
    fn none() -> Self {
        Self { phase: None, patch: PlannedPatch::None, actuations: Vec::new() }
    }

    fn provisioning(
        placement: &Placement,
        placement_decision: Option<PlacementDecision>,
        waiting_for: impl Into<String>,
        actuations: Vec<Actuation>,
    ) -> Self {
        let waiting_for = legible_waiting_for(waiting_for.into(), placement_decision.as_ref());
        Self { phase: None, patch: provisioning_patch(placement, placement_decision, waiting_for), actuations }
    }

    fn checkout_churn(
        placement: &Placement,
        placement_decision: Option<PlacementDecision>,
        checkout_name: &str,
        actuations: Vec<Actuation>,
    ) -> Self {
        Self {
            phase: None,
            patch: PlannedPatch::CheckoutChurn {
                observed_kind_ref: placement.kind.metadata.name.clone(),
                observed_kind_version: placement.kind.metadata.resource_version.clone(),
                placement_decision,
                message: format!("checkout {checkout_name} is missing shortly after a create attempt; retrying after backoff"),
                image: composed_image(placement, None),
            },
            actuations,
        }
    }

    fn legacy_unavailable_environment(environment: &ResourceObject<Environment>) -> Option<Self> {
        let status = environment.status.as_ref()?;
        let patch = match status.phase {
            EnvironmentPhase::Lost => PlannedPatch::Lost {
                message: status.message.clone().unwrap_or_else(|| {
                    format!(
                        "lost, recoverable: environment {} disappeared; rehydration is not available yet (#2872)",
                        environment.metadata.name
                    )
                }),
            },
            EnvironmentPhase::Failed => PlannedPatch::Failed {
                message: status.message.clone().unwrap_or_else(|| format!("environment {} failed", environment.metadata.name)),
            },
            _ => return None,
        };
        Some(Self { phase: None, patch, actuations: Vec::new() })
    }

    fn waiting() -> Self {
        Self { phase: None, patch: PlannedPatch::Waiting, actuations: Vec::new() }
    }

    fn failed(message: impl Into<String>) -> Self {
        Self { phase: None, patch: PlannedPatch::Failed { message: message.into() }, actuations: Vec::new() }
    }

    fn interrupted(role: String, terminal_name: &str, restart: bool, replaces: Option<String>, mut actuations: Vec<Actuation>) -> Self {
        if restart {
            actuations.push(Actuation::RestartTerminalSession { name: terminal_name.to_string(), replaces });
        }
        Self {
            phase: None,
            patch: PlannedPatch::Interrupted {
                roles: BTreeSet::from([role]),
                message: format!("crew terminal session {terminal_name} disappeared; relaunching from its durable brief"),
            },
            actuations,
        }
    }
}

impl VesselReconciler {
    async fn prepare_plan(&self, obj: &ResourceObject<Vessel>) -> Result<VesselPrepared, ResourceError> {
        let convoy = match Self::dependency(&self.convoys, self.federated_convoys.as_ref(), obj, &obj.spec.convoy_ref).await {
            Ok(convoy) => convoy,
            Err(ResourceError::NotFound { .. }) => return Ok(VesselPrepared::failed(format!("convoy {} not found", obj.spec.convoy_ref))),
            Err(err) => return Err(err),
        };
        // Convoy cleanup owns terminal vessels and their checkouts. A vessel
        // awaiting finalization must not provision a child that checkout
        // reconciliation will immediately collect again.
        if convoy.status.as_ref().is_some_and(|status| status.phase.is_terminal()) {
            return Ok(VesselPrepared::none());
        }
        let placement_decision = flotilla_resources::vessel_placement_pin(&convoy, &obj.spec.vessel_name)
            .map(|pin| pin.decision)
            .or_else(|| convoy.status.as_ref().and_then(|status| status.placement_decision.clone()));
        if self.local_host_ref.as_ref().zip(placement_decision.as_ref()).is_some_and(|(local_host_ref, decision)| {
            &decision.target_host.reference != local_host_ref && !self.additional_host_refs.contains(&decision.target_host.reference)
        }) {
            return Ok(VesselPrepared::none());
        }
        // Once backing is known, its failure takes precedence over planning
        // dependencies: retain the recorded death cause and recovery paths.
        if let Some(environment_ref) = obj.status.as_ref().and_then(|status| status.environment_ref.as_ref()) {
            match self.environments.get(environment_ref).await {
                Ok(environment) => {
                    let dependency = self.environment_dependency(&environment)?;
                    if let Some(prepared) = dependency.unavailable {
                        return Ok(prepared);
                    }
                }
                Err(ResourceError::NotFound { .. }) => {}
                Err(error) => return Err(error),
            }
        }

        let placement = match self.placement_dependency(obj, &obj.spec.fulfilment_kind_ref, placement_decision.as_ref()).await {
            Ok(Ok(placement)) => placement,
            Ok(Err(message)) => return Ok(VesselPrepared::failed(message)),
            Err(ResourceError::NotFound { .. }) => {
                return Ok(VesselPrepared::failed(format!("fulfilment kind {} not found", obj.spec.fulfilment_kind_ref)))
            }
            Err(err) => return Err(err),
        };
        let mut strategy = placement_strategy(&placement.kind.spec);
        if let PlacementStrategy::CloneInVessel { clone_path, .. } = &mut strategy {
            let Some(root) = placement_decision.as_ref().and_then(|decision| decision.provider_workspace_root.as_ref()) else {
                return Ok(VesselPrepared::failed("cloud provider workspace root missing from frozen placement"));
            };
            *clone_path = vessel_clone_path(root, &obj.metadata.name);
        }
        if let Some(decision) = placement_decision.as_ref() {
            strategy.canonicalize_host_ref(decision.target_host.reference.as_str());
        }
        let mut declared_agent_adapters = BTreeSet::new();

        // A convoy read without its snapshot is not ready yet: admission has
        // not frozen it, or a replica briefly holds an older body (#4137).
        let Some(snapshot) = convoy.status.as_ref().and_then(|status| status.workflow_snapshot.as_ref()) else {
            let waited = Utc::now() - obj.metadata.creation_timestamp;
            if waited > VESSEL_SNAPSHOT_WAIT_WARN_AFTER {
                warn!(vessel = %obj.metadata.name, convoy = %obj.spec.convoy_ref, %waited, "vessel still waiting for its convoy's workflow snapshot");
            } else {
                debug!(vessel = %obj.metadata.name, convoy = %obj.spec.convoy_ref, "vessel waiting for its convoy's workflow snapshot");
            }
            return Ok(VesselPrepared::waiting());
        };
        let (vessel_index, requirement) = match snapshot.vessels.iter().enumerate().find(|(_, vessel)| vessel.name == obj.spec.vessel_name)
        {
            Some((vessel_index, requirement)) => (vessel_index, requirement),
            None => return Ok(VesselPrepared::failed(format!("vessel {} missing from convoy snapshot", obj.spec.vessel_name))),
        };
        let required_adapters = match required_agent_adapters(&requirement.crew) {
            Ok(adapters) => adapters,
            Err(message) => return Ok(VesselPrepared::failed(message)),
        };
        // A composed image installs exactly the harnesses its roles resolve
        // to; interior discovery verifies them.
        if composed_image(&placement, None).is_some() {
            declared_agent_adapters.clone_from(&required_adapters);
        }
        let effective_stance = strategy.effective_stance();

        let repository_refs = requirement
            .repository_refs
            .clone()
            .unwrap_or_else(|| convoy.spec.repositories.iter().map(|repository| repository.repo_ref.clone()).collect());
        if repository_refs.is_empty() && requirement.repository_refs.is_some() {
            return Ok(VesselPrepared::failed(format!("vessel {} has an empty repository scope", obj.spec.vessel_name)));
        }
        let git_ref = if repository_refs.is_empty() {
            None
        } else {
            match convoy.spec.r#ref.clone() {
                Some(git_ref) => Some(git_ref),
                None => return Ok(VesselPrepared::failed("convoy ref is missing".to_string())),
            }
        };
        let checkout_slug = git_ref.as_deref().map(checkout_path_component);
        let convoy_checkout_slug = checkout_path_component(&convoy.metadata.name);
        if repository_refs.len() > 1 && !obj.spec.adopted_checkout_refs.is_empty() {
            return Ok(VesselPrepared::failed("adopted checkouts are not supported for multi-repository vessel workspaces".to_string()));
        }
        let mut convoy_repositories = Vec::new();
        for repo_ref in &repository_refs {
            let Some(repository) = convoy.spec.repositories.iter().find(|repository| &repository.repo_ref == repo_ref) else {
                return Ok(VesselPrepared::failed(format!(
                    "vessel {} references repository {repo_ref} outside its convoy",
                    obj.spec.vessel_name
                )));
            };
            if convoy_repositories.iter().any(|existing: &&flotilla_resources::ConvoyRepositorySpec| existing.repo_ref == *repo_ref) {
                return Ok(VesselPrepared::failed(format!(
                    "vessel {} repository scope contains duplicate {repo_ref}",
                    obj.spec.vessel_name
                )));
            }
            convoy_repositories.push(repository);
        }
        let multi_repository = convoy.spec.repositories.len() > 1;
        let workspace_slugs = convoy_repositories
            .iter()
            .map(|repository| (repository.repo_ref.clone(), repository.workspace_slug.clone()))
            .collect::<BTreeMap<_, _>>();

        let clone_env_ref = host_direct_environment_name(strategy.host_ref());
        let mut actuations = Vec::new();
        let shared_clone_root = if strategy.needs_shared_clone() {
            let clone_env = match self.environments.get(&clone_env_ref).await {
                Ok(environment) => environment,
                Err(ResourceError::NotFound { .. }) => {
                    return Ok(VesselPrepared::failed(self.missing_host_local_environment(&clone_env_ref, strategy.host_ref())))
                }
                Err(err) => return Err(err),
            };
            let clone_env_spec = match clone_env.spec.host_direct.as_ref() {
                Some(spec) => spec,
                None => return Ok(VesselPrepared::failed(format!("environment {clone_env_ref} is not a host_direct environment"))),
            };
            if !self.environment_dependency(&clone_env)?.ready {
                return Ok(VesselPrepared::provisioning(
                    &placement,
                    placement_decision.clone(),
                    format!("environment {clone_env_ref} to become ready"),
                    actuations,
                ));
            }
            Some(clone_env_spec.repo_default_dir.clone())
        } else {
            None
        };

        let precreated_environment = match &strategy {
            PlacementStrategy::CloneInVessel { image, .. } => {
                let env_name = environment_name(&obj.metadata.name);
                let image = match self.environments.get(&env_name).await {
                    Ok(existing) => {
                        let dependency = self.environment_dependency(&existing)?;
                        if let Some(prepared) = dependency.unavailable {
                            return Ok(prepared);
                        }
                        if !dependency.ready {
                            let waiting_for = dependency.waiting.unwrap_or_else(|| format!("environment {env_name} to become ready"));
                            return Ok(VesselPrepared::provisioning(&placement, placement_decision.clone(), waiting_for, actuations));
                        }
                        match image_stamp(&existing, &placement) {
                            Ok(image) => image,
                            Err(message) if dependency.engine_held => {
                                // Readiness can commit before the provider's image observations are published.
                                return Ok(VesselPrepared::provisioning(&placement, placement_decision.clone(), message, actuations));
                            }
                            Err(message) => return Ok(VesselPrepared::failed(message)),
                        }
                    }
                    Err(ResourceError::NotFound { .. }) => {
                        let probe_needs = requirement
                            .crew
                            .iter()
                            .flat_map(|crew| &crew.needs)
                            .filter(|need| need.requires_guest_probe())
                            .cloned()
                            .collect();
                        let spec = match cloud_environment_spec(&placement, image, probe_needs, &required_adapters) {
                            Ok(spec) => spec,
                            Err(message) => return Ok(VesselPrepared::failed(message)),
                        };
                        actuations.push(Actuation::CreateEnvironment {
                            meta: owned_child_meta(&env_name, obj, environment_provider_labels(&placement)),
                            spec,
                        });
                        return Ok(VesselPrepared::provisioning(
                            &placement,
                            placement_decision.clone(),
                            format!("environment {env_name} to become ready"),
                            actuations,
                        ));
                    }
                    Err(err) => return Err(err),
                };
                Some((env_name, image))
            }
            _ => None,
        };

        let workspace_convoy_slug = convoy_checkout_slug.as_str();
        let workspace_root = if multi_repository {
            match &strategy {
                PlacementStrategy::HostDirect { .. } | PlacementStrategy::DockerHostCheckoutAndMount { .. } => {
                    format!(
                        "{}/{}/{}",
                        shared_clone_root.as_deref().expect("shared-clone placement has a root").trim_end_matches('/'),
                        workspace_convoy_slug,
                        checkout_slug.as_deref().expect("repository workspace requires a branch")
                    )
                }
                PlacementStrategy::CloneInVessel { clone_path, .. } => clone_path.clone(),
            }
        } else {
            String::new()
        };
        let mut checkout_refs = BTreeMap::new();
        let mut degraded = BTreeMap::new();
        let mut checkout_paths = BTreeMap::new();
        let mut contained_worktree_checkouts = Vec::new();
        // The vessel shares one environment; Git ignores tracking entries for branches absent from a checkout.
        let mut contained_branches = BTreeSet::new();
        let mut waiting_for_checkouts = Vec::new();
        let mut fork_stance = false;
        // Slug allocation needs the repository catalog only when a shared clone
        // is missing; read it at most once per prepare, not once per repository.
        let repository_catalog = tokio::sync::OnceCell::new();
        for convoy_repository in convoy_repositories {
            let repository_spec = match self.repositories.get(&convoy_repository.repo_ref.to_string()).await {
                Ok(repository) => repository.spec,
                Err(ResourceError::NotFound { .. }) => {
                    // Convoy URLs are clone transports and may use SCP-like SSH syntax.
                    // Pre-canonicalize them before constructing the identity-bearing spec.
                    let repository_spec = match canonicalize_repo_url(&convoy_repository.url).and_then(RepositorySpec::remote) {
                        Ok(spec) => spec,
                        Err(message) => {
                            return Ok(VesselPrepared::failed(format!(
                                "convoy repository {} has invalid URL: {message}",
                                convoy_repository.repo_ref
                            )))
                        }
                    };
                    if let Err(message) = repository_spec.verify_key(&convoy_repository.repo_ref) {
                        return Ok(VesselPrepared::failed(message));
                    }
                    actuations.push(Actuation::CreateRepository { key: convoy_repository.repo_ref.clone(), spec: repository_spec.clone() });
                    repository_spec
                }
                Err(err) => return Err(err),
            };
            if let Err(message) = repository_spec.verify_key(&convoy_repository.repo_ref) {
                return Ok(VesselPrepared::failed(message));
            }
            fork_stance |= repository_spec.is_fork();
            let clone_identity = match repository_spec.clone_key(&clone_env_ref) {
                Ok(key) => key,
                Err(error) => return Ok(VesselPrepared::failed(error)),
            };
            let repository_key = convoy_repository.repo_ref.clone();
            let repo_key = repository_key.to_string();
            let adopted_checkout_ref = obj.spec.adopted_checkout_refs.get(&repository_key).cloned();
            let clone_name = if adopted_checkout_ref.is_none() && strategy.needs_shared_clone() {
                let clone_name = format!("clone-{clone_identity}");
                match self.clones.get(&clone_name).await {
                    Ok(existing) => {
                        if existing.spec.repo_ref != repository_key || existing.spec.env_ref != clone_env_ref {
                            return Ok(VesselPrepared::failed(format!("clone {clone_name} does not match expected repo/env tuple")));
                        }
                    }
                    Err(ResourceError::NotFound { .. }) => {
                        let repository_catalog = repository_catalog.get_or_try_init(|| self.repositories.list()).await?;
                        let keyed_repositories = repository_catalog
                            .items
                            .iter()
                            .filter(|candidate| candidate.spec.key() != repository_key)
                            .map(|candidate| (candidate.spec.key(), &candidate.spec))
                            .chain(std::iter::once((repository_key.clone(), &repository_spec)))
                            .collect::<Vec<_>>();
                        let clone_directory_slug = repository_workspace_slugs(keyed_repositories.iter().map(|(key, spec)| (key, *spec)))
                            .remove(&repository_key)
                            .expect("the current repository was included in slug allocation");
                        let clone_path = format!(
                            "{}/{}",
                            shared_clone_root.as_deref().expect("shared-clone placement has a root").trim_end_matches('/'),
                            clone_directory_slug
                        );
                        actuations.push(Actuation::CreateClone {
                            meta: InputMeta::builder()
                                .name(clone_name.clone())
                                .labels(BTreeMap::from([
                                    (REPO_KEY_LABEL.to_string(), repo_key.clone()),
                                    (ENV_LABEL.to_string(), clone_env_ref.clone()),
                                    (REPO_LABEL.to_string(), convoy_repository.workspace_slug.clone()),
                                ]))
                                .build(),
                            spec: CloneSpec {
                                repo_ref: repository_key.clone(),
                                url: convoy_repository.url.clone(),
                                env_ref: clone_env_ref.clone(),
                                path: clone_path,
                            },
                        });
                    }
                    Err(err) => return Err(err),
                }
                Some(clone_name)
            } else {
                None
            };
            let checkout_name = adopted_checkout_ref.clone().unwrap_or_else(|| match &strategy {
                PlacementStrategy::HostDirect { .. } | PlacementStrategy::DockerHostCheckoutAndMount { .. } => checkout_name(
                    &convoy.metadata.name,
                    &convoy_repository.workspace_slug,
                    multi_repository,
                    checkout_placement_scope(&convoy, self.local_host_ref.as_ref().map(|_| strategy.host_ref())).as_deref(),
                ),
                PlacementStrategy::CloneInVessel { .. } => checkout_name(
                    &obj.metadata.name,
                    &convoy_repository.workspace_slug,
                    multi_repository,
                    checkout_placement_scope(&convoy, self.local_host_ref.as_ref().map(|_| strategy.host_ref())).as_deref(),
                ),
            });
            let checkout_target_path = match &strategy {
                PlacementStrategy::HostDirect { .. } | PlacementStrategy::DockerHostCheckoutAndMount { .. } => {
                    if multi_repository {
                        format!("{workspace_root}/{}", convoy_repository.workspace_slug)
                    } else {
                        checkout_target_path(
                            shared_clone_root.as_deref().expect("shared-clone placement has a root"),
                            workspace_convoy_slug,
                            &convoy_repository.workspace_slug,
                            checkout_slug.as_deref().expect("repository checkout requires a branch"),
                        )
                    }
                }
                PlacementStrategy::CloneInVessel { clone_path, .. } => {
                    if multi_repository {
                        format!("{}/{}", clone_path.trim_end_matches('/'), convoy_repository.workspace_slug)
                    } else {
                        clone_path.clone()
                    }
                }
            };
            match self.checkouts.get(&checkout_name).await {
                Ok(existing) => {
                    if existing.spec.repo_ref() != &repository_key {
                        return Ok(VesselPrepared::failed(format!("checkout {checkout_name} belongs to a different repository")));
                    }
                    if adopted_checkout_ref.is_some() && existing.metadata.lifecycle_authority()? != Some(LifecycleAuthority::Adopted) {
                        return Ok(VesselPrepared::failed(format!("checkout {checkout_name} is not adopted")));
                    }
                    let dependency = self.checkout_dependency(&existing)?;
                    if dependency.phase == CheckoutPhase::Gone {
                        if adopted_checkout_ref.is_some() {
                            return Ok(VesselPrepared::failed(format!("adopted checkout {checkout_name} is gone")));
                        }
                        if existing.metadata.deletion_timestamp.is_none() {
                            actuations.push(Actuation::DeleteCheckout { name: checkout_name.clone() });
                        }
                        waiting_for_checkouts.push(checkout_name);
                        continue;
                    }
                    if dependency.phase == CheckoutPhase::Failed {
                        let message = dependency.failure.unwrap_or_else(|| format!("checkout {checkout_name} failed"));
                        return Ok(VesselPrepared::failed(message));
                    }
                    if dependency.phase == CheckoutPhase::Ready {
                        if let Some(message) = existing.status.as_ref().and_then(|status| status.degraded.clone()) {
                            degraded.insert(checkout_name.clone(), message);
                        }
                        let Some(path) = dependency.path else {
                            return Ok(VesselPrepared::failed(format!("checkout {checkout_name} is ready but has no target path")));
                        };
                        if matches!(&strategy, PlacementStrategy::DockerHostCheckoutAndMount { .. }) {
                            if let CheckoutSpec::Worktree(spec) = &existing.spec {
                                contained_branches.insert(spec.r#ref.strip_prefix("refs/heads/").unwrap_or(&spec.r#ref).to_string());
                            }
                            contained_worktree_checkouts.push((
                                checkout_name.clone(),
                                match &existing.spec {
                                    CheckoutSpec::Worktree(spec) => Some(spec.clone_ref.clone()),
                                    CheckoutSpec::FreshClone(_) | CheckoutSpec::Observed(_) => None,
                                },
                            ));
                        }
                        checkout_refs.insert(repository_key.clone(), checkout_name);
                        checkout_paths.insert(repository_key, path);
                    } else {
                        waiting_for_checkouts.push(checkout_name);
                    }
                }
                Err(ResourceError::NotFound { .. }) => {
                    if adopted_checkout_ref.is_some() {
                        return Ok(VesselPrepared::failed(format!("adopted checkout {checkout_name} not found")));
                    }
                    let now = Instant::now();
                    let mut recent = self.recent_checkout_create_attempts.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    recent.retain(|_, created_at| now.duration_since(*created_at) < CHECKOUT_RECREATE_BACKOFF);
                    if recent.contains_key(&checkout_name) {
                        warn!(checkout = %checkout_name, convoy = %convoy.metadata.name, "checkout is missing shortly after a create attempt; backing off");
                        return Ok(VesselPrepared::checkout_churn(&placement, placement_decision.clone(), &checkout_name, actuations));
                    }
                    recent.insert(checkout_name.clone(), now);
                    drop(recent);
                    let spec = match &strategy {
                        PlacementStrategy::HostDirect { .. } | PlacementStrategy::DockerHostCheckoutAndMount { .. } => {
                            CheckoutSpec::Worktree(CheckoutWorktreeSpec {
                                repo_ref: repository_key.clone(),
                                env_ref: clone_env_ref.clone(),
                                r#ref: git_ref.clone().expect("repository checkout requires a convoy ref"),
                                base_ref: Some(convoy_repository.source_ref.clone()),
                                target_path: checkout_target_path,
                                clone_ref: clone_name.expect("shared-clone strategy requires clone name"),
                            })
                        }
                        PlacementStrategy::CloneInVessel { .. } => CheckoutSpec::FreshClone(FreshCloneCheckoutSpec {
                            source_clone_ref: None,
                            repo_ref: repository_key.clone(),
                            env_ref: precreated_environment
                                .as_ref()
                                .map(|(environment_ref, _)| environment_ref.clone())
                                .expect("fresh-clone strategy should precreate environment"),
                            r#ref: git_ref.clone().expect("repository checkout requires a convoy ref"),
                            base_ref: Some(convoy_repository.source_ref.clone()),
                            target_path: checkout_target_path,
                            url: convoy_repository.url.clone(),
                        }),
                    };
                    let env_ref = spec.env_ref().expect("managed checkout should have an env_ref").to_string();
                    let mut labels = BTreeMap::from([(ENV_LABEL.to_string(), env_ref), (REPO_KEY_LABEL.to_string(), repo_key)]);
                    if let Some(change_request) =
                        convoy.spec.change_request.as_ref().filter(|change_request| change_request.repository_ref == repository_key)
                    {
                        labels.insert(CHANGE_REQUEST_ID_LABEL.to_string(), change_request.id.clone());
                    }
                    let mut meta = convoy_owned_child_meta(&checkout_name, &convoy, obj, labels);
                    if matches!(spec, CheckoutSpec::FreshClone(_)) {
                        // #4218: a cloud vessel clones before any crew session exists,
                        // so its checkout carries the grants that cover this repository.
                        flotilla_resources::CloneCredentials::for_repository(
                            &repository_key,
                            &requirement.credential_refs,
                            &requirement.credential_scopes,
                            &requirement.credential_permissions,
                        )
                        .annotate(&mut meta.annotations);
                    }
                    actuations.push(Actuation::CreateCheckout { meta, spec });
                    waiting_for_checkouts.push(checkout_name);
                }
                Err(err) => return Err(err),
            }
        }
        if !waiting_for_checkouts.is_empty() {
            let checkout_label = if waiting_for_checkouts.len() == 1 { "checkout" } else { "checkouts" };
            return Ok(VesselPrepared::provisioning(
                &placement,
                placement_decision.clone(),
                format!("{checkout_label} {} to become ready", waiting_for_checkouts.join(", ")),
                actuations,
            ));
        }
        if obj.status.as_ref().map_or(!degraded.is_empty(), |status| status.degraded != degraded) {
            flotilla_store::apply_status_patch(
                &self.backend.using::<Vessel>(&self.namespace),
                &obj.metadata.name,
                &VesselStatusPatch::ObserveDegradation { conditions: degraded },
            )
            .await?;
        }
        let has_repositories = !repository_refs.is_empty();
        let workspace_root = if multi_repository {
            match &strategy {
                // A vessel's clones stay where they were made, whatever the layout
                // says now (a vessel cloned at its provider's root before #4165).
                PlacementStrategy::CloneInVessel { .. } => recorded_workspace_root(checkout_paths.values()).unwrap_or(workspace_root),
                _ => workspace_root,
            }
        } else if let Some(path) = checkout_paths.values().next() {
            path.clone()
        } else {
            shared_clone_root.clone().unwrap_or_default()
        };
        let (resolved_environment_ref, image) = match &strategy {
            PlacementStrategy::HostDirect { host_ref, .. } => {
                let env_name = host_direct_environment_name(host_ref);
                let environment = match self.environments.get(&env_name).await {
                    Ok(environment) => environment,
                    Err(ResourceError::NotFound { .. }) => {
                        return Ok(VesselPrepared::failed(self.missing_host_local_environment(&env_name, host_ref)))
                    }
                    Err(err) => return Err(err),
                };
                let dependency = self.environment_dependency(&environment)?;
                if let Some(prepared) = dependency.unavailable {
                    return Ok(prepared);
                }
                if !dependency.ready {
                    return Ok(VesselPrepared::provisioning(
                        &placement,
                        placement_decision.clone(),
                        format!("environment {env_name} to become ready"),
                        actuations,
                    ));
                }
                (env_name, None)
            }
            PlacementStrategy::DockerHostCheckoutAndMount { host_ref, image, .. } => {
                let mount_path = realisation_settings::DOCKER_WORKSPACE;
                let git_mounts = match self.contained_git_mounts(&contained_worktree_checkouts).await {
                    Ok(mounts) => mounts,
                    Err(ResourceError::Invalid { message }) => return Ok(VesselPrepared::failed(message)),
                    Err(error) => return Err(error),
                };
                let env_name = environment_name(&obj.metadata.name);
                let image = match self.environments.get(&env_name).await {
                    Ok(existing) => {
                        // Docker bind mounts cannot change in place. Refuse a stale
                        // grant set until the environment is explicitly recreated.
                        let existing_git_mounts = existing
                            .spec
                            .docker
                            .as_ref()
                            .map(|docker| docker.mounts.iter().filter(|mount| mount.target_path != mount_path).cloned().collect::<Vec<_>>())
                            .unwrap_or_default();
                        if !git_mounts_match(&existing_git_mounts, &git_mounts) {
                            return Ok(VesselPrepared {
                                phase: None,
                                patch: PlannedPatch::EnvironmentRecreation {
                                    observed_kind_ref: placement.kind.metadata.name.clone(),
                                    observed_kind_version: placement.kind.metadata.resource_version.clone(),
                                    placement_decision: placement_decision.clone(),
                                    message: format!(
                                        "environment {env_name} Git checkout membership or protection mounts changed; \
                                         stop crews and recreate the environment before launching crews"
                                    ),
                                    image: composed_image(&placement, None),
                                },
                                actuations,
                            });
                        }

                        let dependency = self.environment_dependency(&existing)?;
                        if let Some(prepared) = dependency.unavailable {
                            return Ok(prepared);
                        }
                        if !dependency.ready {
                            let waiting_for = dependency.waiting.unwrap_or_else(|| format!("environment {env_name} to become ready"));
                            return Ok(VesselPrepared::provisioning(&placement, placement_decision.clone(), waiting_for, actuations));
                        }
                        match image_stamp(&existing, &placement) {
                            Ok(image) => image,
                            Err(message) if dependency.engine_held => {
                                // Readiness can commit before the provider's image observations are published.
                                return Ok(VesselPrepared::provisioning(&placement, placement_decision.clone(), message, actuations));
                            }
                            Err(message) => return Ok(VesselPrepared::failed(message)),
                        }
                    }
                    Err(ResourceError::NotFound { .. }) => {
                        let mut mounts = has_repositories
                            .then(|| EnvironmentMount {
                                source_path: workspace_root.clone(),
                                target_path: mount_path.to_owned(),
                                mode: EnvironmentMountMode::Rw,
                            })
                            .into_iter()
                            .collect::<Vec<_>>();
                        // Preserve absolute host paths in both mount domains so
                        // .git pointers and host registration backlinks remain valid.
                        mounts.extend(git_mounts);
                        let mut env = enabling_env(&required_adapters);
                        if let Err(message) = configure_contained_tracking(&mut env, &contained_branches) {
                            return Ok(VesselPrepared::failed(message));
                        }
                        let (image_ref, image_build_ref) = match environment_image(image) {
                            Ok(image) => image,
                            Err(message) => return Ok(VesselPrepared::failed(message)),
                        };
                        actuations.push(Actuation::CreateEnvironment {
                            meta: owned_child_meta(&env_name, obj, environment_provider_labels(&placement)),
                            spec: EnvironmentSpec {
                                cloud_sandbox: None,
                                provider_address: Some(
                                    flotilla_address::Address::root(flotilla_address::Kind::Host, host_ref)
                                        .and_then(|place| {
                                            place.child(
                                                flotilla_address::Kind::Provider,
                                                environment_provider_labels(&placement)
                                                    .get(ENVIRONMENT_PROVIDER_INSTANCE_LABEL)
                                                    .map(String::as_str)
                                                    .unwrap_or("docker"),
                                            )
                                        })
                                        .map_err(|error| ResourceError::other(error.to_string()))?,
                                ),
                                host_direct: None,
                                docker: Some(DockerEnvironmentSpec {
                                    image_build_ref,
                                    host_ref: host_ref.clone(),
                                    image: image_ref.clone(),
                                    declared_agent_adapters: declared_agent_adapters.clone(),
                                    required_agent_adapters: required_adapters.clone(),
                                    pull_policy: realisation_settings::DOCKER_PULL_POLICY,
                                    mounts,
                                    env: environment_with_credentials(
                                        {
                                            let mut env = env.clone();
                                            env.insert(
                                                flotilla_resources::CREW_SKILLS_ENV.to_string(),
                                                serde_json::to_string(
                                                    &requirement
                                                        .crew
                                                        .iter()
                                                        .filter(|crew| matches!(crew.source, CrewSource::Agent { .. }))
                                                        .map(|crew| (&crew.role, &crew.skills.selected))
                                                        .collect::<BTreeMap<_, _>>(),
                                                )
                                                .expect("serialize resolved skills"),
                                            );
                                            env
                                        },
                                        &requirement.credential_refs,
                                        &requirement.credential_scopes,
                                        &requirement.credential_permissions,
                                    ),
                                }),
                            },
                        });
                        return Ok(VesselPrepared::provisioning(
                            &placement,
                            placement_decision.clone(),
                            format!("environment {env_name} to become ready"),
                            actuations,
                        ));
                    }
                    Err(err) => return Err(err),
                };
                (env_name, Some(image))
            }
            PlacementStrategy::CloneInVessel { .. } => {
                let (environment_ref, image) = precreated_environment.expect("fresh-clone strategy should resolve environment first");
                (environment_ref, Some(image))
            }
        };
        let terminal_cwd = strategy.terminal_cwd(&workspace_root, has_repositories);
        let issue_repository_refs = convoy.spec.issues.iter().map(|issue| issue.repository_ref.as_ref()).collect::<Vec<_>>();
        let copy_to_all_repositories = issue_repository_refs.iter().any(|repository_ref| repository_ref.is_none());
        let brief_copies = checkout_paths
            .iter()
            .filter(|(repo_ref, _)| {
                copy_to_all_repositories
                    || issue_repository_refs.is_empty()
                    || issue_repository_refs.iter().flatten().any(|relevant| *relevant == *repo_ref)
            })
            .map(|(repo_ref, path)| match &strategy {
                PlacementStrategy::DockerHostCheckoutAndMount { .. } => {
                    let mount_path = realisation_settings::DOCKER_WORKSPACE;
                    if multi_repository {
                        format!("{}/{}", mount_path.trim_end_matches('/'), workspace_slugs[repo_ref])
                    } else {
                        mount_path.to_owned()
                    }
                }
                PlacementStrategy::HostDirect { .. } | PlacementStrategy::CloneInVessel { .. } => path.clone(),
            })
            .collect::<Vec<_>>();

        let mut terminal_refs = Vec::new();
        let starts_active = |index: usize, process: &flotilla_resources::CrewSpec| {
            requirement.starts_eagerly(index)
                || convoy
                    .status
                    .as_ref()
                    .and_then(|status| status.crew_work.get(&obj.spec.vessel_name))
                    .and_then(|crew| crew.get(&process.role))
                    .is_some_and(|work| matches!(work.phase, CrewWorkPhase::Working | CrewWorkPhase::Interrupted))
        };
        for (crew_index, process) in requirement.crew.iter().enumerate() {
            let should_start = starts_active(crew_index, process);
            let identity = TerminalSessionIdentity::builder()
                .vessel_ref(obj.metadata.name.clone())
                .convoy(obj.spec.convoy_ref.clone())
                .vessel(obj.spec.vessel_name.clone())
                .role(process.role.clone())
                .vessel_index(vessel_index)
                .crew_index(crew_index)
                .labels(process.labels.clone())
                .build();
            let terminal_name = identity.name();
            match self.terminal_sessions.get(&terminal_name).await {
                Ok(existing) => {
                    let (existing, launch) =
                        flotilla_store::read_terminal_session(Arc::clone(&self.lifecycle_reads), &existing, None).await?;
                    terminal_refs.push(terminal_name.clone());
                    let phase = existing.status.as_ref().map(|status| status.phase);
                    if phase == Some(TerminalSessionPhase::Failed) {
                        let message = existing
                            .status
                            .as_ref()
                            .and_then(|status| status.message.clone())
                            .unwrap_or_else(|| format!("terminal session {terminal_name} stopped"));
                        return Ok(VesselPrepared::failed(message));
                    }
                    if phase == Some(TerminalSessionPhase::Stopped) {
                        if matches!(process.source, CrewSource::Agent { .. }) {
                            let work_phase =
                                convoy.status.as_ref().and_then(|status| status.work.get(&obj.spec.vessel_name)).map(|work| work.phase);
                            let crew_phase = convoy
                                .status
                                .as_ref()
                                .and_then(|status| status.crew_work.get(&obj.spec.vessel_name))
                                .and_then(|crew| crew.get(&process.role))
                                .map(|work| work.phase);
                            let active = matches!(crew_phase, Some(CrewWorkPhase::Working | CrewWorkPhase::Interrupted))
                                || (matches!(crew_phase, None | Some(CrewWorkPhase::Pending))
                                    && should_start
                                    && matches!(work_phase, Some(WorkPhase::Launching | WorkPhase::Running)));
                            if !active {
                                continue;
                            }
                            return Ok(VesselPrepared::interrupted(
                                process.role.clone(),
                                &terminal_name,
                                work_phase == Some(WorkPhase::Interrupted),
                                launch,
                                actuations,
                            ));
                        }
                        let message = existing
                            .status
                            .as_ref()
                            .and_then(|status| status.message.clone())
                            .unwrap_or_else(|| format!("terminal session {terminal_name} stopped"));
                        return Ok(VesselPrepared::failed(message));
                    }
                    if phase == Some(TerminalSessionPhase::Starting) || (should_start && phase != Some(TerminalSessionPhase::Running)) {
                        return Ok(VesselPrepared::provisioning(
                            &placement,
                            placement_decision.clone(),
                            format!("terminal session {terminal_name} to become running"),
                            actuations,
                        ));
                    }
                }
                Err(ResourceError::NotFound { .. }) => {
                    if !should_start {
                        continue;
                    }
                    // #3907: a requester whose binding was revoked waits with the
                    // reason; only a rebind or a new authorization starts it again.
                    if let Some(reason) = self.account_wait(&obj.spec.convoy_ref, process).await? {
                        return Ok(VesselPrepared::provisioning(&placement, placement_decision.clone(), reason, actuations));
                    }
                    let source = match &process.source {
                        CrewSource::Tool { command } => flotilla_resources::TerminalSessionSource::Tool { command: command.clone() },
                        CrewSource::Agent { selector, prompt, brief_template } => {
                            let context = flotilla_resources::TerminalCrewContext {
                                namespace: self.namespace.clone(),
                                convoy: obj.spec.convoy_ref.clone(),
                                vessel_ref: obj.metadata.name.clone(),
                            };
                            let members = requirement
                                .crew
                                .iter()
                                .enumerate()
                                .map(|(index, member)| CrewBriefMember {
                                    role: member.role.clone(),
                                    state: if starts_active(index, member) { "active" } else { "latent" }.to_string(),
                                    is_agent: matches!(member.source, CrewSource::Agent { .. }),
                                })
                                .collect::<Vec<_>>();
                            let assignment = match prompt.as_deref() {
                                Some(prompt) => CrewAssignment::Prompt(prompt),
                                None if !convoy.spec.issues.is_empty() => CrewAssignment::CarriedIssue,
                                None if convoy.spec.change_request.is_some() => CrewAssignment::CarriedChangeRequest,
                                None => CrewAssignment::Unassigned,
                            };
                            let mut render_options = self.brief_templates.render_options_with_fork_stance(
                                brief_template.as_deref(),
                                convoy.spec.project_ref.as_deref(),
                                checkout_paths.values().map(PathBuf::from),
                                fork_stance,
                            );
                            render_options.apply_cascade(
                                convoy
                                    .status
                                    .as_ref()
                                    .and_then(|status| status.workflow_snapshot.as_ref())
                                    .and_then(|workflow| workflow.cascade.as_deref()),
                                &process.role,
                            );
                            render_options.has_credential_scope = !requirement.credential_scopes.is_empty();
                            let mut brief = if convoy.metadata.annotations.contains_key(BRIEF_ARTIFACTS_ANNOTATION) {
                                let name = artifact_slot_record_name(&context.convoy, &process.role, "brief", &context.convoy)
                                    .map_err(ResourceError::invalid)?;
                                let artifact = match self.artifacts.get(&name).await {
                                    Ok(artifact) => artifact.object,
                                    Err(ResourceError::NotFound { .. }) => {
                                        return Ok(VesselPrepared::provisioning(
                                            &placement,
                                            placement_decision.clone(),
                                            format!("brief artifact {name} to replicate"),
                                            actuations,
                                        ));
                                    }
                                    Err(error) => return Err(error),
                                };
                                flotilla_resources::TerminalBrief {
                                    path: flotilla_agent::adapter::crew_brief_path(&process.role),
                                    content: String::new(),
                                    artifact_digest: Some(artifact.spec.digest),
                                    copies: Vec::new(),
                                }
                            } else {
                                let mut brief = match build_convoy_crew_brief_with_options(
                                    &convoy,
                                    &context,
                                    &obj.spec.vessel_name,
                                    &process.role,
                                    assignment,
                                    &members,
                                    &render_options,
                                ) {
                                    Ok(brief) => brief,
                                    Err(message) => return Ok(VesselPrepared::failed(message)),
                                };
                                append_convoy_work_context(
                                    &mut brief.content,
                                    &convoy,
                                    &repository_refs,
                                    &requirement.credential_scopes,
                                    Some(&terminal_cwd),
                                );
                                if let Some(project) = convoy.spec.project_ref.as_deref() {
                                    let address = format!("{project}/{}/{}/{}", convoy.metadata.name, obj.spec.vessel_name, process.role);
                                    let book = flotilla_messaging::crew_address_book(&self.backend, &self.namespace, &address).await?;
                                    brief.content.push('\n');
                                    brief.content.push_str(&book.render());
                                }
                                brief
                            };
                            brief.copies = brief_copies.clone();
                            flotilla_resources::TerminalSessionSource::Agent {
                                selector: selector.clone(),
                                brief,
                                context: Box::new(context),
                                message: None,
                            }
                        }
                    };
                    let mut terminal_meta = identity.input_meta();
                    if let Some(revision) = convoy
                        .status
                        .as_ref()
                        .and_then(|status| status.workflow_snapshot.as_ref())
                        .and_then(|workflow| workflow.cascade.as_ref())
                        .and_then(|cascade| cascade.charter_commit.as_ref())
                    {
                        terminal_meta.annotations.insert(flotilla_messaging::BRIEF_CHARTER_REVISION_ANNOTATION.into(), revision.clone());
                    }
                    terminal_meta.annotations.extend(actuator_annotations(obj));
                    if !requirement.credential_refs.is_empty() {
                        terminal_meta.annotations.insert(
                            CREDENTIAL_REFS_ANNOTATION.to_string(),
                            serde_json::to_string(&requirement.credential_refs).expect("credential names serialize"),
                        );
                    }
                    if !requirement.credential_scopes.is_empty() {
                        terminal_meta.annotations.insert(
                            CREDENTIAL_SCOPES_ANNOTATION.to_string(),
                            serde_json::to_string(&requirement.credential_scopes).expect("credential scopes serialize"),
                        );
                    }
                    if !requirement.credential_permissions.is_empty() {
                        terminal_meta.annotations.insert(
                            CREDENTIAL_PERMISSIONS_ANNOTATION.to_string(),
                            serde_json::to_string(&requirement.credential_permissions).expect("credential permissions serialize"),
                        );
                    }
                    actuations.push(Actuation::CreateTerminalSession {
                        meta: terminal_meta,
                        spec: TerminalSessionSpec {
                            env_ref: resolved_environment_ref.clone(),
                            role: process.role.clone(),
                            source,
                            cwd: terminal_cwd.clone(),
                            env: Default::default(),
                            pool: strategy.pool().to_string(),
                        },
                    });
                    return Ok(VesselPrepared::provisioning(
                        &placement,
                        placement_decision.clone(),
                        format!("terminal session {terminal_name} to become running"),
                        actuations,
                    ));
                }
                Err(err) => return Err(err),
            }
        }

        let mut configured_limits =
            self.environments.get(&resolved_environment_ref).await?.status.and_then(|status| status.configured_limits);
        for terminal_ref in &terminal_refs {
            if let Some(limits) = self.terminal_sessions.get(terminal_ref).await?.status.and_then(|status| status.configured_limits) {
                // Only host-direct launches record terminal limits today. Merge
                // known fields so future launch caps cannot erase a container quota.
                configured_limits = Some(configured_limits.take().unwrap_or_default().with_overrides(limits));
                break;
            }
        }
        Ok(VesselPrepared {
            phase: None,
            patch: PlannedPatch::Ready {
                configured_limits,
                placement_decision,
                environment_ref: resolved_environment_ref,
                image,
                checkout_refs,
                terminal_session_refs: terminal_refs,
                requested_stance: effective_stance,
                effective_stance,
            },
            actuations,
        })
    }
}

impl Reconciler for VesselReconciler {
    type Resource = Vessel;
    type Prepared = VesselPrepared;

    async fn prepare(&self, obj: &ResourceObject<Self::Resource>) -> Result<Self::Prepared, ResourceError> {
        // Controllers mutate this home's Vessel records; replicas are read-only view inputs.
        let reading = self.lifecycle_reads.try_reading(ProvisioningKind::Vessel, &obj.metadata.name, None).map_err(ResourceError::other)?;
        let phase = VesselPhase::from_reading(reading.as_ref(), obj.status.as_ref());
        let mut prepared =
            if matches!(phase, VesselPhase::Failed | VesselPhase::Lost) { VesselPrepared::none() } else { self.prepare_plan(obj).await? };
        prepared.phase = Some(phase);
        Ok(prepared)
    }

    fn reconcile(
        &self,
        obj: &ResourceObject<Self::Resource>,
        prepared: &Self::Prepared,
        now: DateTime<Utc>,
    ) -> ReconcileOutcome<Self::Resource> {
        let patch =
            match &prepared.patch {
                PlannedPatch::None | PlannedPatch::Waiting => None,
                PlannedPatch::Provisioning { observed_kind_ref, observed_kind_version, placement_decision, waiting_for, image } => {
                    Some(VesselStatusPatch::MarkProvisioning {
                        observed_kind_ref: observed_kind_ref.clone(),
                        observed_kind_version: observed_kind_version.clone(),
                        placement_decision: placement_decision.clone(),
                        started_at: now,
                        message: provisioning_stuck_message(obj, prepared.phase, waiting_for, now),
                        image: image.clone(),
                    })
                }
                PlannedPatch::EnvironmentRecreation { message, .. } if prepared.phase == Some(VesselPhase::Ready) => {
                    (obj.status.as_ref().and_then(|status| status.message.as_deref()) != Some(message))
                        .then(|| VesselStatusPatch::RequireEnvironmentRecreation { message: message.clone() })
                }
                PlannedPatch::CheckoutChurn { observed_kind_ref, observed_kind_version, placement_decision, message, image }
                | PlannedPatch::EnvironmentRecreation { observed_kind_ref, observed_kind_version, placement_decision, message, image } => {
                    if obj.status.as_ref().is_some_and(|status| {
                        prepared.phase == Some(VesselPhase::Provisioning) && status.message.as_deref() == Some(message)
                    }) {
                        None
                    } else {
                        Some(VesselStatusPatch::MarkProvisioning {
                            observed_kind_ref: observed_kind_ref.clone(),
                            observed_kind_version: observed_kind_version.clone(),
                            placement_decision: placement_decision.clone(),
                            started_at: now,
                            message: Some(message.clone()),
                            image: image.clone(),
                        })
                    }
                }
                PlannedPatch::Ready {
                    configured_limits,
                    placement_decision,
                    environment_ref,
                    image,
                    checkout_refs,
                    terminal_session_refs,
                    requested_stance,
                    effective_stance,
                } => Some(VesselStatusPatch::MarkReady {
                    configured_limits: configured_limits.clone(),
                    placement_decision: placement_decision.clone(),
                    environment_ref: Some(environment_ref.clone()),
                    image_ref: image.as_ref().map(|image| image.image_ref.clone()),
                    local_image_id: image.as_ref().map(|image| image.local_image_id.clone()),
                    registry_digest: image.as_ref().and_then(|image| image.registry_digest.clone()),
                    checkout_refs: checkout_refs.clone(),
                    terminal_session_refs: terminal_session_refs.clone(),
                    requested_stance: *requested_stance,
                    effective_stance: *effective_stance,
                    ready_at: now,
                    image: image.as_ref().and_then(|image| image.composed.clone()),
                }),
                PlannedPatch::Interrupted { roles, message }
                    if obj.status.as_ref().is_none_or(|status| {
                        prepared.phase != Some(VesselPhase::Interrupted)
                            || status.interrupted_roles != *roles
                            || status.message.as_deref() != Some(message.as_str())
                    }) =>
                {
                    Some(VesselStatusPatch::MarkInterrupted { roles: roles.clone(), message: message.clone() })
                }
                PlannedPatch::Interrupted { .. } => None,
                PlannedPatch::Lost { message } => Some(VesselStatusPatch::MarkLost { message: message.clone() }),
                PlannedPatch::Failed { message } => Some(VesselStatusPatch::MarkFailed { message: message.clone() }),
            };

        let mut outcome = ReconcileOutcome::with_actuations(patch, prepared.actuations.clone());
        if matches!(&prepared.patch, PlannedPatch::CheckoutChurn { .. }) {
            outcome.requeue_after = Some(CHECKOUT_RECREATE_BACKOFF);
        } else if matches!(
            &prepared.patch,
            PlannedPatch::Provisioning { .. } | PlannedPatch::EnvironmentRecreation { .. } | PlannedPatch::Waiting
        ) {
            outcome.requeue_after = Some(VESSEL_PROVISIONING_REQUEUE_AFTER);
        } else if matches!(&prepared.patch, PlannedPatch::Interrupted { .. }) {
            outcome.requeue_after = Some(VESSEL_INTERRUPTED_REQUEUE_AFTER);
        }
        outcome
    }

    async fn run_finalizer(&self, obj: &ResourceObject<Self::Resource>) -> Result<(), ResourceError> {
        let selector = BTreeMap::from([(VESSEL_REF_LABEL.to_string(), obj.metadata.name.clone())]);

        delete_lifecycle_owned_matching(&self.terminal_sessions, &selector).await?;
        // Checkouts are convoy-owned and are removed by the convoy finalizer's
        // ownership cascade, independently of vessel teardown.
        delete_lifecycle_owned_matching(&self.environments, &selector).await?;

        Ok(())
    }

    fn finalizer_name(&self) -> Option<&'static str> {
        Some("flotilla.work/vessel-workspace-teardown")
    }
}

fn image_stamp(environment: &ResourceObject<Environment>, placement: &Placement) -> Result<ImageStamp, String> {
    let status =
        environment.status.as_ref().ok_or_else(|| format!("environment {} awaiting image observations", environment.metadata.name))?;
    let image_ref =
        status.image_ref.clone().ok_or_else(|| format!("environment {} is missing its image ref", environment.metadata.name))?;
    let local_image_id =
        status.local_image_id.clone().ok_or_else(|| format!("environment {} is missing its image digest", environment.metadata.name))?;
    // A composed environment provisioned from its build's digest.
    let composed = environment
        .spec
        .docker
        .as_ref()
        .and_then(|docker| docker.image_build_ref.as_ref())
        .and_then(|_| composed_image(placement, Some(image_ref.clone())));
    Ok(ImageStamp { image_ref, local_image_id, registry_digest: status.registry_digest.clone(), composed })
}

/// Git's ordinary upstream writes target the common config, even with
/// extensions.worktreeConfig. Supply branch tracking at process scope instead:
/// plain push publishes the current branch and pull follows that same branch.
/// This leaves the host-owned remotes, hooks, and config overlays untouched and
/// does not enable an extension that the host config guard deliberately rejects.
fn configure_contained_tracking(env: &mut BTreeMap<String, String>, branches: &BTreeSet<String>) -> Result<(), String> {
    if branches.is_empty() {
        return Ok(());
    }
    let mut count = env
        .get("GIT_CONFIG_COUNT")
        .map_or(Ok(0), |value| value.parse::<usize>())
        .map_err(|_| "invalid GIT_CONFIG_COUNT in contained vessel environment".to_string())?;
    let mut settings = vec![("push.default".to_string(), "current".to_string())];
    for branch in branches {
        settings.push((format!("branch.{branch}.remote"), "origin".to_string()));
        settings.push((format!("branch.{branch}.merge"), format!("refs/heads/{branch}")));
    }
    for (key, value) in settings {
        let next = count.checked_add(1).ok_or("GIT_CONFIG_COUNT overflow in contained vessel environment")?;
        env.insert(format!("GIT_CONFIG_KEY_{count}"), key);
        env.insert(format!("GIT_CONFIG_VALUE_{count}"), value);
        count = next;
    }
    env.insert("GIT_CONFIG_COUNT".to_string(), count.to_string());
    Ok(())
}

fn environment_with_credentials(
    mut env: BTreeMap<String, String>,
    credentials: &std::collections::BTreeSet<String>,
    scopes: &BTreeMap<String, BTreeSet<flotilla_resources::RepositoryKey>>,
    permissions: &BTreeMap<String, BTreeMap<String, String>>,
) -> BTreeMap<String, String> {
    if !credentials.is_empty() {
        env.insert(CREDENTIAL_REFS_ENV.to_string(), serde_json::to_string(credentials).expect("credential names serialize"));
    }
    if !scopes.is_empty() {
        env.insert(CREDENTIAL_SCOPES_ENV.to_string(), serde_json::to_string(scopes).expect("credential scopes serialize"));
    }
    if !permissions.is_empty() {
        env.insert(CREDENTIAL_PERMISSIONS_ENV.to_string(), serde_json::to_string(permissions).expect("credential permissions serialize"));
    }
    env
}

fn placement_strategy(spec: &FulfilmentKindSpec) -> PlacementStrategy {
    let (host_ref, pool) = (spec.host_ref.clone(), spec.pool.clone());
    match &spec.realisation {
        FulfilmentRealisation::HostDirect => PlacementStrategy::HostDirect { host_ref, pool },
        FulfilmentRealisation::DockerPerVessel { image } => {
            PlacementStrategy::DockerHostCheckoutAndMount { host_ref, pool, image: image.clone() }
        }
        // The guest's root comes from the frozen decision.
        FulfilmentRealisation::CloudSandboxPerVessel { image, .. } => {
            PlacementStrategy::CloneInVessel { host_ref: String::new(), pool, image: image.clone(), clone_path: String::new() }
        }
    }
}

fn provisioning_patch(placement: &Placement, placement_decision: Option<PlacementDecision>, waiting_for: String) -> PlannedPatch {
    PlannedPatch::Provisioning {
        observed_kind_ref: placement.kind.metadata.name.clone(),
        observed_kind_version: placement.kind.metadata.resource_version.clone(),
        placement_decision,
        waiting_for,
        image: composed_image(placement, None),
    }
}

fn provisioning_stuck_message(
    obj: &ResourceObject<Vessel>,
    phase: Option<VesselPhase>,
    waiting_for: &str,
    now: DateTime<Utc>,
) -> Option<String> {
    let started_at = obj.status.as_ref().filter(|_| phase == Some(VesselPhase::Provisioning)).and_then(|status| status.started_at)?;
    (now.signed_duration_since(started_at) >= chrono::Duration::seconds(VESSEL_PROVISIONING_STUCK_SECONDS))
        .then(|| format!("provisioning is stalled while waiting for {waiting_for}; reconciliation will continue retrying"))
}

fn legible_waiting_for(mut waiting_for: String, placement_decision: Option<&PlacementDecision>) -> String {
    if let Some(decision) = placement_decision {
        let target_environment = host_direct_environment_name(decision.target_host.reference.as_str());
        let display_environment = host_direct_environment_name(&decision.target_host.display_name);
        waiting_for = waiting_for.replace(&target_environment, &display_environment);
    }
    waiting_for
}

fn environment_name(vessel_name: &str) -> String {
    format!("env-{vessel_name}")
}

fn checkout_name(vessel_name: &str, workspace_slug: &str, multi_repository: bool, placement_scope: Option<&str>) -> String {
    let repository_suffix = if multi_repository { format!("-{workspace_slug}") } else { String::new() };
    let placement_suffix = placement_scope.map(|scope| format!("-{scope}")).unwrap_or_default();
    format!("checkout-{vessel_name}{repository_suffix}{placement_suffix}")
}

fn checkout_placement_scope(convoy: &ResourceObject<Convoy>, host_ref: Option<&str>) -> Option<String> {
    let host_ref = host_ref?;
    let mut hash = Sha256::new();
    hash.update(b"managed-checkout-placement-v1\0");
    hash.update(host_ref.as_bytes());
    hash.update([0]);
    hash.update(convoy.metadata.creation_timestamp.to_rfc3339().as_bytes());
    hash.update([0]);
    if let Some(origin_root) = convoy.metadata.annotations.get(ACTUATOR_SOURCE_ROOT_ANNOTATION) {
        hash.update(origin_root.as_bytes());
    }
    let digest = format!("{:x}", hash.finalize());
    Some(digest[..12].to_string())
}

fn checkout_target_path(repo_default_dir: &str, convoy_slug: &str, repo_slug: &str, branch_slug: &str) -> String {
    format!("{}/{}/{}.{}", repo_default_dir.trim_end_matches('/'), convoy_slug, repo_slug, branch_slug)
}

/// A cloud sandbox is provisioned by its own provider instance from typed
/// inputs alone: no Docker spec, and nothing smuggled through its env (#3981).
/// The variables that enable the crew's adapters in its vessel, as each place's discovery reads them.
fn enabling_env(required_adapters: &BTreeSet<String>) -> BTreeMap<String, String> {
    required_adapters
        .iter()
        .filter_map(|adapter| adapter_enabling_env(adapter))
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

fn cloud_environment_spec(
    placement: &Placement,
    image: &str,
    probe_needs: BTreeSet<flotilla_resources::CapabilityNeed>,
    required_adapters: &BTreeSet<String>,
) -> Result<EnvironmentSpec, String> {
    let instance = placement
        .instance
        .as_ref()
        .ok_or_else(|| format!("cloud placement {} names no provider instance", placement.kind.metadata.name))?;
    let address = flotilla_address::Address::root(flotilla_address::Kind::Account, instance)
        .and_then(|account| account.child(flotilla_address::Kind::Provider, instance))
        .map_err(|error| error.to_string())?;
    let size = match &placement.kind.spec.realisation {
        FulfilmentRealisation::CloudSandboxPerVessel { size: Some(size), .. } => size.clone(),
        _ => realisation_settings::CLOUD_SANDBOX_SIZE,
    };
    Ok(EnvironmentSpec {
        provider_address: Some(address),
        cloud_sandbox: Some(flotilla_resources::CloudSandboxEnvironmentSpec {
            image: image.to_owned(),
            size,
            lifetime_cap_seconds: None,
            probe_needs,
            env: enabling_env(required_adapters),
        }),
        ..EnvironmentSpec::default()
    })
}

/// The directory a vessel's recorded checkouts sit side by side in, when they
/// all share one; otherwise the layout's own root stands.
fn recorded_workspace_root<'a>(paths: impl IntoIterator<Item = &'a String>) -> Option<String> {
    let mut parents = paths.into_iter().map(|path| std::path::Path::new(path).parent());
    let first = parents.next()??;
    parents.all(|parent| parent == Some(first)).then(|| first.to_string_lossy().into_owned())
}

/// Where a vessel clones below its provider's workspace root: a directory of
/// its own, since where vessels share one filesystem (drydock's `local`) the
/// root is theirs together.
fn vessel_clone_path(workspace_root: &str, vessel: &str) -> String {
    format!("{}/{vessel}", workspace_root.trim_end_matches('/'))
}

fn environment_provider_labels(placement: &Placement) -> BTreeMap<String, String> {
    placement.instance.iter().map(|instance| (ENVIRONMENT_PROVIDER_INSTANCE_LABEL.into(), instance.clone())).collect()
}

fn owned_child_meta(name: &str, workspace: &ResourceObject<Vessel>, mut extra_labels: BTreeMap<String, String>) -> InputMeta {
    extra_labels.insert(VESSEL_REF_LABEL.to_string(), workspace.metadata.name.clone());
    InputMeta::builder()
        .name(name.to_string())
        .labels(extra_labels)
        .annotations(actuator_annotations(workspace))
        .owner_references(vec![OwnerReference {
            api_version: format!("{}/{}", Vessel::API_PATHS.group, Vessel::API_PATHS.version),
            kind: Vessel::API_PATHS.kind.to_string(),
            name: workspace.metadata.name.clone(),
            controller: true,
        }])
        .build()
}

fn actuator_annotations(workspace: &ResourceObject<Vessel>) -> BTreeMap<String, String> {
    [ACTUATOR_HOST_REF_ANNOTATION, ACTUATOR_SOURCE_ROOT_ANNOTATION]
        .into_iter()
        .filter_map(|key| workspace.metadata.annotations.get(key).map(|value| (key.to_string(), value.clone())))
        .collect()
}

fn convoy_owned_child_meta(
    name: &str,
    convoy: &ResourceObject<Convoy>,
    workspace: &ResourceObject<Vessel>,
    mut extra_labels: BTreeMap<String, String>,
) -> InputMeta {
    extra_labels.insert(CONVOY_LABEL.to_string(), convoy.metadata.name.clone());
    InputMeta::builder()
        .name(name.to_string())
        .labels(extra_labels)
        .annotations(actuator_annotations(workspace))
        .owner_references(vec![OwnerReference {
            api_version: format!("{}/{}", Convoy::API_PATHS.group, Convoy::API_PATHS.version),
            kind: Convoy::API_PATHS.kind.to_string(),
            name: convoy.metadata.name.clone(),
            controller: true,
        }])
        .build()
}

impl PlacementStrategy {
    fn effective_stance(&self) -> Stance {
        match self {
            Self::HostDirect { .. } => Stance::Trusted,
            Self::DockerHostCheckoutAndMount { .. } | Self::CloneInVessel { .. } => Stance::Contained,
        }
    }

    fn host_ref(&self) -> &str {
        match self {
            Self::HostDirect { host_ref, .. }
            | Self::DockerHostCheckoutAndMount { host_ref, .. }
            | Self::CloneInVessel { host_ref, .. } => host_ref,
        }
    }

    fn pool(&self) -> &str {
        match self {
            Self::HostDirect { pool, .. } | Self::DockerHostCheckoutAndMount { pool, .. } | Self::CloneInVessel { pool, .. } => pool,
        }
    }

    fn needs_shared_clone(&self) -> bool {
        !matches!(self, Self::CloneInVessel { .. })
    }

    fn terminal_cwd(&self, workspace_root: &str, has_repositories: bool) -> String {
        match self {
            Self::HostDirect { .. } => workspace_root.to_string(),
            Self::DockerHostCheckoutAndMount { .. } => realisation_settings::DOCKER_WORKSPACE.to_string(),
            // Where the vessel's checkouts are recorded: one checkout's path, or several's parent.
            Self::CloneInVessel { .. } if has_repositories => workspace_root.to_string(),
            Self::CloneInVessel { .. } => "/".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        sync::{Arc, Mutex},
    };

    use flotilla_protocol::{NodeId, PlacementDecision, PlacementTargetHost};
    use flotilla_resources::{
        Convoy, ConvoySpec, FulfilmentKind, FulfilmentKindSpec, FulfilmentRealisation, HostDirectPlacementPolicyCheckout,
        HostDirectPlacementPolicySpec, InputMeta, PlacementPolicy, PlacementPolicySpec, ResourceError, Vessel, VesselSpec,
        ACTUATOR_SOURCE_ROOT_ANNOTATION, PLACEMENT_SNAPSHOT_KIND, PREPARED_SNAPSHOT_LABEL,
    };
    use flotilla_store::ResourceBackend;

    use super::{
        checkout_name, checkout_placement_scope, configure_contained_tracking, environment_provider_labels, environment_with_credentials,
        legible_waiting_for, placement_strategy, PlacementStrategy, VesselReconciler,
    };

    // #2682: deduplicate shared parents, retain every own-admin overlay, and
    // refuse invalid/unavailable metadata instead of granting writable siblings.
    #[tokio::test]
    async fn contained_mounts_cover_empty_duplicate_and_invalid_metadata() {
        use flotilla_resources::{Checkout, CheckoutSpec, CheckoutWorktreeSpec, Clone, CloneSpec, EnvironmentMountMode, RepositoryKey};
        use flotilla_vcs::vcs::WorktreeMetadata;
        struct Metadata(Result<WorktreeMetadata, String>);
        // VCS process boundary fake; stores and mount planning are real.
        #[async_trait::async_trait]
        impl super::WorktreeMetadataResolver for Metadata {
            async fn worktree_metadata(&self, _env_ref: &str, _target: &str) -> Result<WorktreeMetadata, String> {
                self.0.clone()
            }
        }
        let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
        let namespace = "test-mounts";
        let repo_ref = RepositoryKey("test-repo".into());
        backend
            .clone()
            .using::<Clone>(namespace)
            .create(
                &InputMeta::builder().name("clone".to_string()).build(),
                &CloneSpec {
                    repo_ref: repo_ref.clone(),
                    url: "https://example.com/repo".into(),
                    env_ref: "host".into(),
                    path: "/clone".into(),
                },
            )
            .await
            .expect("clone");
        backend
            .clone()
            .using::<Checkout>(namespace)
            .create(
                &InputMeta::builder().name("own".to_string()).build(),
                &CheckoutSpec::Worktree(CheckoutWorktreeSpec {
                    repo_ref,
                    env_ref: "host".into(),
                    r#ref: "branch".into(),
                    base_ref: None,
                    target_path: "/own".into(),
                    clone_ref: "clone".into(),
                }),
            )
            .await
            .expect("checkout");
        let valid = WorktreeMetadata { common_dir: "/clone/.git".into(), admin_dir: "/clone/.git/worktrees/actual".into() };
        let reconciler = VesselReconciler::new(backend.clone(), namespace).with_worktree_metadata(Arc::new(Metadata(Ok(valid.clone()))));
        assert!(reconciler.contained_git_mounts(&[]).await.expect("empty").is_empty());
        let own = ("own".into(), Some("clone".into()));
        let one = reconciler.contained_git_mounts(std::slice::from_ref(&own)).await.expect("own mounts");
        let repeated = reconciler.contained_git_mounts(&[own.clone(), own.clone()]).await.expect("duplicate");
        assert_eq!(one, repeated, "duplicate checkout membership must not duplicate grants");
        assert_eq!(one.len(), 5);
        assert_eq!(one[3].source_path, "/clone/.git/worktrees");
        assert_eq!(one[3].mode, EnvironmentMountMode::Ro);
        assert_eq!(one[4].source_path, "/clone/.git/worktrees/actual");
        assert_eq!(one[4].mode, EnvironmentMountMode::Rw);
        for metadata in [
            Err("transport unavailable".into()),
            Ok(WorktreeMetadata { common_dir: "relative/.git".into(), ..valid.clone() }),
            Ok(WorktreeMetadata { admin_dir: "/clone/.git".into(), ..valid.clone() }),
            Ok(WorktreeMetadata { admin_dir: "/sibling".into(), ..valid.clone() }),
            Ok(WorktreeMetadata { admin_dir: "/clone/.git/worktrees/own/nested".into(), ..valid.clone() }),
        ] {
            let refusing = VesselReconciler::new(backend.clone(), namespace).with_worktree_metadata(Arc::new(Metadata(metadata)));
            assert!(refusing.contained_git_mounts(std::slice::from_ref(&own)).await.is_err(), "invalid metadata must fail closed");
        }
        let unavailable = VesselReconciler::new(backend, namespace);
        assert!(
            matches!(unavailable.contained_git_mounts(&[own]).await, Err(ResourceError::Other { .. })),
            "missing resolver must retry without guessing paths or failing the vessel"
        );
    }

    // Glue: append Git's fixed settings without replacing policy or credential environment.
    #[test]
    fn contained_tracking_appends_policy_config_and_deduplicates_branches() {
        let mut env = BTreeMap::from([
            ("GIT_CONFIG_COUNT".into(), "1".into()),
            ("GIT_CONFIG_KEY_0".into(), "pull.ff".into()),
            ("GIT_CONFIG_VALUE_0".into(), "only".into()),
            ("GIT_CONFIG_GLOBAL".into(), "/credentials/gitconfig".into()),
        ]);
        configure_contained_tracking(&mut env, &BTreeSet::from(["fix/a".into(), "fix/b".into(), "fix/a".into()])).expect("tracking config");
        assert_eq!(env["GIT_CONFIG_COUNT"], "6");
        assert_eq!(env["GIT_CONFIG_KEY_0"], "pull.ff");
        assert_eq!(env["GIT_CONFIG_VALUE_0"], "only");
        assert_eq!(env["GIT_CONFIG_GLOBAL"], "/credentials/gitconfig");
        assert_eq!(env["GIT_CONFIG_KEY_2"], "branch.fix/a.remote");
        assert_eq!(env["GIT_CONFIG_VALUE_3"], "refs/heads/fix/a");
        assert_eq!(env["GIT_CONFIG_KEY_4"], "branch.fix/b.remote");
        assert_eq!(env["GIT_CONFIG_VALUE_5"], "refs/heads/fix/b");
    }

    // Empty vessels keep their environment; malformed/overflowing policy counts fail closed.
    #[test]
    fn contained_tracking_handles_empty_and_invalid_config_counts() {
        let mut env = BTreeMap::new();
        configure_contained_tracking(&mut env, &BTreeSet::new()).expect("empty vessel");
        assert!(env.is_empty());
        for count in ["", "-1", "invalid", &usize::MAX.to_string()] {
            let mut env = BTreeMap::from([("GIT_CONFIG_COUNT".into(), count.into())]);
            assert!(configure_contained_tracking(&mut env, &BTreeSet::from(["main".into()])).is_err());
        }
    }

    #[derive(Clone)]
    struct LogCaptureWriter(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for LogCaptureWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("log output lock should be healthy").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn snapshot_meta(name: &str) -> InputMeta {
        InputMeta::builder()
            .name(name.to_string())
            .labels(BTreeMap::from([(PREPARED_SNAPSHOT_LABEL.to_string(), PLACEMENT_SNAPSHOT_KIND.to_string())]))
            .build()
    }

    fn decision(provider_instance: Option<&str>) -> PlacementDecision {
        PlacementDecision {
            provider_instance: provider_instance.map(str::to_owned),
            provider_workspace_root: None,
            minimal_alternatives: Vec::new(),
            escalation_reason: None,
            kind: "kind".to_string(),
            target_host: PlacementTargetHost {
                reference: flotilla_protocol::CanonicalHostId::resolved("host"),
                display_name: "host".into(),
            },
            refused_candidates: Vec::new(),
            viable_not_selected: Vec::new(),
            allocation: None,
        }
    }

    // A vessel reads its placement snapshot from whichever origin froze it, as a kind, or for
    // one roll as the PlacementPolicy a convoy admitted before kinds froze. Only snapshots are
    // read from another origin. The provider instance is the decision's, else the old snapshot's.
    #[tokio::test]
    async fn vessel_resolves_prepared_placement_snapshot_from_a_third_origin() {
        let author = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default()).with_local_root(NodeId::new("author"));
        let driver = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default()).with_local_root(NodeId::new("driver"));
        let kind_snapshot = "placement-snapshot-012345abcdef";
        let policy_snapshot = "placement-snapshot-fedcba543210";
        let kind = |pool: &str| {
            FulfilmentKindSpec::builder().host_ref("h".into()).pool(pool.into()).realisation(FulfilmentRealisation::HostDirect).build()
        };
        let kinds = author.using::<FulfilmentKind>("flotilla");
        kinds.create(&snapshot_meta(kind_snapshot), &kind("passthrough")).await.expect("author kind snapshot");
        kinds.create(&InputMeta::builder().name("ordinary-kind".into()).build(), &kind("cleat")).await.expect("author ordinary kind");
        let policy = PlacementPolicySpec::builder()
            .pool("zellij".to_string())
            .host_direct(HostDirectPlacementPolicySpec { host_ref: "h".into(), checkout: HostDirectPlacementPolicyCheckout::Worktree })
            .provider_requirements(flotilla_resources::ProviderRequirements { instance: Some("frozen".into()), ..Default::default() })
            .build();
        author.using::<PlacementPolicy>("flotilla").create(&snapshot_meta(policy_snapshot), &policy).await.expect("author policy snapshot");
        driver
            .replica_writer::<FulfilmentKind>(NodeId::new("author"), "flotilla")
            .replace(&kinds.list().await.expect("author kinds"), chrono::Utc::now())
            .await
            .expect("replicate kinds");
        driver
            .replica_writer::<PlacementPolicy>(NodeId::new("author"), "flotilla")
            .replace(&author.using::<PlacementPolicy>("flotilla").list().await.expect("author policies"), chrono::Utc::now())
            .await
            .expect("replicate policies");
        let reconciler = VesselReconciler::new(driver.clone(), "flotilla")
            .with_federated_dependencies(&driver, flotilla_protocol::CanonicalHostId::resolved("driver-host"));
        for (name, origin) in [("local-vessel", None), ("remote-vessel", Some("convoy-origin"))] {
            let mut meta = InputMeta::builder().name(name.to_string()).build();
            if let Some(origin) = origin {
                meta.annotations.insert(ACTUATOR_SOURCE_ROOT_ANNOTATION.to_string(), origin.to_string());
            }
            let vessel = driver
                .using::<Vessel>("flotilla")
                .create(
                    &meta,
                    &VesselSpec {
                        convoy_ref: "convoy".to_string(),
                        vessel_name: "work".to_string(),
                        fulfilment_kind_ref: kind_snapshot.to_string(),
                        adopted_checkout_refs: BTreeMap::new(),
                    },
                )
                .await
                .expect("vessel");
            let resolve = |name: &'static str, decision: Option<PlacementDecision>| {
                let (reconciler, vessel) = (&reconciler, &vessel);
                async move { reconciler.placement_dependency(vessel, name, decision.as_ref()).await }
            };
            let placement = resolve(kind_snapshot, Some(decision(Some("decided")))).await.expect("kind snapshot").expect("a kind");
            assert_eq!((placement.kind.spec.pool.as_str(), placement.instance.as_deref()), ("passthrough", Some("decided")));
            let placement = resolve(policy_snapshot, None).await.expect("policy snapshot").expect("its kind");
            assert_eq!((placement.kind.spec.pool.as_str(), placement.instance.as_deref()), ("zellij", Some("frozen")));
            assert_eq!(placement.kind.spec.realisation, FulfilmentRealisation::HostDirect);
            let placement = resolve(policy_snapshot, Some(decision(Some("decided")))).await.expect("policy snapshot").expect("its kind");
            assert_eq!(placement.instance.as_deref(), Some("decided"));
            assert!(matches!(resolve("ordinary-kind", None).await, Err(ResourceError::NotFound { .. })));
        }
    }

    // #4137: a vessel reading its convoy from a replica that lacks the frozen snapshot
    // waits for it, leaving the vessel untouched; a snapshot without the vessel fails it.
    #[tokio::test]
    async fn a_vessel_waits_for_a_replicated_convoy_snapshot() {
        use flotilla_store::controller::Reconciler;
        let author = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default()).with_local_root(NodeId::new("author"));
        let driver = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default()).with_local_root(NodeId::new("driver"));
        let kind =
            FulfilmentKindSpec::builder().host_ref("h".into()).pool("cleat".into()).realisation(FulfilmentRealisation::HostDirect).build();
        let kinds = author.using::<FulfilmentKind>("flotilla");
        kinds.create(&InputMeta::builder().name("kind".into()).build(), &kind).await.expect("kind");
        driver
            .replica_writer::<FulfilmentKind>(NodeId::new("author"), "flotilla")
            .replace(&kinds.list().await.expect("author kinds"), chrono::Utc::now())
            .await
            .expect("replicate kinds");
        let convoys = author.using::<Convoy>("flotilla");
        let convoy = convoys
            .create(&InputMeta::builder().name("convoy".into()).build(), &ConvoySpec::builder().workflow_ref("workflow".into()).build())
            .await
            .expect("convoy");
        let replicate = || async {
            driver
                .replica_writer::<Convoy>(NodeId::new("author"), "flotilla")
                .replace(&convoys.list().await.expect("author convoys"), chrono::Utc::now())
                .await
                .expect("replicate convoys")
        };
        replicate().await;
        let reconciler = VesselReconciler::new(driver.clone(), "flotilla")
            .with_federated_dependencies(&driver, flotilla_protocol::CanonicalHostId::resolved("driver-host"));
        let mut meta = InputMeta::builder().name("vessel".into()).build();
        meta.annotations.insert(ACTUATOR_SOURCE_ROOT_ANNOTATION.to_string(), "author".to_string());
        let vessel = driver
            .using::<Vessel>("flotilla")
            .create(
                &meta,
                &VesselSpec {
                    convoy_ref: "convoy".into(),
                    vessel_name: "work".into(),
                    fulfilment_kind_ref: "kind".into(),
                    adopted_checkout_refs: BTreeMap::new(),
                },
            )
            .await
            .expect("vessel");

        let prepared = reconciler.prepare(&vessel).await.expect("prepare");
        assert!(matches!(prepared.patch, super::PlannedPatch::Waiting), "{:?}", prepared.patch);
        let outcome = reconciler.reconcile(&vessel, &prepared, chrono::Utc::now());
        assert!(outcome.patch.is_none());
        assert_eq!(outcome.requeue_after, Some(super::VESSEL_PROVISIONING_REQUEUE_AFTER));

        let mut status = flotilla_store_testkit::fixtures::convoy_status(flotilla_resources::ConvoyPhase::Active);
        status.workflow_snapshot = Some(flotilla_resources::WorkflowSnapshot {
            cascade: None,
            exit: None,
            turn_delivery: Default::default(),
            stall_nudges: Default::default(),
            supervision: None,
            vessels: Vec::new(),
        });
        convoys.update_status("convoy", &convoy.metadata.resource_version, &status).await.expect("freeze snapshot");
        replicate().await;
        let prepared = reconciler.prepare(&vessel).await.expect("prepare");
        assert!(
            matches!(&prepared.patch, super::PlannedPatch::Failed { message } if message == "vessel work missing from convoy snapshot"),
            "{:?}",
            prepared.patch
        );
    }

    // A vessel no admission froze an instance for takes the one provider its kind's host
    // advertises for the realisation, and none before any is observed; a frozen instance wins.
    #[tokio::test]
    async fn an_unfrozen_vessel_takes_the_one_provider_its_host_advertises() {
        use flotilla_protocol::provider::{
            CheckoutStrategy, EnvironmentClass, MaterialMode, ProviderAdvertisement, ProviderCapabilities, ProviderKind, ProviderReadiness,
        };
        let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
        let hosts = backend.using::<flotilla_resources::Host>("flotilla");
        let host = hosts
            .create(
                &InputMeta::builder().name("h".into()).build(),
                &flotilla_resources::HostSpec { display_name: "h".into(), ..Default::default() },
            )
            .await
            .expect("host");
        let reconciler = VesselReconciler::new(backend.clone(), "flotilla");
        let kind = FulfilmentKindSpec::builder()
            .host_ref("h".into())
            .pool("cleat".into())
            .realisation(FulfilmentRealisation::DockerPerVessel { image: None })
            .build();
        backend.using::<FulfilmentKind>("flotilla").create(&InputMeta::builder().name("docker".into()).build(), &kind).await.expect("kind");
        let vessel = backend
            .using::<Vessel>("flotilla")
            .create(
                &InputMeta::builder().name("vessel".into()).build(),
                &VesselSpec {
                    convoy_ref: "convoy".into(),
                    vessel_name: "work".into(),
                    fulfilment_kind_ref: "docker".into(),
                    adopted_checkout_refs: BTreeMap::new(),
                },
            )
            .await
            .expect("vessel");
        let unobserved = reconciler.placement_dependency(&vessel, "docker", None).await.expect("read").expect("a placement");
        assert_eq!(unobserved.instance, None);
        let providers = [("host-direct", EnvironmentClass::HostDirect), ("only", EnvironmentClass::Dummy)].map(|(instance, class)| {
            ProviderAdvertisement {
                instance: instance.into(),
                kind: Some(ProviderKind::new(class, "fixture")),
                readiness: ProviderReadiness::Ready,
                capabilities: ProviderCapabilities {
                    material_modes: vec![MaterialMode::Shared],
                    checkout_strategies: vec![CheckoutStrategy::HostWorktree],
                    ..Default::default()
                },
            }
        });
        let status = flotilla_resources::HostStatus {
            capabilities: [(flotilla_resources::ENVIRONMENT_PROVIDERS_CAPABILITY.into(), serde_json::json!(providers))]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        hosts.update_status("h", &host.metadata.resource_version, &status).await.expect("host status");
        let placement = reconciler.placement_dependency(&vessel, "docker", None).await.expect("read").expect("a provider");
        assert_eq!(placement.instance.as_deref(), Some("only"));
        let placement =
            reconciler.placement_dependency(&vessel, "docker", Some(&decision(Some("frozen")))).await.expect("read").expect("frozen");
        assert_eq!(placement.instance.as_deref(), Some("frozen"));
    }

    #[tokio::test]
    async fn managed_checkout_names_are_scoped_by_placement_and_dispatch_attempt() {
        let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
        let convoy = backend
            .using::<Convoy>("flotilla")
            .create(
                &InputMeta::builder().name("same-name".to_string()).build(),
                &ConvoySpec::builder().workflow_ref("scratch".to_string()).build(),
            )
            .await
            .expect("create convoy");
        let mut later_attempt = convoy.clone();
        later_attempt.metadata.creation_timestamp += chrono::Duration::seconds(1);

        let feta = checkout_placement_scope(&convoy, Some("feta-host")).expect("feta scope");
        let kiwi = checkout_placement_scope(&convoy, Some("kiwi-host")).expect("kiwi scope");
        let later_feta = checkout_placement_scope(&later_attempt, Some("feta-host")).expect("later feta scope");

        assert_ne!(checkout_name("same-name", "flotilla", false, Some(&feta)), checkout_name("same-name", "flotilla", false, Some(&kiwi)));
        assert_ne!(
            checkout_name("same-name", "flotilla", false, Some(&feta)),
            checkout_name("same-name", "flotilla", false, Some(&later_feta))
        );
    }

    // #3981: a cloud vessel's Environment names its frozen provider instance and carries
    // typed inputs only: no Docker spec, nothing in its env, and the policy's size.
    #[test]
    fn cloud_environment_is_typed_provider_input() {
        use super::{cloud_environment_spec, Placement};
        let spec = FulfilmentKindSpec::builder()
            .host_ref(String::new())
            .pool("cleat".into())
            .realisation(FulfilmentRealisation::CloudSandboxPerVessel { image: "image@sha256:digest".into(), size: None })
            .build();
        let kind: flotilla_resources::ResourceObject<FulfilmentKind> = serde_json::from_value(serde_json::json!({
            "metadata": { "name": "cloud", "namespace": "flotilla", "resource_version": "1", "annotations": {}, "labels": {},
                "creation_timestamp": "2026-10-23T00:00:00Z" },
            "spec": spec
        }))
        .unwrap();
        let needs: BTreeSet<flotilla_resources::CapabilityNeed> = ["toolchain:rust".parse().unwrap()].into_iter().collect();
        let unfrozen = Placement { kind: kind.clone(), instance: None };
        assert!(cloud_environment_spec(&unfrozen, "image@sha256:digest", needs.clone(), &BTreeSet::new())
            .unwrap_err()
            .contains("names no provider instance"));
        let placement = Placement { kind, instance: Some("daytona".into()) };
        let spec = cloud_environment_spec(&placement, "image@sha256:digest", needs.clone(), &BTreeSet::new()).expect("a typed spec");
        assert!(spec.docker.is_none() && spec.host_direct.is_none());
        assert_eq!(spec.provider_address.unwrap().to_string(), "account:daytona/provider:daytona");
        assert_eq!(
            environment_provider_labels(&placement).get(flotilla_environment::environment::ENVIRONMENT_PROVIDER_INSTANCE_LABEL),
            Some(&"daytona".to_string())
        );
        let cloud = spec.cloud_sandbox.unwrap();
        assert_eq!(cloud.probe_needs, needs);
        assert_eq!(cloud.size, flotilla_resources::realisation_settings::CLOUD_SANDBOX_SIZE, "no policy size falls back to the default");
        assert!(cloud.env.is_empty());

        let policy_size = flotilla_resources::CloudSandboxSize { cpus: 4, memory_bytes: 8 << 30, disk_bytes: 10 << 30 };
        let mut sized_kind = placement.kind;
        sized_kind.spec.realisation =
            FulfilmentRealisation::CloudSandboxPerVessel { image: "image@sha256:digest".into(), size: Some(policy_size.clone()) };
        let sized = Placement { kind: sized_kind, instance: Some("daytona".into()) };
        let spec = cloud_environment_spec(&sized, "image@sha256:digest", needs, &BTreeSet::new()).expect("a typed spec");
        assert_eq!(spec.cloud_sandbox.unwrap().size, policy_size);
    }

    // A cloud vessel's discovery reads its adapters' enabling variables from the
    // Environment's declared env, as a Docker vessel's does: a crew that needs
    // fleet-canary enables it, and one that does not declares nothing.
    #[test]
    fn cloud_environment_declares_the_crews_adapter_enabling_env() {
        use super::{cloud_environment_spec, Placement};
        let spec = FulfilmentKindSpec::builder()
            .host_ref(String::new())
            .pool("cleat".into())
            .realisation(FulfilmentRealisation::CloudSandboxPerVessel { image: "image@sha256:digest".into(), size: None })
            .build();
        let kind: flotilla_resources::ResourceObject<FulfilmentKind> = serde_json::from_value(serde_json::json!({
            "metadata": { "name": "cloud", "namespace": "flotilla", "resource_version": "1", "annotations": {}, "labels": {},
                "creation_timestamp": "2026-10-23T00:00:00Z" },
            "spec": spec
        }))
        .unwrap();
        let placement = Placement { kind, instance: Some("daytona".into()) };
        let env = |adapters: &[&str]| {
            let adapters = adapters.iter().map(|adapter| adapter.to_string()).collect();
            cloud_environment_spec(&placement, "image@sha256:digest", BTreeSet::new(), &adapters).unwrap().cloud_sandbox.unwrap().env
        };
        assert_eq!(env(&["fleet-canary"]), BTreeMap::from([("FLOTILLA_FLEET_CANARY".to_string(), "1".to_string())]));
        assert!(env(&["claude"]).is_empty());
    }

    // #4165: vessels whose provider declares one workspace root for all of them
    // each clone into a directory of their own below it.
    // #4165: a multi-repository cloud vessel's crew runs where its checkouts were
    // recorded side by side, in either layout, and nowhere they disagree.
    #[test]
    fn a_multi_repository_vessel_runs_where_its_checkouts_sit_together() {
        use super::recorded_workspace_root;
        let paths = |paths: &[&str]| paths.iter().map(|path| path.to_string()).collect::<Vec<_>>();
        for (recorded, root) in [
            (paths(&["/vessels/convoy-a-work/cleat", "/vessels/convoy-a-work/flotilla"]), Some("/vessels/convoy-a-work")),
            (paths(&["/workspace/cleat", "/workspace/flotilla"]), Some("/workspace")),
            (paths(&["/vessels/convoy-a-work/cleat", "/workspace/flotilla"]), None),
            (paths(&["/"]), None),
            (paths(&[]), None),
        ] {
            assert_eq!(recorded_workspace_root(&recorded).as_deref(), root, "{recorded:?}");
        }
    }

    #[test]
    fn each_vessel_clones_below_the_workspace_root_in_its_own_directory() {
        use super::vessel_clone_path;
        for root in ["/workspace", "/workspace/", "/tmp/trial/vessels"] {
            let (first, second) = (vessel_clone_path(root, "convoy-a-work"), vessel_clone_path(root, "convoy-b-work"));
            assert_ne!(first, second);
            for path in [&first, &second] {
                let parent = std::path::Path::new(path).parent().expect("below the root");
                assert_eq!(parent, std::path::Path::new(root));
            }
        }
    }

    #[test]
    fn waiting_message_replaces_the_structured_host_direct_environment_name() {
        let decision = PlacementDecision {
            provider_instance: None,
            provider_workspace_root: None,
            minimal_alternatives: Vec::new(),
            escalation_reason: None,
            kind: "host-direct-test".to_string(),
            target_host: PlacementTargetHost {
                reference: flotilla_protocol::CanonicalHostId::resolved("01HXYZ"),
                display_name: "kiwi".to_string(),
            },
            refused_candidates: Vec::new(),
            viable_not_selected: Vec::new(),
            allocation: None,
        };

        assert_eq!(
            legible_waiting_for("environment host-direct-01HXYZ to become ready".to_string(), Some(&decision)),
            "environment host-direct-kiwi to become ready"
        );
        assert_eq!(
            legible_waiting_for("checkout checkout-01HXYZ to become ready".to_string(), Some(&decision)),
            "checkout checkout-01HXYZ to become ready",
            "unrelated names containing the host ref must not be rewritten"
        );
    }

    #[test]
    fn placement_strategy_replaces_display_name_alias_with_canonical_host_reference() {
        let kind = FulfilmentKindSpec::builder()
            .host_ref("udder".to_string())
            .pool("cleat".to_string())
            .realisation(FulfilmentRealisation::HostDirect)
            .build();
        let decision = PlacementDecision {
            provider_instance: None,
            provider_workspace_root: None,
            minimal_alternatives: Vec::new(),
            escalation_reason: None,
            kind: "host-direct-udder".to_string(),
            target_host: PlacementTargetHost {
                reference: flotilla_protocol::CanonicalHostId::resolved("1c8df992"),
                display_name: "udder".to_string(),
            },
            refused_candidates: Vec::new(),
            viable_not_selected: Vec::new(),
            allocation: None,
        };
        let mut strategy = placement_strategy(&kind);

        strategy.canonicalize_host_ref(decision.target_host.reference.as_str());

        assert!(
            matches!(strategy, PlacementStrategy::HostDirect { ref host_ref, .. } if host_ref == "1c8df992"),
            "placement strategy should carry the canonical host reference from the decision"
        );
    }

    #[test]
    fn environment_metadata_round_trips_non_empty_credential_scopes() {
        let repository = flotilla_resources::RepositoryKey("github.com-flotilla-org-flotilla".to_string());
        let credentials = BTreeSet::from(["github-app".to_string()]);
        let scopes = BTreeMap::from([("github-app".to_string(), BTreeSet::from([repository]))]);

        let env = environment_with_credentials(BTreeMap::new(), &credentials, &scopes, &BTreeMap::new());

        assert_eq!(
            serde_json::from_str::<BTreeSet<String>>(&env[flotilla_resources::CREDENTIAL_REFS_ENV]).expect("decode credential refs"),
            credentials
        );
        assert_eq!(
            serde_json::from_str::<BTreeMap<String, BTreeSet<flotilla_resources::RepositoryKey>>>(
                &env[flotilla_resources::CREDENTIAL_SCOPES_ENV]
            )
            .expect("decode credential scopes"),
            scopes
        );
    }

    #[test]
    fn missing_host_local_environment_warns_with_store_and_host_context() {
        let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
        let reconciler = VesselReconciler::new(backend.clone(), "flotilla")
            .with_federated_dependencies(&backend, flotilla_protocol::CanonicalHostId::resolved("feta-host"));
        let log_output = Arc::new(Mutex::new(Vec::new()));
        let message;
        {
            let writer = LogCaptureWriter(Arc::clone(&log_output));
            let subscriber = tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_target(false)
                .with_max_level(tracing::Level::WARN)
                .with_writer(move || writer.clone())
                .finish();
            let _guard = tracing::subscriber::set_default(subscriber);
            message = reconciler.missing_host_local_environment("host-direct-kiwi", "kiwi-host");
        }

        let logs =
            String::from_utf8(log_output.lock().expect("log output lock should be healthy").clone()).expect("logs should be valid UTF-8");
        assert!(logs.contains(" WARN "), "missing warning level: {logs}");
        assert!(logs.contains("environment_ref=host-direct-kiwi"), "missing Environment context: {logs}");
        assert!(logs.contains("placement_host_ref=kiwi-host"), "missing placement-host context: {logs}");
        assert!(logs.contains("reconciler_host_ref=feta-host"), "missing store-host context: {logs}");
        assert!(message.contains("resource store for reconciler host feta-host"));
    }
    // The crew sees Environment engine readiness, waits and endings with no copied status.
    // Enumerate all old phases, success, both Gone endings and empty/Unicode give-up causes.
    #[tokio::test]
    async fn vessel_reads_environment_dependencies_without_a_status_copy() {
        use flotilla_provisioning::engine::environment::EnvironmentBinding;
        use flotilla_provisioning_testkit::{object, Ending, LifecycleFixture};
        use flotilla_resources::{Environment, EnvironmentPhase, EnvironmentSpec, EnvironmentStatus, HostDirectEnvironmentSpec};
        let env = object::<Environment>(
            "env",
            EnvironmentSpec {
                host_direct: Some(HostDirectEnvironmentSpec { host_ref: "host".into(), repo_default_dir: "/work".into() }),
                ..Default::default()
            },
            EnvironmentStatus { phase: EnvironmentPhase::Ready, message: Some("stale".into()), ..Default::default() },
        );
        let fixture = LifecycleFixture::<EnvironmentBinding>::new(false);
        let reconciler = VesselReconciler::new(ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default()), "flotilla")
            .with_lifecycle_reads(fixture.reads.clone());
        fixture.request(&env);
        assert!(!reconciler.environment_dependency(&env).expect("requested").ready);
        fixture.finish(&env, Ending::Ready("/work"));
        assert!(reconciler.environment_dependency(&env).expect("ready").ready);
        fixture.finish(&env, Ending::Lost("/work"));
        let plan = reconciler.environment_dependency(&env).expect("lost").unavailable.expect("unavailable");
        assert!(matches!(plan.patch, super::PlannedPatch::Lost { message } if message == "environment env observed absent"));
        fixture.poison();
        assert!(reconciler.environment_dependency(&env).err().expect("checked read").to_string().contains("poisoned"));
    }
    // A held lease decides terminality even while old status disagrees; read errors never fall back.
    #[tokio::test]
    async fn vessel_reads_its_lease_without_a_status_copy() {
        use flotilla_protocol::CanonicalHostId;
        use flotilla_provisioning_testkit::{object, Ending, VesselFixture};
        use flotilla_resources::{VesselPhase, VesselStatus};
        use flotilla_store::controller::Reconciler;
        let mut vessel = object::<Vessel>(
            "vessel",
            VesselSpec {
                convoy_ref: "convoy".into(),
                vessel_name: "work".into(),
                fulfilment_kind_ref: "local".into(),
                adopted_checkout_refs: Default::default(),
            },
            VesselStatus {
                phase: VesselPhase::Lost,
                placement_decision: Some(
                    PlacementDecision::builder()
                        .kind("local".into())
                        .target_host(PlacementTargetHost { reference: CanonicalHostId::resolved("host"), display_name: "host".into() })
                        .build(),
                ),
                ..Default::default()
            },
        );
        let fixture = VesselFixture::new(false);
        let reconciler = VesselReconciler::new(ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default()), "flotilla")
            .with_lifecycle_reads(fixture.reads.clone());
        fixture.request(&vessel);
        // A Requested lease is provisioning even beside stale ready or interrupted status.
        for old_phase in [VesselPhase::Lost, VesselPhase::Ready, VesselPhase::Interrupted] {
            vessel.status.as_mut().expect("status").phase = old_phase;
            let prepared = reconciler.prepare(&vessel).await.expect("prepared");
            assert_eq!(prepared.phase, Some(VesselPhase::Provisioning));
            assert!(reconciler.reconcile(&vessel, &prepared, chrono::Utc::now()).patch.is_some());
        }
        fixture.finish(&vessel, Ending::Released);
        vessel.status.as_mut().expect("status").phase = VesselPhase::Ready;
        let prepared = reconciler.prepare(&vessel).await.expect("ended");
        assert!(reconciler.reconcile(&vessel, &prepared, chrono::Utc::now()).patch.is_none());
        fixture.poison();
        assert!(reconciler.prepare(&vessel).await.expect_err("checked read").to_string().contains("poisoned"));
    }

    // Vessel checkout gates take phase, failure and realised path from one engine judgement.
    // A stale path cannot override the realised identity or hide a checked read failure.
    #[tokio::test]
    async fn vessel_reads_checkout_dependencies_without_a_copy() {
        use flotilla_provisioning::engine::checkout::CheckoutBinding;
        use flotilla_provisioning_testkit::{object, Ending, LifecycleFixture};
        use flotilla_resources::{Checkout, CheckoutPhase, CheckoutSpec, CheckoutStatus, CheckoutWorktreeSpec, RepositoryKey};
        let mut checkout = object::<Checkout>(
            "checkout",
            CheckoutSpec::Worktree(CheckoutWorktreeSpec {
                repo_ref: RepositoryKey("repo".into()),
                env_ref: "env".into(),
                r#ref: "feature".into(),
                base_ref: None,
                target_path: "/work".into(),
                clone_ref: "source".into(),
            }),
            CheckoutStatus { phase: CheckoutPhase::Gone, path: Some("/stale".into()), ..Default::default() },
        );
        checkout.metadata.labels.insert(flotilla_resources::CONVOY_LABEL.into(), "crew".into());
        let fixture = LifecycleFixture::<CheckoutBinding>::new(false);
        let reconciler = VesselReconciler::new(ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default()), "flotilla")
            .with_lifecycle_reads(fixture.reads.clone());
        fixture.request(&checkout);
        assert_eq!(reconciler.checkout_dependency(&checkout).expect("requested").phase, CheckoutPhase::Pending);
        fixture.finish(&checkout, Ending::Ready("/engine-path"));
        let ready = reconciler.checkout_dependency(&checkout).expect("ready");
        assert_eq!((ready.phase, ready.path.as_deref()), (CheckoutPhase::Ready, Some("/engine-path")));
        fixture.finish(&checkout, Ending::Lost("/engine-path"));
        assert_eq!(reconciler.checkout_dependency(&checkout).expect("lost").phase, CheckoutPhase::Gone);
        fixture.poison();
        assert!(reconciler.checkout_dependency(&checkout).err().expect("checked read").to_string().contains("poisoned"));
    }
}
