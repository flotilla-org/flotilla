mod backend;
mod checkout;
mod clone;
pub mod controller;
mod convoy;
mod environment;
mod error;
mod host;
mod http;
mod in_memory;
mod labels;
mod placement_policy;
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
pub mod tls;
mod vessel;
mod watch;
mod workflow_template;

pub use backend::{ReplicaReadResolver, ReplicaWriter, ResourceBackend, TypedResolver};
pub use checkout::{
    Checkout, CheckoutBranchProvenance, CheckoutIntegrationStatus, CheckoutPhase, CheckoutSpec, CheckoutStatus, CheckoutStatusPatch,
    CheckoutWorktreeSpec, ConditionValue, FreshCloneCheckoutSpec, IntegrationCondition, LandedEvidence, ObservedCheckoutSpec,
};
pub use clone::{Clone, ClonePhase, CloneSpec, CloneStatus, CloneStatusPatch};
pub use convoy::{
    controller_patches, external_patches, provisioning_patches, reconcile, Convoy, ConvoyEvent, ConvoyIssue, ConvoyPhase, ConvoyReconciler,
    ConvoyRepositorySpec, ConvoySpec, ConvoyStatus, ConvoyStatusPatch, CrewWorkPhase, CrewWorkState, InputValue, IssueSnapshot,
    PlacementStatus, ReconcileOutcome, WorkCompletionAuthority, WorkPhase, WorkState, WorkflowSnapshot,
};
pub use environment::{
    DockerEnvironmentSpec, Environment, EnvironmentMount, EnvironmentMountMode, EnvironmentPhase, EnvironmentSpec, EnvironmentStatus,
    EnvironmentStatusPatch, HostDirectEnvironmentSpec,
};
pub use error::ResourceError;
pub use flotilla_protocol::PrincipalRef;
pub use host::{Host, HostSpec, HostStatus, HostStatusPatch, AGENT_ADAPTERS_CAPABILITY, TERMINAL_POOLS_CAPABILITY};
pub use http::{ensure_crd, ensure_namespace, HttpBackend};
pub use in_memory::InMemoryBackend;
pub use labels::{
    LifecycleAuthority, AUTHORITY_LABEL, CONVOY_LABEL, CREW_ORDINAL_LABEL, REPO_KEY_LABEL, REPO_LABEL, RESERVED_PREFIX, ROLE_LABEL,
    VESSEL_LABEL, VESSEL_ORDINAL_LABEL, VESSEL_REF_LABEL,
};
pub use placement_policy::{
    DockerCheckoutStrategy, DockerPerVesselPlacementPolicySpec, HostDirectPlacementPolicyCheckout, HostDirectPlacementPolicySpec,
    PlacementPolicy, PlacementPolicySpec,
};
pub use presentation::{Presentation, PresentationPhase, PresentationSpec, PresentationStatus, PresentationStatusPatch};
pub use principal_attention::{
    Demand, DemandAddressee, DemandKind, DemandPoolRef, DemandSpec, DemandState, DemandStatus, DemandStatusPatch, DemandTransition, Regard,
    RegardExpiryPolicy, RegardSource, RegardSpec, RegardStatus, RegardStatusPatch,
};
pub use project::{
    normalize_project_spec, resolve_project_issue_sources, IssueSource, IssueSourceResolution, IssueSourceUnavailable, Project,
    ProjectRepositorySpec, ProjectSpec,
};
pub use provisioning_identity::{canonicalize_repo_url, clone_key, descriptive_repo_slug, repo_key};
pub use registry::{
    apply_resource_document, get_resource_kind, list_resource_kind, list_resource_kind_including_replicas, resource_list_api_version,
    watch_resource_kind, watch_resource_kind_from, watch_resource_kind_including_replicas, DynamicResourceList, DynamicResourceObject,
    DynamicResourceWatch, RegisteredResourceKind, REGISTERED_RESOURCE_KINDS,
};
pub use replica::{ReadResourceList, ReadResourceObject, ReadWatchEvent, ReplicaCursor, ReplicationClass, ResourceProvenance};
pub use repository::{
    ensure_repository, repository_display_labels, repository_workspace_slugs, resolve_default_branch, DefaultBranchObservation,
    DefaultBranchProvenance, ForgeIdentity, Repository, RepositoryCheckoutKind, RepositoryCheckoutRef, RepositoryIdentity, RepositoryKey,
    RepositorySpec, RepositoryStatus, RepositoryStatusPatch,
};
pub use resource::{
    api_version, ApiPaths, InputMeta, K8sListMeta, K8sObjectMeta, K8sResourceList, K8sResourceObject, K8sWatchEvent, ObjectMeta,
    OwnerReference, Resource, ResourceObject,
};
pub use retention::{EventRetention, ResourceStoreDiagnostics, ResourceStoreWarning};
pub use sqlite::SqliteBackend;
pub use status_patch::{apply_status_patch, apply_status_patch_checked, NoStatusPatch, StatusPatch};
pub use terminal_session::{
    terminal_session_attach_target, CrewSessionStatus, InnerCommandStatus, TerminalAttention, TerminalAttentionSource,
    TerminalAttentionState, TerminalBrief, TerminalCrewContext, TerminalCrewMessage, TerminalSession, TerminalSessionAttachTarget,
    TerminalSessionIdentity, TerminalSessionPhase, TerminalSessionSource, TerminalSessionSpec, TerminalSessionStatus,
    TerminalSessionStatusPatch, TerminalSessionTag,
};
pub use vessel::{Vessel, VesselPhase, VesselSpec, VesselStatus, VesselStatusPatch};
pub use watch::{ResourceList, WatchEvent, WatchStart, WatchStream};

#[doc(hidden)]
#[macro_export]
macro_rules! for_each_registered_resource {
    ($callback:ident, $($argument:expr),* $(,)?) => {{
        $callback::<$crate::Checkout>($($argument),*);
        $callback::<$crate::Clone>($($argument),*);
        $callback::<$crate::Convoy>($($argument),*);
        $callback::<$crate::Demand>($($argument),*);
        $callback::<$crate::Environment>($($argument),*);
        $callback::<$crate::Host>($($argument),*);
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
    interactive_single_workflow_spec, single_agent_contained_workflow_spec, validate, CrewSource, CrewSpec, InputDefinition,
    InterpolationField, InterpolationLocation, Selector, Stance, ValidationError, VesselRequirement, WorkflowTemplate,
    WorkflowTemplateSpec,
};
