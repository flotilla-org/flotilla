//! The Vessel kind's provider work (#3108): a vessel launch holds its environment, clones and
//! checkouts, which its planner requests at this home, and is realised in its environment once
//! each is realised.
//!
//! The planner ([`VesselPlanner`]) reads the launch's convoy and fulfilment kind and requests what
//! the launch holds; this actuator turns what it found into the shared controller's holdings, tries
//! and observations. A plan that cannot be made fails non-retryably; one that waits for an input
//! that has not arrived, such as a replica of its convoy, is a retryable failure. Waiting for a
//! holding to be realised spends no try.
//!
//! Once realised, its plan seats its crews, and their terminal sessions are its holdings: a
//! session's commit observes it again, which seats the next crew or breaks a launch whose crew
//! cannot be seated.
//!
//! The launch's holder is its convoy, read by the vessel requester, which releases the launch;
//! what it holds ends with it through their own holders. Nothing is torn down here.

use std::sync::Arc;

use async_trait::async_trait;
use flotilla_resources::vessel_planning::{Held, HeldKind, VesselPlan, VesselPlanner};
use flotilla_schema::{Identity, Kind, KindCode};
use flotilla_schema_sqlite::{Read, StoreError};
use flotilla_state_machines::{
    machines::{
        lifecycle::Lifecycle,
        vessel::{VesselKind, VesselSpec},
    },
    Cause, CauseCategory, Resource,
};

use super::controller::{Actuator, Backing, Holding, Holdings, ProviderFailure, Usability};
use crate::stored::{checkout::CheckoutKind, clone::CloneKind, environment::EnvironmentKind, terminal_session::TerminalSessionKind};

const ENVIRONMENT: KindCode = <Resource<Lifecycle<EnvironmentKind>> as Kind>::CODE;
const CLONE: KindCode = <Resource<Lifecycle<CloneKind>> as Kind>::CODE;
const CHECKOUT: KindCode = <Resource<Lifecycle<CheckoutKind>> as Kind>::CODE;
const TERMINAL_SESSION: KindCode = <Resource<Lifecycle<TerminalSessionKind>> as Kind>::CODE;

/// The Vessel kind's actuator over its planner.
pub struct VesselActuator {
    planner: Arc<dyn VesselPlanner>,
}

impl VesselActuator {
    pub fn new(planner: Arc<dyn VesselPlanner>) -> Self {
        Self { planner }
    }

    async fn plan(&self, name: &str) -> Result<VesselPlan, ProviderFailure> {
        // A planner that could not read what it plans from tries again at the next change.
        self.planner.plan(name).await.map_err(|error| ProviderFailure::new(Cause::new(CauseCategory::Transient, error), true))
    }
}

fn usability(held: &Held) -> Usability {
    let kind = match held.kind {
        HeldKind::Environment => ENVIRONMENT,
        HeldKind::Clone => CLONE,
        HeldKind::Checkout => CHECKOUT,
        HeldKind::Terminal => TERMINAL_SESSION,
    };
    Usability { fact: Identity { kind, name: held.launch.clone(), home: held.home.clone() }, usable: held.ready }
}

/// A plan that failed, as one try's failure, as the planner's step recorded it before.
fn failure(detail: String, retryable: bool) -> ProviderFailure {
    ProviderFailure::new(Cause::new(CauseCategory::External, detail), retryable)
}

/// What a plan shows of the launch's backing: present in its environment once realised, absent
/// while it still waits.
fn backing(plan: &VesselPlan) -> Backing {
    match plan {
        VesselPlan::Ready { environment, .. } | VesselPlan::Waiting { realised_in: Some(environment), .. } => {
            Backing::Present { identity: environment.clone() }
        }
        VesselPlan::Waiting { realised_in: None, .. } => Backing::Absent { reason: None },
        VesselPlan::Lost { reason } => Backing::Absent { reason: Some(reason.clone()) },
        VesselPlan::Failed { cause, .. } => Backing::Failed { reason: cause.clone() },
    }
}

/// What a plan holds: complete only once it reaches its environment, as each step of a plan needs
/// the one before it realised.
fn held(plan: &VesselPlan) -> Result<Holdings, ProviderFailure> {
    match plan {
        VesselPlan::Ready { held, .. } => Ok(Holdings { held: held.iter().map(usability).collect(), complete: true }),
        VesselPlan::Waiting { held, .. } => Ok(Holdings { held: held.iter().map(usability).collect(), complete: false }),
        // Before it is realised, a lost environment is one try: the policy bounds how many.
        VesselPlan::Lost { reason } => Err(failure(reason.clone(), true)),
        VesselPlan::Failed { cause, retryable } => Err(failure(cause.clone(), *retryable)),
    }
}

#[async_trait]
impl Actuator<VesselKind> for VesselActuator {
    /// Present in its environment once realised, absent while it still waits: an unrealised
    /// launch's observation writes nothing, and a realised one waiting again keeps running.
    async fn observe(&self, name: &str, _: &VesselSpec) -> Result<Backing, Cause> {
        Ok(backing(&self.plan(name).await.map_err(|failure| failure.cause)?))
    }

    /// One plan serves both: what the launch holds and whether it is realised.
    async fn observe_holding(&self, name: &str, _: &VesselSpec) -> (Option<Holdings>, Result<Backing, Cause>)
    where
        VesselSpec: Sync,
    {
        match self.plan(name).await {
            Ok(plan) => (held(&plan).ok(), Ok(backing(&plan))),
            Err(failure) => (None, Err(failure.cause)),
        }
    }

    async fn realise(&self, name: &str, _: &VesselSpec) -> Result<String, ProviderFailure> {
        match self.plan(name).await? {
            VesselPlan::Ready { environment, .. } => Ok(environment),
            // A holding changed between its read and this try: the next holdings read waits for it.
            VesselPlan::Waiting { reason, .. } => Err(failure(format!("waiting for {reason}"), true)),
            VesselPlan::Lost { reason } => Err(failure(reason, true)),
            VesselPlan::Failed { cause, retryable } => Err(failure(cause, retryable)),
        }
    }

    async fn release(&self, _: &str, _: &VesselSpec, _: Option<&str>) -> Result<(), ProviderFailure> {
        Ok(())
    }

    fn followed(&self) -> Vec<KindCode> {
        vec![ENVIRONMENT, CLONE, CHECKOUT, TERMINAL_SESSION]
    }

    fn dependencies(&self, _: &VesselSpec) -> Vec<Identity> {
        Vec::new()
    }

    /// Released by its requester, never here.
    fn holder(&self, _: &mut Read<'_>, _: &str, _: &VesselSpec) -> Result<Holding, StoreError> {
        Ok(Holding::open())
    }

    fn usability(&self, _: &mut Read<'_>, _: &VesselSpec) -> Result<Vec<Usability>, StoreError> {
        Ok(Vec::new())
    }

    fn holds(&self) -> bool {
        true
    }

    /// Complete only once the plan reaches its environment: each step of a plan needs the one
    /// before it realised, as a contained environment's mounts need its checkouts.
    async fn holdings(&self, name: &str, _: &VesselSpec) -> Result<Holdings, ProviderFailure> {
        held(&self.plan(name).await?)
    }
}
