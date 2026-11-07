//! The provisioning kinds as engine lifecycle kinds (ADR 0064 "Shared lifecycle",
//! #3629 Track A layer 1): Environment, ImageBuild, Clone and Checkout; and
//! TerminalSession (Track B layer 5), which names its Environment.
//!
//! Each kind's machine is the engine's shared [`Lifecycle`], so its stored
//! kind is `Resource<Lifecycle<K>>`: the request below as its spec (the
//! columns and child tables declared here), the lifecycle state the engine
//! supplies (`Requested { failures, last_failure } | Realised { identity } |
//! Gone | GaveUp { cause }`), and its edges, attempts among them, as entries
//! in the home's log. ADR 0064 has each lifecycle kind declare its spec
//! "beside its request, in the crate that owns that request"; this crate
//! realises all four, and it is the lowest owner the build graph allows
//! (`flotilla-resources` is reached by crew and admission, which must not
//! reach the engine).
//!
//! Only the request is stored. What the old statuses held besides the phase
//! is one of:
//! - the realised identity (a container id, an image digest, a path);
//! - the attempt tally the lifecycle keeps while Requested;
//! - an edge's arguments and gate reads, in its log entry (probes, build logs);
//! - an observation, which lives in the observed store and never in the log
//!   (runtime samples, configured limits, integration, degradations);
//! - a judgement (in progress, suspect, lost, next attempt).
//!
//! Behind `[primitives]` the daemon opens its decided store with these kinds
//! registered beside the engine's ([`register`], layer 2). The provisioning
//! reconcilers write theirs through the engine (layer 3); nothing writes
//! TerminalSession until the crew reconcilers move (layer 7).
//!
//! [`Lifecycle`]: flotilla_state_machines::machines::lifecycle::Lifecycle

pub mod account;
pub mod artifact;
pub mod checkout;
pub mod clone;
pub mod definition;
pub mod environment;
pub mod forge_observation;
pub mod host;
pub mod image_build;
pub mod launch_facts;
pub mod manifest_root;
pub mod placement;
pub mod project;
pub mod repository;
pub mod terminal_facts;
pub mod terminal_session;

use std::collections::BTreeMap;

use flotilla_schema::{Kind, KindRegistry, RegistryError, Stored};
use flotilla_state_machines::{
    machines::{ensure::Ensure, vessel::Vessel},
    Diagram, Machine, Resource,
};

/// Who a resource is held for: `convoy:<name>` or `vessel:<name>`. Interned text in its own domain (`hl`), indexed, with no
/// foreign key and no ownership semantics: nothing cascades from it and nothing
/// checks that the holder exists (owner's look-over, 2026-10-22, ruling 4). It
/// is one known field, not a label mechanism. Its text uses ADR 0066's segment
/// form, but it is not an Address: a vessel's address nests under its convoy.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Stored)]
#[store(intern = "hl")]
pub struct Holder(pub String);

impl Holder {
    /// A convoy's: its checkouts.
    pub fn convoy(name: &str) -> Holder {
        Holder(format!("convoy:{name}"))
    }

    /// A vessel's: its environment.
    pub fn vessel(name: &str) -> Holder {
        Holder(format!("vessel:{name}"))
    }
}

/// One second, in the engine's microseconds: the unit of an `AttemptPolicy`'s delays.
const SECOND: i64 = 1_000_000;

/// Registers the kinds declared here into `registry`, each under the code it
/// holds for good: Environment `ev`, ImageBuild `ib`, Clone `cl`, Checkout
/// `ck` and TerminalSession `ts`.
pub fn register(registry: &mut KindRegistry) -> Result<(), RegistryError> {
    register_provisioning(registry)?;
    registry.register::<Resource<terminal_session::TerminalSession>>()?;
    account::register(registry)?;
    manifest_root::register(registry)?;
    definition::register(registry)?;
    repository::register(registry)?;
    project::register(registry)?;
    artifact::register(registry)?;
    forge_observation::register(registry)?;
    placement::register(registry)?;
    host::register(registry)?;
    registry.register::<flotilla_resources::AccessDeliverySpec>()?;
    Ok(())
}

fn register_provisioning(registry: &mut KindRegistry) -> Result<(), RegistryError> {
    registry.register::<Resource<environment::Environment>>()?;
    registry.register::<launch_facts::EnvironmentLaunchFacts>()?;
    registry.register::<Resource<image_build::ImageBuild>>()?;
    registry.register::<Resource<clone::Clone>>()?;
    registry.register::<clone::CloneFacts>()?;
    registry.register::<Resource<checkout::Checkout>>()?;
    registry.register::<checkout::CheckoutFacts>()?;
    registry.register::<checkout::CheckoutIntegration>()?;
    Ok(())
}

/// The Mermaid diagram of each kind's machine registered by [`register`], by
/// kind name, for the decided store's table views. A plain kind (AccountProvision) has none.
pub fn diagrams() -> BTreeMap<String, String> {
    fn diagram<M: Machine>() -> (String, String)
    where
        Resource<M>: Kind,
    {
        (<Resource<M> as Kind>::KIND.to_owned(), M::definition().mermaid(Diagram::Full))
    }
    BTreeMap::from([
        diagram::<environment::Environment>(),
        diagram::<image_build::ImageBuild>(),
        diagram::<clone::Clone>(),
        diagram::<checkout::Checkout>(),
        diagram::<terminal_session::TerminalSession>(),
        diagram::<account::AccountBinding>(),
        diagram::<manifest_root::ManifestRoot>(),
    ])
}

/// The four provisioning kinds alone, for their DDL snapshot and benchmark.
/// The daemon's decided store registers them through [`register`].
pub fn registry() -> Result<KindRegistry, RegistryError> {
    let mut registry = KindRegistry::new();
    register_provisioning(&mut registry)?;
    Ok(registry)
}

/// The crew kinds (Track B): Vessel and Ensure, declared by the engine, and
/// TerminalSession, for their DDL snapshot.
pub fn crew_registry() -> Result<KindRegistry, RegistryError> {
    let mut registry = KindRegistry::new();
    registry.register::<Resource<Vessel>>()?;
    registry.register::<Resource<Ensure>>()?;
    registry.register::<Resource<terminal_session::TerminalSession>>()?;
    Ok(registry)
}

/// PROPOSAL, for the owner's schema look-over only (#3108): TerminalSession with the records its
/// launches would need on the engine ([`terminal_facts`]). No store opens this registry.
pub fn terminal_proposal_registry() -> Result<KindRegistry, RegistryError> {
    let mut registry = KindRegistry::new();
    registry.register::<Resource<terminal_session::TerminalSession>>()?;
    registry.register::<terminal_facts::TerminalLaunchFacts>()?;
    registry.register::<terminal_facts::EnvironmentHealth>()?;
    registry.register::<terminal_facts::TerminalDrafts>()?;
    Ok(registry)
}

/// The production registry (`flotilla_state_machines::kinds`) with the
/// provisioning kinds registered beside the engine's: a code the engine
/// already holds is refused, so the two never share one (#3373 ruling 6).
pub fn production_kinds() -> Result<KindRegistry, RegistryError> {
    let mut registry = flotilla_state_machines::kinds()?;
    register(&mut registry)?;
    Ok(registry)
}

/// Every kind the daemon's decided store holds (ADR 0064, #3108): Message and AgentSession
/// (#3317), the kinds registered by [`register`], and the engine's Vessel and Ensure (#3629),
/// each under its production code.
pub fn decided_registry() -> Result<KindRegistry, RegistryError> {
    let mut registry = flotilla_engine_store::registry()?;
    register(&mut registry)?;
    flotilla_engine_store::register_crew(&mut registry)?;
    Ok(registry)
}

/// The machine diagram of each kind in [`decided_registry`], by kind name.
pub fn decided_diagrams() -> Result<BTreeMap<String, String>, RegistryError> {
    let mut all = flotilla_engine_store::machine_registry()?.diagrams;
    all.extend(diagrams());
    all.extend(flotilla_engine_store::crew_diagrams());
    Ok(all)
}
