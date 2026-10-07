# Decision ledger

Record every decision where the brief was silent, least-confident first. Keep at least one numbered decision with exactly the four nonempty fields below. Follow the crew brief for the final shepherd snapshot, artifact submission and completion order.

```markdown
## Decision ledger

1. **Brief silence:** Where the brief was silent.
- **Choice:** What you chose.
- **Alternative:** What you considered instead.
- **If asking were free:** What you would have asked.

### Friction

- A misleading tool, missing command, slow or flaky check, or brief silence or conflict encountered while working.
```

The trailing `### Friction` section is optional; existing ledgers without it remain valid. Omit it when there is nothing to report. Use free-form prose or bullets to describe the symptom, its effect on the work, and any workaround. Keep friction outside the numbered decisions: it informs later retrospectives and session-log analysis without changing the required decision fields. The whole artifact remains subject to the 32 KiB UTF-8 size limit.

Ledgers settle as artifacts. Submission validates and stores the body without writing to a forge. The presentation catalog publishes the latest ledger artifact reference per convoy; fetch it with `flotilla artifact get artifact/<name> --output /tmp/ledger.md`. A PR description may link to its stable artifact view URL. Previous projected comments remain historical evidence.
