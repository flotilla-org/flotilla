# Triage Labels

The skills speak in terms of five canonical triage roles. This file maps those roles to the actual label strings used on `flotilla-org/flotilla`.

| Label in mattpocock/skills | Label in our tracker | Meaning                                                                    |
| -------------------------- | -------------------- | -------------------------------------------------------------------------- |
| `needs-triage`             | `needs-triage`       | Maintainer needs to evaluate this issue                                     |
| `needs-info`               | `needs-info`         | Waiting on reporter for more information                                    |
| `ready-for-agent`          | `ready`              | Earned: body alone is a dispatchable contract — an agent can start NOW      |
| `ready-for-human`          | `ready-for-human`    | Requires human implementation                                               |
| `wontfix`                  | `wontfix`            | Will not be actioned                                                        |

When a skill mentions a role (e.g. "apply the AFK-ready triage label"), use the corresponding label string from this table.

## `ready` is earned, not applied

`ready` follows the board's body-is-contract discipline: the issue **body alone** must be a dispatchable contract — an agent with no other context can pick it up and start. After a grill or a split, rewrite the body into that contract *before* touching the label. Never apply `ready` as a courtesy or a guess.

## Orthogonal to types and topic labels

Triage roles say where an issue sits in the triage state machine. They are orthogonal to the issue **type** (`Task`/`Bug`/`Feature`/`Brainstorm`) and to topic labels (`bug`, `ui`, `multi-host`, `quick-win`, …) — see `docs/agents/issue-tracker.md` and the "Issue Types and Labels" section of `CLAUDE.md`.
