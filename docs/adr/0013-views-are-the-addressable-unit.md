# Views are the addressable unit

**Status:** Accepted
**Date:** 2026-07-13
**Relates to:** ADR 0005 (composition-of-panels view model, consumer-side),
ADR 0011 (named-query result sets), ADR 0012 (generic tables — its structured
scope params, including `host`, ride this scheme's optional `?key=value`
channel; see the host-free rule below), issue #589 (arbitrary tab model),
issue #667 (manifest connector — will embed these addresses in materialise
recipes).

The TUI's tabs were hardcoded (overview, one tab per registered repo, a
bolted-on convoys tab). Issue #589 makes tabs arbitrary, and the PM capability
model needs a deep-link: *one pane = one flotilla-TUI view scoped to one node*.
Because the #667 connector's materialise recipes will hold copies of these
deep links, the address scheme is a quasi-external contract and gets an ADR.

## Decision

**A View is the addressable unit: an instance of a ViewKind with typed
parameters.** It is surface-agnostic — the TUI renders an open View as a tab,
a Presentation Manager as a scoped pane, a future web surface as a page. A
"tab" is TUI-local vocabulary for a container of an open View, never a domain
concept (see CONTEXT.md: **View**).

- **Identity is kind + parameters.** Opening an address that is already open
  focuses the existing View; it never duplicates. Labels are a display
  projection (short default, disambiguated on collision, user-overridable)
  and are never part of identity or address.
- **Address syntax: kind-rooted path.**
  `<kind>[/<param>...]` with percent-encoded segments; optional parameters,
  if a kind ever grows them, as a `?key=value` suffix. A `flotilla://` prefix
  is accepted and stripped on parse (and prepended where a full URI is
  required). Initial kinds:

  | Kind | Address |
  |---|---|
  | overview | `overview` |
  | repo | `repo/<authority>/<path>` (canonical remote identity) |
  | convoys | `convoys/<namespace>` |
  | convoy | `convoy/<namespace>/<name>` |
  | vessel | `vessel/<namespace>/<convoy>/<vessel>` |
  | project | `project/<namespace>/<name>` |

- **The frozen surface is exactly: kind names + their positional parameter
  signatures.** No version token. Evolution is additive only — new kinds, new
  optional `?` parameters. A shipped kind's name and positional order never
  change.
- **Composition never enters the address.** A View renders one or more
  panels, each bound to a named query (ADR 0011) scoped by the View's
  parameters; the composition is declared by the ViewKind on the consumer
  side (ADR 0005) and may evolve freely under a stable address. Panels are
  not addressable — no fragment/anchor syntax. A recipe minted today still
  opens the right View after its composition gains panels.
- **Address identity is host-free.** Positional parameters name resources
  (`namespace/name` identities), not placements; host is routing data stamped
  on rows, and no kind ever takes a host as a positional parameter. Views
  with a "local" notion filter to the current machine by default. This
  refines (does not conflict with) ADR 0012's structured scope params: a
  *host filter* on a table view (like any scope param) is an optional
  `?key=value` — additive, deep-linkable, and distinct-by-address, without
  host ever becoming part of a resource's identity.
- **Parse failure and dangling references fail loudly, per View.** An
  unknown kind, malformed address, or address naming a resource that no
  longer exists renders that View's error state naming the address — never a
  silent fallback to another view, and never degrading other open Views.
- **One parser.** A `ViewAddress` type (`FromStr`/`Display`) in
  `flotilla-protocol` is shared by the TUI, the CLI, and the future
  connector.
- **`flotilla view <address>` launches scoped mode:** exactly that View, no
  tab shell (no tab bar, no overview, no tab bindings), and no coupling to
  the persisted open-view set. Drill-down inside a scoped View navigates in
  place (with a back stack), preserving one-pane-one-node. This command
  string is what materialise recipes embed.

## Alternatives considered

- **Full-URI-only addresses** (`flotilla://...` mandatory): noisier in
  recipes and CLI for no information gain; kept as an accepted superset
  instead.
- **GroupPath-style key=value segments** (the PM spec's latent-view
  identity, kind derived by projection from which keys appear): maximal PM
  alignment, but kind-by-inference is implicit, and non-node-scoped kinds
  (overview, future approvals) need fake keys. Kind-first keeps parsing
  total; the connector owns the GroupPath→address mapping table, which is
  already its chartered job (#667).
- **Panel anchors** (`project/...#issues`): would leak composition into the
  frozen contract. A pane wanting one panel at some scope is a future
  single-panel kind, not an anchor.
- **A version token in the address**: rejected in favour of the additive
  compatibility contract; a token would force every recipe to carry and
  every consumer to dispatch on it, for a scheme whose whole surface is
  deliberately tiny.

## Consequences

- The TUI's open tab set becomes an explicit, ordered, TUI-owned list of
  addresses persisted in `open-views.toml` (`{address, label_override}`);
  `tab-order.json` dies and the daemon loses its tab-order writer.
  Registered repo ≠ open tab: registration is a daemon concern, showing is a
  Surface concern.
- `repo/<authority>/<path>`'s current renderer is the Plane-A repo page;
  the address is durable and the renderer is swappable to a composed view
  when checkout/clone/issue queries exist, with no address change.
- Per-View policy (pinned, closable, binding-mode stack) hangs off the
  ViewKind; tab-shell bindings (switch, move, close, open) are app-global
  and uniform, guarded only by `pinned`.
- Open Views drive the `SubscribeQueries` set (union across open Views,
  recomputed on open/close); Plane-A repo streams stay global and outside
  this lifecycle until they are deleted with Plane A.

## Amendment: curated query-family Views (2026-07-20)

Issues, checkouts, and independents are addressable as single-table Views.
Their canonical grammar is `issues?project=<namespace%2Fname>`,
`checkouts?project=<namespace%2Fname>`, and
`independents?project=<namespace%2Fname>`. Bare `checkouts` and
`independents` are their Fleet-wide forms; bare `issues` is invalid. Family
names and percent escapes render canonically in lowercase/uppercase
respectively, and parsed addresses deduplicate by their typed identity. The
earlier experimental `?repo=` form is not part of the contract.

The client-visible query scope is Project-only: Repository membership is an
Aggregator implementation detail. Source search is transient state of an
open issues View, not part of its persisted address; leaving search restores
the base materialized window.
