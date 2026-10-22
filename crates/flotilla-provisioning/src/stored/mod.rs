//! The provisioning kinds as engine lifecycle kinds (ADR 0064 "Shared lifecycle",
//! #3629 Track A layer 1): Environment, ImageBuild, Clone and Checkout.
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
//! registered beside the engine's ([`register`], layer 2). Nothing writes
//! them yet: the reconcilers write through the engine in layer 3.
//!
//! [`Lifecycle`]: flotilla_state_machines::machines::lifecycle::Lifecycle

pub mod checkout;
pub mod clone;
pub mod environment;
pub mod image_build;

use std::collections::BTreeMap;

use flotilla_schema::{Kind, KindRegistry, RegistryError};
use flotilla_state_machines::{Diagram, Machine, Resource};

/// Registers the four provisioning kinds into `registry`, each under the code
/// it holds for good: Environment `ev`, ImageBuild `ib`, Clone `cl`, Checkout `ck`.
pub fn register(registry: &mut KindRegistry) -> Result<(), RegistryError> {
    registry.register::<Resource<environment::Environment>>()?;
    registry.register::<Resource<image_build::ImageBuild>>()?;
    registry.register::<Resource<clone::Clone>>()?;
    registry.register::<Resource<checkout::Checkout>>()?;
    Ok(())
}

/// The Mermaid diagram of each kind's machine, by kind name, for the
/// decided store's table views.
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
    ])
}

/// The provisioning kinds alone, for their DDL snapshot. No store opens with
/// them yet; the write path (layer 3) installs them behind `[primitives]`.
pub fn registry() -> Result<KindRegistry, RegistryError> {
    let mut registry = KindRegistry::new();
    register(&mut registry)?;
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
