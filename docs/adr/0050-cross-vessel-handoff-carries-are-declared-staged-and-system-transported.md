# 50. Cross-vessel handoff: carries are declared, staged before the turn, and transported by the system

Date: 2026-09-29

## Status

Accepted; amended by [ADR 0053](0055-agent-messages-are-durable-receiver-homed-records.md), which represents cross-vessel carries as typed Message references. Reference visibility gating follows in #2711.

Grilled 2026-09-29, prompted by automatic multi-vessel allocation (ADR 0046 §3, #2143/#2188). Refines ADR 0042/0043's "cross-vessel handoffs go through artifacts". Relates to ADR 0045 (stalls), ADR 0049 (subject identity), #2111 and #2164 (staging before delivery), and #2165 (dehydration).

## Context

Within a vessel, a handoff between crew members is free: they share a filesystem, a checkout, and whatever temporary state the last role left behind. Once admission allocates roles into **separate vessels** (ADR 0046 B), nothing is responsible for moving that ambient state:
- repository changes, both committed and uncommitted;
- generated files and build outputs;
- context such as briefs and ledgers.

Agents pushing and fetching for themselves is fragile, and it is exactly the drudgery flotilla exists to remove.

## Decision

### 1. What moves is declared as intent; how it moves is the system's job

- **A workflow declares default carries per handoff edge,** as kinds, not mechanisms:
  - **repository state** carries on every edge by default;
  - named artifact kinds (build outputs, review rounds);
  - context (brief, ledger).
- **The sending agent may add carries** at handoff time (`flotilla crew handoff --carry <path|artifact>`) for anything unforeseen: scratch files, generated fixtures, logs. It says what, never how.
- **The system chooses transport per item and topology** (§3).
- **Within one vessel every carry is a no-op.**

### 2. Staged before the turn; a missing carry is a stall

The receiving crew's turn is not deliverable until every carried item is present and verified (by digest or ref). This is the same guarantee #2111 gives credentials. A carry that can't be staged is a stall (ADR 0045), never a silent partial handoff. Each handoff is recorded as a **handoff manifest** artifact (items, sources, digests), which makes it inspectable and replayable, and it is what rehydration (#2165) uses.

### 3. Repository-state carries

- **Scope** is exactly what `git status` sees: commits, staged and unstaged changes, and untracked files that aren't ignored. Ignored files (`target/`, `node_modules/`) never carry; an artifact carry covers any that matter.
- **History stays clean.** Commits carry as commits. Uncommitted work travels as a `git stash create`-style object on a handoff ref, and the receiver checks out the sender's HEAD and **re-applies** the work as uncommitted changes. No work-in-progress snapshot commit ever reaches a PR branch.
- **One carry per checkout** in multi-repository vessels, keyed by repository (in ADR 0049's subject identity).
- **The chain is linear per checkout.** Only the active role mutates.
  - A receiver's own un-carried local changes are snapshotted before applying; nothing is discarded.
  - Concurrent changes to the same repository in two vessels are a **conflict**, which raises a stall to the supervision ladder. The system never auto-merges code.
- **Roles declared non-writing** (for example platform verifiers) return artifacts. If such a role has repository changes, they are flagged, not carried.
- **Credentials and vessel configuration never carry.** They are re-staged per vessel (#2164).

### 4. Where handoff refs live, and for how long

- **The software defines a configurable handoff store; it hard-codes no location.** A handoff store is any daemon-reachable git remote. It is configured per installation, as a declared resource or daemon config, with a URL template per repository and a daemon credential. Candidate backends:
  - a private repository on any forge;
  - a bare repository over ssh;
  - daemon-served bare repositories.

  Refs go under `refs/flotilla/convoy/<id>/handoff/<n>`, authenticated with the **daemon's** credentials, never a crew's. The recommended configuration is a private, fleet-internal store: fast, not public, and it works for forks we can't push to.
- **This fleet's choice** (a deployment preference, not a software requirement) is lab Forgejo `handoff/<repo>` repositories.
- **On the same host,** transfer is point-to-point: the receiver fetches from the sender's checkout.
- **The forge** is used only when a workflow explicitly asks.
- **Allocation counts cross-host handoff cost** (ADR 0046 §3).
- **Retention:** handoff refs are deleted when the convoy lands or is reaped. Manifests persist under their artifact kind's retention.

## Consequences

- Multi-vessel workflows get the same "the next role finds what it needs" behaviour a single vessel gives for free, without agents doing plumbing.
- Handoffs become inspectable records instead of implicit filesystem state.
- **Cost:**
  - a handoff manifest kind;
  - carry declarations in workflow templates;
  - a `crew handoff --carry` extension;
  - staging integrated with turn delivery;
  - a handoff-store configuration surface. Each installation provisions its own store; for this fleet, lab Forgejo repositories.
