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
use flotilla_core::agent_adapter::AgentAdapterRegistry;
use flotilla_core::aggregator_projection::AggregatorProjectionState;
use flotilla_core::config::ConfigStore;
use flotilla_core::discovery_api::EnvironmentAssertion;
use flotilla_core::discovery_api::EnvironmentBag;
use flotilla_core::in_process::InProcessDaemon;
use flotilla_core::in_process::StandingConvoyBackingInspector;
use flotilla_core::in_process::DEFAULT_PROVISIONING_NAMESPACE as NAMESPACE;
use flotilla_core::providers::change_request::ChangeRequestTracker;
use flotilla_core::providers::discovery::EnvVars;
use flotilla_core::providers::discovery::ProviderCategory;
use flotilla_core::providers::discovery::ProviderDescriptor;
use flotilla_core::providers::environment::CreateOpts;
use flotilla_core::providers::environment::EnvironmentHandle;
use flotilla_core::providers::environment::EnvironmentProvider;
use flotilla_core::providers::environment::EnvironmentVariableUpdate;
use flotilla_core::providers::environment::PreparedEnvironmentAuth;
use flotilla_core::providers::environment::ProvisionedEnvironment;
use flotilla_core::providers::environment::ProvisionedMount;
use flotilla_core::providers::environment::ProvisionedMountMode;
use flotilla_core::providers::registry::ProviderRegistry;
use flotilla_core::providers::terminal::ScreenActivity;
use flotilla_core::providers::terminal::TerminalEnvVars;
use flotilla_core::providers::terminal::TerminalPool;
use flotilla_core::providers::terminal::TerminalSession as ProviderTerminalSession;
use flotilla_core::providers::terminal::TerminalSessionLiveness;
use flotilla_core::providers::terminal::TerminalSessionTag;
use flotilla_core::providers::terminal::TerminalSize;
use flotilla_core::providers::types::ChangeRequest as ProviderChangeRequest;
use flotilla_core::providers::ChannelLabel;
use flotilla_core::providers::CommandOutput;
use flotilla_core::providers::CommandRunner;
use flotilla_core::providers::ProcessCommandRunner;
use flotilla_credentials::crew_git_identity_environment;
use flotilla_credentials::AgentMaterialRegistry;
use flotilla_credentials::CredentialRefreshError;
use flotilla_credentials::CredentialStore;
use flotilla_credentials::CONTAINER_CODEX_HOME;
use flotilla_credentials::FLOTILLA_SKILLS_DIR_ENV;
use flotilla_credentials_testkit::GITHUB_APP_TEST_PRIVATE_KEY;
use flotilla_daemon_api::daemon::DaemonHandle;
use flotilla_discovery_testkit::fake_discovery_with_provider_set;
use flotilla_discovery_testkit::git_process_discovery;
use flotilla_discovery_testkit::DiscoveryMockRunner;
use flotilla_discovery_testkit::FakeChangeRequest;
use flotilla_discovery_testkit::FakeChangeRequestFactory;
use flotilla_discovery_testkit::FakeDiscoveryProviders;
use flotilla_discovery_testkit::FakeTerminalPool;
use flotilla_discovery_testkit::MergedPrProcessRunner;
use flotilla_discovery_testkit::TestEnvVars;
use flotilla_paths::path_context::DaemonHostPath;
use flotilla_paths::path_context::ExecutionEnvironmentPath;
use flotilla_protocol::{
    CanonicalHostId, Command, CommandAction, CommandValue, CrewCommandContext, DaemonEvent, EnvironmentId, HostName, HostSummary, ImageId,
    NodeId, NodeInfo, PeerConnectionState, PlacementDecision, PlacementTargetHost, TerminalStatus,
};
use flotilla_replay_testkit::Masks;
use flotilla_replay_testkit::ReplayHttpClient;
use flotilla_replay_testkit::Session;
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
