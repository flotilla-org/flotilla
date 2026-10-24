# Lifecycle ending: spec and schema review checkpoint

The proposed shared lifecycle retains the reason a resource ended in its state:

```text
Requested -- failed attempt, GiveUp --> GaveUp(cause)
Requested or Realised -- Release --> Gone(Released)
Realised -- Vanish(Lost) --> Gone(Vanished(Lost))
Realised -- Vanish(Failed(reason)) --> Gone(Vanished(Failed(reason)))
```

The Quint edits are the proposal. The Rust lifecycle state, edge arguments, read
judgements and generated schema snapshots are deliberately unchanged until spec
review. The independent binding fix records provisioning failures through the
existing Attempted and GiveUp gates. Placement returns the lease for either
terminal lifecycle state. The Vessel composition now accepts those two events;
it still derives placement occupancy from the lifecycle's liveness.

Quint constructors share a namespace. The attempt outcome is renamed
`AttemptFailed` to leave `Failed(str)` available for the disappearance cause.
This is a spec spelling change; the existing Rust `Outcome::Failed` stays as it
is. Failure before readiness still carries the existing typed `Cause` in Rust.

## Proposed table view

The [DDL patch](lifecycle-endings.ddl.patch) is a hand-proposed diff against both
current generated snapshots, supplied for schema review, not a replacement for
generating them from the approved Rust types. Both proposed SQL files execute in
SQLite. Final generated SQL must be compared with this proposal after review.

`Gone` adds three columns to each lifecycle root, with no child table or index
change. `ending` is `Released` (`rl`) or `Vanished` (`vn`); a vanished ending has
`ending_cause`, `Lost` (`lo`) or `Failed` (`fa`); only Failed has a non-null
`ending_cause_reason`. The existing `cause_category` and `cause_detail` remain
reserved for GaveUp. All ending columns are null outside Gone. Existing state
labels and all six kinds' names, homes and resource codes stay unchanged.

| Table | Columns before → proposed | Enum expansion outside state | Ending expansion | Child tables | Indexes |
|---|---:|---|---|---|---|
| checkout | 21 → 24 | source: 2 | 3 | none | by_environment, by_clone, by_holder |
| clone | 16 → 19 | none | 3 | none | by_environment |
| environment | 16 → 19 | none | 3 | none | live_by_provider, by_image_build, by_holder |
| image_build | 19 → 22 | delivery: 2 | 3 | none | by_composition |
| terminal_session | 23 → 26 | source: 6 | 3 | terminal_session_env: 3 columns | by_environment, by_holder |
| vessel | 16 → 19 | none | 3 | vessel_shape: 3 columns | live_by_host, by_convoy |

No table exceeds 30 columns. Ensure and its child tables are unchanged. No
out-of-repository source authors these engine lifecycle records; their requests
are written by the daemon's bindings and demand writers.

## Review boundary and compatibility question

The pre-readiness fix needs no shared lifecycle state change. The after-readiness
change affects every lifecycle kind, so its Rust implementation, the actual
stored-schema snapshots, and removal of the Vessel copy wait for spec review.
Other reader migrations and copy removals can proceed independently.

The proposed DDL describes new writes. Preceding-generation Gone records have
no ending column. A held final Release or Vanish entry can supply its ending,
but a snapshot-seeded replica can lack that entry. Its historical failure text
cannot be recovered from the engine, because the preceding-generation Vanish
argument never contained it. The N→N+1 decoder must preserve that uncertainty
rather than invent a release or failure reason. The owner review needs to settle
whether to retain a deserialize-only legacy Gone state for one roll or require
an explicit import/rehearsal decision for those records. Neither is implemented
by this proposal.
