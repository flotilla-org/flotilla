# zj-radar vs andamento: overlap, divergence, and what is worth lifting

**Date:** 2026-08-21

**Prompt:** Robert flagged [marktoda/zj-radar](https://github.com/marktoda/zj-radar)
as "seems like some bits of andamento".

**Status:** Research note, not a ruling. No ADR follows from it directly.

**Sourcing note:** zj-radar claims cite a clone of `main` at commit `8520d41`
(2026-08-21) — paths below are relative to that checkout, and the same files are
readable at `https://github.com/marktoda/zj-radar/blob/main/<path>`. Andamento
claims cite `/Users/robert/dev/andamento` at `e509437`. Zellij fork claims cite
`/Users/robert/dev/zellij` (rjwittams fork) against `/Users/robert/dev/zellij-main`
(upstream `b558b31`). Repo metadata (stars, releases, issues) came from the GitHub
API on 2026-08-21.

---

## Executive summary

The two projects overlap in *silhouette* — both are Zellij sidebar plugins in
Rust/wasm that put a left rail beside your tabs and let you click a row to jump —
and diverge almost everywhere below that.

**zj-radar is an agent-status product.** Its entire data model is one versioned
pipe payload (`zj_radar.status.v1`) carrying `{pane, status, repo, branch, msg,
task}`, pushed by per-agent hooks. Everything it does — roll-up, notification,
cross-session presence, tab naming — is downstream of that one wire type. It is
finished, packaged, and distributed (crates.io, prebuilt binaries, a Claude Code
marketplace plugin, `curl | sh` installer), and it runs on **stock Zellij 0.44.3**.

**Andamento is a presentation substrate.** Its data model is a general metadata
plane — patches of `(target, key, source) → value` with precedence/ordinal/TTL,
projected into hierarchical group paths by a rule catalog, and rendered by a
layered KDL template system with variables, regions, fragments, and inheritance.
Agent status is one small fact (`SetPaneStatus` / `status.attention`) among many.
It is a prototype, unpublished, and **depends on four APIs that exist only in
Robert's Zellij fork**.

The overlap that matters is narrow and mostly at the *bottom* of zj-radar and the
*edge* of andamento: pane-status intake, per-tab roll-up, and how a rail is pinned
into a layout. The genuinely interesting borrowables are three specific
mechanisms — the activity model's Job/Service/Companion classification, the
doc-as-test-oracle rail spec, and the shared-`/cache` presence + snapshot
discipline — plus one packaging pattern (`setup zellij` layout patching). There is
no code to vendor: the fork dependency makes andamento non-portable to zj-radar's
host, and zj-radar's model is too narrow to lift wholesale into andamento's.

---

## 1. zj-radar: what it is

### Identity and activity

- MIT licensed (`LICENSE`, "Copyright (c) 2026 Mark Toda").
- Created 2026-06-29, last push 2026-08-21, 30 stars, 8 forks, 3 open issues
  (GitHub API, `repos/marktoda/zj-radar`).
- Ten tagged releases, `v0.1.0` (2026-06-30) through `v0.3.1` (2026-08-10).
- Two real contributors: Mark Toda (94 commits) and gretzke (33), per
  `git log --format='%an'`. Development is active and current — the head commit
  the day this note was written is a follow-up review pass on the activity model
  (`03d1844 refactor: cohesion pass — one minute band, class-aware heartbeat`).
- Self-described "alpha" in the README badge row, but the shipped surface is
  well past prototype: CI, e2e workflow, nightly-failure issue automation,
  `deny.toml`, a Nix flake, a `RELEASING.md`, and an MSRV job.

Notably, the repo carries `AGENTS.md`/`CLAUDE.md` and a 24 KB `CONTEXT.md` domain
glossary — the same working conventions this repo uses. The design docs credit
"Mark Toda (with Claude)" (`docs/design.md` header).

### Build shape

Three-crate workspace (`Cargo.toml`):

| Crate | What | Published |
|---|---|---|
| `crates/core` (`zj-radar-core`) | Wire schema + classification. No `clap`, no `zellij-tile`. | crates.io |
| `crates/cli` (`zj-radar`) | Host CLI: `notify`, `setup`, `run`. Embeds the wasm via `include_bytes!` in `build.rs`. | crates.io |
| `crates/plugin` (`zj-radar-plugin`) | The wasm sidebar, `wasm32-wasip1`. | release artifact only |
| `plugins/zj-radar-claude/` | A Claude Code plugin that registers status hooks. | Claude marketplace |

About 39k lines of Rust including tests. The plugin pins
`zellij-tile = "=0.44.3"` from crates.io, and scopes it to
`[target.'cfg(target_arch = "wasm32")'.dependencies]` so the host test build
never pulls Zellij's native chain (`crates/plugin/Cargo.toml`). Only
`crates/plugin/src/lib.rs` touches the host API; everything else — runtime,
stores, roll-up, renderer — is pure and testable with plain `cargo test`
(README "Repo layout").

### Data model

One versioned JSON payload, broadcast by name over `zellij pipe`
(`docs/producers.md`):

```json
{ "v": 1, "source": "claude", "pane": {"type":"terminal","id":12},
  "status": "running", "repo": "pinky", "branch": "fix/x",
  "msg": "running tests…", "task": "fix the flaky auth test" }
```

`status` is a five-value severity-ordered enum — `Idle < Done < Running < Pending
< Error` (`docs/activity-model.md` §3). Per-pane state keys on `PaneId`; per-tab
state is a roll-up with highest-severity-wins and a `done/total` count over panes
that ever reported non-idle (`docs/design.md` §4.2).

Two *origins* feed the same store: `StatusPipe` (pushed by hooks) and `Command`
(derived from Zellij's `CommandChanged` events, classified by a `TOOL_RULES`
table in `crates/core/src/command.rs` — `cargo test` → Test, `npm run dev` →
Server, and so on).

### UI surface

A pinned borderless left column, `size=32`, placed *outside* `children` in the
tab template so swap-layout cycling never disturbs it — the same mechanism
Zellij's own bars use (`docs/design.md` §6). Rows are a tab header line plus one
line per tracked pane; glyphs carry status; right-edge glyph cells are hotspots
(`✓` acknowledge, `✕` dismiss). Below the live rows sit a completion ledger
(`─ earlier ─`) and, when a second session exists, a cross-session badge.

It is deliberately a *passive renderer*: `set_selectable(false)`, no `Key`
subscription, mouse-click only; keybindings arrive as verbs on a
`zj_radar.cmd.v1` pipe via Zellij's `MessagePlugin` binding
(`docs/design.md` §12). A launchable floating mode is an explicit non-goal.

### The load-bearing constraint

zj-radar's predecessor (`zellij-smart-tabs`) polled every pane on every output
event with blocking host calls and melted a many-agent session; the postmortem
is `docs/smart-tabs-postmortem.md` and the resulting rule is absolute: **no
blocking host queries on any path** (`get_pane_running_command`, `get_pane_cwd`,
`get_session_list`). Every signal is pushed. This constraint explains most of the
design's odd corners, including why cross-session presence is read from file
mtimes rather than `SessionUpdate` (`docs/design.md` §13, "Why not
`SessionUpdate`").

---

## 2. Andamento: what it is

### Build shape

Four workspace crates plus the legacy single-plugin root crate
(`/Users/robert/dev/andamento/Cargo.toml`), about 32k lines of Rust across 16
files:

| Crate | Role (`README.md` "Controller/Rail Prototype") |
|---|---|
| `andamento-controller` | Background state owner: metadata store, grouping, pins, ordering, pane statuses, rail UI snapshot. 6.2k lines in `state.rs` alone. |
| `andamento-rail` | The visible sidebar renderer, one instance per tab. 10.6k lines in `render.rs`. |
| `andamento-config` | In-rail settings, template diagnostics, stats, inspect views. |
| `andamento-shared` | Wire types, grouping config, template config (2.9k lines), segment bar. |

Unpublished; the intended home is `flotilla-org/andamento` (`README.md` line 9).

### Data model

Andamento's plane is metadata patches, not statuses. From
`docs/sidebar-design/metadata-and-roadmap.md`:

```
store[metadata_target][metadata_key][source_id] = MetadataEntry { value, updated_at, ttl?, precedence?, ordinal? }

MetadataPatch { metadata_target, source_id, set: {key: value}, unset: {key} }
MetadataTarget = Pane | Tab | Group(GroupPath) | Entity{kind, id}
```

`set` replaces per-source, `unset` removes per-source, omitted keys are
unchanged, `updated_at` is stamped by the plugin not trusted from the producer,
and `precedence`/`ordinal` are producer-supplied ordering hints so the
aggregation layer never needs to know what a key *means*.

Group identity is a structured path of key/value segments, derived from flat
facts by an ordered **rule catalog**: the bundled Flotilla rule orders the spine
project → repo → convoy → vessel → session → issue → checkout, then falls back
to git repo/branch, then to raw pane cwd (`README.md` "Grouping Catalog" and
"External Grouping Rules"). Producers publish flat facts only; they never publish
group paths.

Pane status exists but is one narrow message among ~32 named pipes
(`grep -rhoE '"andamento-[a-z-]+"' crates`): `SetPaneStatus` with a six-value
`Priority { Idle, Info, Success, Waiting, Warning, Error }`
(`crates/andamento-shared/src/lib.rs:41`).

Flotilla is already a producer against this plane. `crates/flotilla-manifest/`
in this repo mirrors andamento's patch serde exactly (fixture round-tripped
against andamento's real serde — `crates/flotilla-manifest/src/wire.rs` header)
and ships a `flotilla.*` fact dialect over the
`andamento-apply-metadata-patch` pipe (`crates/flotilla-manifest/src/keys.rs:9`).

### Presentation model

This is where andamento's mass is, and it has no counterpart in zj-radar. From
`README.md`:

- **Templates** bound by *convention name* (`<entity.kind>/<compact|detail>`,
  `tab/title`, `<kind>/full`), with `extends=`, named-field replacement,
  `remove`, reusable `fragment`s, and numeric `priority` that doubles as
  drop-order when the rail is narrow.
- **A layer resolver** walking user → repository → project → fleet → bundled,
  where repo and project layers participate only when that node's `vcs.repo` /
  `flotilla.project` membership matches, so config cannot leak across nodes.
- **Chrome as template primitives** — `box`, `toggle`, `fill`, `dim`, `indent` —
  resolved into the controller view model, so the rail never parses KDL per
  frame.
- **Node-scoped variables** with declared value sets, inheritance from the
  nearest ancestor, and an Inspect view showing effective value, winning setter,
  source layer/file, and overridden history.
- **Regions** — the sidebar itself is an ordered, declared stack
  (`header`, `attention`, `tree`, `controls`) with per-region root template,
  entity form, pinning, and `promote when=` rules. The attention region consumes
  Flotilla's normalized `status.attention` fact.
- **Latent tabs**: entities that are not yet Zellij tabs render as latent rows
  and materialize on demand (`LatentTab`, `MaterializeLatentRequest` —
  `crates/andamento-shared/src/lib.rs:262–320`).

Andamento also renders **image cards** — it drives `PluginGraphicsOp::PlaceImage`
against the fork's kitty-graphics plugin API, gated on a change signature so
unchanged frames don't resync (`crates/andamento-rail/src/lib.rs:1761–1786`).

Rail placement is configurable `left | right | top | bottom`, and the rail
resizes its own boundary via `resize_pane_with_id_to`
(`crates/andamento-rail/src/lib.rs:154`, `1530`).

---

## 3. Overlap map

| Capability | zj-radar | Andamento | Verdict |
|---|---|---|---|
| Pinned per-tab sidebar in the layout template | Yes, `size=32` left column outside `children` | Yes, placement-configurable | **Duplicate** — same Zellij mechanism |
| Click a row to switch tab | Yes, `switch_tab_to(position+1)` | Yes | **Duplicate** |
| One plugin instance per tab, shared state problem | Solved by `/cache` snapshot rehydration | Solved by a background controller plugin + broadcast snapshot | **Same problem, different cut** |
| Per-pane status intake over a named pipe | `zj_radar.status.v1` | `andamento-set-pane-status` | **Duplicate in role, not in shape** |
| Per-tab roll-up of pane statuses | Severity order + `done/total` | `TabStatusSummary` off `Priority` | **Duplicate** |
| Attention surfacing | Header `n!` badge, `attention-next` jump | Declared `attention` region promoting `status.attention` | **Duplicate in intent** |
| Agent producers (Claude Code, Codex) | Shipped, two of them, plus `notify generic` | None — a shell helper and a git-watcher script only | **zj-radar only** |
| Desktop notifications | Yes, with per-status config and cross-instance claim election | None (`grep` finds no notify path) | **zj-radar only** |
| Cross-session presence badge | Yes, mtime-based liveness over shared `/cache` | None | **zj-radar only** |
| Completion ledger ("what finished earlier") | Yes, merged ring across instances | None | **zj-radar only** |
| Command classification from argv | `TOOL_RULES` + interactive set | None | **zj-radar only** |
| Push-only tab naming | `TabNamer`, 557 lines | Not a feature | **zj-radar only** |
| Packaged distribution and installer | crates.io, `curl \| sh`, `setup zellij` layout patching | None | **zj-radar only** |
| General metadata plane with precedence/TTL/source | No — one fixed payload | Yes | **Andamento only** |
| External producers attaching facts to semantic groups | No | Yes (`Entity`/`Group` targets) | **Andamento only** |
| Rule-driven hierarchical grouping | No — flat tab list | Yes, ordered catalog with fallbacks | **Andamento only** |
| Templated rendering with inheritance and layers | No — hand-written renderer | Yes, KDL template system | **Andamento only** |
| Declared region stack / configurable sidebar shape | No (density presets only) | Yes | **Andamento only** |
| Node-scoped variables + inspect provenance | No | Yes | **Andamento only** |
| Latent (not-yet-materialized) workflow rows | No | Yes | **Andamento only** |
| Image/graphics cards | No | Yes (fork API) | **Andamento only** |
| Runs on stock Zellij | Yes, `=0.44.3` | **No** — fork-only APIs | **zj-radar only** |

The honest summary of "some bits of andamento": zj-radar has re-solved the
*sidebar plumbing* problems andamento also had to solve — pinning into the tab
template, per-tab instance state convergence, click targeting, roll-up — and has
gone much further than andamento on the *agent-status product* built on top. It
has not touched the metadata plane, grouping, or templating that constitute most
of andamento's design.

---

## 4. The decisive divergence: fork vs stock

This is the constraint that governs any adoption or collaboration question.

Andamento depends on `zellij-tile` **by path**, from Robert's fork
(`Cargo.toml`: `zellij-tile = { path = "../zellij/zellij-tile" }`), and uses four
APIs that exist only there. Verified absent from upstream `zellij-main`
(`b558b31`) and present in the fork:

| API | Fork commit | Andamento use site |
|---|---|---|
| `zellij_tile::vfs::{resolve_host_path, expand_env}` | `f52779a`, `d262df5` | `crates/andamento-shared/src/grouping_config.rs:304`, `template_config.rs:83,94` — resolving `file:$ANDAMENTO_ROOT/...` config paths |
| `zellij_tile::output::print` (cross-target macros) | `3ad6928` | `crates/andamento-rail/src/lib.rs:256` |
| `resize_pane_with_id_to` (boundary-targeted plugin pane resize) | `e6f6db2` | `crates/andamento-rail/src/lib.rs:1530` — rail width sync |
| `PluginGraphicsOp` (plugin graphics/kitty images) | `dd5b675`, `bbd8870` | `crates/andamento-rail/src/lib.rs:1771` — image cards |

Client-scoped plugin messaging (`0afda3d`) is a fifth fork capability in the same
family.

zj-radar, by contrast, treats stock-Zellij compatibility as a product
requirement: the exact pin *is* the supported-version floor, welded to a doctor
gate by a guard test (`crates/plugin/Cargo.toml` comment), and its one wish for
an upstream API (`is_alternate_screen` / `is_raw_mode` on `PaneInfo`) is written
up as a *future contribution*, with the name-list classifier explicitly designed
to decay into a fallback when it lands (`docs/activity-model.md` §4 layer 4).

Consequences:

1. **Andamento cannot ship to zj-radar's audience** without either upstreaming
   those four APIs or degrading each of the four features. `vfs` and `output`
   are small conveniences with plausible workarounds; `resize_pane_with_id_to`
   and `PluginGraphicsOp` are not.
2. **zj-radar cannot adopt andamento code** — anything touching the rail
   renderer would drag in the fork surface.
3. Conversely, the fork APIs are exactly the kind of thing a second serious
   Zellij-plugin author is a useful ally on. zj-radar's author has an open,
   documented interest in extending `PaneInfo` upstream; a shared upstreaming
   push (`PaneInfo` terminal-mode flags, plugin pane resize, a `vfs`-shaped path
   translation) is the most concrete collaboration surface between the two
   projects.

---

## 5. Assessment

**Is zj-radar a competitor to andamento?** Not really, and not on the axis that
matters. zj-radar is an agent-status rail for people who already run Zellij and
want to know which agent needs them. Andamento is the presentation substrate for
Flotilla's control plane — attention is one region among a declared stack, and
the interesting content is convoys, vessels, latent workflow items, and
project/repo hierarchy that zj-radar has no concept of. If andamento ever ships
publicly as a general sidebar, the two overlap in the shop window; today they do
not overlap in purpose.

**Is there a risk of being scooped?** On the agent-status niche, zj-radar has
already shipped it — packaged, installable, and on crates.io, with a Claude Code
marketplace plugin. There is no value in andamento racing it there. Andamento's
defensible ground is everything downstream of the metadata plane.

**Is there anything to watch?** Two things. First, whether zj-radar grows a
general fact/metadata intake — right now `zj_radar.status.v1` is closed, and
`docs/producers.md` frames extension as "write a producer for the same payload",
so the trajectory is more producers rather than a richer plane. Second, whether
its author lands `PaneInfo` terminal-mode flags upstream, which andamento would
benefit from for free.

**Is there grounds for collaboration?** Yes, in one narrow band: upstream Zellij
plugin API gaps that both projects hit. Two independent plugin authors asking for
the same `PaneInfo` fields, or for a supported plugin-pane resize, is a much
stronger case than one fork carrying private patches.

---

## 6. Adoptable ideas

Ranked by value to andamento (and, where noted, to Flotilla proper). Each names
the mechanism, not the vibe.

### 6.1 The three-axis activity model — the strongest single idea

`docs/activity-model.md` separates a pane's presentation into three *orthogonal*
facts: **origin** (pushed vs observed), **kind** (claude/codex/test/build/server/…),
and **class** (`Job` | `Service` | `Companion`). The governing principle is one
sentence: *"a spinner means bounded work in progress — animation is a promise:
this will complete, and you will want to know when."* Classes fall out of it —
`Job` is bounded work, `Service` is unending by design (a dev server: no spinner,
a steady `▸` mark, and its *exit* is the news), and `Companion` is interactive and
therefore never activity at all (nvim, less, htop render a muted identity label
and are suppressed at intake).

Two implementation details are worth stealing verbatim:

- **The class is a semantic vocabulary, deliberately not an enum.** The doc
  traces every consumer and finds no site that would match all three variants, so
  the code realizes it as an intake policy (`Companion`) plus a
  `Kind::is_service()` predicate, following the existing `Status::needs_attention`
  pattern. That is exactly the reification test this repo's jargon discipline
  asks for, applied to a type rather than a word.
- **Quiet pendings** (§5): an interactive command is admitted as a
  *non-promotable pending* rather than dropped. It keeps its identity — so a
  `zellij run -- nvim` exit reads `nvim ✓` instead of a blank row — but never
  materializes a Running row and never pins the 1 Hz timer. The rejected
  alternative (an ignore-branch) discards identity and is forward-incompatible
  with the future alt-screen classifier. Andamento's `Priority` enum currently has
  no such distinction: a `Waiting` is a `Waiting` whether a human or a build is
  the thing being waited on.

Andamento's relevance: its attention region promotes on a boolean
`status.attention` fact. The Job/Service/Companion distinction is what decides
whether a fact *should* be attention at all, and it belongs on the producer side
of Flotilla's fact dialect (`crates/flotilla-manifest/src/keys.rs`), not in the
renderer.

**Relevance to Flotilla beyond andamento:** the same three-way split is directly
applicable to ADR 0018's Demand model. A demand raised by a service that merely
exited is a different queue entry from a demand raised by a bounded job awaiting
review, and "never notify for a Companion" is a principal-attention economy rule
stated in one line.

### 6.2 The doc-as-test-oracle rail spec

`docs/rail-reference.md` is an *executable* spec: each scenario carries a
` ```rail-input` ` block (the state) and a ` ```rail-expect` ` block (the exact
ANSI-stripped grid). `crates/plugin/src/reference_tests.rs` parses the markdown,
runs the real `aggregate` + `render_rail`, and asserts the grid matches. Editing
the doc edits the test.

This is strictly better than andamento's current `insta` frame snapshots
(`crates/andamento-rail/src/render/snapshots/`) on the axis that matters here:
the expected output lives in a human-readable document that also explains *why*
each rule holds (the doc's numbered "design rules" carry decision markers like
`⟦D6 ✓ = 6⟧`), so a reviewer reads intent and expectation together, and a snapshot
can never be blessed without editing the prose that justifies it. Given this
repo's standing rule that TUI snapshots are a signal and not a formality
(`CLAUDE.md`), the pattern is a direct fit — for andamento's rail first, and
plausibly for `crates/flotilla-tui/src/widgets/table.rs` later.

Cost is low: the parser in `reference_tests.rs` is a few dozen lines of
fence-scanning.

### 6.3 Shared-`/cache` presence and snapshot discipline

Three composable file-coordination mechanisms, all in
`crates/plugin/src/session_files.rs` and `docs/design.md` §5/§13:

1. **Instance rehydration.** A broadcast is never replayed to instances spawned
   later, so a tab opened after agents were running starts blank. Every instance
   mirrors its store to a snapshot on mutation and seeds from it in `load()`.
   Root selection is `/cache` → `/tmp/zj-radar` → disabled, because `/data` is
   scoped `<plugin_id>-<client_id>` and removed on unload despite the docs
   calling it shared. Writes are temp-file + atomic rename.
2. **Content-edge-gated writes plus a liveness heartbeat.** Presence files are
   written only when the projected content actually differs (with the timestamp
   zeroed out of the compare), so a ticking clock alone never rewrites — but a
   60 s unconditional heartbeat keeps the mtime fresh against a 90 s staleness
   threshold. That is a clean separation of *change* from *liveness*, and the
   50% margin is stated as the reason for the numbers.
3. **Staleness is a display state, never a membership filter.** A peer that stops
   heartbeating dims and becomes cycle-ineligible but is never dropped: "a
   session the badge has ever shown must never silently vanish from it." A
   separate, much looser 6 h sweep is the only true forgetting.

Andamento sidesteps (1) with a background controller plugin, which is a cleaner
cut. But (2) and (3) are general: Flotilla's own multi-host presence and the
observed-resource projection face exactly the "is this peer gone or just quiet"
question, and "dim, never drop; separate the sweep from the staleness threshold"
is a good default.

There is also a **cross-instance notification claim**
(`crates/plugin/src/notify_rules.rs`): every per-tab instance computes the same
notification edge and the same deterministic `claim_key` (`p<pane>.<status>.<fnv
hash of title+body>`), then a shared claim file elects exactly one dispatcher.
Worth knowing if andamento ever grows notifications with a per-tab rail.

### 6.4 `setup zellij` — layout patching as a first-class install step

`crates/cli/src/setup/` (analyze / detect / edit / zellij, ~3k lines) does the
thing every Zellij plugin has to make the user do by hand: install the wasm,
manage a **plugin alias** in `config.kdl` so layouts reference the bare name
`radar` rather than a path, inject the sidebar pane into the common layout
shapes, print a paste snippet for shapes it cannot recognize, and drive the
Zellij permission grant — each behind its own `y/N` prompt. `--dry-run`,
`--check`, and `--uninstall` are all present, and it leaves layouts user-owned.

The alias indirection is the specific trick worth copying: it makes the per-layout
snippet path-free, which is what lets users compose the pane into *their* layout
instead of adopting the project's.

The neighbouring `zj-radar run` / `just dev` pattern is also good practice —
a sandboxed throwaway session rooted under `target/dev/data` via
`ZJ_RADAR_DATA_DIR`/`ZJ_RADAR_WASM`, always a fresh uniquely-named session
because attaching to a leftover would silently run the previous wasm, exited
leftovers swept and live sessions never killed. Andamento's `run-andamento.sh`
is a three-line launcher by comparison.

### 6.5 The real-Zellij PTY e2e harness

`crates/plugin/tests/e2e/harness.rs` (976 lines, behind an `e2e` feature so
`cargo test` stays hermetic) drives a real Zellij under `portable-pty` and
parses the frame with `vt100`. Its header documents four traps that cost real
debugging time and would cost andamento the same: `--new-session-with-layout`
is required to create rather than attach from inside a session;
`DIRENV_DISABLE=1` plus `/tmp` as cwd avoids a 30 s devenv stall; permission
grants must be pre-seeded into an isolated temp `HOME` (`permissions.kdl`);
`dump-screen` only dumps the *focused* pane, so the plugin's own output has to
come from the PTY master buffer.

This is the natural home for the small class of andamento bugs that only appear
against a real host — and given cleat already does PTY and VT work in this
portfolio, the harness is a pattern to mirror rather than a dependency to add.

### 6.6 Smaller, still worth noting

- **`self_limiting_pipe_argv`** (`zj-radar-core`'s `pipe` module, documented in
  `docs/producers.md`): `zellij pipe` is *not* fire-and-forget — Zellij holds the
  client until every loaded plugin instance consumes the message, so a plugin
  stuck at a permission prompt blocks it forever, and a per-tool-call producer
  then leaks one process plus two server FDs per event until EMFILE crashes the
  session. The fix is a watchdog *inside the spawned subtree* (a detached
  `sleep <deadline>; kill` alongside the pipe), because a producer killed by its
  hook runner never runs its own kill-on-deadline. **Flotilla's manifest sink
  writes to `andamento-apply-metadata-patch` over exactly this channel**
  (`crates/flotilla-manifest/src/sink.rs`) — it has a
  `BLOCKED_WRITE_WARNING_AFTER` and a respawn ladder, which is the right shape,
  but the orphan class described here is worth a deliberate check.
- **Cadence as an explicit design object.** zj-radar enumerates precisely what
  keeps the 1 Hz timer armed — animating Jobs plus a closed list of scheduled
  one-shots — and notes the trap that removing an implicit tick source (the
  service exclusion) silently starved unrelated one-shots
  (`docs/activity-model.md` §3). Any andamento work on tick cost should start
  from an explicit list of what pins the fast cadence and why.
- **Three lists, three contracts** (`docs/activity-model.md` §4): `IGNORE_NAMES`
  ("this pane is back at a shell prompt"), `AGENT_NAMES` ("owned by the push
  pipe"), and the interactive set ("a real command that never earns a Running
  row") are kept disjoint by a guard test, because a name drifting into two of
  them silently recreates a bug. The general form — *when several string sets
  look alike, pin their disjointness with a test rather than a comment* — is
  cheap and applies anywhere.

### 6.7 Explicitly not worth adopting

- **The status payload itself.** Andamento's patch model is strictly more
  general, and Flotilla already speaks a richer fact dialect over it
  (`crates/flotilla-manifest/src/wire.rs`). Adopting `zj_radar.status.v1` would
  be a downgrade.
- **The renderer.** `crates/plugin/src/render.rs` is a hand-written 1.8k-line
  vertical card renderer with no template layer. Andamento's KDL template system
  is the deliberate investment that replaces it.
- **The no-blocking-host-calls absolutism.** Correct for zj-radar, whose
  predecessor died of polling, but andamento already makes bounded, one-shot
  host calls by design — one cwd lookup per pane lifetime, never retried per
  event, with `CwdChanged` carrying updates thereafter
  (`crates/andamento-controller/src/state.rs`, `cwd_requested`). That is a
  sounder rule than "never", and andamento should keep it.
