# Release pipeline: format as contract, rehearsed generations, declarative deploys

**Status:** Accepted
**Date:** 2026-08-25
**Relates to:** fleet#2 on the lab hub (the incident file: four unversioned
validators hand-patched during one payload change), issue #1795 (restart
survival — prerequisite for the declarative phase), ADR 0036 (whose skill-pin
payload triggered the drift), the 2026-08-25 outage chain (`FLOTILLA_SKILLS_DIR`
unit drift, required-list content, private-source assumption — three stacked
never-live-tested failures).

The fleet's deploy chain was grown by hand where each capability lived:
promoter and finalizer on raclette, signer on comte, `fleet-install` snapshots
per host — five independent, unversioned copies of one implicit contract
("what is a valid generation"), none exercised by any test between `cargo
test` and the live fleet. One payload change broke four of them; the release
that followed shipped three provisioning bugs that unit and mock coverage
could not see, taking dispatch down for a night. Ruled with the operator as
one program:

## Decision

**1. The generation *format* is the public contract; hosting is
per-installation.** Amended by the [#1848 owner ruling](https://github.com/flotilla-org/flotilla/issues/1848#issuecomment-6013492819)
(2026-10-06), implemented first in #2762: the format is **component artifacts
plus a signed generation pin list**. A component is anything with an independent
pin: today flotilla (flotilla and flotillad), cleat (with runtime libghostty-vt),
and skills (the pinned skill trees, platform-independent). Artifacts are what
gets installed; Ghostty/Zig prefixes and Cargo targets are build caches inside
a component build, never fleet artifacts.

A component's identity is (component, source SHA, platform, recipe hash), with
toolchain pins and build features in the recipe; identical inputs build once
and are reused. Its archive digest is the truth, with an optional version label
for people and compatibility. Darwin signing happens per component, once,
before its digest is recorded. Component builds prove execution and linkage and
**measure provides** from binaries and payload (capabilities, protocol, skill
trees). Components declare requires; compose refuses unmet requirements against
the pinned components' provides and names the gap. Component manifests and both
generation schemas live in the one shared validation module; see the
[component format contract](../../ci/fleet-candidates/component-manifests.md).

`generation.json` schema v2 is a signed pin list of component identities and
archive digests per platform, with platform-independent components listed once.
Components are digest-addressed in the package store. Builds reuse identities
already in the store; promotion composes, checks requirements and publishes.
Rehearsal consumes that composition unchanged (§3); install verifies digests
and signatures against the generation manifest, fetching only missing
components. Rollback remains a manifest switch, and retention keeps components
of the current, previous and recent generations.

There are two distribution routes: fleet components pinned in the generation,
and project tools/skills distributed from their own repositories, resolved at
a project ref and staged into its crews. A repository-distributed tool can be
promoted to a fleet component. Today's bundled mattpocock-skills and rjw-sdlc
from rjw-skills form the skills fleet component; the owner may revisit that
membership.

Hosting is per-installation: the lab Forgejo registry today, a plain directory
for rehearsal/disconnected installs, or other servers for public releases.
`fleet-install` speaks only "channel URL + format"; no tool below the contract
may know which server it is talking to. Migration is one dual-published
transition generation (v2 pin list plus a v1-compatible bundle for the old
installer's §2 self-update handoff); validators and installers read v1 and v2
for that generation, removing v1 one fleet roll later. Per-component build jobs
and compose/publication/installer support follow in #2763 and #2764.

**2. Tooling homes by nature.**
- *Contract and validators* — schema plus all validation logic (promote,
  finalize, install verification) — live in this repository beside
  `ci/fleet-candidates/`, sharing **one validation module** so the contract
  exists exactly once. A contract change updates schema and every validator in
  one reviewed PR, and the orchestration/validator mirror race (r183) dies
  structurally.
- *Lab deployment configuration* — service units, signing plumbing, tokens,
  stage placement — is installation-declared state (project-map / fleet repo),
  referencing tool versions, never containing tool code.
- *`fleet-install` self-updates*: the running installer verifies a new
  generation (signatures, manifest), then hands off the flip to that
  generation's own installer. The trust anchor is the old installer's
  verification; hosts track tooling automatically and the stale-snapshot class
  dies. Hosts that cannot run flotillad (raclette, comte) keep an explicit
  provisioning step until the reconciler reaches them.

**3. Generations are rehearsed before they exist.** A compose environment on a
silo guest consumes the built candidates through the format contract (served
from a plain directory) and must pass: `fleet-install` on **both** the fresh
first-install and upgrade-from-previous paths; daemon start from the generated
unit; one contained probe convoy reaching provisioned-environment,
skills-staged-from-the-real-pinned-repos-with-the-real-App-grant,
terminal-session-alive, checkout-created. The probe stops **before any LLM
turn** — it proves the machine, not the agent; live-agent smoke remains the
governors' job on the deployed fleet. The rehearsal writes an attestation (run
id, candidate digests, verdict) and **the promoter refuses a run without
one** — an unrehearsed build structurally cannot become a generation.
Accepted gap, held explicitly: the rehearsal covers linux-x86_64 only;
Darwin/kiwi keeps install-last-with-rollback discipline.

**4. Deploys become declarative — direction ruled, designed later.** The
desired generation is declared state in project-map; each host reconciles
itself toward it (verify through the contract, self-install via the handoff,
restart, report). The per-host ssh loop dies; deploy is a manifest change and
rollback is a revert. Deliberately sequenced last: it depends on the
self-update handoff and on #1795's answer to daemon-restart survival, and its
self-replacement mechanics deserve their own design.

## Consequences

- "What is a valid generation" becomes one tested module instead of five
  drifting copies; payload additions are one-PR changes.
- The class of failure that cost the 2026-08-25 night — never-live-tested
  provisioning paths shipping — is converted from fleet outage into a red
  rehearsal before promotion.
- The rehearsal doubles as the first consumer of the public format contract,
  keeping it honest before any public release exists.
- New moving parts: the shared validation module and re-homed validators, the
  installer handoff, the rehearsal harness and attestation, the promoter gate,
  and eventually the generation reconciler.
