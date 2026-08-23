# Convoy identity: generations, role addresses, and actuation locality

**Status:** Accepted
**Date:** 2026-08-17
**Relates to:** ADR 0027 (ensure entries — the standing convoys this ADR
names), ADR 0030 (the governor whose kills and naming exposed every defect
here), #1559 (generations ruling), #1525 (role addressing), #1545 (cross-host
reconciler defect), #1483 (typed host identity), #1524 (dual host rows),
#1530 (first governor kill post-mortem).

## Context

The first standing convoy surfaced four identity defects in one week. Its
session rendered as `terminal-governor-govern-governor` — convoy × vessel ×
role concatenated into a name no human should speak, and shaped to collide
with every other project's governor (#1525). Restarting it required manual
resource surgery because its abandoned husk held the name its ensure needed
back (#1559). It was killed twice by a reconciler on the *admitting* host
demanding a local provider registry for an environment only the *actuating*
host could ever hold (#1545, #1530). And the host-identity comparisons
involved are stringly, so the alias-vs-canonical bug class recurs faster than
call sites can be audited (#1483, from #1473's review).

These are one problem: the system never separated a convoy's **stable
identity** (what operators address and ensures maintain) from its
**incarnation** (one record's lifetime of history) from its **actuation**
(which host does local work). This ADR fixes the cuts.

## Decision

### 1. A Convoy record is one incarnation

A Convoy resource records one generation of a standing convoy: its crew
turns, archive pointers, and terminal reason. Records are never reused across
restarts. Abandoned or otherwise terminal generations are retained as
history — the ledger of that life — and an ensure rebuilds by creating the
*next* generation, never by resurrecting or colliding with a husk.

### 2. Record names are machine plumbing; identity lives in labels

Convoy record names are short generated identifiers, unique per incarnation
by construction. Stable identity lives in labels/fields:
`project`, `role`, `generation`. No surface (attach, sidebars, `ls`,
andamento) shows a generated name; they display and accept the role form —
**`governor @ andamento`**. Ensure ops-file entries declare `{project, role}`
(project implied by the declaring repository); the triple-repetitive
generated names of the pre-ADR era die with this.

The invariant **at most one live (non-terminal) generation per
{project, role}** is enforced at admission on the owner host, where it is a
local transaction (see 4).

### 3. Resolution is selector-in-context

- In project context (project tab, project-scoped CLI, andamento sidebar),
  a bare role — `flotilla attach governor` — resolves to that project's live
  generation.
- In bare context, a bare role resolves iff exactly one live candidate exists
  fleet-wide; otherwise it refuses and lists the qualified candidates.
- The explicit form `role@project` resolves from anywhere, always. Refusal
  messages teach it.

### 4. Admission completes on the primary placement host

The convoy record is created and owned by its primary placement host —
owner=actuator is structural for single-host convoys, not a mitigation.
`convoy start` from any host *routes* admission to the placement host; if it
is unreachable, start fails loudly. A desk never owns an always-on record.

### 5. Actuation is per-child local; cross-host convoys stay legal

Each child resource (Environment, TerminalSession) is reconciled by the host
that actuates it. The convoy-level reconciler aggregates **replicated child
status**; it never performs a child's local work and never requires a
provider registry for a child it does not actuate. A convoy with vessels on
two hosts is therefore legal: the record lives on one placement host, each
host reconciles its own children. The 2026-08-15 governor kills are the
outlawed shape: "a reconciler demanded a local provider registry for a child
it doesn't actuate."

### 6. Typed identities at the resolution boundary

Two identity classes get newtypes now, because their confusion is proven or
imminent: **`CanonicalHostId`** (produced only by the shared resolver;
raw spec strings no longer type-check where canonical identity is compared —
#1483 as written) and **`RoleAddress`** (`{project, role}`, parsed from
`role@project`), with the resolver returning a typed live-record handle
rather than a raw name. Spec authoring keeps friendly strings; the boundary
is resolution. Generation numbers and record names stay plain — they cross
no ambiguous comparison boundary alone.

## Consequences

- **#1559's mechanical fixes unblock**: the ensure supersedes terminal husks
  by admitting the next generation under a fresh generated name; the durable
  Abandoned stamp remains required; no manual surgery.
- **The #1524 class shrinks**: admission reads placement-host state where it
  is authoritative, and `CanonicalHostId` makes the alias comparisons that
  seeded the dual-row confusion uncompilable.
- **Attach composes unchanged**: recursive next-hop resolution (protocol 19
  ruling) now begins with RoleAddress → live record → owner host, then hops
  as before.
- **Rename-stability**: project renames touch labels, not record names, so
  history survives; this matches the Project resource's rename-stable
  identity direction.
- **Migration is cheap now**: exactly one standing convoy exists (the
  andamento governor). It is re-admitted once under the new shape; no
  compatibility path is built.
