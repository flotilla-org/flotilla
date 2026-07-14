# Intent completion at admission

**Status:** Accepted
**Date:** 2026-07-14
**Relates to:** ADR 0008 (agentic-first orchestration — meta-agents become
upstream completers, never prerequisites), ADR 0010 (crew provisioning — the
Brief this ADR fills), ADR 0014 (the issue references whose snapshots feed
the Brief), issue #633 (M1 story: start a convoy from an issue with a single
contained crew agent).

Starting a convoy from an issue requires *something* to turn sparse human
intent ("this issue, go") into a full-fledged `ConvoySpec`: a branch name, a
convoy name, a workflow choice, a Brief. Plane A answers this with an
`AiUtility` call (haiku) generating branch names from the TUI's input flow.
The eventual best incumbent is the governor — it will hold project context.
But making the governor the *owner* of completion would couple starting work
to a meta-agent that does not exist yet. Completion is therefore a **seam**,
not an agent's job.

## Completion is an admission-time, daemon-side stage of the create verb

Callers submit **intent**: an issue reference, optionally a workflow choice,
optionally free text — with any spec fields left blank. Before the
`ConvoySpec` persists, the convoy's home daemon completes it: branch name and
convoy name (generated **together** by one call so they cohere), checkout
path (derived from branch), workflow (the Project's `default_workflow_ref`
when unspecified). **The resource store never holds a half-spec**; nothing
downstream — reconcilers, queries, tables — ever sees one. This is the k8s
defaulting/mutating-admission pattern.

Every caller — TUI issue-row action, bare CLI, future governor — hits the
same verb and gets consistent completion. The verb is **flag-complete and
non-interactively drivable by construction** (recorded M1 constraint): a
caller that supplies every field gets no completion; that caller is exactly
what a governor will be.

## Completers

The M1 name completer reuses the Plane-A `AiUtility` provider (haiku via the
Anthropic API), called from the admission stage instead of the TUI branch
flow, with a **deterministic fallback** when keyless or offline: slugified
issue title, truncated, suffixed with the issue number
(`fix-attachable-set-leak-719`). Creating work must never fail on a naming
nicety.

Meta-agents (governor, bosun) arrive later as **upstream completers**: they
submit intent with more fields pre-filled (naming conventions observed,
workflow chosen, crew provisioning proposed — hooks, skills, system prompt,
tokens, proxies). A human-approval gate between a proposed spec and admission
is a **policy knob on this same pipeline**, not a new mechanism. Admission
defaulting remains the backstop for whatever any caller leaves blank.

## Only branches and convoys are named at admission

Vessel names are workflow-template-defined roles (`implement`, `review`) —
positional, not christened. **Hulls carry the persistent names**: the hull is
the physical ship; crews rotate, voyages end, the ship keeps her name. A hull
is christened once at allocation (the same naming facility, second call
site), stable across reuse, convoys, and crews — it is what a metaphor
visualization keys a ship model/variant on, and what lets a human recognize
"the same environment as yesterday." A vessel's display identity composes as
*hull name + role*. (Hull is portfolio-defined; the christening convention
belongs in the project-map glossary too.)

## The Brief embeds an issue snapshot

The crew Brief for issue-initiated work is: a generated preamble (convoy,
branch, repo, role, completion verbs) + the **issue snapshot** — title, body,
labels as-of launch, stamped `as_of`, with the full reference alongside — +
the human's free-text slot (later also the governor's instruction slot).

Snapshot-primary, walked from the scenarios that force it: a **contained**
crew may have no `gh`, no credentials, no network — a bare reference would
make its first act an impossible fetch; the Brief is the durable
script-as-executed and issue bodies mutate, so only a snapshot is an honest
record; the reference keeps live refresh possible for stances that allow it.
This matters more as stances lock down (e.g. crews that can push a branch —
possibly only to a mirror — but not open PRs: forge capability is part of
the Stance posture, realized walls-first through which credentials hull-prep
installs).

## Consequences

- The governor becomes adoptable incrementally: same verb, richer caller.
  Nothing built for the human path needs undoing.
- Admission needs the Anthropic key reachable from the convoy's home daemon —
  matching where Plane A already makes this call, and homes follow the work.
- Name generation adds one LLM round-trip to create; the deterministic
  fallback bounds the failure mode.
- Brief snapshots duplicate issue text into vessel workspaces by design;
  that is the record, not a cache to invalidate.
