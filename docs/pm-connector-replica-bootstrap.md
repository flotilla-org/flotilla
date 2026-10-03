# PM connector merged-resource bootstrap

The connector bootstraps each subject kind/namespace from its resource watch's
own initial snapshot. It does not list first, and it does not request cursor
resume for an include-replicas watch. The merged-resource layer registers local
and replica streams before taking its snapshot. The consumer applies that
snapshot once, followed by every queued event on the same watch. The overlay
cursor is descriptive; reconnect starts new watches and fresh query cursors.

Definitions are merged logical members. Their initial snapshots and live
removals use the same local logical provenance, even when the last source was a
replica. Observation resources keep independent source provenance, so deleting
one replica preserves a surviving local or replica observation.

One `Connector` owns the last publication across connections. Each connection
owns a fresh row/resource state, a query subscription and its watch tasks. Setup
finishes before the first publication. Failure unsubscribes named queries and
aborts the watch tasks; dropping each resource watch cancels its command. The
in-process query lifetime token also removes demand on cancellation; socket
query lifetime belongs to the connection. Reconnect diffs the fresh complete
bootstrap against the last catalog to retract facts removed offline and refresh
all surviving TTL facts. Resource validation refusals terminate with their
original error instead of reconnecting; transient connection/watch failures
still reconnect, with a delay between established sessions.

## Regression checks

```sh
cargo test -p flotilla-tui --lib --locked pm_connect
cargo test -p flotilla-resources --lib --locked registry_watch_tests
cargo test -p flotilla-tui --test pm_connector_e2e --locked
```

The first suite exercises the real daemon's resource-watch command consumer,
including rejection of unsupported cursor parameters, cancellation, failed
setup and namespace discovery. The resource suite runs a shared generated
membership contract on memory and SQLite storage, plus pinned final-replica,
name-only deletion and queued-bootstrap arrival/removal cases. The end-to-end
suite starts an isolated daemon runtime and Aggregator with local and replica
observations, project subjects, standing-role attempts and opening recipes. It
queues several updates, removes individual sources, remains subscribed for seven
seconds, and reconnects after an offline deletion.

## Disposable Wheelhouse acceptance

This Linux harness uses Wheelhouse's real HTTP/UDS ingress and the real Andamento
C decoder. It does not need a GUI or any production daemon. Check out both
repositories **outside** the vessel checkout, then build the fixture:

```sh
cd "$WHEELHOUSE_CHECKOUT"
python3 tools/prepare-andamento-build.py "$ANDAMENTO_CHECKOUT"
cargo build --manifest-path build/andamento/Cargo.toml \
  -p andamento-ffi -p wheelhouse-native-deps --locked
cd "$FLOTILLA_CHECKOUT"
python3 scripts/test-pm-wheelhouse-ingress.py \
  "$WHEELHOUSE_CHECKOUT" "$ANDAMENTO_CHECKOUT" \
  "$WHEELHOUSE_CHECKOUT/build/andamento/target/debug" "$LOG_DIRECTORY"
```

Use the default `TMPDIR`. The fixture's UDS path is independently allocated
beneath `/tmp` to fit `SUN_LEN`. The harness records source revisions and checkout
state, connector logs, and every accepted wire patch. It requires successful
real decoding, project/subject/role/recipe facts, no unsupported-cursor error,
and exactly two subscriptions: the stable session and the deliberate reconnect.

Initial acceptance used Flotilla base
`ce586b597444da52c03c0237cc40c27e091d0440`, Wheelhouse
`77268760fe990651703b3b049a9735e57ec2c9c2`, and Andamento
`afa24619405ab34196bd46ae414b039ce9edebfb`. Its real ingress accepted 23 patches
with two intentional subscriptions in 7.51 seconds. Final candidate revision and
logs are recorded in the PR's acceptance artifact. The independent Wheelhouse
ingress suite passed 12 tests with three platform/environment skips.

Live Wheelhouse acceptance against production daemons remains operator work
after merge. This change alters no serialized resource schema, stored records,
or external manifests.
