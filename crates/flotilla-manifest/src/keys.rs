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
/// Producer provenance exposed as a fact for presentation surfaces.
pub const SOURCE_FLOTILLA: &str = "flotilla";

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
/// Canonical `<namespace>/<convoy>@<origin>` resource identity.
pub const KEY_CONVOY: &str = "flotilla.convoy";
pub const KEY_NAMESPACE: &str = "flotilla.namespace";
pub const KEY_HOST: &str = "flotilla.host";
pub const KEY_CREW_ROLE: &str = "flotilla.crew.role";
pub const KEY_ATTACH_REF: &str = "flotilla.attach.ref";

// Catalog keys (`flotilla pm connect`, Entity targets, TTL'd).

pub const KEY_PROJECT_NAME: &str = "flotilla.project.name";
pub const KEY_CONVOY_PHASE: &str = "flotilla.convoy.phase";
pub const KEY_CONVOY_WORKFLOW: &str = "flotilla.convoy.workflow";
pub const KEY_CONVOY_MESSAGE: &str = "flotilla.convoy.message";
/// `WorkPhase` — the state of the work aboard a vessel, never a vessel
/// lifecycle (vessels don't complete).
pub const KEY_WORK_PHASE: &str = "flotilla.work.phase";
pub const KEY_VESSEL_HOST: &str = "flotilla.vessel.host";
/// Host currently carrying an independent terminal session.
pub const KEY_INDEPENDENT_HOST: &str = "flotilla.independent.host";
pub const KEY_VESSEL_ENV: &str = "flotilla.vessel.env";
pub const KEY_CREW_ROLES: &str = "flotilla.crew.roles";

// Cross-producer vocabulary (proposed for the Leg-1 freeze, design §6/§9).

/// Canonical presentation entity kind. Its value belongs to the
/// publisher-owned open entity-kind vocabulary.
pub const KEY_ENTITY_KIND: &str = "entity.kind";
/// Canonical id within `entity.kind`'s one permitted id dialect.
pub const KEY_ENTITY_ID: &str = "entity.id";
/// Concise human label used by grouping levels and fallback rendering.
pub const KEY_DISPLAY_LABEL: &str = "display.label";
/// Optional medium-width, display-only companion to [`KEY_DISPLAY_LABEL`].
pub const KEY_DISPLAY_LABEL_MEDIUM: &str = "display.label.medium";
/// Optional shortest, acronym-like display companion to [`KEY_DISPLAY_LABEL`].
pub const KEY_DISPLAY_LABEL_SHORT: &str = "display.label.short";
/// Producer provenance suitable for a surface badge.
pub const KEY_SOURCE: &str = "source";
/// Badge state: `idle | waiting | active | done | failed`.
pub const KEY_STATUS_STATE: &str = "status.state";
/// Boolean: needs a human/crew look.
pub const KEY_STATUS_ATTENTION: &str = "status.attention";
/// Proposed annotation-tier connectivity fact: `connected | disconnected`.
/// This is deliberately outside the frozen `status.state` badge vocabulary;
/// producers do not emit it until the disconnected annotation slice lands.
pub const KEY_STATUS_CONNECTIVITY: &str = "status.connectivity";
/// Short human summary line, e.g. "2/3 vessels done".
pub const KEY_SUMMARY_TEXT: &str = "summary.text";
/// External change-request number without display decoration such as `PR #`.
pub const KEY_CHANGE_REQUEST_NUMBER: &str = "change_request.number";
/// Checkout branch without its path or other display decoration.
pub const KEY_CHECKOUT_BRANCH: &str = "checkout.branch";
/// Full checkout path without its branch or other display decoration.
pub const KEY_CHECKOUT_PATH: &str = "checkout.path";
pub const KEY_COUNT_TOTAL: &str = "count.total";
pub const KEY_COUNT_ISSUES: &str = "count.issues";
pub const KEY_COUNT_CONVOYS: &str = "count.convoys";
pub const KEY_COUNT_VESSELS: &str = "count.vessels";
pub const KEY_COUNT_CHECKOUTS: &str = "count.checkouts";
pub const KEY_COUNT_INDEPENDENTS: &str = "count.independents";
/// Stable target of the primary action. Equal targets focus the same live
/// attachment even when different entities expose the affordance.
pub const KEY_PRIMARY_ACTION_TARGET: &str = "action.primary.target";
pub const KEY_PRIMARY_ACTION_KEY: &str = "action.primary.key";
pub const KEY_PRIMARY_ACTION_LABEL: &str = "action.primary.label";
pub const KEY_PRIMARY_ACTION_VEHICLE: &str = "action.primary.vehicle";
pub const KEY_PRIMARY_ACTION_RECIPE: &str = "action.primary.recipe";
/// Labels for facts used as grouping levels. Identity facts stay canonical;
/// these are display-only companions selected through `label-key`.
pub const KEY_REPO_NAME: &str = "vcs.repo.name";
/// Human-readable convoy name without an associated change-request suffix.
pub const KEY_CONVOY_NAME: &str = "flotilla.convoy.name";
pub const KEY_VESSEL_NAME: &str = "flotilla.vessel.name";

// Hierarchy fact keys used by presentation-side grouping templates.

/// Project resource-name segment. Only Project knowledge may mint it.
pub const SEGMENT_PROJECT: &str = "flotilla.project";
/// Canonical forge slug, or the Repository's `host:path` fallback when it
/// has no forge slug. Shared with repository observers such as git-watcher.
pub const SEGMENT_REPO: &str = "vcs.repo";
pub const SEGMENT_CONVOY: &str = "flotilla.convoy";
pub const SEGMENT_VESSEL: &str = "flotilla.vessel";
/// Independent terminal-session name, never a convoy vessel name.
pub const SEGMENT_INDEPENDENT: &str = "flotilla.independent";
/// Checkout awareness-entry identity, never a vessel or session name.
pub const SEGMENT_CHECKOUT: &str = "flotilla.checkout";
pub const SEGMENT_ISSUE: &str = "flotilla.issue";
