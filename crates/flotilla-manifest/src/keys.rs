//! Every key spelling, source id, and segment key flotilla writes into a PM
//! metadata plane. One place, so the Leg-1 contract rename is mechanical.
//!
//! Key map: design §5 on flotilla-org/flotilla#667. `flotilla.leg.*` is
//! reserved and deliberately absent — legs are unmaterialised (#680).

/// Pipe name patches are written to (v0: andamento's current spelling;
/// Leg 1 renames it `manifest-apply-patch`).
pub const APPLY_METADATA_PATCH_PIPE: &str = "andamento-apply-metadata-patch";
/// Pipe name that returns the identities the PM has observed on its panes
/// (v0 spelling; Leg 1: `manifest-observed-identities`).
pub const OBSERVED_IDENTITIES_PIPE: &str = "andamento-observed-identities";

/// Source id for catalog facts published by `flotilla pm connect`.
pub const SOURCE_CONNECTOR: &str = "flotilla-connector";
/// Source id for the pane stamp published by `flotilla attach`.
pub const SOURCE_ATTACH: &str = "flotilla-attach";
/// Source id for tab stamps published by the workspace actuator.
pub const SOURCE_ACTUATOR: &str = "flotilla-actuator";

/// TTL for catalog facts. Pane/tab stamps carry no TTL: they are facts about
/// the binding, not about the daemon being alive.
pub const CATALOG_TTL_MS: u64 = 30_000;
/// Re-assertion cadence for TTL'd catalog facts.
pub const REASSERT_INTERVAL_MS: u64 = 10_000;
/// Ordering hint stamped on archipelago-level groups (free-floating vessels
/// with no project segment) so they group and order first by default —
/// lower is earlier (ordering semantics: Leg-1 gap report §9.6).
pub const ARCHIPELAGO_ORDINAL: i64 = -100;

// Pane-stamp keys (`flotilla attach`, Pane target, no TTL).

/// `<host>/<namespace>/<session-name>` — the canonical pane → identity join
/// key; the catalog publishes its facts against this same identity.
pub const KEY_SESSION: &str = "flotilla.session";
/// Denormalized binding facts: survive daemon outages and give grouping
/// rules something direct to match on.
pub const KEY_VESSEL: &str = "flotilla.vessel";
pub const KEY_CONVOY: &str = "flotilla.convoy";
pub const KEY_NAMESPACE: &str = "flotilla.namespace";
pub const KEY_HOST: &str = "flotilla.host";
pub const KEY_CREW_ROLE: &str = "flotilla.crew.role";
pub const KEY_ATTACH_REF: &str = "flotilla.attach.ref";

// Catalog keys (`flotilla pm connect`, Group/Identity targets, TTL'd).

pub const KEY_PROJECT_NAME: &str = "flotilla.project.name";
pub const KEY_CONVOY_PHASE: &str = "flotilla.convoy.phase";
pub const KEY_CONVOY_WORKFLOW: &str = "flotilla.convoy.workflow";
pub const KEY_CONVOY_MESSAGE: &str = "flotilla.convoy.message";
/// `WorkPhase` — the state of the work aboard a vessel, never a vessel
/// lifecycle (vessels don't complete).
pub const KEY_WORK_PHASE: &str = "flotilla.work.phase";
pub const KEY_VESSEL_HOST: &str = "flotilla.vessel.host";
pub const KEY_VESSEL_ENV: &str = "flotilla.vessel.env";
pub const KEY_CREW_ROLES: &str = "flotilla.crew.roles";

// Cross-producer vocabulary (proposed for the Leg-1 freeze, design §6/§9).

/// Badge state: `idle | waiting | active | done | failed`.
pub const KEY_STATUS_STATE: &str = "status.state";
/// Boolean: needs a human/crew look.
pub const KEY_STATUS_ATTENTION: &str = "status.attention";
/// Short human summary line, e.g. "2/3 vessels done".
pub const KEY_SUMMARY_TEXT: &str = "summary.text";
/// GroupPath an identity/tab belongs to — the join's catalog half
/// (v0 spelling; Leg 1: `workspace.scope`).
pub const KEY_SCOPE: &str = "tab.scope";
/// Workspace kind stamped on flotilla-created tabs (v0 spelling; Leg 1:
/// `workspace.kind`).
pub const KEY_TAB_KIND: &str = "tab.kind";
/// `workspace | pane` — what materialising this entry produces.
pub const KEY_MATERIALIZE_TARGET: &str = "materialize.target";
/// The command a PM runs to materialise this entry (recipe schema pending
/// the Leg-1 freeze, gap report §9.1).
pub const KEY_MATERIALIZE_RECIPE: &str = "materialize.recipe";
/// Dedupe key for factory-produced nodes: `flotilla:<kind>/<ns>/<name>`.
pub const KEY_FACTORY_ID: &str = "factory.id";

// GroupPath segment keys (design §4).

/// Project segment key — deliberately the same spelling the git-watcher
/// uses, so flotilla groups and git-watcher groups collide into one project
/// cluster (v0; Leg 1 renames both producers to `vcs.repo` at once).
pub const SEGMENT_PROJECT: &str = "git.repo";
pub const SEGMENT_CONVOY: &str = "flotilla.convoy";
pub const SEGMENT_VESSEL: &str = "flotilla.vessel";
pub const SEGMENT_ISSUE: &str = "flotilla.issue";
