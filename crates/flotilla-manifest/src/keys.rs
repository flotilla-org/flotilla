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
/// Known-empty is represented by zero; absence means the Project definition is unavailable.
pub const KEY_PROJECT_REPOSITORY_COUNT: &str = "flotilla.project.repository_count";
pub const KEY_MEMBERSHIP_PROJECT: &str = "flotilla.membership.project";
pub const KEY_MEMBERSHIP_REPOSITORY_KEY: &str = "flotilla.membership.repository_key";
pub const KEY_MEMBERSHIP_REPOSITORY_SLUG: &str = "flotilla.membership.repository_slug";
pub const KEY_MEMBERSHIP_SUBPATH: &str = "flotilla.membership.subpath";
/// Terminal attempt with a newer generation of the same project and role.
pub const KEY_CONVOY_SUPERSEDED: &str = "flotilla.convoy.superseded";
pub const KEY_CONVOY_PHASE: &str = "flotilla.convoy.phase";
pub const KEY_CONVOY_WORKFLOW: &str = "flotilla.convoy.workflow";
pub const KEY_CONVOY_MESSAGE: &str = "flotilla.convoy.message";
/// Presentation state judged from live evidence and the authority's stall condition.
pub const KEY_SURFACE_STATE: &str = "flotilla.surface.state";
/// The active handled stall rung, when `flotilla.surface.state` is `stalled_handled`.
pub const KEY_SURFACE_RUNG: &str = "flotilla.surface.rung";
/// `WorkPhase` — the state of the work aboard a vessel, never a vessel
/// lifecycle (vessels don't complete).
pub const KEY_WORK_PHASE: &str = "flotilla.work.phase";
pub const KEY_VESSEL_HOST: &str = "flotilla.vessel.host";
/// Host currently carrying an independent terminal session.
pub const KEY_INDEPENDENT_HOST: &str = "flotilla.independent.host";
pub const KEY_VESSEL_ENV: &str = "flotilla.vessel.env";
pub const KEY_CREW_ROLES: &str = "flotilla.crew.roles";
/// Standing project role (a `ConvoyEnsure` declaration). Distinct from
/// `flotilla.crew.role(s)`, which name crew members aboard a vessel.
pub const KEY_ROLE: &str = "flotilla.role";
pub const KEY_ROLE_NAME: &str = "flotilla.role.name";
/// Optional presentation annotation from the declaration, e.g. `fleet`.
pub const KEY_ROLE_PRESENTS_AS: &str = "flotilla.role.presents_as";
/// Why automatic admission is suspended: `backing_unverified | restart_limit`.
pub const KEY_ROLE_HOLD: &str = "flotilla.role.hold";
/// Boolean on every entity carrying [`KEY_CONVOY`] (the convoy, its vessels
/// and entries grouped under it): that attempt was admitted by a standing-role
/// declaration, whether or not the declaration is still known. [`KEY_ROLE`]
/// propagates the same way while the declaration is known, like
/// [`KEY_CONVOY_SUPERSEDED`].
pub const KEY_CONVOY_STANDING: &str = "flotilla.convoy.standing";

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
/// Direct Cleat packet endpoint; all five facts appear or disappear together.
pub const KEY_PRIMARY_DIRECT_TRANSPORT: &str = "action.primary.direct.transport";
pub const KEY_PRIMARY_DIRECT_HOST: &str = "action.primary.direct.host";
pub const KEY_PRIMARY_DIRECT_RUNTIME_ROOT: &str = "action.primary.direct.runtime_root";
pub const KEY_PRIMARY_DIRECT_DAEMON: &str = "action.primary.direct.daemon";
pub const KEY_PRIMARY_DIRECT_SESSION: &str = "action.primary.direct.session";
/// Why the command recipe is the only available opening path.
pub const KEY_PRIMARY_DIRECT_REASON: &str = "action.primary.direct.reason";
/// Managed primary content (Andamento): `ready | held`. Published together
/// with [`KEY_WORKSPACE_PRIMARY_TARGET`], the recipe and the cwd.
pub const KEY_WORKSPACE_PRIMARY_STATE: &str = "workspace.primary.state";
/// Identity of the backing instance currently resolving the stable intent in
/// [`KEY_PRIMARY_ACTION_TARGET`]. Changes whenever that instance is replaced.
pub const KEY_WORKSPACE_PRIMARY_TARGET: &str = "workspace.primary.target";
/// Labels for facts used as grouping levels. Identity facts stay canonical;
/// these are display-only companions selected through `label-key`.
pub const KEY_REPO_NAME: &str = "vcs.repo.name";
/// Human-readable convoy name without an associated change-request suffix.
pub const KEY_CONVOY_NAME: &str = "flotilla.convoy.name";
pub const KEY_VESSEL_NAME: &str = "flotilla.vessel.name";

// Hierarchy fact keys used by presentation-side grouping templates.

/// Text project entity id, derived from Project knowledge or resource project references. On issue and change_request subjects, published only
/// when linking convoys and roles identify exactly one project; never a list.
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

// ADR 0051 replicated subjects and typed graph edges.
// Subject entities use forge-relative ids: <service>/<scope>!<number> for a
// change request and <service>/<scope>#<number> for an issue. All edges below
// are EntityRefs, including single-target forge/current-attempt edges.
// subject_of is the union; subject_of.<relationship> preserves edge typing.
// Subject labels are ReferenceContext short references at every width tier.
// They never decorate a convoy label. Number is text to preserve identity.
// Raw Unknown values are absent while their observation time remains visible.
// title, author, head_sha, state, checks, mergeable, review_decision and
// updated_at are Text. labels/assignees are StringList, requested-from-owner
// and actionable-at-head are Bool. CR state: open|draft|merged|closed;
// issue state: open|closed; checks: pass|fail|pending; mergeable:
// mergeable|conflicting; review_decision: approved|changes_requested|required|none.
// Readiness: ready_to_merge|awaiting_review_response|ci_failing|conflicting|
// draft|merged_not_landed|closed. All observed_at companions are Text.
// Subject entities are scoped to live convoys and strictly less than 24 hours
// after landed finished_at; failed/cancelled/abandoned links are excluded.
// Missing status and incomplete observations use awaiting_review_response,
// matching the existing readiness function and closed ADR vocabulary. This
// means waiting for evidence; absence of raw fields distinguishes unknown.
// Readiness is the ADR 0051 closed vocabulary. Orphans are deferred to the
// producer provenance/retention follow-up; unmatched observations are excluded.
// Attempts are ordered by generation, oldest first. The current attempt is
// the live generation; it is absent between admissions. desired_state is
// running (ensure declaration present), state is active or held, restart_count
// is integer, next_attempt is an optional RFC 3339 backoff deadline.
// Forge URL templates substitute web_url, scope and number; kind is github
// or forgejo. The forge web_url has no trailing slash.
pub const KEY_SUBJECT_PRODUCES: &str = "flotilla.subject.produces";
pub const KEY_SUBJECT_ADOPTS: &str = "flotilla.subject.adopts";
pub const KEY_SUBJECT_WORKS_ON: &str = "flotilla.subject.works_on";
pub const KEY_SUBJECT_SUPERSEDES: &str = "flotilla.subject.supersedes";
pub const KEY_SUBJECT_REFERENCES: &str = "flotilla.subject.references";
pub const KEY_SUBJECT_OF: &str = "flotilla.subject_of";
pub const KEY_SUBJECT_OF_PRODUCES: &str = "flotilla.subject_of.produces";
pub const KEY_SUBJECT_OF_ADOPTS: &str = "flotilla.subject_of.adopts";
pub const KEY_SUBJECT_OF_WORKS_ON: &str = "flotilla.subject_of.works_on";
pub const KEY_SUBJECT_OF_SUPERSEDES: &str = "flotilla.subject_of.supersedes";
pub const KEY_SUBJECT_OF_REFERENCES: &str = "flotilla.subject_of.references";
pub const KEY_SUBJECT_SERVICE: &str = "flotilla.subject.service";
pub const KEY_SUBJECT_SCOPE: &str = "flotilla.subject.scope";
pub const KEY_SUBJECT_NUMBER: &str = "flotilla.subject.number";
pub const KEY_SUBJECT_KIND: &str = "flotilla.subject.kind";
pub const KEY_SUBJECT_REPOSITORY_ALIAS: &str = "flotilla.subject.repository_alias";
pub const KEY_FORGE: &str = "flotilla.forge";
pub const KEY_FORGE_KIND: &str = "flotilla.forge.kind";
pub const KEY_FORGE_WEB_URL: &str = "flotilla.forge.web_url";
pub const KEY_FORGE_CHANGE_REQUEST_URL_TEMPLATE: &str = "flotilla.forge.change_request_url_template";
pub const KEY_FORGE_ISSUE_URL_TEMPLATE: &str = "flotilla.forge.issue_url_template";
pub const KEY_CHANGE_REQUEST_READINESS: &str = "flotilla.change_request.readiness";
pub const KEY_ROLE_CURRENT_ATTEMPT: &str = "flotilla.role.current_attempt";
/// Current attempt's observed materialize terminals, using canonical session ids.
pub const KEY_ROLE_CREW_SESSIONS: &str = "flotilla.role.crew_sessions";
pub const KEY_ROLE_ATTEMPTS: &str = "flotilla.role.attempts";
pub const KEY_ROLE_DESIRED_STATE: &str = "flotilla.role.desired_state";
pub const KEY_ROLE_STATE: &str = "flotilla.role.state";
pub const KEY_ROLE_RESTART_COUNT: &str = "flotilla.role.restart_count";
pub const KEY_ROLE_NEXT_ATTEMPT: &str = "flotilla.role.next_attempt";
/// Observed value; absent when Unknown. Its companion time is RFC 3339.
pub const KEY_CHANGE_REQUEST_TITLE: &str = "flotilla.change_request.title";
pub const KEY_CHANGE_REQUEST_TITLE_OBSERVED_AT: &str = "flotilla.change_request.title.observed_at";
/// Observed value; absent when Unknown. Its companion time is RFC 3339.
pub const KEY_CHANGE_REQUEST_AUTHOR: &str = "flotilla.change_request.author";
pub const KEY_CHANGE_REQUEST_AUTHOR_OBSERVED_AT: &str = "flotilla.change_request.author.observed_at";
/// Observed value; absent when Unknown. Its companion time is RFC 3339.
pub const KEY_CHANGE_REQUEST_STATE: &str = "flotilla.change_request.state";
pub const KEY_CHANGE_REQUEST_STATE_OBSERVED_AT: &str = "flotilla.change_request.state.observed_at";
/// Observed value; absent when Unknown. Its companion time is RFC 3339.
pub const KEY_CHANGE_REQUEST_CHECKS: &str = "flotilla.change_request.checks";
pub const KEY_CHANGE_REQUEST_CHECKS_OBSERVED_AT: &str = "flotilla.change_request.checks.observed_at";
/// Observed value; absent when Unknown. Its companion time is RFC 3339.
pub const KEY_CHANGE_REQUEST_MERGEABLE: &str = "flotilla.change_request.mergeable";
pub const KEY_CHANGE_REQUEST_MERGEABLE_OBSERVED_AT: &str = "flotilla.change_request.mergeable.observed_at";
/// Observed value; absent when Unknown. Its companion time is RFC 3339.
pub const KEY_CHANGE_REQUEST_HEAD_SHA: &str = "flotilla.change_request.head_sha";
pub const KEY_CHANGE_REQUEST_HEAD_SHA_OBSERVED_AT: &str = "flotilla.change_request.head_sha.observed_at";
/// Observed value; absent when Unknown. Its companion time is RFC 3339.
pub const KEY_CHANGE_REQUEST_REVIEW_ACTIONABLE_AT_HEAD: &str = "flotilla.change_request.review.actionable_at_head";
pub const KEY_CHANGE_REQUEST_REVIEW_ACTIONABLE_AT_HEAD_OBSERVED_AT: &str = "flotilla.change_request.review.actionable_at_head.observed_at";
/// Observed value; absent when Unknown. Its companion time is RFC 3339.
pub const KEY_CHANGE_REQUEST_REVIEW_DECISION: &str = "flotilla.change_request.review_decision";
pub const KEY_CHANGE_REQUEST_REVIEW_DECISION_OBSERVED_AT: &str = "flotilla.change_request.review_decision.observed_at";
/// Observed value; absent when Unknown. Its companion time is RFC 3339.
pub const KEY_CHANGE_REQUEST_REVIEW_REQUESTED_FROM_OWNER: &str = "flotilla.change_request.review_requested_from_owner";
pub const KEY_CHANGE_REQUEST_REVIEW_REQUESTED_FROM_OWNER_OBSERVED_AT: &str =
    "flotilla.change_request.review_requested_from_owner.observed_at";
/// Observed value; absent when Unknown. Its companion time is RFC 3339.
pub const KEY_ISSUE_TITLE: &str = "flotilla.issue.title";
pub const KEY_ISSUE_TITLE_OBSERVED_AT: &str = "flotilla.issue.title.observed_at";
/// Observed value; absent when Unknown. Its companion time is RFC 3339.
pub const KEY_ISSUE_STATE: &str = "flotilla.issue.state";
pub const KEY_ISSUE_STATE_OBSERVED_AT: &str = "flotilla.issue.state.observed_at";
/// Observed value; absent when Unknown. Its companion time is RFC 3339.
pub const KEY_ISSUE_LABELS: &str = "flotilla.issue.labels";
pub const KEY_ISSUE_LABELS_OBSERVED_AT: &str = "flotilla.issue.labels.observed_at";
/// Observed value; absent when Unknown. Its companion time is RFC 3339.
pub const KEY_ISSUE_ASSIGNEES: &str = "flotilla.issue.assignees";
pub const KEY_ISSUE_ASSIGNEES_OBSERVED_AT: &str = "flotilla.issue.assignees.observed_at";
/// Observed value; absent when Unknown. Its companion time is RFC 3339.
pub const KEY_ISSUE_UPDATED_AT: &str = "flotilla.issue.updated_at";
pub const KEY_ISSUE_UPDATED_AT_OBSERVED_AT: &str = "flotilla.issue.updated_at.observed_at";
