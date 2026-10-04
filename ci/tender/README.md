# Cleat through Tender over real SSH

The dedicated `tender-ssh-cleat.yml` job implements [#2562](https://github.com/flotilla-org/flotilla/issues/2562).
It builds cleat `00c072b207dc943f6c93fe3b6b09abaa257695a6` with its pinned Rust,
Zig (checksum verified) and Ghostty revisions and a static Ghostty VT library.
The provenance artifact records source revisions, toolchain configuration,
binary version/hash and installed OpenSSH package versions.

Normal workspace tests retain the non-ignored local Unix socket adapter contracts
and exactly one ignored real-sshd Forward smoke test. The cleat proof is a
separate, non-ignored Linux test target behind `ssh-cleat-proof`; missing tools
fail the provisioned job rather than silently skipping coverage.

To reproduce with OpenSSH client/server and a functional pinned cleat:

```sh
# Use default TMPDIR; do not point at a long checkout-local directory.
unset TMPDIR
export TENDER_TEST_CLEAT=/absolute/path/to/cleat
cargo test -p tender --locked --test ssh_adapter forward_carries_contract_and_reclaims_after_route_replacement -- --ignored
cargo test -p tender --locked --features ssh-cleat-proof --test ssh_cleat -- --nocapture
```

`TENDER_TEST_SSHD` optionally selects an absolute path to sshd. Nothing uses
production hosts or keys: the fixture binds loopback only, creates ephemeral
host/client keys and pins known_hosts, disables ambient SSH configuration and
agent identities, and uses private default-temp directories.

The server publishes cleat's independently owned daemon socket as an ordinary
endpoint. SSH forwards the Tender server, and Tender owns the stable local
exposure used by `cleat packets --socket` and `cleat attach --socket`. The packet
client must find the session in its directory and receive an initial render;
an absent session must be refused. The attach client runs inside a separate
private cleat daemon, providing a real PTY and Ghostty screen capture. No cleat
framing or packet codec is implemented here. Client HOME/runtime/state directories start empty
and must remain empty.

A fixture service disables echo, responds to input, and appends each received
line to a log. The test checks render after input, kills only the SSH process,
observes Tender Unavailable and refusal by a fresh cleat client, and captures
the still-living service directly. Replacing the SSH route lets fresh clients
discover and render the same session, then deliver new input. The exact service
log proves neither previous input nor reconnect input was replayed. Test
subprocess deadlines, child guards and private-daemon teardown bound failures
and clean up processes.
