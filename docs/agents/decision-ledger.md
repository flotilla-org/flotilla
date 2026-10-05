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

Decision-ledger comment creation is reserved durably at the artifact's home, per change request. Concurrent callers on any host receive at most one permission to POST; retries list comments by the artifact marker, and revisions PATCH the existing identity. Artifact spec revisions retain the reservation.

A reservation does not expire. If the daemon loses the POST response, lookup recovers an accepted comment. If no matching comment is visible, the artifact records a pending or uncertain projection rather than issuing another POST: a request might still be in flight. This also covers a daemon stopping after reservation but before sending. Reconcile that attempt with the forge before manually supplying a comment; do not delete the artifact or its reservation to retry creation. The ledger body remains stored even when its comment projection is uncertain.
