# Repository identity spans remotes: one repository, many mirrors

**Status:** Accepted
**Date:** 2026-08-22
**Relates to:** ADR 0014 (Repository as convergent-facts resource — this ADR
widens its identity), ADR 0020 (Projects and membership — whose duplicate
materialization this kills), issue #1640 (the observation), the lab-forks
plan in the portfolio map (the mirror topology this legitimizes).

Three repositories tracked through both their GitHub remote and their lab
Forgejo mirror materialized **two Projects each** (`X` and `X-lab`):
Repository identity was remote-URL-derived, so a mirror counted as a
different repository, and tracking its checkout materialized a
whole-repository Project for it. One codebase, two Projects, association
and dispatch split by which remote a checkout happened to clone from —
with the `-lab` Projects carrying issue-source bridges back to GitHub as
standing evidence they were always the same intent.

## Decision

**A Repository declares its remotes; its first-declared remote is its stable
identity.** The remotes list is part of the Repository record
(user-editable, definitions-class like the rest of the record):

- Identity derivation from a checkout resolves *any* declared remote to
  the same Repository. A checkout cloned from a mirror is a checkout of
  the canonical Repository, full stop.
- Whole-repository Project materialization keys on the Repository — so a
  mirror checkout can never materialize a second Project.
- The declaration is **per-Repository, not per-installation** (ruled
  explicitly: a fleet-level `lab/* mirrors github.com/flotilla-org/*`
  rule may exist as *sugar that generates or suggests* per-Repository
  declarations, but the resource record is the truth — the model must
  survive installations with different or no lab forge).
- An undeclared second remote observed in the wild (a checkout whose
  origin matches no known Repository remote) materializes a provisional
  Repository as today, with the provenance-edge machinery (ADR 0020's
  identity-upgrade path) merging it into the canonical Repository when
  the declaration is added.
- When an already-associated checkout observes that its live remote moved, the
  Repository is updated in place: the moved URL joins the declared remotes and
  becomes the first (live) transport, while `identity.canonical_remote` remains
  the original birth identity. Forge operations and credential scoping derive
  their repository name from that live remote, never from the identity remote.

## Migration

The three standing pairs (`andamento`, `cleat`, `flotilla` × `-lab`):
declare the lab remote on each canonical Repository; re-point the `-lab`
Projects' associated history (convoys, issues carry project refs — these
re-associate to the canonical Project or are annotated with provenance);
delete the `-lab` Projects and the `lab/X` Repository records through the
raw path. The `-lab` issue-source bridges collapse into the canonical
Project's issue-source configuration.

## Consequences

- Checkout attribution, change-request lookup, and issue association all
  key on canonical identity regardless of clone remote.
- Fleet hosts keep cloning from the lab mirror for speed; nothing about
  transport changes — only identity.
- The forks ledger's open-source forks (zellij, ghostty, raddebugger) are
  *not* mirrors of our repositories and are untouched: a fork is its own
  Repository with its own identity; this ADR is about the same repository
  under multiple remotes.
