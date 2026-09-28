# 47. Stored data stays decodable across one generation; wire formats still change freely

Date: 2026-09-28

## Status

Accepted

Decided 2026-09-28 after the #2168 near-miss. Replaces the blanket "no backwards compatibility" statement in CLAUDE.md with a narrower rule.

## Context

CLAUDE.md declared a **no backwards compatibility** phase: protocol types, snapshot formats, config formats and wire formats change freely, with no migration logic or deprecation paths. That fitted a fleet whose state was disposable, and it still fits the wire:
- generations roll as a unit;
- the protocol fingerprint handshake refuses mismatched peers;
- so version skew between peers never has to be handled.

It no longer fits **stored data**. The fleet now keeps durable state that outlives a generation, much of it written by the system itself rather than by an operator:
- convoy statuses with frozen workflow snapshots;
- turn-delivery history and superseded claims;
- standing governor ensures and their templates;
- placement snapshots;
- manifests in project-map.

In practice, "no backwards compatibility" meant each schema change was a trap at roll time:
- **r302:** a credential consumer shape changed without its project-map manifest, and was quarantined on all hosts (#2057).
- **r326:** the credential-grant selector change needed a hand-choreographed delete, install, merge and SQL-clear.
- **2026-09-28:**
  - #2161 removed `VesselRequirement.stance` while adding `deny_unknown_fields`;
  - #2135 renamed `head_sha` to `subject_revision` without an alias;
  - between them, every convoy status and stored template on the fleet became undecodable.

  Only a manual dump-and-decode before the roll caught it (#2168). The old build *required* `stance` and the new one *refused* it, so the data couldn't have been cleaned up in advance.

The policy wasn't saving work. It moved the work to roll time, where it was manual and risky and caught by luck, and it read to crews as permission to break stored shapes.

## Decision

### 1. Wire and protocol formats still change freely

- No compatibility is required between different generations' protocol types, socket messages, peer envelopes or replication wire formats.
- The fleet rolls as a unit, and the fingerprint handshake keeps mismatched peers apart.
- There are no multi-version peers, no protocol negotiation and no deprecation windows.

### 2. Stored data is N→N+1 compatible

Generation N+1 must decode everything generation N stored:
- resource specs **and statuses**, including embedded structures such as a convoy's workflow snapshot;
- manifests authored outside the repository (project-map, ops repositories);
- daemon config files.

It writes only the new shape. Tolerance for an old shape may be removed **one roll later**, once the corpus (§4) has been refreshed from the new generation, meaning records have been rewritten or reaped.

The tools are local and cheap:
- serde `alias` for renames;
- `default` for new fields;
- a deserialize-only record type that still refuses unknown fields but accepts and drops a retired one (`VesselRequirement`'s `stance`, #2168).

There is still **no migration framework**, and no versioned-record machinery.

### 3. Out-of-repo authors move with the change

A change to a shape that project-map or an ops repository authors either stays N→N+1 compatible (§2) or ships the manifest update in the same roll (#2057). The pre-roll manifest gate (#2155) checks the manifests.

### 4. Enforced mechanically, not by memory

- **At PR time:** a golden corpus of real stored records from the current generation is decoded by `cargo test` (#2169). A shape change that breaks stored data fails on the PR that makes it, naming the field. Authors fix the decoder, never the corpus. The corpus is refreshed only after each roll.
- **At roll time:** before install, the candidate binary decodes each host's live store, specs and statuses (#2167). Any failure aborts the roll.

## Consequences

- Types still reshape freely. What changes is one alias, default or record type per breaking change, carried for one roll.
- A class of roll failure (fleet-wide decode quarantine) is caught at PR time instead of on live hosts.
- Crews get an unambiguous rule: "wire free, storage N→N+1".
- **Cost:** the corpus must be refreshed after each roll, and a retired-field shim must be deleted one roll after it is added. Legacy shims should carry a comment naming the roll after which they can go.
- **Supersedes** the Development Phase paragraph of CLAUDE.md, which is rewritten to match.
