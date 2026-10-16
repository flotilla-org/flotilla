# Issue 3392: owner spec checkpoint

`quint.patch` is the proposed machine-rule diff for an owner ruling under
issue 3392. It is intentionally **not applied** to the active specs. No
conformance or replay code depends on it. The rest of issue 3392 remains to
be implemented; this checkpoint is not a completed implementation.

The proposal:

- Message removes Dropped's redundant `at`, retains the expiry watermark,
  and carries the receiver into Settled (zero when settled from Pending).
  Deliver and Undeliverable evidence refers to versioned Receive entries
  about the message, rather than a separate receipt lookup.
- AgentSession adds `Receive { message, transport }`, a Live-to-same-Live
  edge. `message` is its master and becomes the log entry's indexed `about`.
  Ended refuses Receive as it refuses all other edges.
- Ensure replaces Fail with Attempted carrying Succeeded or
  Failed { retryable }, and GiveUp with Hold. Hold requires two consecutive
  failures since the last success in the current window, or a final
  nonretryable failure. Authorised Resume advances the window; Held has no
  automatic exit. Backoff stays outside the machine.

The attempt history has no production length limit. After the ruling, the
finite exploration harness must bound generated attempt edges, rather than
inventing a machine refusal when history fills. Scenario actions, graph
conformance adapters, replay, and mutant regeneration follow the approved
rules; none have been changed at this checkpoint.

## Owner ruling requested

Please rule on the proposed machine diff before conformance is changed.
The independent second checkpoint will present the stored types, every
generated table's column count and enum expansions, child tables, and the
DDL snapshot diff. No code reading or writing reshaped tables will precede
that schema review.

## Validation

The patch applies to the current machine sources with `git apply --check`.
The proposal has not been typechecked: this vessel has neither Quint nor
npm installed. Active specs and runtime code are unchanged.
