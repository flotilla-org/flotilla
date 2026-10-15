use super::InProcessDaemon;
use async_trait::async_trait;
use flotilla_issue_tracker::IssueProvider;
use flotilla_projections::{
    convoy_branch_refresh::ConvoyBranchRefresh, AttachCapabilityResolver, ConvoyChangeRequestResolver, IssueMaterializationResolver,
};
use flotilla_protocol::{HostName, IssueSource, QueryScope, RepositoryKey};
use flotilla_resources::ResolvedIssueSourceBinding;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
#[async_trait]
impl ConvoyChangeRequestResolver for InProcessDaemon {
    async fn refresh_branch(
        &self,
        repositories: &[RepositoryKey],
        branch: &str,
        binding: Option<&flotilla_resources::BoundChangeRequest>,
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

    async fn origin_host_names(&self, origins: &HashSet<flotilla_protocol::NodeId>) -> HashMap<flotilla_protocol::NodeId, HostName> {
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
pub(super) struct ProjectionEnvironments<'a>(pub &'a flotilla_executor::environment_manager::EnvironmentManager);
#[async_trait]
impl flotilla_projections::ports::Environments for ProjectionEnvironments<'_> {
    fn local_inventory(&self) -> flotilla_protocol::ToolInventory {
        crate::convert::inventory_from_bag(&self.0.local_environment_bag())
    }
    async fn host_summary_environments(&self) -> Vec<flotilla_protocol::EnvironmentInfo> {
        self.0.host_summary_environments().await
    }
    async fn visible_environments(&self) -> Vec<flotilla_protocol::EnvironmentInfo> {
        self.0.visible_environments().await
    }
}

use crate::leaf_engine::LeafSubscriptionTable;
use flotilla_change_request::observation::ChangeRequestRef;
use flotilla_projections::read_projections::LeafSubscriptionRead;
use flotilla_provider_runtime::ObservationError;
use flotilla_resources::{LeafFiringRecord, LeafSubscriptionRow};
use std::time::Duration;
#[async_trait]
impl LeafSubscriptionRead for LeafSubscriptionTable {
    fn change_request_stale_after(&self) -> Duration {
        LeafSubscriptionTable::change_request_stale_after(self)
    }

    async fn change_request_observation_error(&self, subject: &ChangeRequestRef) -> Option<ObservationError> {
        LeafSubscriptionTable::change_request_observation_error(self, subject).await
    }

    async fn diagnostics(&self) -> Vec<(LeafSubscriptionRow, Vec<LeafFiringRecord>)> {
        LeafSubscriptionTable::diagnostics(self).await
    }
}
#[async_trait]
impl flotilla_projections::ports::Environments for InProcessDaemon {
    fn local_inventory(&self) -> flotilla_protocol::ToolInventory {
        crate::convert::inventory_from_bag(&self.environment_manager.local_environment_bag())
    }
    async fn host_summary_environments(&self) -> Vec<flotilla_protocol::EnvironmentInfo> {
        self.environment_manager.host_summary_environments().await
    }
    async fn visible_environments(&self) -> Vec<flotilla_protocol::EnvironmentInfo> {
        self.environment_manager.visible_environments().await
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
