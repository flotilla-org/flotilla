# Message delivery receiver reads (#3433)

A delivery pass now collects its active messages' receiver addresses and uses
`MessageReceiverSnapshot::load_for_addresses`. Topics expand to subscribed role
candidates before loading holders; current presence and sender exclusion still
choose the recipient. The operator batch-failure and idle gate-repair paths use
the same loader. Legacy receipts without `role_address` find the recorded crew
through the existing `status.crew.id` declared query.

## Read-counter regression

`addressed_receiver_reads_ignore_unrelated_holders` runs a real delivery pass
against both memory and SQLite stores, with zero and 48 unrelated Convoys,
TerminalSessions and ConvoyEnsures. Explicit and standing-role addresses share
the same receiver. With 48 unrelated holders:

| Receiver kind | Before decoded rows | After decoded rows |
| --- | ---: | ---: |
| Convoy | 49 | 1 |
| TerminalSession | 49 | 1 |
| ConvoyEnsure | 49 | 1 |

The new assertion failed against the original full-snapshot delivery caller.
The existing empty-pass and 10/200-address budgets remain covered.

## Dummy-fleet measurement

Run:

```sh
cargo test -p flotilla-daemon --locked --test dummy_fleet_load receiver_snapshot_decode_measurement -- --ignored --nocapture
```

The test uses the same six-convoy, 25-second plan as `load_driver_smoke`, then
captures a consistent SQLite database with `VACUUM INTO`. It compares the old
full loader with the scoped loader for one explicit crew address on that same
frozen database. This isolates the receiver stage of a delivery pass from
concurrent controllers and transport. Store-testkit counts decoded objects;
bytes are the exact UTF-8 `body_json` input lengths for the selected local rows.
This single-home fixture has no replicas. Message reads and other delivery work
are excluded from both measurements.

Measured on 2026-10-17:

| Receiver stage per pass | Rows decoded | Bytes decoded |
| --- | ---: | ---: |
| Before: whole receiver inventory | 41 | 186,477 |
| After: addressed convoy and crew | 2 | 8,374 |

That is 95.1% fewer rows and 95.5% fewer bytes. The fleet is dynamic, so exact
inventory counts and sizes vary across runs; the test asserts two scoped rows
and a reduction rather than fixing the full inventory count.

## Workload and scope

The root `WORKLOAD.md` targets 1,000 running convoys, with a ceiling of 10,000
running and 1M retained historical convoys. At its 21 KB mean Convoy size,
whole-kind loading at the history ceiling implies roughly 21 GB of Convoy
bodies per wake alone. A single explicit address now hydrates its convoy and
matching terminal, roughly 41 KB using the root mean sizes, independently of
unrelated inventory. Multiple addresses share batched declared queries.

This reuses the existing query pushdown and replica precedence, without a new
cache, schema, retention policy or controller. Topic hierarchy/default
validation retains its existing declaration reads. Inventory/contacts/charter
snapshot callers remain outside this targeted delivery fix, as do the sibling
#3430 readers. The store replacement #3392 remains the eventual replacement
for this path.
