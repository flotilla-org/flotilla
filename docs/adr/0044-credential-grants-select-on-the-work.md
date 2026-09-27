# 44. Credential grants select on the work; delivery follows fulfilment

Date: 2026-09-27

## Status

Accepted

Grilled 2026-09-27 on #2026 (rulings recorded there). Amends ADR 0022.

## Context

ADR 0022 made grants **stance-first**, and by "stance" it meant *repository trust*: fork-stance crews get model-API credentials only, and trusted-repo crews add the crew forge identity. Its migration clause also let trusted crews inherit **ambient** human identity. Two things have happened since:

- **The selector drifted to vessel isolation.** Live grants now select on `stance: contained` / `stance: trusted`, which describes *how a vessel is fulfilled* (in a container vs host-direct), not what the work is. So moving a crew from a container to host-direct silently changes what it may do. On 2026-09-27 a host-direct crew on kiwi got no GitHub credential at all, because every GitHub grant was `stance: contained` (#2086). The stopgap was a per-project `github-trusted-pr` grant.
- **Ambient fallback is gone.** #2033 made crews refuse to act without their own authenticated credential, so the migration clause no longer describes the system. Host-direct crews stopped acting as the human as a side effect, and nobody decided it (#2024).

One incident also showed that **every crew gets the same minted GitHub token**:
- a governor's token could create an issue but not comment on or edit one;
- coders can't re-run failed Actions jobs (#2048);
- nothing narrows a token by role.

## Decision

### 1. Grants select on the work

A grant's selector names the **work**:
- project,
- repositories,
- **role** (coder, reviewer, governor, …),
- **repository trust** (own vs fork, from the Repository's declared upstream relation; ADR 0022's original intent and #978).

**Vessel isolation is not a selector.** A crew doing the same work receives the same credentials whether it runs contained or host-direct.

Policy stays declarative. Richer rules later are expressed as grant data, never as hard-coded exceptions.

### 2. Delivery follows fulfilment

Isolation decides only **how** granted material reaches the crew:
- a contained crew gets it staged into its credential directory inside the container;
- a host-direct crew gets it through the same credential staging and the launch environment allowlist.

The adapter path is the same; only the transport differs. A workflow may still *require* containment, but isolation never widens or empties what a crew is entitled to.

### 3. Grants declare what a minted token may do

For minted credentials (the GitHub App installation token today), a grant declares the **permission set** to mint. Different roles hold different grants, e.g.:
- coder: contents, pull requests, actions;
- governor: issues and comments, plus pull requests on its ops repo;
- reviewer: read.

When several grants match one crew, their permission sets **union** per credential, capped by the permissions the credential declaration (and the App installation itself) allows. Grants can narrow what the App can do, never exceed it.

Token permissions cannot prevent a merge: contents plus pull-request write can merge. "Crews don't merge their own work" (#954) therefore stays a settlement-time check (`mergedBy` is not the crew identity), not a permission.

### 4. No scarce-material abstraction

Credential material is delivered, never pooled or leased. Subscription limits (a weekly model budget, provider rate limits) are a **dispatch** concern: route work to where there's headroom (#1928, #1394). Deleting `MaterialPool` (#1931) was right.

### 5. One delivery and refresh contract for every kind

Material source → the daemon prepares it → staged into the crew's credential directory → the adapter wires env and helpers. The same contract covers codex, the GitHub App, Claude, Forgejo, and skill-source credentials (ADR 0038).
- **Static material** is a read-only **copy, never a bind mount** (rotation replaces the file, and a mount would pin the old one), redelivered on resync.
- **Minted material** is refreshed **daemon-side** and replaced atomically, so a failed refresh never removes still-valid material (#2036).
- A refresh that keeps failing **surfaces**: a warning on the driver host and attention the crew and operator can see.
- A crew **never degrades to anonymous** access (#2033).

Store credentials never reach crews at all (ADR 0042).

### 6. Acting as the human principal is explicit

A crew acts under the human's identity only through a grant that says so, never as a fallback from missing material or from where it happens to run. What that grant looks like, and how acting-as is attributed, belongs to the identity model (#2024).

## Consequences

- Moving work between contained and host-direct placement no longer changes what it may do. Per-project patches like `github-trusted-pr` retire.
- Least privilege becomes data: each role's token carries only what that role's grants declare.
- Changing the grant schema breaks grant manifests authored outside the repo (project-map). They must be migrated in the same roll (#2057).

## Amendment to ADR 0022

- "Stance first" is replaced by "work first" (§1). The selector keys are project, repositories, role and repository trust; vessel isolation moves to delivery (§2).
- The migration clause letting trusted crews inherit ambient identity is superseded (§5, §6).
