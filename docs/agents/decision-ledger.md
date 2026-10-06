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

For manual recovery, an operator must:

1. Establish that the original writer and its forge request have finished, and resolve whether the request was accepted. Stopping the local writer alone does not prove that an in-flight forge request has finished. If the outcome remains unknown, do not create another comment.
2. Inspect `flotilla resource get Artifact <name> --namespace <namespace> --host <artifact-home> --json`. Read `spec.summary.projection_error` for the diagnostic, and retain `metadata.name` and `spec.digest` for the marker. `flotilla convoy explain` also renders a missing projection and its error. Fetch the exact ledger body with `flotilla artifact get artifact/<name> --output /tmp/ledger-body.md`.
3. List all comments on the bound change request and look for a final-line marker beginning `<!-- flotilla-decision-ledger:<name>:`. If a matching comment exists, have the original crew retry its `flotilla artifact put --kind decision-ledger <path>`; lookup will recover its identity.
4. Only when the original request is definitively known not to have left a comment, supply the exact ledger body as a comment with a final line `<!-- flotilla-decision-ledger:<name>:<digest> -->`. For GitHub, use the repository-scoped credential and `gh pr comment <number> -R <owner/repo> --body-file /tmp/ledger-comment.md`; for Forgejo, use its authenticated issue-comment API. Build that body file from the fetched ledger, an added newline, and the marker, substituting the resource's actual name and digest. Have the original crew retry artifact put under the same convoy and producer role; it will find the supplied marker and retain that comment's identity. Keep the authority reservation intact throughout.

An operator-authorized reconciliation command and typed definitive-refusal handling are tracked in [#2740](https://github.com/flotilla-org/flotilla/issues/2740).
