//! Serving native verbs (#4182, #4197, #4267): typed operations on a subject kind, declared by
//! the crates that perform their effects, listed in the `verbs` table and invoked through
//! `POST /verbs/{name}`. The daemon declares no verbs; the composition root collects them into a
//! `flotilla_verbs::VerbRegistry` (`runtime::verb_registry`).
//!
//! - [`catalog`]: the registered verbs, checked at load.
//! - [`invoke`]: one invocation: request checks, per-subject checks and runs, and the
//!   `verb.invocation` span that records the intent.
//! - [`when`]: applicability conditions over a subject's row.
//! - [`subjects`]: subjects' rows, from the table describing each kind's rows.
pub(crate) mod catalog;
pub(crate) mod invoke;
pub(crate) mod subjects;
pub(crate) mod when;

use std::sync::Arc;

use flotilla_verbs::VerbRegistry;

use crate::server::tables::DerivedSet;
use catalog::{Catalog, Subjects};
pub(crate) use invoke::{Refusal, Verbs};

/// The verb route over `registry`'s verbs and `subjects`' rows.
pub(crate) fn serve(registry: VerbRegistry, subjects: Arc<dyn Subjects>) -> Verbs {
    Verbs { catalog: Catalog::new(registry, subjects.as_ref()), subjects, policy: invoke::allow_all }
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
