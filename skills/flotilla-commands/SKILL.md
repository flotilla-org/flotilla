---
name: flotilla-commands
description: Operate Flotilla crew lifecycle, messages, stalls, PR settlement and convoy inspection as a crew member or governor.
---

# Flotilla commands

Use the calling crew's identity and repository charter. Run crew and convoy
operations from the top-level crew session. Follow the assignment's delivery
contract and use its named forge and target branch.

Start orientation with:

```sh
flotilla crew list
flotilla crew capabilities
flotilla message contacts
```

Replace uppercase example values with the actual identity, address, message or
path; shell-quote values containing spaces. Read only the area needed now:

- To submit a ledger, complete work, resume or hand off, read [crew lifecycle](references/crew-lifecycle.md).
- To address another crew or carry evidence, read [messages](references/messages.md).
- To report a blocker or supervise stalled work, read [stalls](references/stalls.md).
- To settle PR checks, reviews and claims, read [PR shepherding](references/pr-shepherding.md).
- To inspect convoy state or fleet stalls, read [convoy inspection](references/convoy-inspection.md).

## Promises

Declare separately checkable deliverables before starting multi-part work:

```sh
flotilla crew promise --promise implementation --kind pr
flotilla crew submit --promise implementation --kind pr --reference PR_URL
flotilla convoy CONVOY explain
```

Attach submission evidence with optional JSON metadata, for example
`--metadata '{"commit":"HEAD_SHA"}'`.

Supported kinds are `pr` (kept when merged) and `decision-ledger` (kept when its
artifact exists). Submission without a matching promise creates a crew-sourced
promise; discovered produced PRs participate too. A closed, unmerged PR rejects
its submission, retains the verdict in history, and reopens the promise. Submit
the next attempt against the same promise.

When several PR promises are open, branch discovery creates a separate
crew-sourced promise instead of guessing which declaration it fulfills. Use
explicit `--promise` submissions to bind that PR to the intended declarations;
all promises for the merged PR become kept.

Retract your own crew-sourced promise with
`flotilla crew retract --promise IDENTIFIER --reason REASON`. Retraction of a
workflow- or dispatch-sourced promise proposes retraction through a stall for
its supplier to decide. Inspect `convoy list` counts and `convoy explain` history.

`crew complete` is a separate settlement claim and needs no deliverable
reference. It refuses while any of your promises is open or submitted. After a
PR merges, continue the remaining promises; submit the ledger and complete only
when every promise is kept or retracted.
