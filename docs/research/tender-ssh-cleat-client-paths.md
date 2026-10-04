# Cleat client paths for Tender SSH slice 2

Investigated 2026-10-03 against cleat revision
`fd66a7121149e85eb8fbc57a99a388b0c91dae6c`. The installed
`/usr/local/bin/cleat version` reports that same build revision, protocol 11,
Ghostty, Linux x86_64. Source acquisition used the injected authenticated `gh`
wrapper and a scratch clone outside the vessel checkout.

## Current proof status

[Cleat#302](https://github.com/flotilla-org/cleat/issues/302) shipped in
[cleat#303](https://github.com/flotilla-org/cleat/pull/303). The dedicated
`Tender SSH cleat proof` job builds revision
`00c072b207dc943f6c93fe3b6b09abaa257695a6` and exercises cleat's own connect-only
`packets --socket` and `attach --socket` clients through a Tender-owned exposure
over real loopback SSH. See [the maintained proof](../../ci/tender/README.md)
for setup, provenance, interruption, refusal and recovery assertions.

The historical protocol-11 Python experiment below is evidence about the old
client limitation, not maintained CI coverage. Tender maintains no cleat protocol
client. Normal adapter contracts remain on local Unix sockets, with exactly one
ignored real-sshd Forward smoke test.

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
can create a daemon when given an absent layout. The historical connect-only proof helper
avoided that path entirely. Sources: [CLI options and command dispatch](https://github.com/flotilla-org/cleat/blob/fd66a7121149e85eb8fbc57a99a388b0c91dae6c/crates/cleat/src/cli.rs),
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

## Historical isolated setup

The investigation launched a separate cleat daemon in a private default `/tmp`
directory using `cleat --runtime-root <root> --server tender launch tender-proof
--cmd cat --no-record --json`. Runtime sockets are
`<root>/<physical-daemon>/socket`, with `daemon.pid` alongside; generation aliases
select a physical daemon. Source:
[RuntimeLayout](https://github.com/flotilla-org/cleat/blob/fd66a7121149e85eb8fbc57a99a388b0c91dae6c/crates/cleat/src/runtime.rs).

A temporary independent protocol-11 client returned directory and render frames,
and capture confirmed input reached the `cat` PTY. That helper has been removed:
maintaining a second cleat protocol implementation is unnecessary for Tender’s
ordinary-service contract. The maintained proof now uses cleat’s own connect-only client; see the
current proof status above. The fixture session and daemon
used in the investigation were stopped.

## Historical real SSH adapter experiment

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

## Maintained Tender coverage

Normal `cargo test -p tender --locked` runs all ten applicable slice-1 scenarios
against both MemoryTender and SshTender directly connected to Server over a
private local Unix socket. The intermediary scenario remains an in-memory
contract for slice 3; the direct adapter explicitly tests refusal of `via`.
Ordinary carriage uses a generic Rust request/response endpoint, directional EOF,
route interruption, service survival and fresh-client recovery without replay.

With OpenSSH client/server installed, run the single ignored Forward smoke test:

```sh
cargo test -p tender --locked -- --ignored
```

It exercises authenticated publication, raw bytes, SSH interruption, reserved
identity, explicit generation reclaim and fresh opens without replay.
`TENDER_TEST_SSHD` selects a nonstandard sshd binary. The fixture uses ephemeral
keys, pinned known_hosts, private directories, loopback only, and cleans its own
processes. No cleat or Python dependency is needed by Tender tests. The dedicated
cleat proof uses the opt-in target documented in [ci/tender](../../ci/tender/README.md).
