use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    process::Command as ProcessCommand,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex as StdMutex,
    },
    time::Duration,
};

use async_trait::async_trait;
use chrono::Utc;
use flotilla_controllers::reconcilers::clone::runtime::{CloneControllerRuntime, CloneFlights};
use flotilla_controllers::reconcilers::{
    checkout::runtime::CheckoutControllerRuntime, CheckoutReconciler, CheckoutRemoval, CheckoutRemovalOutcome, CheckoutRuntime,
    CloneReconciler, CloneRuntime, DockerEnvironmentRuntime, EnvironmentReconciler, TerminalDeliveryFailure, TerminalDeliveryOutcome,
    TerminalDeliveryReadiness, TerminalLiveness, TerminalRuntime, TerminalSessionReconciler, VesselReconciler,
};
use flotilla_core::{
    agent_adapter::AgentAdapterRegistry,
    aggregator_projection::AggregatorProjectionState,
    config::ConfigStore,
    in_process::{InProcessDaemon, StandingConvoyBackingInspector, DEFAULT_PROVISIONING_NAMESPACE as NAMESPACE},
    providers::{
        change_request::ChangeRequestTracker,
        discovery::{
            test_support::{
                fake_discovery_with_provider_set, git_process_discovery, DiscoveryMockRunner, FakeChangeRequest, FakeChangeRequestFactory,
                FakeDiscoveryProviders, FakeTerminalPool, MergedPrProcessRunner, TestEnvVars,
            },
            EnvVars, EnvironmentAssertion, EnvironmentBag, ProviderCategory, ProviderDescriptor,
        },
        environment::{
            CreateOpts, EnvironmentHandle, EnvironmentProvider, EnvironmentVariableUpdate, PreparedEnvironmentAuth, ProvisionedEnvironment,
            ProvisionedMount, ProvisionedMountMode,
        },
        registry::ProviderRegistry,
        replay::{Masks, ReplayHttpClient, Session},
        terminal::{
            ScreenActivity, TerminalEnvVars, TerminalPool, TerminalSession as ProviderTerminalSession, TerminalSessionLiveness,
            TerminalSessionTag, TerminalSize,
        },
        types::ChangeRequest as ProviderChangeRequest,
        ChannelLabel, CommandOutput, CommandRunner, ProcessCommandRunner,
    },
};
use flotilla_credentials::{
    crew_git_identity_environment, test_support::GITHUB_APP_TEST_PRIVATE_KEY, AgentMaterialRegistry, CredentialRefreshError,
    CredentialStore, CONTAINER_CODEX_HOME, FLOTILLA_SKILLS_DIR_ENV,
};
use flotilla_daemon_api::daemon::DaemonHandle;
use flotilla_paths::path_context::{DaemonHostPath, ExecutionEnvironmentPath};
use flotilla_protocol::{
    CanonicalHostId, Command, CommandAction, CommandValue, CrewCommandContext, DaemonEvent, EnvironmentId, HostName, HostSummary, ImageId,
    NodeId, NodeInfo, PeerConnectionState, PlacementDecision, PlacementTargetHost, TerminalStatus,
};
use flotilla_resources::{
    clone_key,
    controller::{Actuation, ControllerLoop, Reconciler},
    delete_resource_kind, home_bound_authorship_collisions, watch_resource_kind, watch_resource_kind_including_replicas, Checkout,
    Checkout as ResourceCheckout, CheckoutIntegrationStatus, CheckoutPhase as ResourceCheckoutPhase, CheckoutSpec,
    CheckoutSpec as ResourceCheckoutSpec, CheckoutStatus as ResourceCheckoutStatus, CheckoutWorktreeSpec, Clone, CloneSpec, ConditionValue,
    ControllerRetry, ControllerRetryDisposition, Convoy, ConvoyEnsure, ConvoyEnsureSpec, ConvoyPhase, ConvoyProvisioningState,
    ConvoyReconciler, ConvoyRepositorySpec, ConvoySpec, ConvoyStatus, ConvoyTeardownRuntime, CredentialConsumer, CredentialGrant,
    CredentialLifecycle, CredentialPlacementRequirements, CredentialSource, CredentialSpec, CredentialSpecSpec, CrewSource, CrewSpec,
    Demand, DockerCheckoutStrategy, DockerPerVesselPlacementPolicySpec, Environment, EnvironmentPhase, EnvironmentSpec,
    EnvironmentStatusPatch, Forge, ForgeSpec, FulfilmentFacts, FulfilmentKind, FulfilmentKindSpec, FulfilmentRealisation, Host,
    HostDirectEnvironmentSpec, HostDirectPlacementPolicyCheckout, HostDirectPlacementPolicySpec, HostSpec, HostStatus, HostStatusPatch,
    InMemoryBackend, InputMeta, LifecycleAuthority, ManifestRoot, ModelProbeState, ObservedCheckoutSpec as ResourceObservedCheckoutSpec,
    PlacementPolicy, PlacementPolicySpec, PlacementStatus, Project, ReplicationClass, Repository, RepositoryKey, RepositorySpec,
    RepositoryTrust, Resource, ResourceBackend, ResourceError, ResourceList, Selector, SqliteBackend, StatusPatch, TerminalAttentionSource,
    TerminalAttentionState, TerminalSession, TerminalSessionPhase, TerminalSessionSource, TerminalSessionSpec, TerminalSessionStatus,
    TerminalSessionStatusPatch, Vessel, VesselRequirement, VesselSpec, VesselStatus, WorkPhase, WorkState, WorkflowTemplate,
    WorkflowTemplateSpec, ACTUATOR_HOST_REF_ANNOTATION, CONVOY_LABEL, CREDENTIAL_REFS_ENV, CREDENTIAL_REF_SESSION_TAG,
    CREDENTIAL_SCOPES_ENV, MANAGED_BY_LABEL,
};
use futures::StreamExt;
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::{Mutex, Notify, RwLock};

use super::{
    credentials::*, discovery::*, docker::*, environments::*, health::*, ports::*, seed::*, tasks::*, terminal::*,
    test_git_repo::TestGitRepo, *,
};
use crate::{
    blob_store::{BlobStore, MemoryBlobStore, TieredBlobStore},
    environment_tools::{
        tests::with_fourth_tool, EnvironmentToolProvisioner, CONTAINED_CARGO_SHIM_DIRECTORY, CONTAINED_CARGO_SHIM_PATH,
        CONTAINED_RUSTC_WRAPPER_PATH, ENVIRONMENT_CLEAT_GHOSTTY_LIBRARY_PATH, ENVIRONMENT_CLEAT_LIBRARY_DIR, ENVIRONMENT_CLEAT_PATH,
        ENVIRONMENT_CLEAT_RUNTIME_DIR, ENVIRONMENT_DAEMON_SOCKET_PATH, ENVIRONMENT_FLOTILLA_PATH, RUSTC_LINKER_WRAPPER,
    },
    resource_manifest::materialize_manifest_root,
    startup::{phase, test_support::GatedCredentialPreflight, PENDING_WARNING_THRESHOLD},
    supervisor::{ControllerSupervision, RestartBudgetExhausted},
};
use daemon_support::*;
use environment_support::*;
use launch_support::*;
use lifecycle_support::*;
use placement_support::*;
use recovery_scenario::*;
use terminal_support::*;

mod adopted_checkouts;
mod convoy_lifecycle;
mod credential_delivery;
mod discovery;
mod docker;
mod environments;
mod health;
mod heartbeat;
mod memory_tests;
mod message;
mod seed;
mod terminal;
mod work_credentials;

fn test_controller_vcs(runner: Arc<dyn CommandRunner>, checkout: &str) -> Arc<dyn flotilla_core::vcs::Vcs> {
    use flotilla_core::{
        providers::vcs::git_worktree::GitWorktreeStrategy,
        vcs::{FlotillaVcs, GitCheckoutStrategy},
    };
    Arc::new(FlotillaVcs::new(
        ExecutionEnvironmentPath::new(checkout),
        Arc::clone(&runner),
        GitCheckoutStrategy::Worktree(Box::new(GitWorktreeStrategy::new(".".into(), runner))),
    ))
}
mod daemon_support;

mod environment_support;

mod launch_support;

mod lifecycle_support;

mod placement_support;

mod recovery_scenario;

mod terminal_support;
