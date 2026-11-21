//! The vessel planner's port (#3108): what a vessel launch holds to be realised, as its planner
//! finds it at its home.
//!
//! The Vessel controller at the launch's home asks the planner before each try. The planner
//! requests, idempotently at that home, the environment, clones and checkouts the launch holds,
//! and reports each one's realisation; the launch is realised in its environment once every
//! holding is realised. Once it is realised, the planner seats its crews: each crew's terminal
//! session is a holding of the realised launch, whose commits have it planned again. The planner
//! writes no vessel edge.

/// The kind of a record a vessel launch holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum HeldKind {
    Environment,
    Clone,
    Checkout,
    /// A crew's terminal session, held once the launch is realised.
    Terminal,
}

/// One record a vessel launch holds: a launch of its kind at its home, and whether it is realised.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Held {
    pub kind: HeldKind,
    /// The held launch, `<name>#<n>`.
    pub launch: String,
    pub home: String,
    pub ready: bool,
}

/// What a vessel launch is, as its planner finds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VesselPlan {
    /// It waits for what `reason` names, holding `held` so far: an unrealised holding, a holding
    /// not yet requestable, or an input that has not arrived. A launch already realised names the
    /// environment it runs in.
    Waiting { reason: String, held: Vec<Held>, realised_in: Option<String> },
    /// Every holding is realised: it runs in `environment`.
    Ready { environment: String, held: Vec<Held> },
    /// The environment it ran in is gone, as `reason` says.
    Lost { reason: String },
    /// It cannot be realised, or broke once realised. A retryable failure may pass once what it
    /// lacks arrives; any other is final.
    Failed { cause: String, retryable: bool },
}

/// The vessel planner at a home.
#[async_trait::async_trait]
pub trait VesselPlanner: Send + Sync {
    /// Plans launch `launch` at this home, requesting what it holds.
    async fn plan(&self, launch: &str) -> Result<VesselPlan, String>;
}
