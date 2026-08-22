# Repository remote declarations

A `Repository` may declare more than one transport remote. The first remote is
canonical and defines the Repository key, forge attribution, issue source, and
change-request lookup. Later entries are mirrors: observing any declared remote
resolves to the same Repository.

Declare the remotes in the tracked checkout's `repos/*.toml` file:

```toml
path = "/absolute/path/to/flotilla"
remotes = [
  "https://github.com/flotilla-org/flotilla",
  "https://forgejo.example/lab/flotilla",
]
```

The observed checkout remote must appear in the list. Remotes are normalized
and must be unique. Forks must not be listed as mirrors: declare them as their
own Repository with `upstream.relation = "fork"`.

## Standing lab-mirror sweep

For each of `andamento`, `cleat`, and `flotilla`:

1. Track or refresh the GitHub checkout with `remotes` listing GitHub first and
   `lab/<name>` second.
2. Track or refresh every mirror checkout. Flotilla resolves it to the GitHub
   Repository, re-associates whole-repository Project definitions, and retires
   the generated `<name>-lab` Project and provisional `lab/<name>` Repository.
3. Verify `flotilla project list` contains one row for the codebase and
   `flotilla repo list` contains the GitHub Repository but no provisional lab
   Repository. Durable records that still refer to an old key retain that
   Repository with a `flotilla.work/superseded-by` annotation until their
   provenance is re-associated; refresh those records and repeat the sweep.

This sweep is idempotent and may be repeated on every root.
