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
per-installation.** The layout — schema-versioned `generation.json`,
per-platform archives, signature scheme, N-ary source pins — is documented and
versioned; `fleet-install` speaks only "channel URL + format." The lab Forgejo
registry is one server of it; GitHub Releases or an object-storage bucket can
serve a future public release; a plain directory serves the rehearsal and the
disconnected case. No tool below the contract may know which server it is
talking to.

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
