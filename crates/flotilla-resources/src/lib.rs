mod backend;
mod change_request;
mod checkout;
mod clock;
mod clone;
pub mod controller;
mod convoy;
mod convoy_ensure;
mod credential;
mod definition;
mod dispatch_observation;
mod environment;
mod error;
mod field_ownership;
mod host;
mod http;
mod in_memory;
mod labels;
mod leaf;
mod material_pool;
mod placement_policy;
mod prepared_snapshot;
mod presentation;
mod principal_attention;
mod project;
mod provisioning_identity;
mod registry;
mod replica;
mod repository;
mod resource;
mod retention;
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

pub use backend::{ReplicaReadResolver, ReplicaWriter, ResourceBackend, TypedResolver};
pub use change_request::{
    change_request_record_name, ChangeRequest, ChangeRequestReviewObservation, ChangeRequestSpec, ChangeRequestStatus,
    ChangeRequestStatusPatch, Observation, ObservedChangeRequestState, ObservedChecks, ObservedMergeability,
};
pub use checkout::{
    latch_evidence_backed_integration, ChangeRequestMergeability, ChangeRequestObservation, ChangeRequestState, Checkout,
    CheckoutBranchProvenance, CheckoutIntegrationStatus, CheckoutPhase, CheckoutSpec, CheckoutStatus, CheckoutStatusPatch,
    CheckoutWorktreeSpec, ConditionValue, FreshCloneCheckoutSpec, IntegrationCondition, LandedEvidence, ObservedCheckoutSpec,
};
#[cfg(any(test, feature = "test-support"))]
pub use clock::VirtualClock;
pub use clock::{Clock, SystemClock};
pub use clone::{Clone, ClonePhase, CloneSpec, CloneStatus, CloneStatusPatch};
pub use convoy::{
    bound_change_request_record_name, change_request_address, controller_patches, convoy_sanctions_checkout_reclaim,
    evaluate_landing_settlement, expected_change_request_leaves, expected_checkout_refs, external_patches, instantiate_exit,
    instantiate_turn_delivery, pinned_placement_ref, pinned_workflow_ref, provisioning_patches, reconcile, select_convoy_children,
    BoundChangeRequest, Convoy, ConvoyAttention, ConvoyEvent, ConvoyIssue, ConvoyPhase, ConvoyReconciler, ConvoyRepositorySpec, ConvoySpec,
    ConvoyStatus, ConvoyStatusPatch, ConvoyTeardownRuntime, CrewWorkPhase, CrewWorkState, InputValue, InstantiatedExit,
    InstantiatedExitEntry, InstantiatedTurnDelivery, IssueSnapshot, PendingBrief, PlacementStatus, ReconcileOutcome, SettlementEvaluation,
    SettlementMode, TargetMismatch, TurnDeliveryEpisode, TurnDeliveryOutcome, TurnDeliveryRung, TurnDeliveryStatus,
    UnmetSettlementExpectation, WorkCompletionAuthority, WorkPhase, WorkState, WorkflowSnapshot, PLACEMENT_SNAPSHOT_ANNOTATION,
    WORKFLOW_SNAPSHOT_ANNOTATION,
};
pub use convoy_ensure::{
    ConvoyEnsure, ConvoyEnsureCondition, ConvoyEnsureHoldReason, ConvoyEnsureSpec, ConvoyEnsureStatus, ConvoyEnsureStatusPatch,
    DRIVER_ADMISSION_CONDITION_TYPE,
};
pub use credential::{
    CredentialConsumer, CredentialGrant, CredentialGrantSelector, CredentialGrantSpec, CredentialLifecycle,
    CredentialPlacementRequirements, CredentialSource, CredentialSpec, CredentialSpecSpec, CREDENTIAL_REFS_ANNOTATION, CREDENTIAL_REFS_ENV,
    CREDENTIAL_REF_SESSION_TAG, CREDENTIAL_SCOPES_ANNOTATION, CREDENTIAL_SCOPES_ENV, CREDENTIAL_SCOPES_SESSION_TAG,
};
pub use definition::DefinitionResolver;
pub use dispatch_observation::{DispatchObservation, DispatchObservationSpec, DISPATCH_RECONCILER_PROVENANCE};
pub use environment::{
    DockerEnvironmentSpec, Environment, EnvironmentMount, EnvironmentMountMode, EnvironmentPhase, EnvironmentSpec, EnvironmentStatus,
    EnvironmentStatusPatch, EnvironmentWaitReason, HostDirectEnvironmentSpec,
};
pub use error::ResourceError;
pub use field_ownership::{FieldOwnedResource, FieldOwnership, FieldOwnershipViolation, OwnershipEnforcement, WriterIdentity, WriterRole};
pub use flotilla_protocol::{PrincipalRef, ResourceRef};
pub use host::{
    canonical_host_id, CredentialExpiry, Host, HostCondition, HostSpec, HostStatus, HostStatusPatch, AGENT_ADAPTERS_CAPABILITY,
    AMBIENT_CLAUDE_CREDENTIAL_SCOPE, CREDENTIAL_EXPIRY_CAPABILITY, HEARTBEAT_READY_TTL_SECS, HELD_CREDENTIALS_CAPABILITY,
    SLEEP_INHIBITION_CONDITION_TYPE, TERMINAL_POOLS_CAPABILITY,
};
pub use http::{ensure_crd, ensure_namespace, HttpBackend};
pub use in_memory::InMemoryBackend;
pub use labels::{
    LifecycleAuthority, AUTHORITY_LABEL, CHANGE_REQUEST_ID_LABEL, CONVOY_LABEL, CREW_ORDINAL_LABEL, GENERATION_LABEL, MANAGED_BY_LABEL,
    MANIFEST_RESOLUTION_ANNOTATION, PROJECT_LABEL, REPO_KEY_LABEL, REPO_LABEL, RESERVED_PREFIX, ROLE_LABEL, VESSEL_LABEL,
    VESSEL_ORDINAL_LABEL, VESSEL_REF_LABEL,
};
pub use leaf::{
    admit_leaf, evaluate_leaf, ChangeRequestLeafSubject, ConvoyLeafSubject, LeafEvaluation, LeafSubject, LeafValue, ThreeValue,
    UsageLeafSubject, VesselLeafSubject, WorkLeafSubject, ADMITTED_LEAF_VOCABULARY,
};
pub use material_pool::{
    MaterialPool, MaterialPoolLease, MaterialPoolSpec, MaterialPoolStatus, MaterialPoolStatusPatch, MaterialPoolUnitSpec,
};
pub use placement_policy::{
    DockerCheckoutStrategy, DockerImagePullPolicy, DockerPerVesselPlacementPolicySpec, HostDirectPlacementPolicyCheckout,
    HostDirectPlacementPolicySpec, PlacementPolicy, PlacementPolicySpec,
};
pub use prepared_snapshot::{
    content_hash, is_prepared_snapshot, PreparedSnapshotGarbageCollector, PreparedSnapshotGcResult, PLACEMENT_SNAPSHOT_KIND,
    PREPARED_SNAPSHOT_LABEL, WORKFLOW_SNAPSHOT_KIND,
};
pub use presentation::{Presentation, PresentationPhase, PresentationSpec, PresentationStatus, PresentationStatusPatch};
pub use principal_attention::{
    resolve_demand, Demand, DemandAddressee, DemandExpiry, DemandExpiryDisposition, DemandKind, DemandPoolRef, DemandResponseOption,
    DemandSpec, DemandState, DemandStatus, DemandStatusPatch, DemandTransition, DemandVerdict, DemandVerdictDisposition, Regard,
    RegardExpiryPolicy, RegardSource, RegardSpec, RegardStatus, RegardStatusPatch,
};
pub use project::{
    normalize_project_spec, resolve_project_issue_sources, DispatchPolicy, DispatchQueueAttention, DispatchQueueEntry, IssueSource,
    IssueSourceResolution, IssueSourceUnavailable, OperationalEntriesCondition, Project, ProjectRepositoryRole, ProjectRepositorySpec,
    ProjectSpec, ProjectStatus, ProjectStatusPatch, DEFAULT_DISPATCH_QUEUE_STALE_AFTER_SECONDS,
};
pub use provisioning_identity::{canonicalize_repo_url, clone_key, descriptive_repo_slug, repo_key};
pub use registry::{
    apply_manifest_resource_document, apply_resource_document, collect_resource_replica_kind, delete_resource_kind, get_resource_kind,
    get_resource_kind_including_replicas, home_bound_authorship_collisions, list_resource_kind, list_resource_kind_including_replicas,
    list_resource_kind_replica_sources, patch_resource_annotation, patch_resource_annotations, patch_resource_status,
    quarantine_undecodable_stored_objects, replica_cursor_for_resource_kind, resource_document_spec_hash, resource_list_api_version,
    watch_resource_kind, watch_resource_kind_from, watch_resource_kind_including_replicas, watch_resource_kind_replica_sources,
    DynamicResourceDelete, DynamicResourceList, DynamicResourceObject, DynamicResourceWatch, HomeBoundAuthorshipCollision,
    RegisteredResourceKind, REGISTERED_RESOURCE_KINDS,
};
pub use replica::{ReadResourceList, ReadResourceObject, ReadWatchEvent, ReplicaCursor, ReplicationClass, ResourceProvenance};
pub use repository::{
    ensure_repository, repository_display_labels, repository_workspace_slugs, resolve_default_branch, DefaultBranchObservation,
    DefaultBranchProvenance, ForgeIdentity, Repository, RepositoryCheckoutKind, RepositoryCheckoutRef, RepositoryIdentity, RepositoryKey,
    RepositoryRelation, RepositorySpec, RepositoryStatus, RepositoryStatusPatch, RepositoryUpstream,
};
pub use resource::{
    api_version, ApiPaths, CausalDot, FieldMergeMetadata, InputMeta, K8sListMeta, K8sObjectMeta, K8sResourceList, K8sResourceObject,
    K8sWatchEvent, MergeConflictSibling, MergeMetadata, ObjectMeta, OwnerReference, Resource, ResourceObject,
};
pub use retention::{
    EventRetention, ResourceDecodeQuarantine, ResourceEventDecodeQuarantine, ResourceStoreDiagnostics, ResourceStoreWarning,
};
pub use sqlite::SqliteBackend;
pub use status_patch::{apply_status_patch, apply_status_patch_checked, NoStatusPatch, StatusPatch};
pub use terminal_session::{
    terminal_session_attach_target, CrewCompletionPending, CrewSessionStatus, InnerCommandStatus, TerminalAttention,
    TerminalAttentionSource, TerminalAttentionState, TerminalBrief, TerminalCrewContext, TerminalCrewMessage, TerminalOccupancy,
    TerminalSession, TerminalSessionAttachTarget, TerminalSessionDegradedCondition, TerminalSessionIdentity, TerminalSessionPhase,
    TerminalSessionSource, TerminalSessionSpec, TerminalSessionStatus, TerminalSessionStatusPatch, TerminalSessionTag,
};
pub use usage::{usage_record_name, Usage, UsagePace, UsageProviderCost, UsageSpec, UsageStatus, UsageStatusPatch, UsageWindow};
pub use vessel::{
    Vessel, VesselPhase, VesselSpec, VesselStatus, VesselStatusPatch, ACTUATOR_HOST_REF_ANNOTATION, ACTUATOR_SOURCE_ROOT_ANNOTATION,
};
pub use watch::{ResourceList, ResourceTombstone, WatchEvent, WatchStart, WatchStream};

#[doc(hidden)]
#[macro_export]
macro_rules! for_each_registered_resource {
    ($callback:ident, $($argument:expr),* $(,)?) => {{
        $callback::<$crate::Checkout>($($argument),*);
        $callback::<$crate::ChangeRequest>($($argument),*);
        $callback::<$crate::Clone>($($argument),*);
        $callback::<$crate::Convoy>($($argument),*);
        $callback::<$crate::ConvoyEnsure>($($argument),*);
        $callback::<$crate::CredentialGrant>($($argument),*);
        $callback::<$crate::CredentialSpec>($($argument),*);
        $callback::<$crate::Demand>($($argument),*);
        $callback::<$crate::DispatchObservation>($($argument),*);
        $callback::<$crate::Environment>($($argument),*);
        $callback::<$crate::Host>($($argument),*);
        $callback::<$crate::MaterialPool>($($argument),*);
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
pub use workflow_template::{
    implement_review_workflow_spec, interactive_single_workflow_spec, single_agent_contained_workflow_spec,
    single_agent_shepherd_workflow_spec, single_agent_trusted_workflow_spec, validate, ClaimExit, CrewSource, CrewSpec, ExitDeclaration,
    HoldAct, InputDefinition, InterpolationField, InterpolationLocation, LeafTemplate, Selector, Stance, SubjectVariable, TurnDeliveryRule,
    TurnDeliveryTarget, ValidationError, VesselRequirement, WorkflowTemplate, WorkflowTemplateSpec,
};
