//! Resource-store query projection and demand-backed issue materialization.
//!
//! The daemon supervises this future. Each invocation subscribes afresh and
//! bootstraps durable and observed stores, preserving projection precedence.
// async_trait's generated futures trigger Clippy 1.99's redundant must-use lint.
// Remove when async_trait or Clippy stops producing this warning.
#![allow(clippy::double_must_use)]

use std::sync::Arc;

use crate::aggregator_projection::AggregatorProjectionState;
use flotilla_daemon_api::event_sink::EventSink;
use flotilla_protocol::{DaemonEvent, HostName};
use flotilla_resources::{Checkout, Convoy, Demand, Environment, Project, Regard, Repository, ResourceError};
use flotilla_store::ResourceBackend;
use futures::future::BoxFuture;
use tokio::sync::broadcast;

mod aggregator;
mod issue_materializer;

use aggregator::{Aggregator, AggregatorResolvers};
pub use aggregator::{AttachCapabilityResolver, ConvoyChangeRequestResolver};
pub use issue_materializer::{IssueMaterializationResolver, IssuePollingHealth};
pub mod aggregator_projection;
pub mod awareness_projection;
pub mod dispatch_board;
mod dispatch_ready;
pub mod fleet;
pub mod fleet_health;
pub mod host_registry;
pub mod host_summary;
pub mod ports;
mod project_repositories;
pub mod projections_actions;
pub mod query_registry;
pub mod read_projections;
pub mod resource_explain;
pub mod salience;
mod scoped_store;
mod standing_roles;
pub mod terminal_health;

/// Run one aggregator lifetime, including its provider resolvers and watch sources.
/// Dropping the future releases its watches and materialization tasks; the runtime
/// remains responsible for restart policy and reporting a returned store error.
pub fn run(
    context: AggregatorContext,
    namespace: &str,
    state: AggregatorProjectionState,
    issue_polling: IssuePollingHealth,
) -> BoxFuture<'_, Result<(), ResourceError>> {
    Box::pin(async move {
        let durable = context.durable;
        let observed = context.observed;
        let aggregator = Aggregator::with_events(state, context.host_name, context.event_sink, context.events)
            .with_attach_resolver_arc(context.attach)
            .with_lifecycle_reads(context.lifecycles)
            .with_change_request_resolver_arc(context.change_requests)
            .with_issue_resolver_arc(context.issues)
            .with_issue_polling_health(issue_polling)
            .with_convoy_engine(context.convoy_engine, namespace);
        aggregator
            .run(
                AggregatorResolvers::builder()
                    .durable_convoys(durable.including_replicas::<Convoy>(namespace))
                    .durable_convoy_ensures(durable.including_replicas::<flotilla_resources::ConvoyEnsure>(namespace))
                    .durable_demands(durable.clone().using::<Demand>(namespace))
                    .durable_environments(durable.clone().using::<Environment>(namespace))
                    .durable_fulfilment_kinds(durable.including_replicas::<flotilla_resources::FulfilmentKind>(namespace))
                    .durable_placement_policies(durable.including_replicas::<flotilla_resources::PlacementPolicy>(namespace))
                    .durable_sessions(durable.including_replicas::<flotilla_resources::TerminalSession>(namespace))
                    .durable_projects(durable.including_replicas::<Project>(namespace))
                    .durable_fleet_designation(durable.including_replicas::<flotilla_resources::FleetDesignation>(namespace))
                    .durable_repositories(durable.including_replicas::<Repository>(namespace))
                    .durable_regards(durable.using::<Regard>(namespace))
                    .durable_vessels(durable.including_replicas::<flotilla_resources::Vessel>(namespace))
                    .durable_hosts(durable.including_replicas::<flotilla_resources::Host>(namespace))
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

/// Runtime inputs for one supervised projection lifetime.
pub struct AggregatorContext {
    pub durable: ResourceBackend,
    pub observed: ResourceBackend,
    pub host_name: HostName,
    pub event_sink: Arc<dyn EventSink>,
    pub events: broadcast::Receiver<DaemonEvent>,
    pub attach: Arc<dyn AttachCapabilityResolver>,
    pub change_requests: Arc<dyn ConvoyChangeRequestResolver>,
    pub issues: Arc<dyn IssueMaterializationResolver>,
    /// The engine's provisioning readings; `NoLifecycleReads` with the switch off.
    pub lifecycles: Arc<dyn ports::LifecycleReads>,
    /// The `[primitives]` Convoy path: rows show the engine's promises when it
    /// holds a convoy (#3587 layer 8). Empty while the switch is off.
    pub convoy_engine: flotilla_resources::convoy::engine::EngineConvoyPath,
}
