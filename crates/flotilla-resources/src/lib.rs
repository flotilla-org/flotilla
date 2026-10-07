// async_trait generates #[must_use] on boxed futures; Clippy 1.99 also treats
// those future return types as must-use. The generated annotation is redundant.
// Remove when async_trait or Clippy stops producing this warning.
#![allow(clippy::double_must_use)]

mod artifact;
mod backend;
mod change_request;
mod charter_store;
pub use charter_store::{CharterPointer, CharterSource, CharterStoreBinding};
mod checkout;
mod clock;
mod clone;
pub mod controller;
mod controller_retry;
mod convoy;
mod convoy_ensure;
mod credential;
pub mod crew_defaults;
pub use crew_defaults::{
    resolve_skills, skill_layers, validate_skill_ref, CrewDefaults, CrewDefaultsSpec, ResolvedSkills, SkillCatalogEntry, SkillDecision,
    SkillLayer, SkillOutcome, SkillRefusal,
};
mod crew_image_baseline;
mod image_build;
pub use image_build::{
    read_image_build, ImageBuild, ImageBuildCapacity, ImageBuildFailure, ImageBuildFailureClass, ImageBuildPhase, ImageBuildReason,
    ImageBuildReservation, ImageBuildSpec, ImageBuildStatus, ImageBuildStatusPatch,
};
mod image_layer;
pub use image_layer::{
    capability_satisfies, compose_image, validate_capability, FrozenImageLayer, FrozenImageLayers, ImageComposition, ImageInputAdoption,
    ImageInputPin, ImageInputStability, ImageLayer, ImageLayerParent, ImageLayerSelection, ImageLayerSpec, ImageLayerStage,
    PlacedImageIdentity, ResolvedImageInputs, IMAGE_LAYERS_ANNOTATION,
};
mod definition;
pub mod delivery_hold;
mod digest;
pub use digest::{digest_bucket, DigestQuery, PartitionDigest, DIGEST_FANOUT};
mod dispatch_hold;
mod dispatch_observation;
mod environment;
mod error;
mod event;
mod field_ownership;
mod fleet_designation;
mod forge;
mod fulfilment_kind;
mod host;
mod http;
mod in_memory;
mod issue;
mod labels;
mod landing_gate;
mod leaf;
mod manifest_root;
mod message;
mod message_conditions;
mod message_delivery;
mod message_inbox;
mod owner_gc;
mod placement_policy;
mod platform;
mod prepared_snapshot;
mod presentation;
mod principal_attention;
mod project;
mod project_hierarchy;
mod provisioning_identity;
mod registry;
#[cfg(test)]
mod registry_watch_tests;
mod replica;
mod repository;
mod resource;
mod retention;
mod review_bundle;
mod sqlite;
mod status_patch;
mod terminal_session;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
pub mod tls;
mod usage;
mod vessel;
mod watch;
mod workflow_template;

pub use artifact::{artifact_record_name, reserve_ledger_comment_creation, Artifact, ArtifactSpec, ArtifactStatus, ArtifactStatusPatch};
pub use backend::{ReplicaReadResolver, ReplicaWriter, ResourceBackend, TypedResolver};
pub use change_request::{
    change_request_record_name, change_request_subject, merge_change_request_history, retain_change_request, select_change_request_sources,
    select_change_requests, ChangeRequest, ChangeRequestReviewObservation, ChangeRequestSpec, ChangeRequestStatus,
    ChangeRequestStatusPatch, ChangeRequestSubjectHistory, Observation, ObservedChangeRequestState, ObservedChecks, ObservedMergeability,
    ObservedReviewDecision,
};
pub use checkout::{
    latch_evidence_backed_integration, ChangeRequestMergeability, ChangeRequestObservation, ChangeRequestState, Checkout,
    CheckoutBranchProvenance, CheckoutIntegrationStatus, CheckoutPhase, CheckoutSpec, CheckoutStatus, CheckoutStatusPatch,
    CheckoutWorktreeSpec, ConditionValue, FreshCloneCheckoutSpec, IntegrationCondition, LandedEvidence, ObservedCheckoutSpec,
    RemoteRefObservation,
};
#[cfg(any(test, feature = "test-support"))]
pub use clock::VirtualClock;
pub use clock::{Clock, SystemClock};
pub use clone::{Clone, CloneFailurePolicy, ClonePhase, CloneSpec, CloneStatus, CloneStatusPatch};
pub use controller_retry::{ControllerRetry, ControllerRetryDisposition, RetryBackoff, RetryCeiling};
pub use convoy::{
    active_change_request_subjects, bound_change_request_record_name, change_request_address, change_request_address_with_forges,
    controller_patches, convoy_reference_context, convoy_sanctions_checkout_reclaim, convoy_subject_rows, evaluate_crew_completion,
    evaluate_landing_settlement, expected_change_request_leaves, expected_checkout_refs, external_patches, instantiate_exit,
    instantiate_turn_delivery, issue_address, issue_address_with_forges, observed_change_request_subjects, pinned_placement_ref,
    pinned_workflow_ref, provisioning_patches, reconcile, select_convoy_children, subject_relationship_conflicts, vessel_placement_pin,
    BoundChangeRequest, Convoy, ConvoyAttention, ConvoyEvent, ConvoyIssue, ConvoyPhase, ConvoyProvisioningState, ConvoyReconciler,
    ConvoyRepositorySpec, ConvoySpec, ConvoyStatus, ConvoyStatusPatch, ConvoyTeardownRuntime, CrewCompletionClaim, CrewCompletionRefusal,
    CrewCompletionRefusalCause, CrewWorkPhase, CrewWorkState, DeclaredSubject, DiscoveredSubject, InputValue, InstantiatedExit,
    InstantiatedExitEntry, InstantiatedTurnDelivery, IssueSnapshot, LeafMaker, LifecycleMutation, NudgeObligation, PendingBrief,
    PendingSupervisorTurn, PlacementStatus, QueuedTurnObservation, ReconcileOutcome, SettlementEvaluation, SettlementMode, StallCause,
    StallEvidenceSource, StallNudge, StallProposedDisposition, StallReason, StallRung, StallSupervisor, StalledCondition, SubjectDiscovery,
    SubjectDiscoverySource, TargetMismatch, TurnDeliveryEpisode, TurnDeliveryFailure, TurnDeliveryFailureKind, TurnDeliveryOutcome,
    TurnDeliveryRung, TurnDeliveryStatus, UnmetSettlementExpectation, VesselPlacementPin, WorkCompletionAuthority, WorkPhase, WorkState,
    WorkflowSnapshot, CONVOY_TEARDOWN_FINALIZER, ENSURED_FROM_ANNOTATION, FORCE_TEARDOWN_ANNOTATION, PLACEMENT_SNAPSHOT_ANNOTATION,
    VESSEL_PLACEMENTS_ANNOTATION, WORKFLOW_SNAPSHOT_ANNOTATION,
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
    CREDENTIAL_SCOPES_ANNOTATION, CREDENTIAL_SCOPES_ENV, CREDENTIAL_SCOPES_SESSION_TAG,
};
pub use crew_image_baseline::{CrewImageBaseline, CrewImageBaselineSpec};
pub use definition::DefinitionResolver;
pub use dispatch_hold::{
    DispatchDeployment, DispatchDeploymentSpec, DispatchHold, DispatchHoldSpec, DispatchHoldStatus, DispatchHoldStatusPatch, HoldClearWhen,
};
pub use dispatch_observation::{DispatchObservation, DispatchObservationSpec, DISPATCH_RECONCILER_PROVENANCE};
pub use environment::{
    host_direct_environment_name, DockerEnvironmentSpec, Environment, EnvironmentMemoryPolicy, EnvironmentMount, EnvironmentMountMode,
    EnvironmentPhase, EnvironmentSpec, EnvironmentStatus, EnvironmentStatusPatch, HostDirectEnvironmentSpec,
};
pub use error::{FinalizerWaitReason, ResourceError};
pub use event::{Event, EventRecorder, EventRegarding, EventSpec, ObjectEvent, DEFAULT_EVENT_TTL_SECONDS};
pub use field_ownership::{FieldOwnedResource, FieldOwnership, FieldOwnershipViolation, OwnershipEnforcement, WriterIdentity, WriterRole};
pub use fleet_designation::{FleetDesignation, FleetDesignationSpec, FLEET_DESIGNATION_NAME};
pub use flotilla_protocol::{PrincipalRef, ResourceRef};
pub use forge::{Forge, ForgeKind, ForgeSpec};
pub use fulfilment_kind::{FulfilmentCostClass, FulfilmentGrant, FulfilmentKind, FulfilmentKindSpec, FulfilmentRealisation};
pub use host::{
    canonical_host_id, CachedModelProbe, CredentialExpiry, FulfilmentFacts, FulfilmentImage, HarnessFacts, Host, HostCondition,
    HostConnection, HostSpec, HostStatus, HostStatusPatch, ModelFact, ModelFactSource, ModelProbeState, AGENTLESS_CAPABILITY,
    AGENT_ADAPTERS_CAPABILITY, AMBIENT_CLAUDE_CREDENTIAL_SCOPE, CREDENTIAL_EXPIRY_CAPABILITY, HEARTBEAT_READY_TTL_SECS,
    HELD_CREDENTIALS_CAPABILITY, OWNING_DAEMON_CAPABILITY, PLACEMENT_CAPABILITY, SLEEP_INHIBITION_CONDITION_TYPE,
    TERMINAL_POOLS_CAPABILITY, TRANSPORT_CAPABILITY,
};
pub use http::{ensure_crd, ensure_namespace, HttpBackend};
pub use in_memory::InMemoryBackend;
pub use issue::{issue_record_name, Issue, IssueSpec, IssueStatus, IssueStatusPatch, ObservedIssueState};
pub use labels::{
    label_value, labels_match, LifecycleAuthority, AUTHORITY_LABEL, CHANGE_REQUEST_ID_LABEL, CONVOY_LABEL, CREW_ORDINAL_LABEL,
    GENERATION_LABEL, MANAGED_BY_LABEL, PROJECT_LABEL, REPO_KEY_LABEL, REPO_LABEL, RESERVED_PREFIX, ROLE_LABEL, VESSEL_LABEL,
    VESSEL_ORDINAL_LABEL, VESSEL_REF_LABEL,
};
pub use landing_gate::{evaluate_landing_gate, settlement_human_gate, LandingGateDecision, LANDING_APPROVE_OPTION, LANDING_REFUSE_OPTION};
pub use leaf::{
    actor_obligation, admit_leaf, evaluate_leaf, ArtifactLeafSubject, ChangeRequestLeafSubject, ConvoyLeafSubject, IssueLeafSubject,
    LeafEvaluation, LeafSubject, LeafValue, ThreeValue, UsageLeafSubject, VesselLeafSubject, WorkLeafSubject, ADMITTED_LEAF_VOCABULARY,
};
pub use manifest_root::{
    DocumentKey, DocumentPhase, DocumentState, ManifestRoot, ManifestRootSpec, ManifestRootStatus, ManifestRootStatusPatch, Resolution,
    ResolutionAction, ResolutionOutcome,
};
pub use owner_gc::OwnerGarbageCollector;
pub use placement_policy::{
    DockerCheckoutStrategy, DockerImagePullPolicy, DockerImageSource, DockerPerVesselPlacementPolicySpec,
    HostDirectPlacementPolicyCheckout, HostDirectPlacementPolicySpec, PlacementPolicy, PlacementPolicySpec,
};
pub use platform::Platform;
pub use prepared_snapshot::{
    content_hash, is_prepared_snapshot, PreparedSnapshotGarbageCollector, PreparedSnapshotGcResult, PLACEMENT_SNAPSHOT_KIND,
    PREPARED_SNAPSHOT_LABEL, WORKFLOW_SNAPSHOT_KIND,
};
pub use presentation::{Presentation, PresentationPhase, PresentationSpec, PresentationStatus, PresentationStatusPatch};
pub use principal_attention::{
    resolve_demand, Demand, DemandAddressee, DemandExpiry, DemandExpiryDisposition, DemandKind, DemandPoolRef, DemandResponseOption,
    DemandSpec, DemandState, DemandStatus, DemandStatusPatch, DemandTransition, DemandVerdict, DemandVerdictDisposition, HumanGateContext,
    Regard, RegardExpiryPolicy, RegardSource, RegardSpec, RegardStatus, RegardStatusPatch,
};
pub use project::{
    normalize_issue_source, normalize_project_spec, resolve_project_issue_sources, DeclarationRefusedCondition, DispatchLane,
    DispatchMission, DispatchPolicy, DispatchQueueAttention, DispatchQueueEntry, IssueFieldValue, IssueFilter, IssueSource,
    IssueSourceBindingSpec, IssueSourceResolution, IssueSourceUnavailable, OperationalEntriesCondition, Project, ProjectRepositoryRole,
    ProjectRepositorySpec, ProjectSpec, ProjectStatus, ProjectStatusPatch, ResolvedIssueSourceBinding,
    DEFAULT_DISPATCH_QUEUE_STALE_AFTER_SECONDS,
};
pub use project_hierarchy::ProjectHierarchy;
pub use provisioning_identity::{canonicalize_repo_url, clone_key, descriptive_repo_slug, forge_clone_key, forge_repo_key, repo_key};
pub use registry::{
    apply_manifest_resource_document, apply_resource_document, canonical_resource_kind, collect_resource_replica_kind,
    current_resource_kind_position, decode_stored_resource_document, delete_resource_kind, digest_resource_kind, get_resource_kind,
    get_resource_kind_all_provenances, get_resource_kind_including_replicas, home_bound_authorship_collisions, list_resource_kind,
    list_resource_kind_including_replicas, list_resource_kind_replica_sources, patch_resource_annotation, patch_resource_annotations,
    patch_resource_status, quarantine_undecodable_stored_objects, registered_resource_namespaces, replica_cursor_for_resource_kind,
    resource_document_spec_hash, resource_list_api_version, validate_resource_document, watch_resource_kind, watch_resource_kind_from,
    watch_resource_kind_including_replicas, watch_resource_kind_replica_sources, DynamicResourceDelete, DynamicResourceList,
    DynamicResourceObject, DynamicResourceWatch, HomeBoundAuthorshipCollision, RegisteredResourceKind, MANIFEST_WRITER_SOURCE,
    REGISTERED_RESOURCE_KINDS,
};
pub use replica::{ReadResourceList, ReadResourceObject, ReadWatchEvent, ReplicaCursor, ReplicationClass, ResourceProvenance};
pub use repository::{
    ensure_repository, repository_display_labels, repository_workspace_slugs, resolve_default_branch, DefaultBranchObservation,
    DefaultBranchProvenance, ForgeIdentity, Repository, RepositoryCheckoutKind, RepositoryCheckoutRef, RepositoryGitSpec,
    RepositoryIdentity, RepositoryKey, RepositoryProviderPreference, RepositoryRelation, RepositorySpec, RepositoryStatus,
    RepositoryStatusPatch, RepositoryUpstream, RepositoryVcsSpec,
};
pub use resource::{
    api_version, ApiPaths, CausalDot, FieldMergeMetadata, InputMeta, K8sListMeta, K8sObjectMeta, K8sResourceList, K8sResourceObject,
    K8sWatchEvent, MergeConflictSibling, MergeMetadata, ObjectMeta, OwnerReference, Resource, ResourceObject, BOOTSTRAP_PATH_ANNOTATION,
};
pub use retention::{
    EventRetention, ResourceDecodeQuarantine, ResourceEventDecodeQuarantine, ResourceStoreDiagnostics, ResourceStoreWarning,
};
pub use review_bundle::{
    publish_settlement_claim, validate_settlement_claim, validate_uploaded_settlement_claim, ClaimAdmissibilityError,
    ClaimPublicationError, FindingResolution, ReviewBundleIndex, ReviewBundleLocation, ReviewBundleStore, ReviewBundleStoreConfig,
    ReviewBundleStoreError, ReviewBundleWriteCredential, ReviewCheck, ReviewCheckOutcome, ReviewFinding, ReviewRefPair, ReviewRound,
    SettlementClaimEvidence, REVIEW_BUNDLE_INDEX_FILE, REVIEW_BUNDLE_ROOT,
};
pub use sqlite::SqliteBackend;
pub use status_patch::{apply_status_patch, apply_status_patch_checked, apply_status_patch_with_before_update, NoStatusPatch, StatusPatch};
pub use terminal_session::{
    terminal_session_attach_target, terminal_session_attach_target_with_stale_status, CrewCompletionPending, CrewMessageDelivery,
    CrewMessageSender, CrewSessionStatus, InnerCommandStatus, TerminalAttention, TerminalAttentionSource, TerminalAttentionState,
    TerminalBrief, TerminalCrewContext, TerminalCrewMessage, TerminalOccupancy, TerminalSession, TerminalSessionAttachTarget,
    TerminalSessionDegradedCondition, TerminalSessionIdentity, TerminalSessionPhase, TerminalSessionSource, TerminalSessionSpec,
    TerminalSessionStatus, TerminalSessionStatusPatch, TerminalSessionTag, TERMINAL_DELIVERY_EXPIRED_REASON,
    TERMINAL_DELIVERY_NOT_SUBMITTED_REASON, TERMINAL_DELIVERY_UNCONFIRMED_REASON,
};
pub use usage::{usage_record_name, Usage, UsagePace, UsageProviderCost, UsageSpec, UsageStatus, UsageStatusPatch, UsageWindow};
pub use vessel::{
    Vessel, VesselPhase, VesselSpec, VesselStatus, VesselStatusPatch, ACTUATOR_HOST_REF_ANNOTATION, ACTUATOR_SOURCE_ROOT_ANNOTATION,
};
pub use watch::{ResourceList, ResourcePosition, ResourceTombstone, WatchEvent, WatchStart, WatchStream};

#[doc(hidden)]
#[macro_export]
macro_rules! for_each_registered_resource {
    ($callback:ident, $($argument:expr),* $(,)?) => {{
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
        $callback::<$crate::Event>($($argument),*);
        $callback::<$crate::Host>($($argument),*);
        $callback::<$crate::ManifestRoot>($($argument),*);
        $callback::<$crate::PlacementPolicy>($($argument),*);
        $callback::<$crate::Presentation>($($argument),*);
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
    message_record_name, Message, MessageExpectation, MessagePhase, MessageReference, MessageRelation, MessageSpec, MessageStatus,
    MessageStatusPatch, MessageSubmission, ResolvedMessageReceiver,
};
pub use message_delivery::{MessageBatch, MessageObservation, MessageTransport, MessageTransportOutcome};
pub use message_inbox::{
    message_expectation_open, message_supersedes, qualify_message_address, qualify_message_spec, resolve_message_receiver,
    validate_message_address, MessageAddressContext, MessageAdmission, MessageInbox,
};
pub use workflow_template::{
    current_builtin_workflow_name, implement_review_workflow_spec, interactive_single_workflow_spec, single_agent_shepherd_workflow_spec,
    single_agent_workflow_spec, validate, AllocationDecision, ArtifactSubjectBinding, ClaimExit, CompletionCondition,
    CrewCompletionExpectation, CrewSource, CrewSpec, ExitDeclaration, HoldAct, InputDefinition, InterpolationField, InterpolationLocation,
    LeafTemplate, LegacyCompletionExpectation, RoleHandoff, Selector, StallNudgePolicy, Stance, SubjectVariable, SupervisionTarget,
    TurnDeliveryRule, TurnDeliveryTarget, ValidationError, VesselRequirement, WorkflowTemplate, WorkflowTemplateSpec,
};

pub mod role_cascade;
pub use credential::{HostActionSelector, HostImageAction};
pub use fleet_designation::ImageCacheBinding;
pub use image_build::{is_image_digest, ImageAcquisitionCost, ImageAvailability, IMAGE_DIGESTS_CAPABILITY};
pub use role_cascade::{ResolvedCascade, ResolvedSetting, RoleCascadeLayer, RoleDefinition};
