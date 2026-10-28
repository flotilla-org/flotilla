---
name: flotilla-dispatch
description: Dispatch work to Flotilla crews with `flotilla convoy start`, then follow it to merge. Use when handing an issue or task to a convoy, as a governor, operator session or outside agent.
---

# Dispatching convoys

A convoy is a crew working one contract in its own vessel and checkout. It runs
autonomously: it implements, opens a PR, shepherds it, and submits a decision
ledger. `flotilla convoy start` works from any machine whose daemon is in the
fleet, and placement chooses the host. `--help` on any verb lists its flags;
this skill covers the choices `--help` leaves to you.

Replace uppercase example values with real ones and shell-quote values
containing spaces.

## 1. Make the issue a contract

The crew reads the issue **body**; comments, chat and your own context never
reach it. Before dispatching, check that the body states:

- the problem, and the contract the change must meet;
- the tests that prove it;
- **crew acceptance** (tests, gates, PR) separately from **operator acceptance**
  (live host checks and measurements, written as "Operator, after the roll:
  …"). A crew vessel has no Docker, no SSH to fleet hosts and no running daemon,
  so everything needing those belongs to the operator.

Rewrite the body until it alone is dispatchable. To see the issues a project
already judges ready:

```sh
flotilla dispatch ready --project PROJECT
```

## 2. Start the convoy

```sh
flotilla convoy start --project PROJECT --issue NUMBER --name NAME --agent ADAPTER:MODEL --instruction INSTRUCTION --no-attach
```

- **Project**: `flotilla project list` shows registered projects with their
  repositories and issue sources. When a project has several issue sources,
  add `--issue-service https://github.com --issue-scope OWNER/REPO` to name the
  one holding the issue.
- **Name**: three to five words saying what the convoy does, ending with the
  issue number, for example `capacity-stall-flap-4038`. It is the convoy's
  address on every surface.
- **Model**: name the model on every dispatch, as `--agent
  [capability=]adapter:model`; the bare form applies to the `code` capability.
  A dispatch naming no model gets the harness's own default, and an adapter
  override does not inherit the model the seeded capability table pairs with
  it ([dispatch-time agent selection](https://github.com/flotilla-org/flotilla/blob/main/docs/configuration.md#dispatch-time-agent-selection)).
  Repeat the flag per capability, for example `--agent review=codex:gpt-6.1-sol`.
  `flotilla fulfilment list` shows the harnesses and models each kind offers;
  check it before dispatching, because the model names below go stale.
  - Ordinary code: `claude-code:claude-sonnet-5-5` or `codex:gpt-6.1-sol`.
  - Substrate, design-heavy or concurrency-sensitive code: `claude-code:claude-opus-5-5`.
  - Spec and design-judgement work: Opus or Fable.

  This is also the budget lever: when one subscription's window is spent,
  route work to a harness with headroom. Admission checks the chosen harness
  against each host and names any host that lacks it.
- **Placement**: admission picks the least-privileged host with a free slot.
  When every slot is full the convoy waits in a capacity stall. `flotilla
  fulfilment list` shows each kind's harnesses, toolchains and free slots. When
  the work needs something the hosted boxes lack (Codex, Docker, Node), pin a
  host that has it: `--fulfilment docker-crew-image-feta --escalation-reason
  REASON`. Admission refuses a pin above the least-privileged alternatives,
  or onto capacity reserved for platform work, unless it carries
  `--escalation-reason`; the flag is refused without `--fulfilment`. Add
  `--need CAPABILITY` for a dispatch-specific constraint the issue doesn't
  capture.
- **Instruction**: free text appended to the crew's brief, for
  dispatch-specific direction. A good default: "You choose test seams and
  design details yourself and record them in the decision ledger."
- **Deliverables**: several PRs from one convoy are named promises, for example
  `--promise pr:api-split --promise pr:ui-split`. The workflow's deliverer
  owns each one unless you add `@vessel/role`, and the convoy cannot land
  until every promise is kept or retracted.

The command prints the chosen fulfilment and the alternatives it considered.

## 3. Follow it

```sh
flotilla convoy list
flotilla convoy explain NAME@PROJECT
flotilla attach NAME --watch
flotilla convoy resume NAME@PROJECT --prompt PROMPT
```

`convoy explain` says why the convoy holds where it is. `attach` opens the
crew's live terminal; drop `--watch` to take the controller seat.

Review the PR as normal, then send the crew a `convoy resume` prompt pointing
at the review: a review on an unchanged head leaves the crew idle until
something wakes it. When a crew that has finished needs to rebase or follow up,
add `--reopen`.

## 4. Settle

After the PR merges, tear the convoy down. The gate checks that the checkout is
clean, pushed and landed, and names anything outstanding:

```sh
flotilla convoy delete NAME@PROJECT
```

To stop work that should not land, abandon it. It pushes what it can, tears the
convoy down, and leaves the PR and branch on the forge:

```sh
flotilla convoy abandon NAME@PROJECT --reason REASON
```
