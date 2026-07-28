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
