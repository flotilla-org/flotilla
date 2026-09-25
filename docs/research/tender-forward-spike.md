# Tender raw-forward and replication proof spike (#2012)

Tested on 2026-09-25 at `13e97842` using a loopback SSH server and an in-process Unix-socket proxy. The experiment changed no production wiring. The temporary Rust test edit was removed after the run.

## Raw SSH forward

An unprivileged OpenSSH server listened on `127.0.0.1` in this vessel. A Python process listened on an ordinary Unix socket and accepted exactly one connection. It implemented only a four-byte length prefix followed by raw bytes; there was no Flotilla daemon, Tender, or handshake. The OpenSSH client ran `ssh -N -L <local.sock>:<target.sock> -o ExitOnForwardFailure=yes -o StreamLocalBindUnlink=yes` against that server.

The client sent 1,048,582 bytes, including every byte value and embedded NULs. The target compared every byte and reported SHA-256 `ea0c0aa927bd402a2e01402994b65c8f8cb0ec81a240cb43d40140ed5e312f49`. The target returned a different 786,439-byte binary response; the client compared every byte and reported SHA-256 `9f5a222068aade6d964fe5d611954f5b8be150b23b581ed86a0d8f93a71199e8`. Both comparisons passed. This supports ordered, bidirectional byte delivery through a bare endpoint forward. It does not test cross-host latency, interruption, or recovery.

After the target listener exited, the SSH process and local forwarded socket remained alive. A second client connection failed with `BrokenPipeError`. Thus successful listener setup does not establish continuing target availability; slice 2's adapter needs endpoint-health diagnostics and must let the application reconnect after failure.

## Resource replication through a generic endpoint

I temporarily changed `server::replicator::relay_tests::governor_relay_contract` so its only `feta → kiwi` edge connected through a second Unix socket. A Tokio task accepted on that socket, connected to `feta`'s resource HTTP socket, and copied bytes in both directions. The test supplied the second socket path through `SocketPathSource::new(Some(path)).resolve()` before constructing the existing `HttpBackend`. The other graph edges and the production `replicate_kind_over_http` and `replicate_relay_over_http` paths were unchanged.

`cargo test -p flotilla-daemon --locked in_memory_http_relays_governor_template_across_three_roots -- --nocapture` passed. The contract checks initial replication, live updates, a resource authored after watches start, provenance through a three-root relay, and admission using the replicated template. Because `feta → kiwi → udder` is the only route from the author to `udder`, these checks exercised list and watch traffic over the generic forwarded socket. The path was independent of the peer socket naming scheme. Existing `SocketPathSource` unit tests separately cover initially absent paths and updates between retries; this run did not exercise a path change or real SSH with resource HTTP.

## Slice 2 implication

The two load-bearing assumptions survived these tests: OpenSSH forwards arbitrary bytes to a bare Unix endpoint, and Flotilla's HTTP replication accepts a generic forwarded socket supplied through the existing path seam. No change to the replication protocol appears necessary for this transport cut. The real adapter still needs to prove remote SSH policy, socket lifecycle and permissions, reconnection, and a prepared cleat endpoint on a separate host. The socket path alone conveys reachability; it does not supply publication identity, authorization, or endpoint health.
