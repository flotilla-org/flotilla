# Crew lifecycle

Finish the assignment's checks and review handling, then submit a decision
ledger from a file the daemon can read. Keep it outside the repository. Head it
`## Decision ledger`; include numbered decisions with **Brief silence:**,
**Choice:**, **Alternative:** and **If asking were free:** fields, least-confident
first. Include at least one decision.

```sh
flotilla crew submit --kind pr --reference PR_URL
flotilla artifact put --kind decision-ledger /tmp/decision-ledger.md
flotilla convoy CONVOY explain
```

Submit PR references with `crew submit`; PR promises remain submitted until
merge. Continue remaining promises after each merge. Your part ends when every
promise is kept or retracted. A role holding no promises is never automatically
Done. `crew complete` is advisory: it discovers PR URLs and reports pending
promises, and `--force` remains the operator override.
Use [settlement order](pr-shepherding.md#settlement-order) for the rationale.

As the named supervisor, resume the exact stalled obligation with guidance:

```sh
flotilla crew supervise --convoy CONVOY --vessel VESSEL --role ROLE resume --message GUIDANCE
```

For an authorized follow-up to an existing convoy session, deliver a new brief:

```sh
flotilla convoy resume CONVOY --vessel VESSEL --role ROLE --prompt BRIEF
```

To hand work to a reachable crew, use the addressing procedure in [messages](messages.md).
