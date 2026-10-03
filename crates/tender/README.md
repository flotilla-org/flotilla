# Tender SSH adapter

`ssh::Server` hosts the slice-1 publication authority behind a private Unix
listener inside a required mode-0700 parent directory. `Server::publish_endpoint` publishes an existing Unix socket and pumps
ordinary application bytes; it never starts or cleans the application's daemon.
Host-side grants, browse/connect policy, expiry ticks, and replacement assignment
remain explicit authority controls on `MemoryTender`.

`ssh::Identity::load_or_create` persists a separate Ed25519 instance key with
mode 0600, atomically installing a complete synced file without replacement. Existing readable-by-group keys are refused. Enrolment supplies the remote key's SHA-256 fingerprint out of band.
SSH host authentication is additional transport authentication, not that pin.
The bounded, version-labelled handshake proves possession of both instance
keys with fresh challenges. Every channel authenticates before processing a
control request; callers cannot send a Session identity assertion to the host.

`ssh::Forward::start(destination, remote_socket, ssh_options)` holds an OpenSSH
stream-local forward. The destination uses ordinary SSH account and ProxyJump
configuration. Host-key checking and batch authentication are required; the
forward lives in a private temporary directory and refuses to unlink preexisting
sockets. `Forward::stop` interrupts only SSH. Peer transport now shares Tender's
forward argument construction, path specifications, and socket/process waiter;
Flotilla discovery, hello messages, reverse-path naming, and domain recovery
stay with their current caller.

`ssh::SshTender` implements `Tender` over that forward. Supply only the caller
identities this client may use, and the pinned host fingerprint. Control records
are capped at 64 KiB and the server admits at most 256 live channels; after an authorized Open response each connection becomes
a raw bidirectional stream. Each accepted exposure connection authenticates and
checks host authority anew. Stream-local SSH channels provide multiplexing and
flow control; local byte pumps use bounded buffers and preserve directional EOF.
No control/error bytes enter an ordinary exposed application connection.

`expose_local` creates a private, user-only Unix socket bound to the host,
publication ID, and caller credential. It remains bound while unavailable and
closes fresh connections promptly. Withdrawal stops acceptance and unlinks the
socket, while expiry permits established streams to finish; explicit revocation
closes streams at the host. Returned addresses belong to this adapter's lifetime.
This slice does not add a daemon/configuration persistence layer beyond the
instance key; durable publication authority and restart-persistent exposure
configuration remain the hosting application's responsibility.

After SSH loss, retained browse/watch metadata becomes Unavailable rather than
Withdrawn. `replace_route` supplies a restored forward; watch re-establishes its
control subscription and fresh connects use that route. Existing application
streams are never migrated or replayed. A remote publisher holds a separate
registration control channel: transient Accept failures retry with backoff from
100 ms to eight seconds while that lease is healthy, but losing it reserves the identity and closes its Published
receiver. The owner explicitly reclaims to obtain the new generation's Lease;
an automatic reclaim would silently stale the caller's existing Lease.
Exposure health checks run once a second while healthy and back off to eight
seconds during unavailability; accept-time admission still checks every open. Endpoint-open failure marks the publication Unavailable and closes its
streams; it does not manage the application process. The host's
publication authority and the route are separate, so a consumer SSH outage does
not withdraw a healthy host-side publication.

Container outward publication and authenticated Tender intermediary relays are
slice 3; an asserted `Session::via` is refused by this direct adapter. Windows
local adapters and Flotilla resource consumption are later slices. See the
[cleat proof and client-path findings](../../docs/research/tender-ssh-cleat-client-paths.md)
for real-SSH checks and the installed CLI's connect-only limitations.
