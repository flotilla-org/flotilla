//! Resource-store query projection and demand-backed issue materialization.
//!
//! The daemon supervises this future. Each invocation subscribes afresh and
//! bootstraps durable and observed stores, preserving projection precedence.
// async_trait's generated futures trigger Clippy 1.99's redundant must-use lint.
// Remove when async_trait or Clippy stops producing this warning.
#![allow(clippy::double_must_use)]

use std::sync::Arc;

use flotilla_core::{aggregator_projection::AggregatorProjectionState, in_process::InProcessDaemon};
use flotilla_daemon_api::daemon::DaemonHandle;
use flotilla_resources::{Checkout, Convoy, Demand, Environment, Project, Regard, Repository, ResourceError};
use futures::future::BoxFuture;

mod aggregator;
mod issue_materializer;

use aggregator::{Aggregator, AggregatorResolvers};
pub use issue_materializer::IssuePollingHealth;

/// Run one aggregator lifetime, including its provider resolvers and watch sources.
/// Dropping the future releases its watches and materialization tasks; the runtime
/// remains responsible for restart policy and reporting a returned store error.
pub fn run(
    daemon: Arc<InProcessDaemon>,
    namespace: &str,
    state: AggregatorProjectionState,
    issue_polling: IssuePollingHealth,
) -> BoxFuture<'_, Result<(), ResourceError>> {
    Box::pin(async move {
        let durable = daemon.resource_backend();
        let observed = daemon.observed_resource_backend();
        let aggregator = Aggregator::with_events(state, daemon.host_name().clone(), daemon.event_sink(), daemon.subscribe())
            .with_attach_resolver(Arc::clone(&daemon))
            .with_change_request_resolver(Arc::clone(&daemon))
            .with_issue_resolver(Arc::clone(&daemon))
            .with_issue_polling_health(issue_polling);
        aggregator
            .run(
                AggregatorResolvers::builder()
                    .durable_convoys(durable.including_replicas::<Convoy>(namespace))
                    .durable_convoy_ensures(durable.including_replicas::<flotilla_resources::ConvoyEnsure>(namespace))
                    .durable_demands(durable.clone().using::<Demand>(namespace))
                    .durable_environments(durable.clone().using::<Environment>(namespace))
                    .durable_sessions(durable.including_replicas::<flotilla_resources::TerminalSession>(namespace))
                    .durable_projects(durable.including_replicas::<Project>(namespace))
                    .durable_fleet_designation(durable.including_replicas::<flotilla_resources::FleetDesignation>(namespace))
                    .durable_repositories(durable.including_replicas::<Repository>(namespace))
                    .durable_regards(durable.using::<Regard>(namespace))
                    .durable_vessels(durable.including_replicas::<flotilla_resources::Vessel>(namespace))
                    .durable_checkouts(durable.including_replicas::<Checkout>(namespace))
                    // Clone has no replication contract; remote failures arrive through Checkout status.
                    .durable_clones(durable.using::<flotilla_resources::Clone>(namespace))
                    .observed_convoys(observed.clone().using::<Convoy>(namespace))
                    .observed_sessions(observed.including_replicas::<flotilla_resources::TerminalSession>(namespace))
                    .observed_checkouts(observed.using::<Checkout>(namespace))
                    .observed_checkout_replicas(observed.including_replicas::<Checkout>(namespace))
                    .build(),
            )
            .await
    })
}
