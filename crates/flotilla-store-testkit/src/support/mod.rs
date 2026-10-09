//! Reusable controller and resource-store test harnesses.

mod liveness;
mod write_counting;

pub use liveness::{
    assert_actuation_drop_recovery, assert_bounded_convergence, assert_degradation_not_wedging, assert_quiescence_at_fixpoint,
    assert_staleness_edges, assert_transition_sequence, run_transition_sequence, FixpointPredicate, LivenessEnrollment, LivenessScenario,
    LivenessStep, ReconcileStep, Transition, TransitionDriver, TransitionSequence, WorldBuilder, DEFAULT_PASS_BOUND,
};
pub use write_counting::{WriteCountingBackend, WriteCountingResolver};
