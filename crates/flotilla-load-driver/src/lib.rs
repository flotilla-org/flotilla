//! Load driver for the dummy fleet (#3206).
//!
//! [`run`] keeps a planned number of convoys open on a [`FleetTarget`],
//! replacing each that ends, abandons convoys past their deadline, and measures
//! one window: outcomes and latencies it observes itself, and the target's
//! load metrics (store queues, controller wakes and requeues, replication,
//! turn delivery, memory and database size). The target is the seam: today's
//! control plane implements it in-process, and the ADR 0064 path can implement
//! it to run the same plans.

mod plan;
mod report;
mod run;
mod target;

pub use plan::LoadPlan;
pub use report::{ControllerReport, HomeReport, LoadReport, ReplicationReport, StoreReport, TargetErrors, WindowSummary};
pub use run::run;
pub use target::{ConvoyObservation, ConvoyState, FleetTarget, HomeSample, TargetSample};

#[cfg(test)]
mod tests;
