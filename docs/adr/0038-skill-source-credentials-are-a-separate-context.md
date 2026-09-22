# 38. Skill-source credentials are a separate credential context from the crew git credential

Date: 2026-09-18

## Status

Accepted

## Context

A contained crew needs two distinct kinds of Git access: **write** to its project
repositories (commits, branches, PRs over the whole session) and **read** of the
generation-pinned skill sources at container provisioning. Until now these shared a
single GitHub App token: the private skill fork (`mattpocock-skills`) was unioned into
the crew's project git credential via `AgentMaterialDelivery.github_repository_grants`,
so one token was minted covering *project repos ∪ the skill fork*.

A GitHub App installation is per-account, and an installation token can only be scoped
to repositories within that one installation. The moment a project's repositories and
the skill fork live under different owners — e.g. `rjwittams/katzensteg` (project) and
`flotilla-org/mattpocock-skills` (fork) — the single token cannot cover both, and the
mint fails with HTTP 422. This blocked every contained crew whose project repositories
mint on an installation other than the fork's. The conflation also over-scoped the
token (write-capable, aimed at a repo staging only needs to read) and forced a
hardcoded name→URL invariant to keep the one privileged token from being aimed
elsewhere.

## Decision

Treat the two as separate credential contexts.

- The **project git credential** mints scoped to project repositories only. No skill
  source is ever unioned into it.
- Each **skill source** may name a `CredentialSpec` directly in the generation manifest
  (supply-side; not through project/stance CredentialGrants, which are a demand-side
  crew↔project binding). Staging mints that spec **narrowed to the source's own
  repository** and fetches that source with it; sources without a credential fetch
  anonymously.
- The **runtime** owns minting (it holds the credential store); the skills adapter
  stays a pure consumer of pre-prepared per-source token files.
- Skill-source tokens are **ephemeral one-shot** mints, used for one shallow fetch at
  provisioning and discarded; they never enter the held-credential/refresh path.
- Per-source narrowing confines each credential to one repository by construction, and
  the manifest is generation-pinned (fleet-built, not runtime-supplied), so the
  `PRIVATE_SKILL_REPOSITORY` name→URL hardcode retires with no credential↔source
  allowlist to replace it.

## Consequences

- Projects whose repositories live on any installation stage the private fork correctly;
  the cross-installation 422 is gone, and public sources consume no token at all.
- Skill-source access is least-privilege (read-only, one repo, short-lived) and cannot
  go stale mid-session because there is no mid-session for it.
- The manifest gains schema v5 with an optional per-source `credential`. Supply-side
  credential references are a distinct mechanism from CredentialGrants; readers must not
  conflate the two.
- A referenced skill-source credential must exist in each host's resource store before a
  generation referencing it deploys — a provider-before-config rollout ordering.
- Demand-side per-crew skill requirements (#1790) remain unaddressed and orthogonal.
