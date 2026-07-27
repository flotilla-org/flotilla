# Credentials: replicated declarations, host-local material, stance-first grants

**Status:** Accepted
**Date:** 2026-07-27
**Relates to:** ADR 0016 (replication classes — declarations and grants ride
the definitions class), ADR 0010 (Hull/Crew boundary — credential state is
crew/vessel state), the fork-stance rulings (#978, #1047/#1049), #954
(attribution), issue #1050 (the grill that fixed this contract) and the
credential-pattern research
(`docs/superpowers/research/2026-07-27-agent-harness-credential-patterns.md`),
map #1046.

Today every crew inherits the ambient identity of the human whose machine it
runs on: `gh` acts as Robert, harness logins are copied caches of personal
subscriptions, and an uncontained vessel can read all of `~/.config`. The
research established that no single env-var shape covers the supported
harnesses (Codex requires a login transformation, Docker a config entry,
some consumers a vendor-schema file), and that treating injection as
"copy env vars in" would preserve the ambient-identity problem for exactly
the consumers it appears to solve.

## The split: declarations replicate, material does not

- **Credential material is never a resource and never replicates.** The
  bytes live host-local (generalizing the existing
  `~/.config/lab-forgejo-<agent>-token` pattern), owned by the host that
  provisions vessels with them. Secrets do not travel the resource log,
  replicas, snapshots, or any future archive.
- **`CredentialSpec` declarations replicate** (definitions class): name,
  consumer adapter (`claude`, `codex`, `gh`, `forgejo`,
  `docker-registry`, …), source, lifecycle
  (`static | refreshable | issued`), and placement requirements. The ten
  requirements in the research are the field checklist (vendor-schema
  files, login transformation, mutable caches, multi-field entries, no
  hull/workspace persistence).
- Which credentials a host actually *holds* is an admission fact, same
  family as adapter availability: "feta lacks `codex` → cannot take this
  workflow" is refused early, mesh-wide, without any secret leaving home.

## Host-local resolution, evolving

The `source` axis starts as `file | env | issue-command` and is expected
to grow, all behind the same declaration and invisible to consumers:

- **vault-style managers**, fetched as late as possible;
- **provider-based minting** — GitHub App installation tokens (one hour,
  repo- and permission-bounded) are the model;
- **credential proxies**, where existing permissions are too coarse or
  tokens too limited.

## Delivery: per-consumer adapters with mandatory preflight

A generic transport cannot finish the job (research conclusion). Each
consumer adapter knows its harness's accepted forms, precedence, required
transformations (`codex login --with-api-key`,
`docker login --password-stdin`), and whether its cache must stay
writable. **Preflight is mandatory**: the adapter proves the credential
present and usable before the crew reports started — a missing credential
is a bounded provisioning failure, never a silently retrying crew.

## Identity: crews never carry a human's forge identity

- **GitHub**: a dedicated machine account for crew work now; evolving to
  a GitHub App minting per-crew installation tokens. One machine account
  for all crews initially — per-crew isolation comes from per-crew App
  tokens later, not account sprawl. PRs are authored by the crew
  identity; humans appear as reviewer/merger (#954 resolved honestly).
- **Lab Forgejo**: a crew-class agent user with tracker-scoped tokens,
  extending the per-agent-user pattern. Forgejo PATs do not expire, so
  rotation is operational discipline until an Authorized-Integration
  issuer exists (recorded unknown).
- **Human ambient identities are desk-only** and are never delivered into
  any vessel. Enforcement against an uncontained vessel is best-effort —
  filesystem visibility makes scoping advisory there, and this is stated
  rather than pretended away. A future global security/setup agent gets
  "encourage and drift-check credential discipline" as a standing duty.

## Reach: default-deny grants, stance first

- A declaration says what exists; a **grant** binds credential names to
  selectors — **stance** as the primary key (fork-stance crews receive
  model-API credentials only; trusted-repo crews add the crew forge
  identity), refined by project/repo. A vessel receives exactly the union
  of matching grants, resolved at admission. Nothing is ambient.
- Grants are policy: they replicate alongside declarations.
- **Uncontained vessels get an env allowlist at launch** as the backstop:
  only granted variables pass through.

## Migration: no flag day

1. Contained crews are **default-deny from day one** — a new path with no
   legacy to preserve.
2. Uncontained crews keep working ambiently while the vocabulary lands,
   then move to the allowlist with a warning phase.
3. Operator tasks are explicit HITL work: create the machine account,
   place tokens on hosts, create the Forgejo crew user.
4. Ambient inheritance in crew provisioning is then **removed**, not
   discouraged: reaching a human identity afterwards requires escaping
   the allowlist, not being handed it.
