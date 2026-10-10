# Stalls

Report a blocker with its concrete evidence, the work still achievable and the
input needed to resume. Choose `infra` for credentials, networking, disk, daemon
or CI infrastructure; `access` for permissions; `scope` for a contradictory or
unsolvable brief; `decision` for a required unresolved choice; `other` otherwise.

```sh
flotilla crew stall --reason infra --message EVIDENCE
flotilla crew stall --reason scope --message ACHIEVABLE_SCOPE --propose reduce-scope
```

Use `--propose resume`, `--propose reduce-scope` or `--propose fail` to recommend
supervisor action. Leave disposition to the supervisor. Read [stall rationale](#stall-rationale)
when deciding whether a blocker warrants a stall.

When named as supervisor, inspect the evidence and guide the source obligation:

```sh
flotilla crew stalls --full
flotilla crew supervise --convoy CONVOY --vessel VESSEL --role ROLE resume --message GUIDANCE
flotilla crew supervise --convoy CONVOY --vessel VESSEL --role ROLE convert-to-failed --message REASON
flotilla crew supervise --convoy CONVOY --vessel VESSEL --role ROLE escalate --message EVIDENCE
```

## Stall rationale

A stall keeps the work wanted while recording why progress requires outside
input. Use it for a real blocker. Yield at the turn boundary while PR checks are
pending; complete once the assignment is settled. Pending merge or landing after
accepted completion is a convoy condition, not a new crew blocker.
