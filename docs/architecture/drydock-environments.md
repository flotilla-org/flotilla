# Drydock cloud environments

Flotilla's Drydock adapter implements `EnvironmentProvider` over
`drydock::api::ProviderApi`. A socket client and an embedded implementation use
that same port. The current composition root supervises the Rust language host;
the provider itself does not start processes or resolve vendor credentials.

## Controller setup

Install `drydock-host` and `drydock-agent` beside flotillad from the revision in
`workspace.dependencies`. `DRYDOCK_HOST_BIN` and `DRYDOCK_AGENT_BIN` select explicit
companion paths. The controller and guest currently need compatible Linux fleet
tools, as contained Docker crews do.

Put the Daytona credential in `~/.config/flotilla/credentials/daytona.token`.
The file contains only the token. Reference it from `drydock.toml` beside
`daemon.toml`; do not copy its value into configuration:

```toml
[provider.daytona]
module = "daytona"
credential = { file = "~/.config/flotilla/credentials/daytona.token" }
settings = { organization_id = "YOUR_ORGANIZATION", ssh_host_key = "ssh-ed25519 YOUR_PINNED_GATEWAY_PUBLIC_KEY", region = "us" }
```

Drydock owns direct credential resolution, refresh and host supervision. Missing
host binaries and missing/rejected credentials remain advertised as not-ready.
Settings are module-owned and passed through unchanged. No Daytona credential
enters the vessel or its stored Environment record.

Preparation uses `drydock::launch::build_tools_digest()` from the published
Drydock pin `7f14b46c`. The reference suite takes this production digest path.

The base crew image must contain the required agent harnesses, Git, `tar`, `sh`,
`awk`, and GNU coreutils (`du`, `df`, `stat`, `mktemp`), and support the module's SSH recipe. Opt-in lab forwarding also requires
`socat`. Configure lab reach in host-local `drydock.toml`:

```toml
[flotilla.provider.daytona.lab_reach]
hostname = "forgejo.lab.flotilla.work"
port = 443
```

These controller settings never enter the guest environment. Interim lab forwarding binds
loopback port 443 and writes `/etc/hosts`, so this slice requires root in the
vessel. Drydock appends and starts its agent; Flotilla delivers its CLI, cleat,
initial agent material and per-crew forge/model credentials through that agent.
Credentials are published with private file permissions.

## Placement

Create a new PlacementPolicy with these inputs (inside its `spec`):

```yaml
pool: cleat
provider_requirements:
  instance: daytona
  class: cloud_sandbox
cloud_sandbox_per_vessel:
  image: registry.example/crew@sha256:REPLACE_WITH_64_HEX_DIGITS
  size:
    cpus: 2
    memory_bytes: 4294967296
    disk_bytes: 21474836480
  checkout: clone_in_vessel
  env: {}
```

Admission resolves a unique controlling Host from its provider advertisements.
It freezes the selected instance and its declared workspace root with the
vessel's placement snapshot. The Environment stores
`account:daytona/provider:daytona`: the account segment is the credential reference
used for this configured provider. It stores no controller identity. The existing
FulfilmentKind projection records the resolved host for admission's current
capacity/routing machinery. Checkout stays inside the cloud policy input until
#3108. Image harnesses are verified by interior discovery before crews launch.

Existing policies do not use the new inputs, and no out-of-repo manifest needs a
companion change. Host-direct and Docker records retain their old decoders.
Cloud status uses `backing_id`, its provider descriptor and survivor references;
`docker_container_id` remains available for older records.

## Interim reach and recovery

One pinned SSH session carries the Drydock agent transport and remote forwards:

- The vessel CLI and hooks use its dedicated controller socket, as Docker crews
  do. The directory survives controller socket replacement.
- `forgejo.lab.flotilla.work:443` on guest loopback bridges through that session to
  the controller's lab service when explicitly enabled. TLS remains end to end, with no extra guest
  secret and no guest Tailscale.

These forwards are replaced by #3485/#3486 and #3617/#3616 respectively. Egress is
explicitly Open / Advisory until the controlling-host proxy work lands. Forge
and model credential staging follows Docker's existing interim delivery, under
ADR 0044's amendment, and is replaced by #3312.

A lifetime cap maps to `Gone`, and hence lost work, rather than a resumable stop.
Unknown observations stay unknown. Event cursors resume after disconnect;
incomplete snapshots never imply absence. Restart adoption re-enters idempotent
provisioning to restore tools, credential staging and forwards together.

The protocol currently offers no promise that Daytona storage outlives its cap,
so survivors use a controller checkpoint. It is refreshed every 30 seconds and
includes the checkout, agent homes and cleat state. Status stores only its
reference. Build outputs and caches are excluded; apparent input size and the
exact staged archive size are checked before streaming. Failed refreshes retain
the previous complete archive and report stale health in Environment status; destruction
keeps that archive for the convoy's continue/abandon decision and removes its
SSH keys. The archive is capped at 512 MiB, lives in a validated 0700 directory
with 0600 permissions, and includes staged credentials. Until #3744 adds managed
retention, the operator must remove the referenced checkpoint directory after
recovery or abandonment. Never publish the archive. Recent changes
between successful checkpoints can still be lost; pushed branches remain the
recovery floor.

## Acceptance

The hermetic provider suite uses Drydock's real Rust host/reference module. It
covers lifecycle idempotency, preparation ownership, readiness, loss mappings,
Drydock conformance, protected writes through the real agent, and daemon/lab
forwarding with stream cleanup. Its SSH boundary fake has no live provider or
vendor credential.

After the roll, run one GitHub convoy and one lab Forgejo convoy on the new
policy. Each must land a PR. Destroy both and verify `drydock list` has no
remaining sandbox. Those live operator checks are separate from the hermetic
crew gate.
