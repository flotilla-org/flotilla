# Requirements-first placement; VesselRequirement is the runtime primitive

**Status:** Accepted
**Date:** 2026-07-07

Vessel allocation is **requirements-first, not recipe-first**. A
**VesselRequirement** declares what a Vessel must provide — platform
(`os`, `arch`), capabilities (`gpu`, `sandbox-quality`, `gui: recordable`,
entitlements/signing, profiling tools), and affinity (same hull as another
leg, reuse a warm hull, adopt a specific checkout) — and a **resolver**
matches it against available substrate to produce a concrete hull spec,
which hull allocation (**Clyde**, a future extraction) materialises.
Recipes are a strict subset: a fully-pinned requirement.

## Structure

- **Three sources merge at resolution**, in strict precedence:
  **pins** (launch-time: `host=feta`, `reuse hull Y`, `--adopt-checkout`)
  **> template requirements** (abstract only — never host names; images are a
  hull-recipe detail on the resolver side, not a template concern)
  **> PlacementPolicy preferences** (fleet-level resolver configuration:
  defaults, preferences, cost rules).
  An unsatisfiable merge is a **loud resolution failure** — no silent fallback.
- **VesselRequirement is a first-class runtime primitive** (resource + API),
  authored by any orchestrator: a declarative template, an orchestrating agent
  issuing tool calls (ADR 0008), or a human (the CLI/picker is sugar that emits
  a hand-authored request with pins). The old AttachableSet-era
  `ProvisioningTarget` picker collapses into pins; env-reuse (`=env_id`)
  becomes an affinity requirement.
- **Fan-out**: one requirement may materialise several Vessels
  (`forall: platform in …`). The platform matrix belongs on the **Project**
  (what this island targets), keeping templates fleet-portable and runs
  reproducible; fleet-derived ranges ("everywhere we can, min N") are a
  designed-for later resolver expression.
- **Requirements describe the Hull**; crew-side selectors
  (`capability: code` → concrete agent) are a separate axis resolved the same
  requirements-first way (v1: simple lookup).
- **Leg is the control side** — script, prompts, info-routing — and is
  materialised by zero or more Vessels; it is not itself placed.

## Consequences

- `PlacementPolicy` is reborn as **resolver configuration**; the current
  one-named-policy-per-convoy field (and its fragile lexicographic default) is
  transitional and eventually dies.
- Requirements vocabulary starts as a small closed set (host pin, env kind,
  reuse/affinity, platform triple) and grows only from demonstrated need —
  the capability ontology is the known rathole.
- Nothing is built immediately: the M1 dogfood path continues on the seeded
  policies. First implementation slice, when scheduled: requirements
  vocabulary v1 + deterministic resolver v1 replacing the policy-name
  plumbing.
- Clyde's boundary edges (token install, agent config, network setup — or a
  privileged crew member finishing setup; "refit" of a warm hull) are
  deliberately unpinned; see the project-map glossary.
