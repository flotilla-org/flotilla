# Tender ordinary-service forwarding contracts

**Date:** 2026-09-13  
**Issue:** [flotilla-org/flotilla#1862](https://github.com/flotilla-org/flotilla/issues/1862)  
**Map:** [flotilla-org/flotilla#1861](https://github.com/flotilla-org/flotilla/issues/1861)

## Executive conclusion

Tender can prove useful forwarding without inventing an application protocol. OpenSSH stream-local forwarding already carries an ordered, flow-controlled byte stream between Unix sockets, and both cleat's packet client and porthole's HTTP client can address a local endpoint without knowing that a forward exists. The strongest first proof is therefore a prepared, already-running cleat daemon socket forwarded over SSH to a local Unix socket, followed by porthole's ordinary HTTP control endpoint as the independent second client.

That mechanism is not the whole Tender contract. SSH does not provide publication identity, discovery, grants, stable local names, lifecycle retention, or useful per-accepted-connection diagnostics. Tender must own those control-plane responsibilities and must define byte-pump cancellation, bounded buffering, deadlines, and half-close behavior. It must not absorb cleat daemon spawning/session cleanup, porthole authorization, descriptor/handle transfer, Flotilla resource replication, or Flotilla command routing.

Porthole's capture-transfer socket is a negative boundary test: its protocol transfers Unix file descriptors and shared-memory acquisition state. Those process-local/host-local capabilities cannot be reproduced by an ordinary byte forward. Forward porthole's HTTP control endpoint first; treat remote capture as application preparation or a different data-plane design.

## Sources and revisions inspected

Repository heads were fetched from their actual upstream repositories on 2026-09-13:

| Source | Revision | Role |
|---|---|---|
| [flotilla-org/flotilla](https://github.com/flotilla-org/flotilla/tree/005c1c1a9d7ba0a26a08db0247039ac157582523) | `005c1c1a9d7ba0a26a08db0247039ac157582523` | SSH peer transport and resource replication |
| [flotilla-org/cleat](https://github.com/flotilla-org/cleat/tree/bf47bbb706226bcd9aafe7aa38f3e4b920448baa) | `bf47bbb706226bcd9aafe7aa38f3e4b920448baa` | daemon layout, IPC, packet client/server |
| [flotilla-org/porthole](https://github.com/flotilla-org/porthole/tree/80077606c23042512eaa8fc6c5492efb30cf84a7) | `80077606c23042512eaa8fc6c5492efb30cf84a7` | HTTP control and capture descriptor transfer |

The map has no resolved contract decisions yet. Its baseline separates instance identity from location, publication from browse/connect permission, remembered publication from reachability, and transport reconnect from application recovery; it also requires daemonless ordinary SSH execution to remain possible ([#1861 body](https://github.com/flotilla-org/flotilla/issues/1861)). All six child decision tickets remain open. The comments visible on the map concern dispatch failures/corrections rather than prerequisite contract resolutions, so this report does not treat them as design decisions.

## What an ordinary forwarded stream can promise

### Ordering and backpressure

SSH connection protocol data is sent with explicit per-channel windows: a sender may not exceed the recipient's window, and channel data must be delivered in order. Window adjustment is the protocol's backpressure mechanism ([RFC 4254 sections 5.2 and 5.3](https://www.rfc-editor.org/rfc/rfc4254#section-5.2)). OpenSSH's `-L` and `-R` stream-local forms connect a listening Unix-domain socket to another Unix-domain socket through the secure channel ([OpenSSH `ssh(1)`](https://man.openbsd.org/ssh.1#L)).

This makes SSH forwarding a sound reusable byte substrate, but Tender still needs bounded queues at every adapter or multiplexing seam. Flotilla's existing peer implementation is illustrative rather than reusable as the generic stream contract: it uses bounded 256-message Tokio channels and awaits sends, preserving order and propagating backpressure, but transports newline-delimited `PeerWireMessage` values rather than arbitrary bytes ([`ssh_transport.rs`](https://github.com/flotilla-org/flotilla/blob/005c1c1a9d7ba0a26a08db0247039ac157582523/crates/flotilla-daemon/src/peer/ssh_transport.rs)).

Cleat has its own application-aware policy. Its packet provider multiplexes a directory on channel 0 and session traffic on nonzero channels; rendering permits only one unacknowledged packet and coalesces state so a slow renderer does not block the terminal. Offline input is capped at 1,024 frames ([`provider_daemon.rs`](https://github.com/flotilla-org/cleat/blob/bf47bbb706226bcd9aafe7aa38f3e4b920448baa/crates/cleat/src/provider_daemon.rs)). Tender should preserve these bytes and pressure signals, not reinterpret or duplicate that policy.

### Half-close and closure

SSH distinguishes channel EOF (the sender will send no more data) from channel close, and a peer must still drain previously sent data; EOF does not prohibit data in the opposite direction ([RFC 4254 section 5.3](https://www.rfc-editor.org/rfc/rfc4254#section-5.3)). That is enough substrate for TCP/Unix-style half-close, but a proxy implementation must deliberately map local read EOF to remote EOF, continue the opposite pump, and close only after both directions finish or cancellation/deadline/error terminates them.

Neither examined application offers a portable half-close contract that Tender can merely inherit. Cleat's Windows `shutdown_stream` cancels outstanding I/O on the whole pipe handle, and its cloned handles are duplicate references to the same kernel pipe; this is cancellation/closure, not a directional write shutdown ([`windows.rs`](https://github.com/flotilla-org/cleat/blob/bf47bbb706226bcd9aafe7aa38f3e4b920448baa/crates/cleat/src/platform/ipc/windows.rs)). Porthole delegates HTTP stream shutdown to Tokio/Hyper, but Windows named pipes do not have Unix socket half-close semantics in this adapter ([`porthole-transport`](https://github.com/flotilla-org/porthole/blob/80077606c23042512eaa8fc6c5492efb30cf84a7/crates/porthole-transport/src/lib.rs)). Tender's portable minimum should therefore be prompt full closure on cancellation/error and best-effort directional EOF where both adapters support it; any stronger cross-platform promise needs a proof test.

Broken streams must not be replayed. Reconnect creates a new application connection. Cleat is already prepared for that: its provider reader reconnects with exponential backoff, reopens registered session channels, receives a fresh full render, and reasserts desired size/role while requiring explicit retaking of control ([`provider_daemon.rs`](https://github.com/flotilla-org/cleat/blob/bf47bbb706226bcd9aafe7aa38f3e4b920448baa/crates/cleat/src/provider_daemon.rs)).

## Local endpoints, credentials, and handles

### Unix sockets

Filesystem permissions and parent-directory traversal protect a pathname socket, while peer credentials are a separate OS facility (`SO_PEERCRED` on Linux, for example, returns credentials of the connected peer) ([Linux `unix(7)`](https://man7.org/linux/man-pages/man7/unix.7.html)). A proxy changes the application's peer: the service sees Tender's local process credentials, not the original remote caller. Tender authentication and authorization therefore cannot be represented as preserved Unix peer credentials unless a service-specific trusted adapter conveys identity out of band.

### Windows named pipes

Windows named pipes support duplex byte mode, which both current clients can use as a stream. Microsoft documents byte mode as a sequence of bytes and distinguishes it from message mode; pipe flow is constrained by system buffers and blocking/overlapped I/O ([`CreateNamedPipe`](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-createnamedpipea), [Named Pipe Type, Read, and Wait Modes](https://learn.microsoft.com/en-us/windows/win32/ipc/named-pipe-type-read-and-wait-modes), [Named Pipe Operations](https://learn.microsoft.com/en-us/windows/win32/ipc/named-pipe-operations)). Cleat explicitly creates duplex byte-mode overlapped pipes and publishes their generated name through a marker file ([`windows.rs`](https://github.com/flotilla-org/cleat/blob/bf47bbb706226bcd9aafe7aa38f3e4b920448baa/crates/cleat/src/platform/ipc/windows.rs)). Porthole serves HTTP directly over a Tokio named pipe and notes that its default DACL still needs explicit confirmation/hardening for shared-host use ([`porthole-transport`](https://github.com/flotilla-org/porthole/blob/80077606c23042512eaa8fc6c5492efb30cf84a7/crates/porthole-transport/src/lib.rs)). Tender must set and test an explicit security descriptor: Microsoft's documented default named-pipe ACL grants read access to Everyone and anonymous users, not an owner-only boundary ([Named Pipe Security and Access Rights](https://learn.microsoft.com/en-us/windows/win32/ipc/named-pipe-security-and-access-rights)).

Named-pipe servers may impersonate a connected client, and may obtain the client's process/session identity, but only for the immediately connected client ([Impersonating a Named Pipe Client](https://learn.microsoft.com/en-us/windows/win32/ipc/impersonating-a-named-pipe-client), [`GetNamedPipeClientProcessId`](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-getnamedpipeclientprocessid)). As on Unix, a local Tender adapter terminates that OS-authenticated hop. Tender must use its own authenticated caller identity for policy and must not claim transparent OS credential forwarding.

Windows handles are process-table capabilities. They must be inherited at process creation or duplicated into a specified process, with access controlled by the handle and process rights ([Handle Inheritance](https://learn.microsoft.com/en-us/windows/win32/sysinfo/handle-inheritance), [`DuplicateHandle`](https://learn.microsoft.com/en-us/windows/win32/api/handleapi/nf-handleapi-duplicatehandle)). Unix descriptor passing similarly uses ancillary `SCM_RIGHTS` data, not ordinary payload bytes ([Linux `unix(7)`](https://man7.org/linux/man-pages/man7/unix.7.html)). Tender's generic stream cannot preserve either kind of capability across machines.

## Cleat: discovery is entangled with ownership today

Cleat's `RuntimeLayout` derives daemon directory, `socket`, `daemon.pid`, and session directories from one runtime root and daemon name. Discovery scans roots for directories containing `sessions`; it does not query a separately published endpoint registry ([`runtime.rs`](https://github.com/flotilla-org/cleat/blob/bf47bbb706226bcd9aafe7aa38f3e4b920448baa/crates/cleat/src/runtime.rs), [`server.rs`](https://github.com/flotilla-org/cleat/blob/bf47bbb706226bcd9aafe7aa38f3e4b920448baa/crates/cleat/src/server.rs)).

Ordinary client calls are not ownership-neutral. `ensure_daemon_started` removes a socket it judges stale, creates directories, spawns a daemon, and waits for the endpoint. Dead-daemon listing removes non-recreatable session directories and stale socket/PID files ([`session.rs`](https://github.com/flotilla-org/cleat/blob/bf47bbb706226bcd9aafe7aa38f3e4b920448baa/crates/cleat/src/session.rs), [`server.rs`](https://github.com/flotilla-org/cleat/blob/bf47bbb706226bcd9aafe7aa38f3e4b920448baa/crates/cleat/src/server.rs)). Pointing those APIs at a Tender-owned local proxy path would let cleat delete/recreate the proxy or spawn a daemon into the exposure directory.

The first proof should therefore start the remote cleat daemon normally and expose the socket at a path that the unchanged packet provider can connect to, while avoiding a code path that assumes it owns that path. If the unchanged client cannot be configured to connect without `ensure_daemon_started`, the smallest cleat preparation is an explicit **connect-only external endpoint** mode: it must never spawn, unlink, sweep session data, or infer daemon ownership. This is application preparation, not a Tender dependency.

The packet provider is still the best first-proof client because reconnect/reopen and directory refresh already belong to it. Tender only restores reachability; cleat reconstructs application state.

## Porthole: forward control, not capture capabilities

Porthole's ordinary control service is HTTP over either a Unix socket or Windows named pipe. Its `LocalHttpClient` accepts an endpoint abstraction and is otherwise unaware of the local transport ([`porthole-transport`](https://github.com/flotilla-org/porthole/blob/80077606c23042512eaa8fc6c5492efb30cf84a7/crates/porthole-transport/src/lib.rs)). An unchanged client can therefore be a second proof if Tender exposes the expected endpoint and porthole authorization tokens remain end-to-end HTTP headers.

Capture is deliberately separate. On Unix, `portholed` binds `capture-transfer.sock`; a client sends a bounded JSON preface containing session, track, and bearer token, after which the server hands off Jackstay acquisition state including file descriptors/shared memory. Windows currently disables that capture socket ([`server.rs`](https://github.com/flotilla-org/porthole/blob/80077606c23042512eaa8fc6c5492efb30cf84a7/crates/portholed/src/server.rs), [`capture_registry.rs`](https://github.com/flotilla-org/porthole/blob/80077606c23042512eaa8fc6c5492efb30cf84a7/crates/portholed/src/capture_registry.rs)). Forwarding the pathname alone cannot transport the descriptors, and translating them would require a porthole/Jackstay-specific remote capture protocol. That is outside Tender's ordinary-service proof.

Porthole also keeps application authorization above transport: bearer grants and action classes decide what an agent may observe/drive/manage. Tender publication and connection permission must not replace or widen those grants.

## Flotilla mechanisms: reusable substrate and boundaries

Flotilla's SSH peer transport is useful evidence and a likely extraction source for SSH process lifecycle, socket-path quoting, deterministic forward paths, stale-path cleanup, retry/backoff, keepalives, and diagnostics. It resolves the remote daemon path with a separate SSH command, creates both `-L` and `-R` Unix-socket forwards, requests `ExitOnForwardFailure=yes`, and uses `StreamLocalBindUnlink=yes` for the local bind ([`ssh_transport.rs`](https://github.com/flotilla-org/flotilla/blob/005c1c1a9d7ba0a26a08db0247039ac157582523/crates/flotilla-daemon/src/peer/ssh_transport.rs)). These mechanisms should be extracted or reused below Flotilla domain types rather than expanded inside the frozen peer layer.

The following remain Flotilla responsibilities:

- `PeerWireMessage`, hello/node/session validation, peer generations, keepalives, retirement, routing, and remote commands/steps are Flotilla's daemon-mesh protocol, not Tender's service stream.
- Resource replication uses a reverse-forwarded resource HTTP socket, starts per-kind supervisors, performs list/watch/relist, applies origin/provenance semantics, and reports replication health. Tender may provide the reachable byte endpoint; [`replicator.rs`](https://github.com/flotilla-org/flotilla/blob/005c1c1a9d7ba0a26a08db0247039ac157582523/crates/flotilla-daemon/src/server/replicator.rs) remains the owner of resource meaning and recovery.
- [`peer_runtime.rs`](https://github.com/flotilla-org/flotilla/blob/005c1c1a9d7ba0a26a08db0247039ac157582523/crates/flotilla-daemon/src/server/peer_runtime.rs) correlates connections with host resources and fails pending routed work on disconnect. Tender should report reachability/stream closure; Flotilla decides what that means for resources and commands.
- Direct SSH execution into a host with no remote daemon remains a separate Flotilla/cleat capability. An SSH jump host is routing configuration; it is not automatically a Tender intermediary, publisher, or authority.

## Raw forwarding-listener failure reporting

`ExitOnForwardFailure=yes` only makes SSH terminate when it cannot establish the requested forwarding listeners. OpenSSH explicitly says it does **not** detect failure to connect to the ultimate destination behind a successfully established listener ([`ssh_config(5)`](https://man.openbsd.org/ssh_config#ExitOnForwardFailure)). Therefore:

1. A local proxy `connect()` can succeed because Tender accepted it.
2. Tender's subsequent open of the publication or SSH's subsequent destination connect can fail.
3. A raw stream has no application-neutral error payload. Injecting text/JSON would corrupt an arbitrary protocol.

The honest contract is prompt EOF/reset/closure plus an out-of-band diagnostic correlated to the accepted stream. Tender can report listener-bind failure synchronously when creating an exposure, and can report remote-open failure through its control/status API, logs, and metrics, but cannot universally turn a post-accept remote failure into the original caller's `connect(2)` error. This matches the premise recorded in [#1866](https://github.com/flotilla-org/flotilla/issues/1866).

## Service publication mechanisms versus Tender responsibilities

Existing service managers demonstrate a clean ownership boundary. systemd socket activation owns the listening socket and passes file descriptors to a service; it can instantiate a service per connection with `Accept=yes` ([`systemd.socket(5)`](https://www.freedesktop.org/software/systemd/man/latest/systemd.socket.html), [socket activation](https://systemd.io/SOCKET_ACTIVATION/)). Windows services are registered and controlled through the Service Control Manager, while named-pipe endpoint creation/security remains in the service process ([Microsoft service programs](https://learn.microsoft.com/en-us/windows/win32/services/service-programs), [Named Pipe Security and Access Rights](https://learn.microsoft.com/en-us/windows/win32/ipc/named-pipe-security-and-access-rights)). OpenSSH owns forwarding listeners only for the lifetime of the SSH connection unless separately managed.

DNS-SD can advertise an instance name, service type, domain, target host/port, and TXT metadata, but discovery records describe how to reach a service; they do not prove that the service is currently reachable or authorize a caller ([RFC 6763 sections 4 and 6](https://www.rfc-editor.org/rfc/rfc6763#section-4)). Tender can reuse this publication vocabulary or a DNS-SD backend later, but must keep authoritative publication identity, grants, and reachability in its own contract.

OpenSSH forwarding also requires server policy to permit the requested direction and endpoint class (`AllowTcpForwarding` and `AllowStreamLocalForwarding`) ([`sshd_config(5)`](https://man.openbsd.org/sshd_config)). That is explicit host preparation, not a property Tender can infer. DNS-SD is a reusable future discovery vocabulary—PTR browse, then SRV/TXT resolution—but publishes TCP/UDP services through DNS records rather than ownership of local sockets or pipes; Tender would still own registration authority, lifetime, and access policy ([RFC 6763 sections 4–6](https://www.rfc-editor.org/rfc/rfc6763#section-4)).

Tender may reuse those lifecycle hooks to run or receive listeners, but it still must add:

- stable publication identity distinct from display name, current route, and local exposure path;
- explicit publisher ownership and separate publish/discover/connect authorization;
- remembered metadata versus current reachability, generations, replacement/conflict rules, and garbage collection;
- authenticated intermediary routing and caller attribution;
- collision-safe local endpoint allocation, filesystem/DACL permissions, stale endpoint recovery, and diagnostics;
- bounded copy loops, open/cancel/deadline semantics, and observable closure reasons.

Tender should not launch or supervise arbitrary published services, modify their object directories, manufacture application credentials, translate descriptors/handles, or decide human audience policy.

## Recommended proof paths

### Proof A: prepared cleat over an explicit SSH destination

1. Start a cleat daemon normally on the remote host and record its real runtime socket. Do not ask Tender to discover from cleat's session directory or start/clean the daemon.
2. Publish exactly that endpoint through an explicitly configured SSH destination. Expose it as a separate Tender-owned local Unix socket with restrictive permissions.
3. Connect the current packet provider in connect-only mode. Verify hello then directory ordering, session channel open, input/output, bounded slow-reader behavior, and closure without injected bytes.
4. Break SSH. Verify the old stream closes and no bytes replay. Restore SSH and verify the unchanged provider creates a new connection, refreshes the directory, reopens channels, and receives a full render.
5. If step 3 cannot be done without spawn/cleanup, first add the narrowly scoped cleat external-endpoint mode described above and test that it never mutates the endpoint or runtime tree.

This proves the generic stream with the client most prepared to recover, without making cleat depend on Tender.

### Proof B: porthole HTTP control as an independent protocol

Publish only porthole's control endpoint, expose the corresponding local Unix socket (then named pipe on Windows), and run the existing `LocalHttpClient` with its ordinary bearer header. Exercise `/info` and one authorized control operation, disconnect during an HTTP request, and confirm failure is ordinary transport closure. Explicitly assert that `capture-transfer.sock` is not advertised as remotely usable.

This proves Tender is not cleat-specific and that application credentials remain end-to-end above the byte stream.

### Proof C: raw listener failure behavior

Keep the SSH tunnel and local listener alive while removing/refusing the remote service socket. Verify local acceptance may occur, no protocol bytes are injected, the stream closes promptly, and the correlated Tender status reports the remote-open failure. Separately verify listener creation failure is returned synchronously and that stale Unix/named-pipe paths cannot silently redirect a stable name to a different publication identity.

## Unresolved questions for the contract tickets

1. What is Tender's portable minimum for directional EOF on Windows named pipes: emulation, full close, or an adapter capability flag?
2. What bounded-buffer limits and fairness rules apply per stream, publication, and intermediary, and how are pressure/timeouts surfaced?
3. What exact connect-only endpoint seam should cleat expose, and can its packet provider select it without coupling directory discovery to ownership?
4. Does a stable local exposure remain bound while a publication is unavailable, and how does its out-of-band diagnostic identify the failed publication generation?
5. Which local credentials should Tender use when dialing Unix sockets/named pipes, and which services rely on peer credentials rather than their own protocol authentication?
6. How are Unix socket modes/ownership and Windows pipe DACLs derived for a local exposure without confusing local access with Tender browse/connect grants?
7. What publication and caller identity is visible through an intermediary, and which component is trusted with plaintext?
8. Does initial SSH forwarding require a remote Tender participant, or may a local Tender manage a plain SSH forward to an explicitly configured existing endpoint? The latter best preserves daemonless targets, but the identity/authority tickets must ratify it.
9. Which porthole operations are acceptable for the control-only proof, and what future application-level protocol would be required before remote capture descriptors/shared memory are meaningful?
10. Which SSH lifecycle pieces move into Tender versus a transport-neutral support crate, so Flotilla can consume them without growing its frozen peer interface?

## Decision supplied to the map

Use ordinary ordered byte forwarding as Tender's first data-plane contract, with SSH stream-local forwarding as the initial transport and local Unix sockets/named pipes as adapters. Prove it first with prepared cleat and then porthole HTTP control. Require application reconnect after failure. Treat peer credentials and descriptor/handle transfer as non-transparent across the proxy. Keep discovery, publication identity/authority/lifetime, exposure ownership, bounded adapter behavior, and diagnostics in Tender; keep service spawning/recovery, application authorization, resource replication, and command routing with their current owners.
