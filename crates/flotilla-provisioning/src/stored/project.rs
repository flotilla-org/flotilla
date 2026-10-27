//! Project on the engine (#3824 layer 3b): an eighth definition kind, plain (a spec and no
//! machine), and the project operations' record of each project, plain too.
//!
//! The Project definition keeps the authored spec, 14 nested fields that no read selects by, as
//! one frozen document, like WorkflowTemplate. Its columns are the ones a read does select by: the
//! parent the hierarchy walks, whether it declares a charter, and its repositories by key with
//! the host of each one's charter store. Its [`DefinitionOrigin`] replaces the provenance labels
//! and annotations, as on the other definition kinds.
//!
//! The old Project status has two writers. Project operations' two conditions, the declaration
//! refusal and the operational entries' readiness, become [`ProjectOpsRecord`], one per project
//! at the home that runs its operations: latest values, per the observation ruling. The dispatch
//! queue is recomputed by the dispatch reconciler and is not stored (layer 3b-ii).
//!
//! Nothing reads or writes these tables yet: the import rehearsal restores into them, and they
//! join the daemon's decided store with Project's writers and readers (3b-ii).

use std::collections::BTreeMap;

use flotilla_resources::{ProjectSpec, ProjectStatus};
use flotilla_schema::{Frozen, KindRegistry, RegistryError, Stored, Timestamp};

pub use flotilla_resources::definitions::DefinitionOrigin;

/// One repository of a project, by its key: the host of its charter store, if it has one.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
pub struct ProjectMember {
    pub charter_host: Option<String>,
}

/// A Project, as the engine stores it: kind code `pj`.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(kind = "Project", code = "pj")]
pub struct ProjectDefinition {
    pub origin: DefinitionOrigin,
    /// The declared parent; none inherits the designated fleet.
    pub parent: Option<String>,
    /// Whether the project declares a charter source; charter registration selects by it.
    pub chartered: bool,
    /// The project's repositories by key; attach and the charter index select by them.
    #[store(child)]
    pub repositories: BTreeMap<String, ProjectMember>,
    /// The authored project, read whole.
    #[store(frozen)]
    pub definition: Frozen<ProjectSpec>,
}

impl ProjectDefinition {
    /// The record of `spec`. A spec naming one repository twice is refused: its members are
    /// keyed by repository.
    pub fn new(origin: DefinitionOrigin, spec: &ProjectSpec) -> Result<Self, String> {
        let mut repositories = BTreeMap::new();
        for member in &spec.repositories {
            let held = ProjectMember { charter_host: member.charter_store.as_ref().map(|store| store.host.clone()) };
            if repositories.insert(member.repo.0.clone(), held).is_some() {
                return Err(format!("names repository `{}` twice", member.repo.0));
            }
        }
        Ok(Self {
            origin,
            parent: spec.parent.clone(),
            chartered: spec.charter.is_some(),
            repositories,
            definition: Frozen::new(spec.clone()).map_err(|error| format!("freeze the project: {error}"))?,
        })
    }

    pub fn spec(&self) -> &ProjectSpec {
        self.definition.value()
    }
}

/// The declaration project operations last refused: the entry, the reason, and since when.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
pub struct DeclarationRefusal {
    pub entry_path: String,
    pub message: String,
    pub since: Timestamp,
    pub observed_at: Timestamp,
}

/// Project operations' record of one project, at the home that runs them: kind code `po`, named
/// by the project.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(kind = "ProjectOps", code = "po")]
pub struct ProjectOpsRecord {
    /// The declaration last refused, until a later pass accepts it.
    #[store(flatten)]
    pub refused: Option<DeclarationRefusal>,
    /// Whether the project's operational entries are ready, and why not.
    #[store(flatten)]
    pub entries: Option<EntriesReadiness>,
}

/// The operational entries' readiness project operations last observed.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
pub struct EntriesReadiness {
    pub ready: bool,
    pub message: String,
}

impl ProjectOpsRecord {
    /// The record of the project operations' conditions in `status`, if it holds either.
    pub fn from_status(status: &ProjectStatus) -> Option<Self> {
        if status.declaration_refused.is_none() && status.operational_entries.is_none() {
            return None;
        }
        Some(Self {
            refused: status.declaration_refused.as_ref().map(|refused| DeclarationRefusal {
                entry_path: refused.entry_path.clone(),
                message: refused.message.clone(),
                since: Timestamp::from_datetime(&refused.since),
                observed_at: Timestamp::from_datetime(&refused.observed_at),
            }),
            entries: status
                .operational_entries
                .as_ref()
                .map(|entries| EntriesReadiness { ready: entries.ready, message: entries.message.clone() }),
        })
    }
}

/// Registers Project `pj` and ProjectOps `po` into `registry`. Both are plain kinds.
pub fn register(registry: &mut KindRegistry) -> Result<(), RegistryError> {
    registry.register::<ProjectDefinition>()?;
    registry.register::<ProjectOpsRecord>()?;
    Ok(())
}

/// Project's kinds alone, for their DDL snapshot and the import rehearsal.
pub fn registry() -> Result<KindRegistry, RegistryError> {
    let mut registry = KindRegistry::new();
    register(&mut registry)?;
    Ok(registry)
}
