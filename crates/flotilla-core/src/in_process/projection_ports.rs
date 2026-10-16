use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use flotilla_change_request::observation::ChangeRequestKey;
use flotilla_executor::environment_manager::EnvironmentManager;
use flotilla_issue_tracker::IssueProvider;
use flotilla_projections::{
    convoy_branch_refresh::ConvoyBranchRefresh, ports::Environments, read_projections::LeafSubscriptionRead, AttachCapabilityResolver,
    ConvoyChangeRequestResolver, IssueMaterializationResolver,
};
use flotilla_protocol::{EnvironmentInfo, HostName, IssueSource, NodeId, QueryScope, RepositoryKey, ToolInventory};
use flotilla_provider_runtime::ObservationError;
use flotilla_resources::{BoundChangeRequest, LeafFiringRecord, LeafSubscriptionRow, ResolvedIssueSourceBinding};

use super::InProcessDaemon;
use crate::leaf_engine::LeafSubscriptionTable;

#[async_trait]
impl ConvoyChangeRequestResolver for InProcessDaemon {
    async fn refresh_branch(
        &self,
        repositories: &[RepositoryKey],
        branch: &str,
        binding: Option<&BoundChangeRequest>,
    ) -> ConvoyBranchRefresh {
        self.refresh_convoy_branch(repositories, branch, binding).await
    }

    async fn discover_branch_subjects(
        &self,
        namespace: &str,
        convoy: &str,
        branch: &str,
        resolution: ConvoyBranchRefresh,
    ) -> Result<(), String> {
        self.discover_convoy_branch_subjects_with_resolution(namespace, convoy, branch, Some(&resolution)).await
    }
}

#[async_trait]
impl AttachCapabilityResolver for InProcessDaemon {
    async fn resolvable_attach_targets(&self, targets: &[(String, HostName)]) -> Result<Vec<bool>, String> {
        self.resolvable_attach_targets_internal(targets).await
    }

    async fn origin_host_names(&self, origins: &HashSet<NodeId>) -> HashMap<NodeId, HostName> {
        self.origin_host_names_internal(origins).await
    }
}

#[async_trait]
impl IssueMaterializationResolver for InProcessDaemon {
    async fn resolve_issue_sources(&self, scope: &QueryScope) -> Result<Vec<ResolvedIssueSourceBinding>, String> {
        self.resolve_issue_source_bindings(scope).await
    }

    async fn issue_provider_for(&self, source: &IssueSource) -> Result<Arc<dyn IssueProvider>, String> {
        self.issue_provider_for_source(source).await
    }
}

pub(super) struct ProjectionEnvironments<'a>(pub &'a EnvironmentManager);

#[async_trait]
impl Environments for ProjectionEnvironments<'_> {
    fn local_inventory(&self) -> ToolInventory {
        crate::convert::inventory_from_bag(&self.0.local_environment_bag())
    }
    async fn host_summary_environments(&self) -> Vec<EnvironmentInfo> {
        self.0.host_summary_environments().await
    }
    async fn visible_environments(&self) -> Vec<EnvironmentInfo> {
        self.0.visible_environments().await
    }
}

#[async_trait]
impl LeafSubscriptionRead for LeafSubscriptionTable {
    fn change_request_stale_after(&self) -> Duration {
        LeafSubscriptionTable::change_request_stale_after(self)
    }

    async fn change_request_observation_error(&self, subject: &ChangeRequestKey) -> Option<ObservationError> {
        LeafSubscriptionTable::change_request_observation_error(self, subject).await
    }

    async fn diagnostics(&self) -> Vec<(LeafSubscriptionRow, Vec<LeafFiringRecord>)> {
        LeafSubscriptionTable::diagnostics(self).await
    }
}

#[async_trait]
impl Environments for InProcessDaemon {
    fn local_inventory(&self) -> ToolInventory {
        ProjectionEnvironments(&self.environment_manager).local_inventory()
    }
    async fn host_summary_environments(&self) -> Vec<EnvironmentInfo> {
        ProjectionEnvironments(&self.environment_manager).host_summary_environments().await
    }
    async fn visible_environments(&self) -> Vec<EnvironmentInfo> {
        ProjectionEnvironments(&self.environment_manager).visible_environments().await
    }
}

pub(super) struct MessageReceivers;
#[async_trait]
impl flotilla_projections::read_projections::MessageReceiverLoader for MessageReceivers {
    async fn load(
        &self,
        backend: &flotilla_store::ResourceBackend,
        namespace: &str,
        convoys: Vec<flotilla_resources::ReadResourceObject<flotilla_resources::Convoy>>,
        terminals: Vec<flotilla_resources::ReadResourceObject<flotilla_resources::TerminalSession>>,
    ) -> Result<Box<dyn flotilla_projections::read_projections::MessageReceiverRead>, String> {
        Ok(Box::new(MessageReceiverEvidence(
            flotilla_messaging::MessageReceiverSnapshot::load_from_observations(backend, namespace, convoys, terminals)
                .await
                .map_err(|e| e.to_string())?,
        )))
    }
}
pub(super) struct MessageReceiverEvidence(pub flotilla_messaging::MessageReceiverSnapshot);
impl flotilla_projections::read_projections::MessageReceiverRead for MessageReceiverEvidence {
    fn terminals(&self) -> &[flotilla_resources::ReadResourceObject<flotilla_resources::TerminalSession>] {
        self.0.terminals()
    }
    fn may_address_convoy(&self, address: &str, project: &str, convoy: &str) -> bool {
        self.0.may_address_convoy(address, project, convoy)
    }
    fn resolve(
        &self,
        address: &str,
    ) -> Result<Option<flotilla_resources::ReadResourceObject<flotilla_resources::TerminalSession>>, String> {
        self.0.resolve(address).map_err(|e| e.to_string())
    }
}
