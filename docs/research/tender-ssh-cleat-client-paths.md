# Cleat client paths for Tender SSH slice 2

Investigated 2026-10-03 against cleat revision
`fd66a7121149e85eb8fbc57a99a388b0c91dae6c`. The installed
`/usr/local/bin/cleat version` reports that same build revision, protocol 11,
Ghostty, Linux x86_64. Source acquisition used the injected authenticated `gh`
wrapper and a scratch clone outside the vessel checkout.

## Client-path result

| Client path | Result | Constraint |
| --- | --- | --- |
| Installed `cleat packets` against its private local runtime | Tested successfully | Requires local session metadata and daemon runtime layout |
| Installed CLI `--server /arbitrary/socket` | Tested rejection | Server selects a filesystem-safe daemon name, not an endpoint |
| Installed `packets` with an empty local runtime | Tested `missing session tender-proof` | Socket-only exposure cannot satisfy its local session-directory check |
| Connect-only ordinary packet client over arbitrary Unix socket | Tested successfully locally | Uses HTTP `/connect` upgrade, directory subscription, controller channel, input, render, ACK |
| Provider daemon packet connection | Source-supported socket connect without daemon spawning | Internal function takes `RuntimeLayout`, not an exported CLI socket option |

The CLI's hidden `--runtime-root` selects a runtime directory; it does not expose
a connect-only arbitrary endpoint. `connect_packets` checks the session directory,
then `connect_subscription` calls `ensure_daemon_started`. Consequently the CLI
can create a daemon when given an absent layout. The connect-only proof helper
avoids that path entirely. Sources: [CLI options and command dispatch](https://github.com/flotilla-org/cleat/blob/fd66a7121149e85eb8fbc57a99a388b0c91dae6c/crates/cleat/src/cli.rs),
[SessionService packet connection](https://github.com/flotilla-org/cleat/blob/fd66a7121149e85eb8fbc57a99a388b0c91dae6c/crates/cleat/src/server.rs),
[provider connect_packet_stream](https://github.com/flotilla-org/cleat/blob/fd66a7121149e85eb8fbc57a99a388b0c91dae6c/crates/cleat/src/provider_daemon.rs).

## Ordinary protocol, independent of Tender

Connect an AF_UNIX stream to the published daemon socket. Send HTTP/1.1
`POST /connect`, `Connection: Upgrade`, `Upgrade: cleat-packet/1`,
`Content-Type: application/json`, and the exact body length. The JSON body
`{"selectors":[]}` subscribes to the directory. Output admission also requires
`x-cleat-output-context: {"version":1,"context":{"kind":"remote"}}`.
The remote declaration is appropriate for an SSH exposure; it bypasses local
peer corroboration and local dependency-cycle tracking. The server responds
HTTP 101; the socket then carries ordinary packet frames. Sources:
[subscription request](https://github.com/flotilla-org/cleat/blob/fd66a7121149e85eb8fbc57a99a388b0c91dae6c/crates/cleat/src/http_uds.rs),
[output admission](https://github.com/flotilla-org/cleat/blob/fd66a7121149e85eb8fbc57a99a388b0c91dae6c/crates/cleat/src/output_admission.rs).

Each frame is little-endian channel u32, message type u8, payload length u32,
then a postcard payload (maximum 4 MiB). First frames are channel 0 hello
(type 1, version range 11..11), then directory snapshot (type 2). Open a session
by sending channel 0/type 4 `OpenChannel`, selecting a fresh channel 1,
session ID, `Controller` role, `take:false`, and principal identity. Role state
precedes the initial channel-1 render (type 16). Send input as channel 1/type 18
`Input { event: RawBytes(bytes) }`; acknowledge each render generation on
channel 1/type 17. A render packet is terminal state, not raw PTY output;
assertions about echoed strings should use capture or a real renderer. Sources:
[packet constants, structures and framing](https://github.com/flotilla-org/cleat/blob/fd66a7121149e85eb8fbc57a99a388b0c91dae6c/crates/cleat/src/packet.rs),
[render and input structures](https://github.com/flotilla-org/cleat/blob/fd66a7121149e85eb8fbc57a99a388b0c91dae6c/crates/cleat/src/provider.rs),
[packet ACK handling](https://github.com/flotilla-org/cleat/blob/fd66a7121149e85eb8fbc57a99a388b0c91dae6c/crates/cleat/src/session.rs).

## Reproducible isolated local setup

Use a short private directory directly beneath the default `/tmp` (no TMPDIR
override). These commands start a separate cleat daemon, not Flotilla:

```sh
proof_root=$(mktemp -d /tmp/tender-cleat.XXXXXX)
/usr/local/bin/cleat --runtime-root "$proof_root" --server tender \
  launch tender-proof --cmd cat --no-record --json
# Fresh layout uses physical generation tender@1:
endpoint="$proof_root/tender@1/socket"
python3 crates/tender/tests/common/cleat.py "$endpoint" tender-proof 'proof-input
'
/usr/local/bin/cleat --runtime-root "$proof_root" --server tender capture tender-proof
/usr/local/bin/cleat --runtime-root "$proof_root" --server tender kill tender-proof
# Stop only the daemon created by this fixture, using tender@1/daemon.pid.
```

Runtime sockets are `<root>/<physical-daemon>/socket`, with
`daemon.pid` alongside; generation aliases select a physical daemon. Source:
[RuntimeLayout](https://github.com/flotilla-org/cleat/blob/fd66a7121149e85eb8fbc57a99a388b0c91dae6c/crates/cleat/src/runtime.rs).

The checked-in Python helper owns only its connection. It does not discover,
launch, reconnect, replay input, or touch session metadata. It is deliberately
pinned to protocol 11 and implements the postcard fields needed for this proof,
including render-generation ACKs. Initial local verification returned directory
127 bytes, initial render 54,118 bytes, updated render 6,861 bytes, and capture
confirmed the submitted input reached the `cat` PTY. The CLI packet probe also
returned an initial full render. The fixture session and daemon were stopped.

For the real SSH proof, substitute Tender's locally exposed socket for
`$endpoint`; keep the remote fixture cleat session alive when killing SSH, then
create a fresh client after route recovery. This note establishes the client
path only; the SSH adapter integration test records interruption and recovery.

## Real SSH adapter proof

The slice-2 integration test `cleat_survives_ssh_loss_and_fresh_client_recovers`
passed on 2026-10-03 with an unprivileged OpenSSH 9.6p1 fixture listening only on
loopback. The remote-side Tender listener and cleat daemon ran independently;
SSH forwarded Tender's listener, which authenticated the caller and opened the
ordinary cleat endpoint. No production host was used.

Through the Tender-owned exposure the first packet client received a 141-byte
directory snapshot and a 54,086-byte initial render; input produced a 6,851-byte
updated render. A held packet client observed EOF after killing only the SSH
client. Direct capture of the existing fixture session remained identical.
Browse and watch reported Unavailable, and a new connection to the still-bound
local exposure promptly closed with zero bytes. Restoring SSH and replacing the
private route produced Available again. A fresh client fetched the directory
and session render without replaying any prior input; another fresh client sent
new input successfully (54,108-byte initial and 6,851-byte updated render in the
recorded run). Packet lengths depend on daemon metadata and terminal state; the
test asserts protocol behavior, not these incidental lengths.

The nine applicable slice-1 scenarios also passed unchanged over real SSH:
existing service, lifecycle/stale teardown, namespace denial, grant loss/expiry,
no replay/rebind, restart/replacement, publisher receiver closure, narrowed
audience, and backpressure/cancellation. The container and pinned-intermediary
walkthroughs remain in-memory contracts for slice 3. The shared backpressure
payload is 16 MB, exceeding native SSH channel windows and kernel socket buffers
without assuming the in-memory adapter's smaller capacity.

With `sshd` installed, run from the repository root with default TMPDIR:

```sh
cargo test -p tender --locked
cargo test -p tender --locked -- --ignored
```

The ignored checks require `ssh`, `ssh-keygen`, a local `sshd`, Python 3, and the
pinned protocol-11 cleat build. `TENDER_TEST_SSHD` selects a nonstandard sshd
binary. In this vessel sshd and libwrap were extracted from Ubuntu packages
under `/tmp`; the test run supplied that library directory alongside cleat's
`/usr/local/lib/flotilla` in `LD_LIBRARY_PATH`. The fixture generates private SSH
keys and known_hosts, disables password authentication, listens only on
127.0.0.1, and tears down its own processes. It needs no production credentials
or host configuration.
