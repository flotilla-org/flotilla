# 49. Convoys relate to a typed set of source-scoped subjects, referenced in three forms

Date: 2026-09-28

## Status

Accepted

Grilled 2026-09-28, taking over the scope of #2157 (brainstorm). Relates to ADR 0027–0029 (leaf addresses), ADR 0041 (relay subjects), ADR 0043 (declared completion conditions), ADR 0045 (stalls), and #1955 and #2138 (forge-relative identity).

## Context

A convoy relates to several **source-scoped external subjects**: issues it works on, change requests it produces or adopts, and potentially review threads, CI runs and releases. Each is identified as `(service, scope, id)`. Today each relationship is modelled separately, and the discovered side is never persisted. A code trace (#2157) found four independent derivations of a convoy's PR:
- the `--pr` binding;
- the aggregator's branch enrichment, which is the only source of the published row;
- the claim-message parse, which feeds only landing evidence;
- a checkout-level `gh pr list`.

So a crew publishing from a branch other than `spec.ref` never shows a PR in the row, the row and settlement can name different PRs, and every consumer re-derives the answer. A convoy can also span several repositories, which the single `change_request` field can't express. Even when the model is right, references must be concise and unambiguous for people, agents and tools alike.

## Decision

### 1. The subject set

- **Subject:** `{ kind: change_request | issue | …, source: { service, scope }, id }`. This is the identity the leaf engine and relay already use, and it is forge-relative (#1955, #2138).
- **Relationship:** one of `works_on` (typically an issue), `produces` (a change request the crew opened), `adopts` (bound with `--pr`), `supersedes` (a replacement change request) and `references` (a mention).
- **Declared subjects live in convoy spec.** They are set at admission (`--issue`, `--pr`, governor dispatch) and are the convoy's intent. Discovery never overwrites them.
- **Discovered subjects live in convoy status.** Each is written once and then kept, recording every **source of discovery** (branch lookup, claim or ledger, relay hint, operator verb) and when it happened.
- **Observed state stays on the subject's own record** (`ChangeRequest`, `Issue`). The convoy links to it and never copies it.
- The set is multi-repository by construction.

### 2. Three reference forms

| Form | Example | Used for |
| --- | --- | --- |
| **Internal canonical** (the leaf address) | `cr/github.com/flotilla-org/cleat/281`, `cr/lab/robert/project-map/12` | storage, leaves, JSON and machine interfaces inside flotilla |
| **External canonical** (the forge web URL) | `https://github.com/flotilla-org/cleat/pull/281` | **copy, links and hand-offs**; anything outside flotilla |
| **Short** | `cleat!281`, `flotilla#2157` | what people and agents type and read |

- In the short form, **`#` means an issue and `!` means a change request** (GitLab's convention). The kind stays explicit even where issues and change requests share a number space.
  - `<repo>` is the Project's repository alias, resolved in the viewer's project context.
  - It is qualified only as far as needed: `wheelhouse/cleat!281` across projects, `lab:robert/project-map!12` for a non-default forge.
- **Parsing accepts all three forms,** plus `owner/repo#n`.
- **Rendering picks the shortest form that is unambiguous in context.** Every rendered form parses back to the same subject.
- **Surfaces may compact further for display** (`!281`, or `c!281` when unique in the view). Anything copyable, hovered or clicked yields the external canonical form, or the short form when a subject has no forge URL. Compaction never escapes the view as an ambiguous reference.
- Relationships render next to references where it helps: `works on flotilla#2157 · produces cleat!281, andamento!40`.

### 3. Discovery

- **Sources that may add discovered subjects:**
  - a branch lookup (a change request whose head is a convoy checkout's branch) → `produces`;
  - a claim message or ledger naming a change request → `produces`;
  - relay hints (ADR 0041) for watched or branch-matching subjects;
  - `flotilla convoy link <convoy> <ref> --as <relationship>` and `unlink`.
- Additions are idempotent: the same subject and relationship refreshes its source list.

### 4. Disagreement surfaces

A convoy may produce or adopt **any number of change requests**, in one repository or several. Follow-up PRs, split PRs and multi-repo work are normal, and several `produces` subjects are not a disagreement. When the branch lookup finds `cleat!281` and the claim names `cleat!282`, both are recorded as subjects and both count toward settlement (§5). Nothing is dropped and nothing raises attention merely for being plural. (Amended 2026-10-01: the original text treated two produced change requests as a conflict needing `supersedes` or `unlink`; the owner ruled the one-change-request limit out.)

What remains a disagreement is a **contradiction about a single subject**: for example, a source rediscovers a subject an operator explicitly unlinked (the unlink wins and suppresses it), or two sources assert incompatible relationships for the same change request. Those are recorded and resolved by verb, and nothing wins silently.

`supersedes` keeps its meaning: one change request **replaces** another, so the replaced one no longer has to reach a terminal state for the convoy to land.

### 5. Settlement reads the set, per repository

- By default, every `produces` or `adopts` change request must reach a world terminal (merged or closed) for the convoy to land.
- A `supersedes` link releases the superseded change request.
- `works_on` issues are not required to close. That is the workflow's choice, expressed through declared conditions (ADR 0043).
- This replaces the union of bound change request and checkout integration in `expected_change_request_leaves`. The published row, `convoy explain`, and wheelhouse all read the same set.

## Consequences

- There is one persisted answer to "which PRs and issues is this convoy about", across repositories, and every consumer reads it.
- References work for people (short), for tools and outsiders (URLs), and for flotilla internals (leaf addresses), and each converts to the others.
- Mismatched-branch PRs appear. Conflicting evidence is visible rather than silently resolved.
- **Cost:**
  - convoy spec and status shapes change, carrying ADR 0047 compatibility shims for `change_request` and `issues`;
  - a reference parser and renderer shared by the CLI, TUI and wheelhouse;
  - migrating the four derivations onto the set.
