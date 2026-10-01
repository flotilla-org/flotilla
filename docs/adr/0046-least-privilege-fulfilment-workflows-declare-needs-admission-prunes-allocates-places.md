# 46. Least-privilege fulfilment: workflows declare needs; admission prunes, allocates and places

Date: 2026-09-27

## Status

Accepted

Grilled 2026-09-27 on #2075 (rulings recorded there). Amends ADR 0007, ADR 0010 and one clause of ADR 0044 §2. Builds on ADR 0044 (grants select on the work; delivery follows fulfilment).

## Context

A workflow's **shape** (its roles, handoffs, and what counts as done) is fused with its **fulfilment** (how and where each vessel runs):
- Template vessels carry `stance: contained | trusted`.
- Templates exist in pairs that differ only by stance (`single-agent-contained` / `single-agent-trusted`).
- Admission refuses a host-direct placement for a "contained" workflow.

On 2026-09-27, wanting a crew on the newer Claude Code that only kiwi had forced a *workflow* swap, when only *where* the same work ran had changed. ADR 0044 has since taken stance out of credential grants, so templates are the last place where stance decides behaviour.

Real work needs more than one placement axis:
- RAD Debugger edits compile on Linux, but verifying them needs real macOS and Windows desktops.
- A particular bug might be Windows-only.

And trust is the wrong organising idea. Whose repository the code lives in proves little, because dependencies come from everywhere, so every run is somewhat untrusted. The right principle is **least privilege**.

ADR 0007 already makes placement requirements-first, with `VesselRequirement` as the runtime primitive and fan-out authored by an orchestrator. ADR 0010 makes stance a vessel-level floor of confinement. What is missing:
- needs attached to the *work*;
- the arrangement of roles into vessels treated as a *decision*;
- fulfilments described by what they *grant*.

## Decision

### 1. Least privilege: work declares needs, placement grants the minimum

- Work declares **capability needs**, not stances. For example:
  - `platform: macos | windows | linux`
  - `gui_session`
  - `gpu`
  - `host_devices`
  - `network: <scope>`
  - `container_runtime`
  - `toolchain: <name>`
  - `harness: <adapter> >= <version>`
- Placement grants the **least-privileged fulfilment that covers them** (§4). Containment is simply what least privilege looks like when nothing more is needed.
- Host-direct is chosen only when a need forces it, for example a real Windows desktop, or a harness only that host has.
- **Any placement above the minimum is an escalation.** It is recorded with the operator's or decider's reason, for example "operator override: kiwi-direct for a harness preview".
- `stance` is removed from workflow templates. The paired templates collapse into one each (`single-agent`).
- Credential grants are unaffected (ADR 0044: they select on the work). Fulfilment only changes how credentials are delivered.

### 2. Needs attach to roles and compose by union

- The workflow is **roles and handoffs**, with no vessels. Needs attach to **roles**: a coder needs linux plus a toolchain, a mac verifier needs `platform: macos` + `gui_session`, a reviewer needs nothing.
- Needs compose as a **union across layers that only add**:
  1. **Template role:** needs intrinsic to the role in this workflow.
  2. **Project:** standing needs for the island's work, per role. This includes the platform matrix, which ADR 0007 already places on the Project.
  3. **Issue:** needs attached to the specific work, such as a `needs: platform:windows` field or label.
  4. **Derived:** needs implied by other dispatch decisions. For example, choosing a model only available on a newer harness adds `harness >= X`.
  5. **Dispatch:** explicit operator or governor additions.
- A lower layer cannot remove a need a higher layer added. Going *below* the computed needs is not a thing, since needs are what is necessary. Going *above* least privilege is the recorded escalation of §1.
- The computed needs are frozen into the convoy's admission snapshot.

### 3. Admission is a pipeline: prune, allocate, place

1. **Prune.** Templates are **full fat**: they declare the most a project's work would need, and mark their optional parts (stages, roles, fan-outs). A pluggable **decider** takes the full-fat workflow plus the context bundle (issue, change scope, project, budget) and cuts it down, recording each cut with its reason. For example: skip platform verification for a Linux-only change, or skip in-convoy review for a typo.
   - This is the first concrete use of #1928's admission decision layer.
   - Deciders may be static rules, a router model (a jev-style router), or a reasoning governor. Their cut is an input to the later stages, never revisited by them.
2. **Allocate.** The surviving roles are grouped into vessels. Allocation is the orchestrator that authors concrete `VesselRequirement`s, keeping ADR 0007's rule that fan-out is authored and never a field on a requirement. Its constraints:
   - A vessel's needs and privilege are the **union** of its roles'. Least privilege favours splitting roles whose needs diverge: a reviewer does not share a host-direct GUI vessel.
   - ADR 0044's rule forces a split: roles minting *different* permission sets for one credential cannot share an environment.
   - Handoffs within a vessel are cheap (a shared filesystem). Across vessels they go through artifacts (ADR 0042/0043).
   - Vessel count costs capacity and provisioning time.
3. **Place.** Each vessel is mapped to a fulfilment at least privilege (§4).
4. **Backtracking is bounded and one level deep.** If placement cannot cover a vessel, allocation may split it, because a union of needs that no single fulfilment grants may be coverable by two. Then it re-places. If that still fails, admission refuses, **naming the uncovered role need**, for example "no fulfilment grants `platform: windows` + `gui_session` + `harness >= 2.1.300`". There is no silent fallback.
5. **The admission snapshot records all three decisions** (the cuts, the grouping, and the placement), each with its justification. `convoy explain` shows why the crew is arranged and placed as it is.

### 4. Fulfilment kinds declare grant sets; hosts supply live facts; the placement tie-breaker breaks ties

- **Fulfilment kinds replace placement policies** as what placement chooses between. Each is a declared resource, for example:
  - `docker-per-vessel` on feta with image X;
  - `tart-vm` on comte (macos);
  - `host-direct` on beaufort or kiwi.

  Each declares a **grant set**: `platform`, `gui_session`, `gpu`, `network:<scope>`, `host_account_reach`, `container_runtime`, and so on. Today's per-host placement policies migrate into fulfilment kinds.
- **Hosts observe and advertise live facts** that complete the grants:
  - harness versions and available models (#2073);
  - installed toolchains;
  - whether a GUI session is actually logged in;
  - current capacity.

  They are carried on the Host resource beside heartbeat and credential health. Needs such as `harness >= X` match observed facts, never stale declarations.
- **Privilege is a partial order on grant sets, not a single score.**
  - Placement keeps the candidates whose grants cover the needs, then takes the **minimal** ones: those where no other candidate's grants are a strict subset.
  - Incomparable candidates (a macOS VM against a Windows host) are separated by the needs themselves.
- **Ties among minimal candidates go to cost and availability.** That is the placement tie-breaker's allocation judgement:
  - owned idle capacity first, then subscription-included, then metered;
  - scarce platform capacity (macOS, Windows) reserved for work that names it, **relative to what is available**: it is held back only while an unreserved candidate also covers the needs. When reserved capacity is the only cover, admission uses it without an escalation and records the fallback in the admission decision (amended 2026-10-01, #2395);
  - live availability counts, for example a host that sleeps on a schedule.

  The placement tie-breaker is a pluggable decider for an already selected workflow. The fleet-wide Quartermaster proposed in #1394 remains future work. Reservation judgement that looks beyond the current admission, such as holding scarce capacity for a known upcoming need, belongs to the Quartermaster, not to the tie-breaker.
- **An escalation is choosing a non-minimal candidate,** and it must carry a reason (§1).

### 5. Amendments

- **ADR 0007.**
  - `VesselRequirement` remains the runtime primitive; *allocation* now authors it from role needs.
  - The `sandbox-quality` capability is replaced by the need/grant vocabulary.
  - PlacementPolicy preferences become fulfilment kinds and the tie-breaking decider.
  - Pins remain launch-time inputs; a pin that exceeds least privilege is an escalation, and is recorded.
- **ADR 0010.**
  - Stance stops being declared by workflows. The *effective confinement* of a vessel is the grant set of its fulfilment, recorded on status.
  - The AgentAdapter derives harness permission flags from that grant set (walls-first still holds: realise confinement in the environment, with harness flags as fallback).
  - Under-realisation still fails loudly.
- **ADR 0044.**
  - Its §2 clause that "a workflow may still *require* containment" no longer holds, because workflows do not declare stance (§1).
  - Containment is what least privilege yields when no need forces more. A host-direct placement comes only from a capability need or a recorded escalation.
  - Its grant selection and delivery rules are unchanged.

## Consequences

- **One workflow can yield different vessel arrangements** as needs, cuts and capacity vary. A project keeps one full-fat workflow instead of a template per fulfilment.
- **Moving work between hosts is a placement fact, not a workflow change.** A newer harness on one host is a need or a recorded escalation.
- **Multi-platform work becomes expressible:** a Linux coder, plus mac and Windows verifier roles placed on a VM and a GUI host. RAD Debugger is the proving scenario (with the #1996 map for host-direct GUI crews).
- **Capacity gaps become visible:** an uncovered need refuses admission by name, instead of silently falling back.
- **Cost:**
  - a needs vocabulary and grant vocabulary to maintain;
  - host fact probes per harness and toolchain;
  - an allocation stage with its own tests;
  - migrating existing templates and placement policies.
- **Not decided here:**
  - running agent loops apart from their tool environments, with sandboxes handed between agents. Declaring agent interactions separately from deployment shape makes this possible later.
  - how deciders are chosen per project;
  - layered crew images (#2074), a fulfilment-side concern that sits alongside this.
