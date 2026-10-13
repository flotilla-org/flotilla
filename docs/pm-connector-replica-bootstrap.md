# PM connector merged-resource bootstrap

The connector bootstraps each subject kind/namespace from its resource watch's
own initial snapshot. It does not list first, and it does not request cursor
resume for an include-replicas watch. The merged-resource layer registers local
and replica streams before taking its snapshot. The consumer applies that
snapshot fragments through the explicit bootstrap bookmark, followed by every
queued live event on the same watch. Scope mismatches and live modifications or
deletions before that bookmark are explicit permanent protocol errors. The overlay
cursor is descriptive; reconnect starts new watches and fresh query cursors.

Definitions are merged logical members. Their initial snapshots and live
removals use the same local logical provenance, even when the last source was a
replica. Observation resources keep independent source provenance, so deleting
one replica preserves a surviving local or replica observation.

One `Connector` owns the acknowledged catalog. Each daemon session owns fresh
rows, a query subscription and resource-watch tasks. Publication waits for every
subscribed named query's snapshot and every resource watch's bootstrap bookmark.
A query gap or lag retires all source subscriptions, takes fresh snapshots, then
publishes their diff against the acknowledged catalog in one request. Recovery
recreates every named query and one watch per kind in each active namespace;
that cost deliberately buys a complete barrier and removes obsolete resource
facts. Retiring the old receiver prevents queued pre-gap events from causing a
resnapshot loop.

Wheelhouse subscriptions use connection liveness: the first publication is one
JSON array sent to `POST /v1/metadata/snapshot`, replacing the provider's old facts.
Subsequent nonempty diffs are arrays sent to `POST /v1/metadata/patch`. Neither
carries TTLs. The expiry tick can remove landed subjects but sends nothing when
the projection is unchanged. Wheelhouse issue 378 supplies atomic application,
window seeding and the companion protocol documentation. A snapshot 404 switches
this process to the legacy single-message transport with 30-second TTLs and
10-second full reassertions. Zellij retains that TTL behavior.

The catalog advances only after acknowledged delivery. Failed atomic publications
retry unchanged with exponential delays from 250 ms to 2 seconds, within a
15-second deadline (including blocked transport calls). Uncertain delivery past
that deadline exits with an error, allowing a restart's replacing snapshot to
repair the provider. Legacy delivery retries the remaining individual patches
within the same deadline, logging each failed attempt. The publication deadline
is terminal for **both** liveness modes, as required by issue #3022 item 4.
Zellij retries transient sends but exits if its pipe remains blocked or failing
for the entire 15-second deadline. That connector then needs relaunch; its TTL
facts expire normally. Zellij's in-process reconnects apply to daemon loss, not
indefinitely uncertain sink delivery. Legacy retry warnings identify the stuck
patch target.

After the first connection-mode publication, daemon-session loss exits with an
error so Wheelhouse can mark the provider stale and restart it. Before publication,
daemon connection/setup failures retain the shared jittered retry (500 ms initial
base, 30-second cap); TTL mode also reconnects after publication. Permanent resource
validation refusals and exhausted publication retries always terminate.

The connector captures its parent at startup. Unix checks `getppid()` every second;
Windows holds and probes an owned parent process handle (`Send + Sync`). Parent death cancels even dial
retries or blocked setup. Connection mode also probes `GET /v1/health` every five
seconds and exits after 15 seconds of consecutive connection failures. A received
HTTP response establishes connection health, including an older server's 404.

Failure unsubscribes named queries and aborts watch tasks; dropping each watch
cancels its command. The in-process query lifetime token removes demand on
cancellation; socket query lifetime belongs to the connection.

## Regression checks

```sh
cargo test -p flotilla-tui --lib --locked pm_connect
cargo test -p flotilla-resources --lib --locked registry_watch_tests
cargo test -p flotilla-tui --test pm_connector_e2e --locked
```

The first suite exercises the real daemon's resource-watch command consumer,
including rejection of unsupported cursor parameters, publication readiness,
cancellation, failed setup and namespace discovery. Fragmented bootstrap and
invalid pre-bookmark live events are pinned; virtual-clock scenarios cover
transient retry jitter/capping and healthy-session reset. The resource suite runs a shared generated
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
