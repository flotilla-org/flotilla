//! Composition-root scenarios, grouped by the transaction or projection they exercise.
//! Shared test support keeps cross-area credential staging and forge observations reusable.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::atomic::AtomicUsize,
};

use chrono::TimeZone;
use flotilla_resources::{
    ConvoyStatus, CredentialConsumer, CredentialExpiry, CredentialGrant, CredentialGrantSelector, CredentialGrantSpec, CredentialLifecycle,
    CredentialPlacementRequirements, CredentialSource, CredentialSpec, CredentialSpecSpec, CrewSource, CrewSpec, CrewWorkPhase,
    CrewWorkState, Environment as ResourceEnvironment, EnvironmentSpec as ResourceEnvironmentSpec, FulfilmentFacts, FulfilmentKindSpec,
    FulfilmentRealisation, HarnessFacts, HostCondition, HostDirectEnvironmentSpec, HostDirectPlacementPolicyCheckout,
    HostDirectPlacementPolicySpec, HostSpec, HostStatus, ImageAcquisitionCost, PlacementPolicy, PlacementPolicySpec, Selector,
    TerminalAttention, TerminalAttentionSource, TerminalAttentionState, TerminalSession as ResourceTerminalSession,
    TerminalSessionPhase as ResourceTerminalSessionPhase, TerminalSessionSource, TerminalSessionSpec as ResourceTerminalSessionSpec,
    TerminalSessionStatus as ResourceTerminalSessionStatus, VesselRequirement, VesselSpec, WorkflowTemplateSpec, AGENT_ADAPTERS_CAPABILITY,
    CONVOY_LABEL, GENERATION_LABEL, PROJECT_LABEL, ROLE_LABEL, VESSEL_LABEL, VESSEL_REF_LABEL,
};

use super::{
    convoy_admission::{
        default_convoy_placement_policy, parse_role_address, resolve_workflow_credentials, validate_workflow_agent_adapters,
        validate_workflow_credentials, validate_workflow_credentials_with_capabilities, KindCandidate, PlacementTieBreak,
        RepositoryChangeRequestProvider,
    },
    crew_ops::{convoy_sender_address, queue_pending_crew_message, terminal_meta_with_vessel_credentials},
    *,
};
use crate::{
    admission::AvailableSpaceProbe,
    providers::{
        change_request::ChangeRequestTracker,
        discovery::{Factory, ProviderCategory, ProviderDescriptor, UnmetRequirement},
        types::ChangeRequest,
        vcs::git_worktree::GitWorktreeStrategy,
        CommandOutput,
    },
    repository_inspection::{LocalCheckoutInspection, RepositoryContinuity, RepositoryInspection, RepositoryInspector},
    vcs::GitCheckoutStrategy,
};

use crate::providers::{
    discovery::test_support::{
        fake_discovery, fake_discovery_with_provider_set, fake_discovery_with_runner, FakeChangeRequest, FakeDiscoveryProviders,
        FakeVcsFactory, FakeVcsState,
    },
    testing::MockRunner,
};

mod support;
use support::*;
pub(super) use support::{create_identity_convoy, create_running_session, create_test_environment, test_meta};
mod observation_support;
use observation_support::*;
mod admission;
mod branch_discovery;
mod checkout_providers;
mod credential_admission;
mod credential_handoff;
mod crew_lifecycle;
mod dispatch_board;
mod host_admission;
mod host_events;
mod image_admission;
mod messages;
mod observation_completion;
mod observation_cooldown;
mod observation_ownership;
mod observation_pagination;
mod placement;
mod repository_queries;
mod resume;
mod supervision;
mod turn_delivery;
