//! Native verbs (#4182, #4197): typed operations on a subject kind, implemented in code,
//! listed in the `verbs` table and invoked through `POST /verbs/{name}`.
//!
//! - [`catalog`]: the declarations joined with their presentation entries, checked at load.
//! - [`invoke`]: one invocation: request checks, per-subject checks and runs, and the
//!   `verb.invocation` span that records the intent.
//! - [`convoy`]: convoy subjects and `convoy.explain`.
//!
//! A verb's `when` speaks the condition language of a table read's `where`, parsed and kept per
//! row by `tables::filter`.
pub(crate) mod catalog;
pub(crate) mod convoy;
pub(crate) mod invoke;

use std::sync::Arc;

use flotilla_core::in_process::InProcessDaemon;
use flotilla_store::ResourceBackend;

use crate::server::tables::DerivedSet;
use catalog::{Catalog, Handler, Picks, Presentation};
pub(crate) use invoke::{Refusal, Verbs};

/// The built-in native verbs.
fn natives(daemon: Option<Arc<InProcessDaemon>>) -> Vec<Arc<dyn Handler>> {
    vec![Arc::new(convoy::ExplainConvoy { daemon })]
}

/// The built-in presentation entries, until #4198 bakes them from `verbs/*.yaml`.
fn presentations() -> Vec<Presentation> {
    vec![Presentation {
        verb: "convoy.explain",
        label: Some("Explain convoy"),
        help: Some("Why the convoy is where it is, and what it waits on"),
        picks: Picks::Each,
        ..Presentation::default()
    }]
}

/// The verb route over the daemon's resources, or over a bare backend in resource-handler tests.
pub(crate) fn builtin(backend: ResourceBackend, daemon: Option<Arc<InProcessDaemon>>) -> Verbs {
    let subjects = Arc::new(convoy::ResourceSubjects::new(backend));
    Verbs { catalog: Catalog::new(natives(daemon), presentations(), subjects.as_ref()), subjects, policy: invoke::allow_all }
}

impl Verbs {
    /// The `verbs` and `verbs_params` tables, derived from the catalog.
    pub(crate) fn tables(&self) -> DerivedSet {
        let (verbs, params) = self.catalog.rows();
        DerivedSet { set: Catalog::description(), rows: vec![verbs, params] }
    }
}

#[cfg(test)]
mod tests;
