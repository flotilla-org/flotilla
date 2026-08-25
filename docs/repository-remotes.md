# Repository remote declarations

A `Repository` may declare more than one transport remote. Its first-declared
remote defines the stable Repository key. The current live remote is ordered
first in `spec.remotes` and defines forge attribution, credential scope, issue
source, and change-request lookup. Observing any declared remote resolves to the
same Repository.

Declare remotes in the replicated Repository spec:

```json
{
  "remotes": [
    "https://github.com/flotilla-org/flotilla",
    "https://forgejo.example/lab/flotilla"
  ]
}
```

The observed checkout remote must appear in the list. Remotes are normalized
and must be unique. Forks must not be listed as mirrors: declare them as their
own Repository with `upstream.relation = "fork"`.

Refreshing that checkout writes the declaration to its root's Repository
record. Observation-time identity resolution reads the replica-inclusive
Repository view, so after resource-store replication the declaration applies
on every root; it does not need to be repeated in each root's local config.
Until the declaring root's record has replicated, another root can still
temporarily observe the mirror as a provisional Repository.

## Standing lab-mirror sweep

For each of `andamento`, `cleat`, and `flotilla`:

1. On one root, track or refresh the GitHub checkout with `remotes` listing
   GitHub first and `lab/<name>` second. Wait for that Repository record to be
   visible through the replica-inclusive view on the other roots; the
   declaration then serves the whole fleet and does not need a per-root apply.
2. Track or refresh every mirror checkout. Flotilla resolves it to the GitHub
   Repository, re-associates whole-repository Project definitions, and retires
   the generated `<name>-lab` Project and provisional `lab/<name>` Repository.
3. Verify `flotilla project list` contains one row for the codebase and
   `flotilla repo list` contains the GitHub Repository but no provisional lab
   Repository. Durable records that still refer to an old key retain that
   Repository with a `flotilla.work/superseded-by` annotation until their
   provenance is re-associated; refresh those records and repeat the sweep.

This sweep is idempotent and may be repeated on every root.
