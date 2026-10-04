# Tender SSH adapter

`ssh::Server` hosts the slice-1 publication authority behind a private Unix
listener inside a required mode-0700 parent directory. `Server::publish_endpoint` publishes an existing Unix socket and pumps
ordinary application bytes; it never starts or cleans the application's daemon.
`Server::open(socket, private_state_directory, clock)` owns a durable authority:
it loads or creates `identity`, locks `authority.lock`, and loads the versioned
`authority.json`. The state directory must exist with mode 0700. `Server::bind`
remains the explicit ephemeral adapter/test constructor. Host-side grants,
browse/connect policy and replacement assignment are controls on `server.policy`;
they return errors if durable policy cannot be saved.

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
The hosting runtime persists policy and identity, never routes or streams.
Consumer restart-persistent exposure configuration remains the hosting
application's responsibility.

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
[cleat client-path research and pending proof](../../docs/research/tender-ssh-cleat-client-paths.md)
for real-SSH checks and the installed CLI's connect-only limitations.

## Hosting clocks and storage

Inject `runtime::Clock` into `MemoryTender::hosted` or `Server::open`. Ticks are
milliseconds within the clock's opaque epoch; a grant is invalid at
`expires_at <= current_tick`. Sample the injected clock to issue a deadline,
using checked addition for the desired lifetime. `Clock::read` is fallible;
a clock error refuses work and closes streams without poisoning the authority
lock. Restore the clock and reopen the host to resume. Every publish, connect and
exposure open samples the clock under the authority lock, without publisher or
operator activity. Browse/watch and the server's maintenance loop also observe
expiry. Mere expiry retires the identity and refuses new work while established
streams finish. Explicit revocation closes even streams grandfathered by expiry.

On Linux use `runtime::BootClock::new()`: its boot ID and `CLOCK_BOOTTIME` ticks
survive process restarts and include suspend time. Reboot changes the epoch and
expires prior grants. On other platforms inject an equivalent boot-scoped clock,
or use `ProcessClock` with conservative expiry of all prior grants on restart.
Epoch mismatch or clock regression fails closed, never rebases deadlines into a
fresh lifetime. The store remembers the sampled tick high-water mark.

`runtime::Store` has memory and atomic file adapters. File saves sync a private
temporary file, atomically replace the record, and sync the directory before
acknowledgment. One host owns each store; an advisory file lock rejects competing
processes. Storage failure refuses subsequent work and closes live streams;
operators must restore storage and reopen the authority. Unreadable, malformed,
unsupported-version, or identity-mismatched state is refused, never overwritten
as an empty authority.

The version-1 Tender-owned document stores host fingerprint, grants, capability
sets, intermediary trust, explicit assignments, publication metadata, expiry,
reserved/withdrawn state and identity/generation counters. Available records are
written as reserved Unavailable. No live route, registration sender, stream or
application-domain type is serialized. The independent instance key stays in
`identity`. The fixed `tests/fixtures/authority-v1.json` is the first-generation
compatibility contract: future N+1 decoders must read it; add defaults/aliases
when evolving stored records rather than replacing this fixture. Slice 2 stored
only the key, whose format is unchanged; an absent policy document bootstraps
empty, default-deny policy. No out-of-repo resource authoring sources change.
