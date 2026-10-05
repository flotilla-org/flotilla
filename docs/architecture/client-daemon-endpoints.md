# Client daemon endpoints

A Windows viewer can reach an existing fleet daemon over SSH without hosting
or spawning a local daemon:

```text
flotilla --daemon ssh://udder status
flotilla --daemon ssh://udder pm connect --wheelhouse-socket \\.\pipe\wheelhouse
```

`FLOTILLA_DAEMON=ssh://udder` selects the same endpoint; an explicit `--daemon`
wins. A path after the destination selects a remote executable, for example
`ssh://udder/home/robert/candidates/flotilla`. The destination can include a
user and port. `--socket` and `--daemon` are mutually exclusive CLI options.
An explicit `--socket` overrides `FLOTILLA_DAEMON`. Local daemon lifecycle
commands refuse a remote endpoint.

The remote executable must provide the hidden `daemon-bridge` subcommand.
It connects to that host's existing Unix daemon socket and copies stdin/stdout
bidirectionally, including half-closes. It does not interpret messages, start
the daemon, or replace it. A candidate CLI can therefore be tested beside the
installed fleet CLI while the existing daemon keeps running.

The remote command uses POSIX shell quoting and the non-interactive SSH PATH.
If that PATH does not contain `flotilla`, select its absolute path in the
endpoint suffix.

The client runs OpenSSH with no PTY and batch authentication. SSH owns host-key
and key authentication through the user's existing configuration. The message
session owns the SSH child, terminating it when the session is dropped. Failed
connections report the bounded tail of SSH's stderr; the Hello handshake has a
30-second deadline covering connection, authentication and remote startup.
Long-lived clients currently retry SSH authentication/configuration failures
with backoff; permanent connection-error classification is tracked in #2589.

The client and daemon perform their ordinary Hello handshake end to end.
Protocol version and protocol-source fingerprint must agree; build IDs are
diagnostic metadata. Fingerprinting normalizes checkout line endings so a
Windows checkout can speak to a Unix build of the same protocol sources.

## Relationship to Tender

Tender owns service publication, discovery and routes, and yields raw byte
streams. This bootstrap route uses an explicitly configured SSH destination
without requiring remote Tender publication. `DaemonEndpoint` is client
connection configuration, not a second service directory or forwarding owner.
Its SSH stream is adapted through the existing `stream_message_session` seam.
A future Tender-backed endpoint can adapt an opened stream at the same seam;
protocol handling and callers remain in the client.

Windows command recipes and remote terminal attachment are separate work in
flotilla#2470. This endpoint makes the daemon reachable; it does not change the
shell quoting used by those recipes.
