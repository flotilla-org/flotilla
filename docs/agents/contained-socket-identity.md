# Contained crew socket identity

New Docker environments receive a dedicated host-owned socket directory at
`/run/flotilla-daemon`, containing only `flotilla.sock`. The host daemon's main
socket, peer sockets, lifecycle lock and launcher are absent from this mount.
The directory name is a bounded hash of the Environment resource identity;
resource names never become filesystem paths. Each environment has one accept
loop, registered before container creation. Failed provisioning and teardown
retire that loop and unlink its socket.

The Docker provider explicitly runs vessels with the daemon's host UID and GID
(`--user`); both the socket directory (`0700`) and socket (`0600`) are owned by
that UID. The directory is protected before binding, so the socket's creation
permissions are never exposed to other host users. Differently mapped UIDs or
Docker user-namespace remapping are not supported by this delivery contract;
the main administrative socket already required the daemon's UID (`0600`).

The directory inode survives daemon shutdown. On adoption, the daemon binds a
replacement socket in the same directory, so running containers see it through
their existing directory mount. Teardown removes the empty directory after the
container has been destroyed. New provisioning without a socket server refuses
with `EnvironmentSocketUnavailable`. A bind failure during adoption fails that
reconciliation and is retried by the controller; it does not destroy the vessel
or mark it failed. Logging and continuing would leave the vessel without its
required daemon access while reporting successful reconciliation.

An environment connection serves only framed client requests. HTTP and peer
handshakes are unavailable. The Environment's declared TerminalSessions and
Vessels supply crew identities. A process may select a role placed in that
same environment; roles sharing an environment share its trust boundary. No
caller-supplied convoy, vessel or crew ID can extend that placement. Missing or
ambiguous placement refuses rather than assuming host authority. Placement is
resolved for each request, so retired sessions cannot preserve access.

A declared TerminalSession is not a calling crew until its `status.crew` is
populated by launch/adoption. During startup, missing crew status therefore
produces `EnvironmentIdentityUnavailable`; it does not grant host authority.
Check that the session's launch/adoption has completed before retrying.

The allowlist includes ordinary crew completion (without force), stalls,
promises, same-convoy handoffs, crew list/capabilities/message contacts,
explanation of the caller's convoy, artifact submission and same-convoy artifact
listing, and hooks for the placed TerminalSession. Other requests refuse with
`EnvironmentCommandDenied`; conflicting command identity refuses with
`EnvironmentIdentityMismatch`. The connection processes one request at a time and receives only its own command
completion events. If broadcast lag loses a completion, it sends a
`CommandFinished` error named `EnvironmentCompletionLost` and closes the
connection. The outcome is unknown: the operation may already have completed,
so callers must inspect its durable state before retrying. Live connections
count toward idle-shutdown accounting; disconnect, teardown and explicit daemon
shutdown release their leases. Global queries, shutdown, arbitrary resource mutation,
explicit host routing, peer traffic and global event replay are refused.

## Roll transition

Docker vessels already running when #3270 rolls keep their existing mounts
until they end. Adoption does not rewrite a running container's mount set.
The main framed socket retains its existing behavior in #3270; #3271 checks
its container caller identity independently. Remove this legacy-main-mount
transition note once every Docker vessel created before the #3270 roll has
ended. Host-direct environments remain outside this isolation contract.

Neither change requires a resource schema migration or new out-of-repository
manifest inputs. The stored-record golden corpus must continue to decode
unchanged.
