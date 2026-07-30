use std::{fmt::Debug, sync::Arc};

use async_trait::async_trait;
use chrono::Duration;

use crate::VirtualClock;

pub const DEFAULT_PASS_BOUND: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LivenessScenario {
    Normal,
    ActuationDrop,
    Contradictory,
}

#[derive(Debug)]
pub struct LivenessStep<P, A> {
    pub patch: Option<P>,
    pub actuations: Vec<A>,
}

impl<P, A> LivenessStep<P, A> {
    pub fn new(patch: Option<P>, actuations: Vec<A>) -> Self {
        Self { patch, actuations }
    }
}

#[async_trait]
pub trait WorldBuilder: Send + Sync {
    type World: Send;

    async fn build(&self, scenario: LivenessScenario) -> Result<Self::World, String>;
}

#[async_trait]
pub trait ReconcileStep<W>: Send + Sync {
    type Patch: Send;
    type Actuation: Debug + Send;

    async fn reconcile_step(&self, world: &mut W) -> Result<LivenessStep<Self::Patch, Self::Actuation>, String>;
    async fn apply_patch(&self, world: &mut W, patch: Self::Patch) -> Result<(), String>;
    async fn apply_actuation(&self, world: &mut W, actuation: Self::Actuation) -> Result<(), String>;
}

pub trait FixpointPredicate<W>: Send + Sync {
    fn at_fixpoint(&self, world: &W) -> bool;

    fn write_count(&self, _world: &W) -> usize {
        0
    }

    fn reset_write_count(&self, _world: &W) {}

    fn held(&self, _world: &W) -> bool {
        false
    }

    fn probe_count(&self, _world: &W) -> usize {
        0
    }
}

/// A disturbance or controller action in a hand-written liveness trace.
///
/// Field and value types are fixture-defined so the same vocabulary can drive
/// every enrolled reconciler without teaching the harness about resource
/// schemas. Actuations emitted by [`Transition::Reconcile`] remain pending
/// until a later [`Transition::DeliverActuation`] or
/// [`Transition::DropActuation`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transition<F, V, O> {
    Reconcile,
    ExternalSpecWrite(F, V),
    Delete,
    RestartController,
    AdvanceClock(Duration),
    DropActuation,
    DeliverActuation,
    PartitionStore(O),
}

/// Fixture-specific interpretation of disturbances in a transition trace.
///
/// This is an adapter at the liveness harness seam: the generic runner owns
/// sequencing and actuation control while each enrolled reconciler owns the
/// meaning of fields, deletion, restart, and store origins.
#[async_trait]
pub trait TransitionDriver<W>: ReconcileStep<W> {
    type Field: Debug + Send + Sync;
    type Value: Debug + Send + Sync;
    type OriginRoot: Debug + Send + Sync;

    async fn external_spec_write(&self, world: &mut W, field: &Self::Field, value: &Self::Value) -> Result<(), String>;
    async fn delete(&self, world: &mut W) -> Result<(), String>;
    async fn restart_controller(&self, world: &mut W) -> Result<(), String>;
    async fn partition_store(&self, world: &mut W, origin_root: &Self::OriginRoot) -> Result<(), String>;
}

struct CoverageAssertion<W> {
    description: String,
    predicate: Box<dyn Fn(&W) -> bool + Send + Sync>,
}

/// A hand-written transition trace with non-vacuity assertions.
pub struct TransitionSequence<W, F, V, O> {
    transitions: Vec<Transition<F, V, O>>,
    coverage: Vec<CoverageAssertion<W>>,
}

impl<W, F, V, O> TransitionSequence<W, F, V, O> {
    pub fn new(transitions: impl IntoIterator<Item = Transition<F, V, O>>) -> Self {
        Self { transitions: transitions.into_iter().collect(), coverage: Vec::new() }
    }

    /// Require the predicate to hold after at least one point in the trace,
    /// including the initial fixture state.
    pub fn sometimes(mut self, description: impl Into<String>, predicate: impl Fn(&W) -> bool + Send + Sync + 'static) -> Self {
        self.coverage.push(CoverageAssertion { description: description.into(), predicate: Box::new(predicate) });
        self
    }
}

/// The world-builder, direct-step, and fixpoint-predicate fixture triple used
/// by every enrolled reconciler.
pub struct LivenessEnrollment<B, S, P> {
    pub world_builder: B,
    pub reconciler_step: S,
    pub fixpoint: P,
    pub clock: Arc<VirtualClock>,
    pub staleness_edges: Vec<Duration>,
    pub pass_bound: usize,
}

impl<B, S, P> LivenessEnrollment<B, S, P> {
    pub fn new(world_builder: B, reconciler_step: S, fixpoint: P, clock: Arc<VirtualClock>) -> Self {
        Self { world_builder, reconciler_step, fixpoint, clock, staleness_edges: Vec::new(), pass_bound: DEFAULT_PASS_BOUND }
    }

    pub fn with_staleness_edges(mut self, edges: impl IntoIterator<Item = Duration>) -> Self {
        self.staleness_edges = edges.into_iter().collect();
        self
    }

    pub fn with_pass_bound(mut self, pass_bound: usize) -> Self {
        self.pass_bound = pass_bound;
        self
    }
}

#[derive(Debug, Clone, Copy)]
struct PassObservation {
    writes: usize,
    actuations: usize,
}

async fn run_pass<B, S, P>(
    enrollment: &LivenessEnrollment<B, S, P>,
    world: &mut B::World,
    drop_actuations: bool,
) -> Result<PassObservation, String>
where
    B: WorldBuilder,
    S: ReconcileStep<B::World>,
{
    let outcome = enrollment.reconciler_step.reconcile_step(world).await?;
    let writes = usize::from(outcome.patch.is_some());
    if let Some(patch) = outcome.patch {
        enrollment.reconciler_step.apply_patch(world, patch).await?;
    }
    let actuations = outcome.actuations.len();
    if !drop_actuations {
        for actuation in outcome.actuations {
            enrollment.reconciler_step.apply_actuation(world, actuation).await?;
        }
    }
    Ok(PassObservation { writes, actuations })
}

async fn converge<B, S, P>(enrollment: &LivenessEnrollment<B, S, P>, world: &mut B::World) -> Result<usize, String>
where
    B: WorldBuilder,
    S: ReconcileStep<B::World>,
    P: FixpointPredicate<B::World>,
{
    for pass in 0..=enrollment.pass_bound {
        if enrollment.fixpoint.at_fixpoint(world) {
            return Ok(pass);
        }
        if pass == enrollment.pass_bound {
            break;
        }
        run_pass(enrollment, world, false).await?;
    }
    Err(format!("did not reach a fixpoint within {} passes", enrollment.pass_bound))
}

/// Drive a hand-written disturbance trace and return its final world.
///
/// This sits alongside, rather than replaces, the bounded convergence and
/// quiescence checks. A trace fails if it drops no pending actuation or if any
/// `sometimes` assertion is never observed.
pub async fn run_transition_sequence<B, S, P>(
    enrollment: &LivenessEnrollment<B, S, P>,
    scenario: LivenessScenario,
    sequence: &TransitionSequence<B::World, S::Field, S::Value, S::OriginRoot>,
) -> Result<B::World, String>
where
    B: WorldBuilder,
    S: TransitionDriver<B::World>,
{
    let mut world = enrollment.world_builder.build(scenario).await?;
    let mut pending_actuations = Vec::new();
    let mut coverage = sequence.coverage.iter().map(|assertion| (assertion.predicate)(&world)).collect::<Vec<_>>();

    for (index, transition) in sequence.transitions.iter().enumerate() {
        let result = match transition {
            Transition::Reconcile => {
                let outcome = enrollment.reconciler_step.reconcile_step(&mut world).await?;
                if let Some(patch) = outcome.patch {
                    enrollment.reconciler_step.apply_patch(&mut world, patch).await?;
                }
                pending_actuations.extend(outcome.actuations);
                Ok(())
            }
            Transition::ExternalSpecWrite(field, value) => enrollment.reconciler_step.external_spec_write(&mut world, field, value).await,
            Transition::Delete => enrollment.reconciler_step.delete(&mut world).await,
            Transition::RestartController => enrollment.reconciler_step.restart_controller(&mut world).await,
            Transition::AdvanceClock(delta) => {
                enrollment.clock.advance(*delta);
                Ok(())
            }
            Transition::DropActuation => {
                if pending_actuations.is_empty() {
                    Err("no pending actuation to drop".to_string())
                } else {
                    pending_actuations.clear();
                    Ok(())
                }
            }
            Transition::DeliverActuation => {
                for actuation in pending_actuations.drain(..) {
                    enrollment.reconciler_step.apply_actuation(&mut world, actuation).await?;
                }
                Ok(())
            }
            Transition::PartitionStore(origin_root) => enrollment.reconciler_step.partition_store(&mut world, origin_root).await,
        };
        result.map_err(|error| format!("transition {index} ({transition:?}) failed: {error}"))?;
        for (observed, assertion) in coverage.iter_mut().zip(&sequence.coverage) {
            *observed |= (assertion.predicate)(&world);
        }
    }

    if let Some((_, assertion)) = coverage.iter().zip(&sequence.coverage).find(|(observed, _)| !**observed) {
        return Err(format!("coverage assertion `{}` was never observed", assertion.description));
    }
    Ok(world)
}

pub async fn assert_transition_sequence<B, S, P>(
    enrollment: &LivenessEnrollment<B, S, P>,
    scenario: LivenessScenario,
    sequence: &TransitionSequence<B::World, S::Field, S::Value, S::OriginRoot>,
) where
    B: WorldBuilder,
    S: TransitionDriver<B::World>,
{
    run_transition_sequence(enrollment, scenario, sequence).await.expect("transition sequence");
}

pub async fn assert_bounded_convergence<B, S, P>(enrollment: &LivenessEnrollment<B, S, P>)
where
    B: WorldBuilder,
    S: ReconcileStep<B::World>,
    P: FixpointPredicate<B::World>,
{
    let mut world = enrollment.world_builder.build(LivenessScenario::Normal).await.expect("build normal liveness world");
    converge(enrollment, &mut world).await.expect("bounded convergence");
}

pub async fn assert_quiescence_at_fixpoint<B, S, P>(enrollment: &LivenessEnrollment<B, S, P>)
where
    B: WorldBuilder,
    S: ReconcileStep<B::World>,
    P: FixpointPredicate<B::World>,
{
    let mut world = enrollment.world_builder.build(LivenessScenario::Normal).await.expect("build normal liveness world");
    converge(enrollment, &mut world).await.expect("converge before quiescence check");
    enrollment.fixpoint.reset_write_count(&world);
    let observation = run_pass(enrollment, &mut world, false).await.expect("run quiescence pass");
    assert_eq!(observation.writes, 0, "a fixpoint pass wrote resource state");
    assert_eq!(observation.actuations, 0, "a fixpoint pass emitted actuations");
    assert_eq!(enrollment.fixpoint.write_count(&world), 0, "the decorated backend observed a fixpoint write");
}

pub async fn assert_staleness_edges<B, S, P>(enrollment: &LivenessEnrollment<B, S, P>)
where
    B: WorldBuilder,
    S: ReconcileStep<B::World>,
    P: FixpointPredicate<B::World>,
{
    assert!(!enrollment.staleness_edges.is_empty(), "staleness contract requires at least one TTL");
    let mut world = enrollment.world_builder.build(LivenessScenario::Normal).await.expect("build normal liveness world");
    converge(enrollment, &mut world).await.expect("converge before staleness check");
    for edge in &enrollment.staleness_edges {
        let before = enrollment.fixpoint.probe_count(&world);
        enrollment.clock.advance(*edge + Duration::milliseconds(1));
        run_pass(enrollment, &mut world, false).await.expect("run stale decision pass");
        let after = enrollment.fixpoint.probe_count(&world);
        assert!(after > before, "crossing TTL {edge:?} did not re-probe stale state");
    }
}

pub async fn assert_actuation_drop_recovery<B, S, P>(enrollment: &LivenessEnrollment<B, S, P>)
where
    B: WorldBuilder,
    S: ReconcileStep<B::World>,
    P: FixpointPredicate<B::World>,
{
    let mut world = enrollment.world_builder.build(LivenessScenario::ActuationDrop).await.expect("build actuation-drop world");
    let dropped = run_pass(enrollment, &mut world, true).await.expect("run dropped-actuation pass");
    assert!(dropped.actuations > 0, "actuation-drop scenario emitted no actuation to drop");
    let retried = run_pass(enrollment, &mut world, false).await.expect("run actuation recovery pass");
    assert!(retried.actuations > 0, "a dropped actuation was not re-emitted");
}

pub async fn assert_degradation_not_wedging<B, S, P>(enrollment: &LivenessEnrollment<B, S, P>)
where
    B: WorldBuilder,
    S: ReconcileStep<B::World>,
    P: FixpointPredicate<B::World>,
{
    let mut world = enrollment.world_builder.build(LivenessScenario::Contradictory).await.expect("build contradictory world");
    for _ in 0..enrollment.pass_bound {
        run_pass(enrollment, &mut world, false).await.expect("run contradictory-world pass");
        if enrollment.fixpoint.held(&world) {
            return;
        }
        assert!(!enrollment.fixpoint.at_fixpoint(&world), "contradictory world silently reached a successful fixpoint");
    }
    panic!("contradictory world did not become held within {} passes", enrollment.pass_bound);
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;
    use crate::Clock;

    #[derive(Default)]
    struct TestWorld {
        interesting: bool,
        events: Vec<&'static str>,
    }

    struct TestWorldBuilder;

    #[async_trait]
    impl WorldBuilder for TestWorldBuilder {
        type World = TestWorld;

        async fn build(&self, _scenario: LivenessScenario) -> Result<Self::World, String> {
            Ok(TestWorld::default())
        }
    }

    struct TestStep;

    #[async_trait]
    impl ReconcileStep<TestWorld> for TestStep {
        type Patch = &'static str;
        type Actuation = &'static str;

        async fn reconcile_step(&self, _world: &mut TestWorld) -> Result<LivenessStep<Self::Patch, Self::Actuation>, String> {
            Ok(LivenessStep::new(Some("patch"), vec!["actuation"]))
        }

        async fn apply_patch(&self, world: &mut TestWorld, patch: Self::Patch) -> Result<(), String> {
            world.events.push(patch);
            Ok(())
        }

        async fn apply_actuation(&self, world: &mut TestWorld, actuation: Self::Actuation) -> Result<(), String> {
            world.events.push(actuation);
            Ok(())
        }
    }

    #[async_trait]
    impl TransitionDriver<TestWorld> for TestStep {
        type Field = &'static str;
        type Value = i32;
        type OriginRoot = &'static str;

        async fn external_spec_write(&self, world: &mut TestWorld, field: &Self::Field, _value: &Self::Value) -> Result<(), String> {
            world.events.push(field);
            Ok(())
        }

        async fn delete(&self, world: &mut TestWorld) -> Result<(), String> {
            world.events.push("delete");
            Ok(())
        }

        async fn restart_controller(&self, world: &mut TestWorld) -> Result<(), String> {
            world.events.push("restart");
            Ok(())
        }

        async fn partition_store(&self, world: &mut TestWorld, origin_root: &Self::OriginRoot) -> Result<(), String> {
            world.events.push(origin_root);
            Ok(())
        }
    }

    struct TestFixpoint;

    impl FixpointPredicate<TestWorld> for TestFixpoint {
        fn at_fixpoint(&self, _world: &TestWorld) -> bool {
            false
        }
    }

    #[tokio::test]
    #[should_panic(expected = "coverage assertion `interesting state` was never observed")]
    async fn transition_sequence_rejects_vacuous_coverage() {
        let clock = Arc::new(VirtualClock::new(Utc.with_ymd_and_hms(2026, 7, 29, 12, 0, 0).single().expect("valid test timestamp")));
        let enrollment = LivenessEnrollment::new(TestWorldBuilder, TestStep, TestFixpoint, clock);
        let sequence = TransitionSequence::new([Transition::AdvanceClock(Duration::seconds(1))])
            .sometimes("interesting state", |world: &TestWorld| world.interesting);

        assert_transition_sequence(&enrollment, LivenessScenario::Normal, &sequence).await;
    }

    #[tokio::test]
    async fn transition_sequence_drives_the_complete_vocabulary_and_controls_actuation_delivery() {
        let clock = Arc::new(VirtualClock::new(Utc.with_ymd_and_hms(2026, 7, 29, 12, 0, 0).single().expect("valid test timestamp")));
        let enrollment = LivenessEnrollment::new(TestWorldBuilder, TestStep, TestFixpoint, Arc::clone(&clock));
        let sequence = TransitionSequence::new([
            Transition::Reconcile,
            Transition::DropActuation,
            Transition::ExternalSpecWrite("priority", 10),
            Transition::Delete,
            Transition::RestartController,
            Transition::PartitionStore("root-b"),
            Transition::AdvanceClock(Duration::seconds(5)),
            Transition::Reconcile,
            Transition::DeliverActuation,
        ])
        .sometimes("external write", |world: &TestWorld| world.events.contains(&"priority"))
        .sometimes("delivered actuation", |world: &TestWorld| world.events.contains(&"actuation"));

        let world = run_transition_sequence(&enrollment, LivenessScenario::Normal, &sequence).await.expect("complete transition sequence");

        assert_eq!(
            world.events,
            vec!["patch", "priority", "delete", "restart", "root-b", "patch", "actuation"],
            "the first actuation must be dropped and the second delivered"
        );
        assert_eq!(clock.now(), Utc.with_ymd_and_hms(2026, 7, 29, 12, 0, 5).single().expect("valid advanced timestamp"));
    }
}
