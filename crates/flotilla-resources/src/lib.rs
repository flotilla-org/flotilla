// async_trait generates #[must_use] on boxed futures; Clippy 1.99 also treats
// those future return types as must-use. The generated annotation is redundant.
// Remove when async_trait or Clippy stops producing this warning.
#![allow(clippy::double_must_use)]

pub mod account_access;
pub use account_access::{AccountBinding, AccountBindingSpec, AccountProvision, AccountProvisionSpec};
#[doc(hidden)]
pub mod address;
pub mod artifact;
#[doc(hidden)]
pub mod change_request;
mod charter_store;
pub use charter_store::{CharterPointer, CharterSource, CharterStoreBinding};
#[doc(hidden)]
pub mod checkout;
#[doc(hidden)]
pub mod clock;
mod clone;
mod controller_retry;
#[doc(hidden)]
pub mod convoy;
mod convoy_ensure;
mod credential;
pub mod crew_defaults;
pub use crew_defaults::{
    resolve_skills, skill_layers, validate_skill_ref, CrewDefaults, CrewDefaultsSpec, ResolvedSkills, SkillCatalogEntry, SkillDecision,
    SkillLayer, SkillOutcome, SkillRefusal,
};
mod crew_image_baseline;
#[doc(hidden)]
pub mod image_build;
pub use image_build::{
    image_build_name, ImageBuild, ImageBuildCapacity, ImageBuildFailure, ImageBuildFailureClass, ImageBuildPhase, ImageBuildReservation,
    ImageBuildResourceSpec, ImageBuildSpec, ImageBuildStatus, ImageBuildStatusPatch, ImageDelivery, ImageLayerProbe, ImageProbeOutcome,
    PlacedImageIdentity, IMAGE_BUILD_MAX_ATTEMPTS,
};
mod image_layer;
pub use image_layer::stdlib;
pub mod builtins;
pub use image_layer::{
    capability_satisfies, image_identity, parse_layer_directory, parse_layer_directory_with_executables, resolve_image,
    validate_capability, EffectiveImageCatalogue, ImageCatalogue, ImageContextFile, ImageLayer, ImageLayerPin, ImageLayerResourceSpec,
    ImageLayerSource, ImageLayerSpec, ImageLayerTier, ImagePinDigest, ImageRequest, ImageResolutionError, ImageSelection, ResolvedImage,
    ShadowedImageLayer, LAYER_DEFINITION_FILE, LAYER_FRAGMENT_FILE, MAX_CONTEXT_FILE_BYTES,
};
pub mod delivery_hold;
#[doc(hidden)]
pub mod digest;
pub use digest::{digest_bucket, DigestQuery, PartitionDigest, DIGEST_FANOUT};
mod dispatch_hold;
mod dispatch_observation;
mod environment;
#[doc(hidden)]
pub mod error;
#[doc(hidden)]
pub mod event;
#[doc(hidden)]
pub mod field_ownership;
mod fleet_designation;
mod forge;
mod forge_read;
pub use forge_read::{
    forge_read_name, ForgeRead, ForgeReadHeartbeat, ForgeReadHeartbeatSpec, ForgeReadHeartbeatStatus, ForgeReadRequest, ForgeReadSpec,
    ForgeReadStatus,
};
#[doc(hidden)]
pub mod definition_provenance;
mod fulfilment_kind;
#[doc(hidden)]
pub mod host;
mod issue;
pub mod labels;
mod landing_gate;
mod leaf;
pub mod lifecycle_reads;
mod manifest_root;
#[doc(hidden)]
pub mod message;
mod message_inbox;
mod message_migration;
#[doc(hidden)]
pub mod placement_policy;
mod platform;
#[doc(hidden)]
pub mod prepared_snapshot;
mod principal_attention;
mod project;
mod provisioning_identity;
mod replica;
#[doc(hidden)]
pub mod repository;
#[doc(hidden)]
pub mod resource;
#[doc(hidden)]
pub mod retention;
mod review_bundle;
mod status_patch;
#[doc(hidden)]
pub mod terminal_session;
#[doc(hidden)]
#[doc(hidden)]
pub mod vessel;
mod vessel_occupancy;
pub mod vessel_seats;
mod watch;
#[doc(hidden)]
pub mod workflow_template;

pub use artifact::{Artifact, ArtifactSpec, ArtifactStatus, ArtifactStatusPatch};
pub use change_request::{
    change_request_subject, merge_change_request_history, retain_change_request, select_change_request_sources, select_change_requests,
    ChangeRequest, ChangeRequestReviewObservation, ChangeRequestSpec, ChangeRequestStatus, ChangeRequestStatusPatch,
    ChangeRequestSubjectHistory, Observation, ObservedChangeRequestState, ObservedChecks, ObservedMergeability, ObservedReviewDecision,
};
pub use checkout::{
    latch_evidence_backed_integration, ChangeRequestMergeability, ChangeRequestObservation, ChangeRequestState, Checkout,
    CheckoutBranchProvenance, CheckoutIntegrationStatus, CheckoutPhase, CheckoutSpec, CheckoutStatus, CheckoutStatusPatch,
    CheckoutWorktreeSpec, ConditionValue, FreshCloneCheckoutSpec, IntegrationCondition, LandedEvidence, ObservedCheckoutSpec,
    RemoteRefObservation,
};
pub use clock::{Clock, SystemClock};
pub use clone::{Clone, CloneFailurePolicy, ClonePhase, CloneSpec, CloneStatus, CloneStatusPatch};
pub use controller_retry::{ControllerRetry, ControllerRetryDisposition, RetryBackoff, RetryCeiling, PROVISIONING_RETRY_BACKOFF};
pub use convoy::{
    active_change_request_subjects, bound_change_request_address, change_request_leaf_set, controller_patches, convoy_reference_context,
    convoy_sanctions_checkout_reclaim, convoy_subject_rows, evaluate_crew_completion, evaluate_landing_settlement,
    expected_change_request_leaves, expected_checkout_refs, external_patches, instantiate_exit, instantiate_turn_delivery,
    observed_change_request_subjects, pinned_placement_ref, pinned_workflow_ref, provisioning_patches, reconcile,
    select_convoy_child_sources, select_convoy_children, subject_relationship_conflicts, vessel_placement_pin, BoundChangeRequest,
    ChangeRequestLeafSet, Convoy, ConvoyAttention, ConvoyEvent, ConvoyIssue, ConvoyPhase, ConvoyProvisioningState, ConvoyRepositorySpec,
    ConvoySpec, ConvoyStatus, ConvoyStatusPatch, ConvoyTeardownRuntime, CrewCompletionClaim, CrewCompletionOverride, CrewCompletionRefusal,
    CrewCompletionRefusalCause, CrewWorkPhase, CrewWorkState, DeclaredSubject, DiscoveredSubject, InputValue, InstantiatedExit,
    InstantiatedExitEntry, InstantiatedTurnDelivery, IssueSnapshot, LandingClaim, LandingEntry, LandingTurn, LeafMaker, LifecycleMutation,
    NudgeObligation, PendingBrief, PendingSupervisorTurn, PlacementStatus, QueuedTurnObservation, ReconcileOutcome, ResumeIntent,
    SettlementEvaluation, SettlementMode, SettlementSubject, StallCause, StallEvidenceSource, StallNudge, StallProposedDisposition,
    StallReason, StallRung, StallSupervisor, StalledCondition, SubjectDiscovery, SubjectDiscoverySource, SupersededCrewClaim,
    TargetMismatch, TurnDeliveryEpisode, TurnDeliveryFailure, TurnDeliveryFailureKind, TurnDeliveryOutcome, TurnDeliveryRung,
    TurnDeliveryStatus, UnmetSettlementExpectation, UnresolvedChangeRequestSubject, VesselPlacementPin, WorkCompletionAuthority, WorkPhase,
    WorkState, WorkflowSnapshot, CONVOY_TEARDOWN_FINALIZER, ENSURED_FROM_ANNOTATION, FORCE_TEARDOWN_ANNOTATION,
    PLACEMENT_SNAPSHOT_ANNOTATION, VESSEL_PLACEMENTS_ANNOTATION, WORKFLOW_SNAPSHOT_ANNOTATION,
};
pub use convoy_ensure::{
    ConvoyEnsure, ConvoyEnsureCondition, ConvoyEnsureConfigDrift, ConvoyEnsureHoldReason, ConvoyEnsureSpec, ConvoyEnsureStatus,
    ConvoyEnsureStatusPatch, DRIVER_ADMISSION_CONDITION_TYPE,
};
pub use credential::{
    capped_github_app_permissions, permission_level_rank, validate_matching_grant_permissions, CredentialConsumer, CredentialGrant,
    CredentialGrantSelector, CredentialGrantSpec, CredentialLifecycle, CredentialPlacementRequirements, CredentialSource, CredentialSpec,
    CredentialSpecSpec, LandingCredentialScope, RepositoryTrust, CREDENTIAL_PERMISSIONS_ANNOTATION, CREDENTIAL_PERMISSIONS_ENV,
    CREDENTIAL_PERMISSIONS_SESSION_TAG, CREDENTIAL_REFS_ANNOTATION, CREDENTIAL_REFS_ENV, CREDENTIAL_REF_SESSION_TAG,
    CREDENTIAL_SCOPES_ANNOTATION, CREDENTIAL_SCOPES_ENV, CREDENTIAL_SCOPES_SESSION_TAG, CREW_SKILLS_ENV, INITIAL_AUTHORIZATION_EPOCH,
    RESOLVED_SKILLS_ENV,
};
pub use crew_image_baseline::{CrewImageBaseline, CrewImageBaselineSpec};
pub use dispatch_hold::{
    DispatchDeployment, DispatchDeploymentSpec, DispatchHold, DispatchHoldSpec, DispatchHoldStatus, DispatchHoldStatusPatch, HoldClearWhen,
};
pub use dispatch_observation::{DispatchObservation, DispatchObservationSpec, DISPATCH_RECONCILER_PROVENANCE};
pub use environment::{
    host_direct_environment_name, validate_environment_provider_address, CloudSandboxEnvironmentSpec, CloudSandboxSize,
    DockerEnvironmentSpec, Environment, EnvironmentMemoryPolicy, EnvironmentMount, EnvironmentMountMode, EnvironmentPhase,
    EnvironmentProviderDescriptor, EnvironmentSpec, EnvironmentStatus, EnvironmentStatusPatch, EnvironmentSurvivors,
    HostDirectEnvironmentSpec, HOST_DIRECT_NAME_PREFIX,
};
pub use error::{FinalizerWaitReason, ResourceError};
pub use event::{Event, EventRegarding, EventSpec, ObjectEvent, DEFAULT_EVENT_TTL_SECONDS};
pub use field_ownership::{FieldOwnedResource, FieldOwnership, FieldOwnershipViolation, OwnershipEnforcement, WriterIdentity, WriterRole};
pub use fleet_designation::{FleetDesignation, FleetDesignationSpec, FLEET_DESIGNATION_NAME};
pub use flotilla_protocol::{PrincipalRef, ResourceRef};
pub use forge::{Forge, ForgeKind, ForgeSpec};
pub use fulfilment_kind::{FulfilmentCostClass, FulfilmentGrant, FulfilmentKind, FulfilmentKindSpec, FulfilmentRealisation};
pub use host::{
    canonical_host_id, CachedModelProbe, CredentialExpiry, FoldedHostObservations, FulfilmentFacts, FulfilmentImage, HarnessFacts, Host,
    HostCondition, HostConnection, HostSpec, HostStatus, HostStatusPatch, ModelFact, ModelFactSource, ModelProbeState,
    AGENTLESS_CAPABILITY, AGENT_ADAPTERS_CAPABILITY, AMBIENT_CLAUDE_CREDENTIAL_SCOPE, CREDENTIAL_EXPIRY_CAPABILITY,
    ENVIRONMENT_PROVIDERS_CAPABILITY, HEARTBEAT_READY_TTL_SECS, HEARTBEAT_REFRESH_SECS, HELD_CREDENTIALS_CAPABILITY,
    LOCAL_ROWS_MISSING_REASON, OWNING_DAEMON_CAPABILITY, PLACEMENT_CAPABILITY, PROJECTION_PARITY_CONDITION,
    SLEEP_INHIBITION_CONDITION_TYPE, TERMINAL_POOLS_CAPABILITY, TRANSPORT_CAPABILITY,
};
pub use issue::{Issue, IssueSpec, IssueStatus, IssueStatusPatch, ObservedIssueState};
pub use labels::{
    label_value, labels_match, LifecycleAuthority, AUTHORITY_LABEL, CHANGE_REQUEST_ID_LABEL, CONVOY_LABEL, CREW_ORDINAL_LABEL,
    GENERATION_LABEL, MANAGED_BY_LABEL, PROJECT_LABEL, REPO_KEY_LABEL, REPO_LABEL, RESERVED_PREFIX, ROLE_LABEL, VESSEL_LABEL,
    VESSEL_ORDINAL_LABEL, VESSEL_REF_LABEL,
};
pub use landing_gate::{evaluate_landing_gate, settlement_human_gate, LandingGateDecision, LANDING_APPROVE_OPTION, LANDING_REFUSE_OPTION};
pub use leaf::{
    actor_obligation, admit_leaf, evaluate_leaf, ArtifactLeafSubject, ChangeRequestLeafSubject, ConvoyLeafSubject, IssueLeafSubject,
    LeafEvaluation, LeafSubject, LeafValue, ThreeValue, VesselLeafSubject, WorkLeafSubject, ADMITTED_LEAF_VOCABULARY,
};
pub use manifest_root::{
    DocumentKey, DocumentPhase, DocumentState, ManifestRoot, ManifestRootSpec, ManifestRootStatus, ManifestRootStatusPatch, Resolution,
    ResolutionAction, ResolutionOutcome,
};
pub use placement_policy::{
    CloudSandboxCheckout, CloudSandboxPerVesselPlacementPolicySpec, DockerCheckoutStrategy, DockerImagePullPolicy, DockerImageSource,
    DockerPerVesselPlacementPolicySpec, HostDirectPlacementPolicyCheckout, HostDirectPlacementPolicySpec, PlacementPolicy,
    PlacementPolicySpec, ProviderRequirements, DOCKER_POLICY_NAME_PREFIX,
};
pub use platform::Platform;
pub use prepared_snapshot::{content_hash, is_prepared_snapshot, PLACEMENT_SNAPSHOT_KIND, PREPARED_SNAPSHOT_LABEL, WORKFLOW_SNAPSHOT_KIND};
pub use principal_attention::{
    Demand, DemandAddressee, DemandExpiry, DemandExpiryDisposition, DemandKind, DemandPoolRef, DemandResponseOption, DemandSpec,
    DemandState, DemandStatus, DemandStatusPatch, DemandTransition, DemandVerdict, DemandVerdictDisposition, HumanGateContext, Regard,
    RegardExpiryPolicy, RegardSource, RegardSpec, RegardStatus, RegardStatusPatch,
};
pub use project::{
    normalize_issue_source, normalize_project_spec, resolve_project_issue_sources_from_inventory, DeclarationRefusedCondition,
    DispatchLane, DispatchMission, DispatchPolicy, DispatchQueueAttention, DispatchQueueEntry, IssueFieldValue, IssueFilter, IssueSource,
    IssueSourceBindingSpec, IssueSourceResolution, IssueSourceUnavailable, OperationalEntriesCondition, Project, ProjectRepositoryRole,
    ProjectRepositorySpec, ProjectSpec, ProjectStatus, ProjectStatusPatch, ResolvedIssueSourceBinding,
    DEFAULT_DISPATCH_QUEUE_STALE_AFTER_SECONDS,
};
pub use provisioning_identity::{canonicalize_repo_url, clone_key, descriptive_repo_slug, forge_clone_key, forge_repo_key, repo_key};
pub use replica::{
    compare_replica_reads, pick_replica_read, ReadResourceList, ReadResourceObject, ReadWatchEvent, ReplicaCursor, ReplicationClass,
    ResourceProvenance, ORIGIN_ROOT_ANNOTATION,
};
pub use repository::{
    repository_display_labels, repository_workspace_slugs, resolve_default_branch, DefaultBranchObservation, DefaultBranchProvenance,
    ForgeIdentity, Repository, RepositoryCheckoutKind, RepositoryCheckoutRef, RepositoryGitSpec, RepositoryIdentity, RepositoryKey,
    RepositoryProviderPreference, RepositoryRelation, RepositorySpec, RepositoryStatus, RepositoryStatusPatch, RepositoryUpstream,
    RepositoryVcsSpec,
};
pub use resource::{
    api_version, ApiPaths, CausalDot, FieldMergeMetadata, InputMeta, K8sListMeta, K8sObjectMeta, K8sResourceList, K8sResourceObject,
    K8sWatchEvent, MergeConflictSibling, MergeMetadata, ObjectMeta, OwnerReference, Resource, ResourceObject, BOOTSTRAP_PATH_ANNOTATION,
};
pub use retention::{
    EventHistory, EventRetention, ResourceDecodeQuarantine, ResourceEventDecodeQuarantine, ResourceReplicaQuarantine,
    ResourceStoreDiagnostics, ResourceStoreWarning,
};
pub use review_bundle::{
    FindingResolution, ReviewBundleIndex, ReviewBundleStoreConfig, ReviewBundleWriteCredential, ReviewCheck, ReviewCheckOutcome,
    ReviewFinding, ReviewRefPair, ReviewRound, SettlementClaimEvidence, REVIEW_BUNDLE_INDEX_FILE, REVIEW_BUNDLE_ROOT,
};
pub use status_patch::{NoStatusPatch, StatusPatch};
pub use terminal_session::{
    looks_like_terminal_reply, terminal_session_attach_target, terminal_session_attach_target_with_stale_status, CrewCompletionPending,
    CrewMessageDelivery, CrewMessageSender, CrewSessionStatus, InnerCommandStatus, ScreenObstruction, TerminalAttention,
    TerminalAttentionSource, TerminalAttentionState, TerminalBrief, TerminalCrewContext, TerminalCrewMessage, TerminalOccupancy,
    TerminalSession, TerminalSessionAttachTarget, TerminalSessionDegradedCondition, TerminalSessionIdentity, TerminalSessionPhase,
    TerminalSessionSource, TerminalSessionSpec, TerminalSessionStatus, TerminalSessionStatusPatch, TerminalSessionTarget,
    CONTROLLER_FENCE_UNVERIFIABLE_MESSAGE, CONTROLLER_FENCE_UNVERIFIABLE_REASON, TERMINAL_DELIVERY_EXPIRED_REASON,
    TERMINAL_DELIVERY_NOT_SUBMITTED_REASON, TERMINAL_DELIVERY_UNCONFIRMED_REASON,
};
pub use vessel::{
    vessel_resource_name, Vessel, VesselImage, VesselPhase, VesselSpec, VesselStatus, VesselStatusPatch, ACTUATOR_HOST_REF_ANNOTATION,
    ACTUATOR_SOURCE_ROOT_ANNOTATION,
};
pub use vessel_occupancy::{home_copies_first, HostOccupancy};
pub use watch::{ResourceList, ResourcePosition, ResourceTombstone, WatchEvent, WatchStart, WatchStream};

#[doc(hidden)]
#[macro_export]
macro_rules! for_each_registered_resource {
    ($callback:ident, $($argument:expr),* $(,)?) => {{
        $callback::<$crate::AccountBinding>($($argument),*);
        $callback::<$crate::AccountProvision>($($argument),*);
        $callback::<$crate::Artifact>($($argument),*);
        $callback::<$crate::Checkout>($($argument),*);
        $callback::<$crate::ChangeRequest>($($argument),*);
        $callback::<$crate::Issue>($($argument),*);
        $callback::<$crate::Clone>($($argument),*);
        $callback::<$crate::Convoy>($($argument),*);
        $callback::<$crate::ConvoyEnsure>($($argument),*);
        $callback::<$crate::CredentialGrant>($($argument),*);
        $callback::<$crate::CredentialSpec>($($argument),*);
        $callback::<$crate::CrewImageBaseline>($($argument),*);
        $callback::<$crate::ImageBuild>($($argument),*);
        $callback::<$crate::ImageLayer>($($argument),*);
        $callback::<$crate::CrewDefaults>($($argument),*);
        $callback::<$crate::FulfilmentKind>($($argument),*);
        $callback::<$crate::Demand>($($argument),*);
        $callback::<$crate::DispatchHold>($($argument),*);
        $callback::<$crate::DispatchDeployment>($($argument),*);
        $callback::<$crate::DispatchObservation>($($argument),*);
        $callback::<$crate::Environment>($($argument),*);
        $callback::<$crate::FleetDesignation>($($argument),*);
        $callback::<$crate::Forge>($($argument),*);
        $callback::<$crate::ForgeRead>($($argument),*);
        $callback::<$crate::ForgeReadHeartbeat>($($argument),*);
        $callback::<$crate::Event>($($argument),*);
        $callback::<$crate::Host>($($argument),*);
        $callback::<$crate::ManifestRoot>($($argument),*);
        $callback::<$crate::PlacementPolicy>($($argument),*);
        $callback::<$crate::Project>($($argument),*);
        $callback::<$crate::Regard>($($argument),*);
        $callback::<$crate::Repository>($($argument),*);
        $callback::<$crate::TerminalSession>($($argument),*);
        $callback::<$crate::Vessel>($($argument),*);
        $callback::<$crate::WorkflowTemplate>($($argument),*);
    }};
}
pub use fulfilment_kind::{effective_grants, version_at_least, CapabilityNeed};
pub use message::{
    message_record_name, HeldMessageBatch, Message, MessageExpectation, MessagePhase, MessageReference, MessageRelation, MessageSpec,
    MessageStatus, MessageStatusPatch, MessageSubmission, OperatorMessageDelivery, ResolvedMessageReceiver, STALL_KIND,
};
pub use message_inbox::{
    message_expectation_open, message_supersedes, parse_topic_address, qualify_message_address, qualify_message_spec,
    validate_message_address, MessageAddressContext, ROLE_ADDRESS_ANNOTATION,
};
pub use message_migration::{legacy_message_sender, legacy_message_spec};
pub use workflow_template::{
    builtin_workflow_templates, current_builtin_workflow_name, implement_review_workflow_spec, interactive_single_workflow_spec,
    leaf_operator, legacy_field_path, legacy_template_text, parse_leaf_template, single_agent_shepherd_workflow_spec,
    single_agent_workflow_spec, template_leaf, validate, AllocationDecision, ArtifactSubjectBinding, ClaimExit, CompletionCondition,
    CrewCompletionExpectation, CrewSource, CrewSpec, ExitDeclaration, HoldAct, InputDefinition, InterpolationField, InterpolationLocation,
    LegacyCompletionExpectation, RoleHandoff, Selector, StallNudgePolicy, Stance, SubjectVariable, SupervisionTarget, TemplatePromise,
    TurnDeliveryRule, TurnDeliveryTarget, ValidationError, VesselRequirement, WorkflowTemplate, WorkflowTemplateSpec,
};

pub mod role_cascade;
pub use convoy::promises;
pub use credential::{HostActionSelector, HostImageAction};
pub use fleet_designation::ImageCacheBinding;
pub use image_build::{is_image_digest, ImageAcquisitionCost, LocalImageInventories, IMAGE_DIGESTS_CAPABILITY};
pub use role_cascade::{ResolvedCascade, ResolvedSetting, RoleCascadeLayer, RoleDefinition, RoleSubscription};

mod actuation;
pub use actuation::Actuation;

#[doc(hidden)]
pub mod project_hierarchy;
pub use project_hierarchy::ProjectHierarchy;

mod leaf_diagnostics;
pub use leaf_diagnostics::*;

mod read_evidence;
pub use read_evidence::*;

pub mod convoy_address;
// Preserve existing root query APIs; admission naming is imported directly from the module.
pub use convoy_address::{convoy_disambiguation_address, parse_role_address, resolve_convoy_candidate_indices, ConvoyAddressIdentity};

mod host_projection;
pub use host_projection::canonical_placement_host_ref_from_sources;

/// Marks a convoy whose admission briefs were persisted as artifacts.
pub const BRIEF_ARTIFACTS_ANNOTATION: &str = "flotilla.work/brief-artifacts";

pub mod image_architecture;

pub mod convoy_accounting;

pub mod checkout_integration;
pub mod checkout_path;
