//! Vessel delivery (#3956): making what an occupant needs efficiently available
//! aboard a vessel, for every environment kind.
//!
//! A [`Need`] is a typed item with content identity. A provider instance declares
//! the optional [`HostPathOffer`]s it supports; agent fetch and agent push are
//! mandatory for every vessel agent, so a plan always exists. [`plan`] lowers each
//! need to the cheapest mechanism that meets its requirements, with costs in a
//! [`CostTable`] rather than code, and keeps what has already landed. The
//! planner is pure: needs, mechanisms and what is delivered in, a [`Plan`] out.
//! [`deliver`] is the one executor: it reads what landed aboard, plans, and runs
//! each step through the vessel agent alone, keyed by content digest.
//!
//! This crate knows no convoy and never parses an [`AccessKey`].

mod execute;
mod plan;
mod vocabulary;

pub use execute::{deliver, Delivered, DeliveryError, TreeDelivery, DELIVERY_RECORD};
pub use plan::{plan, Action, CostTable, Landed, Plan, PlanError, Step};
pub use vocabulary::{
    Access, AccessKey, ContentDigest, FetchBy, HostPathOffer, HostReach, Item, Liveness, Mechanism, Need, NeedName, ProviderMechanisms,
    Reachability, RelativePath, TreeSources, Visibility,
};
