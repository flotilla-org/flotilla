# Settlement operations

Run `flotilla crew stalls` from an operator CLI to list stalled crew obligations
across the fleet's stored namespaces, including replicated convoys. The command
needs no crew identity or repository context. Each row includes project and
convoy display names, vessel, role, escalation rung, recorded supervisor or
absence evidence, age, proposed disposition, and evidence.

`flotilla crew stalls --full` includes complete evidence and convoy artifact
references. `flotilla crew stalls --json` preserves complete evidence and
structured identities, timestamps, cause groups, and artifact references.
Matching recorded reason, structured cause, and whitespace-normalized evidence
flag possible shared causes; the view does not infer causes from keywords.

A later declaration can replace a convoy's visible stall condition while other
crew remain stalled. Those obligations are still listed, with unknown rung and
age where the store no longer has per-obligation metadata. Terminal convoys are
excluded. The view reflects the replicated state available to the connected
daemon; a disconnected host's new stalls appear after replication resumes.

An operator connected to the daemon can read evidence with
`flotilla artifact get artifact/<namespace>/<name> --output <path>`. The CLI resolves the
output to an absolute path in the operator's local environment. Unqualified
`artifact/<name>` references use the daemon's provisioning namespace. Crew reads
continue to use their calling session's environment.

Decision ledgers belong to the convoy and producer role. Putting a ledger
projects its complete contents onto every distinct bound change request, across
repositories and supported forges. A digest marker makes each projection
retryable: after a partial failure, retrying finds existing comments before
posting the missing ones. The artifact records the number of projected PRs in
`summary.projection_count` and retains `summary.comment_url` as the first URL
for existing readers. This keeps the summary within its fixed scalar size
budget regardless of the number of projections. A put checks current bindings even when the ledger digest has
not changed, so newly bound change requests receive it too. Completion checks
the stored convoy artifact, independently of the number of bound PRs.
