# Crew lifecycle

Finish the assignment's checks and review handling, then submit a decision
ledger from a file the daemon can read. Keep it outside the repository. Head it
`## Decision ledger`; include numbered decisions with **Brief silence:**,
**Choice:**, **Alternative:** and **If asking were free:** fields, least-confident
first. Include at least one decision.

```sh
flotilla artifact put --kind decision-ledger /tmp/decision-ledger.md
flotilla crew complete --message RESULT
```

Run completion as the final act after the ledger is accepted. Report the PR URL
for a repository assignment. Let the convoy wait for merge and landing after
completion. Supply `--disposition` only when the brief declares a settlement
answer. Use [settlement order](pr-shepherding.md#settlement-order) for the rationale.

As the named supervisor, resume the exact stalled obligation with guidance:

```sh
flotilla crew supervise --convoy CONVOY --vessel VESSEL --role ROLE resume --message GUIDANCE
```

For an authorized follow-up to an existing convoy session, deliver a new brief:

```sh
flotilla convoy resume CONVOY --vessel VESSEL --role ROLE --prompt BRIEF
```

To hand work to a reachable crew, use the addressing procedure in [messages](messages.md).
