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
mod project;
mod provisioning_identity;
mod resource;
mod retention;
mod sqlite;
mod status_patch;
mod task_workspace;
mod terminal_session;
mod watch;
mod workflow_template;

pub use backend::{ResourceBackend, TypedResolver};
pub use checkout::{
    Checkout, CheckoutPhase, CheckoutSpec, CheckoutStatus, CheckoutStatusPatch, CheckoutWorktreeSpec, FreshCloneCheckoutSpec,
    ObservedCheckoutSpec,
};
pub use clone::{Clone, ClonePhase, CloneSpec, CloneStatus, CloneStatusPatch};
pub use convoy::{
    controller_patches, external_patches, provisioning_patches, reconcile, Convoy, ConvoyEvent, ConvoyPhase, ConvoyReconciler,
    ConvoyRepositorySpec, ConvoySpec, ConvoyStatus, ConvoyStatusPatch, InputValue, PlacementStatus, ReconcileOutcome, SnapshotTask,
    TaskPhase, TaskState, WorkflowSnapshot,
};
pub use environment::{
    DockerEnvironmentSpec, Environment, EnvironmentMount, EnvironmentMountMode, EnvironmentPhase, EnvironmentSpec, EnvironmentStatus,
    EnvironmentStatusPatch, HostDirectEnvironmentSpec,
};
pub use error::ResourceError;
pub use host::{Host, HostSpec, HostStatus, HostStatusPatch};
pub use http::{ensure_crd, ensure_namespace, HttpBackend};
pub use in_memory::InMemoryBackend;
pub use labels::{
    LifecycleAuthority, AUTHORITY_LABEL, CONVOY_LABEL, PROCESS_ORDINAL_LABEL, REPO_LABEL, RESERVED_PREFIX, ROLE_LABEL, TASK_LABEL,
    TASK_ORDINAL_LABEL, TASK_WORKSPACE_LABEL,
};
pub use placement_policy::{
    DockerCheckoutStrategy, DockerPerTaskPlacementPolicySpec, HostDirectPlacementPolicyCheckout, HostDirectPlacementPolicySpec,
    PlacementPolicy, PlacementPolicySpec,
};
pub use presentation::{Presentation, PresentationPhase, PresentationSpec, PresentationStatus, PresentationStatusPatch};
pub use project::{Project, ProjectRepositorySpec, ProjectSpec};
pub use provisioning_identity::{canonicalize_repo_url, clone_key, descriptive_repo_slug, repo_key};
pub use resource::{
    api_version, ApiPaths, InputMeta, K8sListMeta, K8sObjectMeta, K8sResourceList, K8sResourceObject, K8sWatchEvent, ObjectMeta,
    OwnerReference, Resource, ResourceObject,
};
pub use retention::{EventRetention, ResourceStoreDiagnostics, ResourceStoreWarning};
pub use sqlite::SqliteBackend;
pub use status_patch::{apply_status_patch, NoStatusPatch, StatusPatch};
pub use task_workspace::{TaskWorkspace, TaskWorkspacePhase, TaskWorkspaceSpec, TaskWorkspaceStatus, TaskWorkspaceStatusPatch};
pub use terminal_session::{
    InnerCommandStatus, TerminalSession, TerminalSessionPhase, TerminalSessionSpec, TerminalSessionStatus, TerminalSessionStatusPatch,
};
pub use watch::{ResourceList, WatchEvent, WatchStart, WatchStream};
pub use workflow_template::{
    validate, InputDefinition, InterpolationField, InterpolationLocation, ProcessDefinition, ProcessSource, Selector, TaskDefinition,
    ValidationError, WorkflowTemplate, WorkflowTemplateSpec,
};
