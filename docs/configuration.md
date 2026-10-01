# Configuration

## Repo tracking

Stored in `~/.config/flotilla/`:

- `observation-roots.toml` — a flat `paths = [...]` list of checkouts observed by this host
- `open-views.toml` — the ordered set of Views opened by the TUI

Repos are added interactively from within flotilla using the `a` key.
The observation-roots schema rejects every field except `paths`; repository
configuration belongs in replicated Repository and Project specs.

### Fork provenance

Repositories that are maintained as forks declare their upstream provenance in
the replicated Repository spec. Issue-source bindings belong to the Project
spec's `issue_source_bindings` list, not the host's observation roots:

```json
{ "upstream": { "url": "https://github.com/zellij-org/zellij", "relation": "fork" } }
```

Fork-stance provisioning clones only the repository's own URL as `origin`; it
does not add the upstream as a remote. Convoy admission requires a workflow
with an in-crew reviewer, such as `implement-review`. A deliberate per-repo
override can admit review-less workflows:

```json
{ "allow_reviewless_workflows": true }
```

## Convoy start attachment

By default, `flotilla convoy start` attaches to the new convoy only when the
daemon has no presentation-manager connector (`flotilla pm connect`) connected.
`--attach` and `--no-attach` always override that heuristic.

To override the default for every convoy start that omits those flags, set
`auto_attach` in `~/.config/flotilla/config.toml`:

```toml
[convoy]
auto_attach = false
```

## Dispatch-time agent selection

Workflow templates name *capabilities* (`code`, `code-review`), never
harnesses. `flotilla convoy start` can bind a capability to a specific agent
harness for that one dispatch:

```sh
flotilla convoy start --project flotilla --issue 1234 \
    --agent claude-code:opus --no-attach

flotilla convoy start --project flotilla --issue 1234 \
    --agent claude-code:sonnet --agent review=codex --no-attach
```

The flag is `--agent [capability=]adapter[:model]` and is repeatable, once per
capability. The bare form applies to the `code` capability — the one every
stock coding workflow's crew selects on — so `--agent claude-code:opus` is
shorthand for `--agent code=claude-code:opus`. Without an override, a
capability resolves through the seeded table: `code` and `coding` to `codex`,
`review` and `code-review` to `claude-code` on `opus`.

Semantics worth knowing:

- The override is written into the convoy's workflow snapshot at admission, not
  carried alongside it. Placement validation, the vessel reconciler, and
  terminal launch all read the same effective requirement from the snapshot's
  selector; the template itself stays capability-only.
- An adapter override does not inherit the seeded capability table's model. The
  seeded `review` capability pairs `claude-code` with `opus`, but
  `--agent review=codex` launches codex with no model, because a model name
  only means something against the harness it was chosen for. Name the model
  explicitly when you want one.
- Placement admission checks the *effective* adapter. Dispatching to a host
  that does not have it is refused, and the refusal names the adapter:
  "workflow requires agent adapter `claude-code`, which is not available in
  placement `lab-feta`".
- Overriding a capability that no agent crew in the workflow selects is
  refused, and the refusal lists the capabilities the workflow does carry.
- Adapter and model tokens are restricted to alphanumerics, `.`, `_`, and `-`.

Reach for it when coding work should run on a different harness than the
default — most often when one subscription's weekly budget is spent and the
work should move to another (the budget direction in
[#1394](https://github.com/flotilla-org/flotilla/issues/1394)). Harness and
model are separate axes: `--agent claude-code:opus` and
`--agent claude-code:sonnet` pick the same harness at different cost.

## Convoy placement admission

### Agentless SSH hosts

An owning daemon can place a trusted, host-direct crew on an SSH host without
running `flotillad` there. Add the host to the owning daemon's
`~/.config/flotilla/hosts.toml`:

```toml
[hosts.beaufort]
hostname = "beaufort.example"
user = "gui-session-user"
agentless_ssh = true
```

The `user` is the SSH login account and the account in which the crew runs.
The remote account needs a writable `HOME`, Git, and either `cleat` or `shpool`
plus the selected agent adapter. SSH access must work in batch mode. The
owning daemon probes the host, publishes its stable Host identity and a
`host-direct-<host-id>` PlacementPolicy, and provisions the checkout and
terminal pool through its SSH connection. Agentless hosts are excluded from
the daemon peer mesh and from contained placement. Attach opens the remote
terminal pool through SSH. No container image is built or pulled.

The admission free-space floor below is checked against the SSH host's
`~/dev/flotilla-repos` checkout volume using its remote `df` result.

Each daemon host refuses new convoy placement when the volume containing its Flotilla
state directory is below a free-space floor. The default is 20 GiB. Override it
per host in that host's `~/.config/flotilla/daemon.toml`:

```toml
[admission]
free_space_floor_gib = 50
```

The state-directory volume is the admission proxy for host capacity; checkout
paths can be configured on other volumes and are not measured by this check.
Hosts that separate those volumes should provide an equivalent capacity guard
for each checkout volume.

If Flotilla cannot identify the state-directory volume or measure its available
space, placement is refused. This fail-closed behavior prevents an unavailable
measurement from silently disabling admission.

Set the floor to `0` only when an external system provides an equivalent
capacity guard.

## Event relay

To receive change-request hints, configure each daemon separately in its
`~/.config/flotilla/daemon.toml`:

```toml
[relay]
endpoint = "https://relay.example.org"
install_id = "my-install"
consumer_token_file = "/home/user/.config/flotilla/relay-consumer-token"
```

The token file contains only the install's consumer token. Keep it readable
only by the daemon's account. Flotilla reads it on each connection, so rotation
takes effect on reconnect. This daemon-owned file is never staged to crews.
The daemon stores its mailbox cursor under its state directory. It tries a
WebSocket first and uses long polling if the upgrade is unavailable. A healthy
connection slows change-request polling to a 15-minute backstop; disconnects
and retention gaps trigger a full refresh of locally owned demanded subjects.

## Credential health

Each daemon's heartbeat probes expiry metadata for held credential material
(the ambient claude login today; declared credentials as adapters learn to
express expiry) and publishes it on the Host resource — timestamps and scope
names only, never material. Expired material refuses dependent dispatch at
admission; expired *and* near-expiry material surfaces in `flotilla host list`
and the TUI fleet health pane. Override the near-expiry warning window
(default 7 days) per host in that host's `~/.config/flotilla/daemon.toml`:

```toml
[credentials]
warning_window_days = 14
```

## Blob stores

Every daemon stores blobs by SHA-256 beneath its state directory in
`blobs/sha256/`. A fleet store is optional. To replicate local blobs to one or
more S3-compatible stores, add an entry per store to the daemon's
`~/.config/flotilla/daemon.toml`:

```toml
[[blob_stores]]
endpoint = "https://storage.example.com"
bucket = "flotilla-artifacts"
region = "us-east-1"
prefix = "installation-a"
credential_file = "/etc/flotilla/blob-store-credentials.json"
view_base_url = "https://artifacts.example.com/flotilla-artifacts/installation-a"
```

`view_base_url` is optional. It is the public viewer root through the bucket
and object prefix; the artifact digest is appended to it. Configure it only
when that URL serves blobs to readers.
Fleet sync is asynchronous, so a URL printed immediately after `artifact put`
may return 404 until the blob reaches the configured store.

Fleet endpoints require HTTPS by default. Plain HTTP is accepted for loopback
hosts (`localhost`, `127.0.0.0/8`, or `::1`) to support local development. For a
trusted non-loopback HTTP endpoint, set `allow_insecure_http = true` in that
store's entry. The daemon logs a warning naming the store for either HTTP
exception and rejects non-loopback HTTP without the opt-in at startup.

The daemon reads the credential file at startup. Its JSON object has
`access_key_id` and `secret_access_key` fields, and optionally `session_token`.
Keep the file readable only by the daemon account. It is never staged into
crew vessels. Writes complete in the local store even when S3 is unavailable;
background sync retries with backoff. `flotilla host list` and
`flotilla host status` show the pending count and last sync error. A blob outlives a failed
host only after sync reaches a fleet store. On startup, sync recovers pending
uploads from local blobs and per-store markers; later passes process new writes
and cache fills without walking the full local blob directory.

## Daemon logging

Each daemon writes structured JSON-lines to
`~/.local/state/flotilla/log/flotillad.jsonl`. The file rotates by size and
remains host-local. Configure the filter and rotation bounds in that host's
`~/.config/flotilla/daemon.toml`:

```toml
[logging]
filter = "info,flotilla_daemon::peer=debug"
max_bytes = 10485760
generations = 4
```

The filter uses `RUST_LOG` directive syntax. When it is omitted, the daemon
uses `RUST_LOG` and then its built-in defaults. Restart the daemon after
changing logging settings; the writer and its rotation bounds are configured
at startup.

Read local or peer logs on demand without SSH:

```bash
flotilla logs --host feta --since 2h --level warn --target flotilla_daemon::peer
```

Output remains JSONL so it can be piped directly to `jq`.

## Resource manifests

A daemon can continuously apply a directory of JSON and YAML resource
documents as additive desired state:

```toml
[manifests]
dir = "/home/alice/dev/project-map/flotilla"
source = "https://github.com/example/project-map"
reconciler_root = "01J..." # stable Host resource name
```

Each file contains one or more full resource envelopes (`apiVersion`, `kind`,
`metadata`, and `spec`). Only the daemon whose Host identity matches
`reconciler_root` starts the loop; other daemons apply nothing even if a stale
clone remains on disk. Remove `[manifests]` entirely from hosts that are not the
declared root. The daemon labels created objects as managed by the manifest
reconciler and records the declared source, relative source path, clean Git
revision, reconciling root, and last-applied spec digest. It fast-forwards a
changed manifest only while the live spec still
matches that digest; live drift and collisions with unmanaged objects are
reported and left untouched. The manifest directory must be tracked and clean;
files with changes not represented by `HEAD` are not applied.

This first manifest-reconciliation slice is deliberately additive: removing a
file does not delete its object, and existing unmanaged objects are never
adopted. Omit `[manifests]` to disable the loop.

## Dependencies

Flotilla auto-detects available tools. Nothing is strictly required beyond git, but more tools unlock more features.

| Tool | Purpose | Required |
|------|---------|----------|
| [git](https://git-scm.com/) | Repo detection, branches, worktrees | Yes |
| [gh](https://cli.github.com/) | GitHub PRs and issues | No |
| [claude](https://docs.anthropic.com/en/docs/claude-code) | Agent sessions, branch name generation | No |
| [cmux](https://cmux.dev) | Terminal workspace manager | No |

## Checkout paths

The Repository spec can override the path template used when creating worktrees:

```json
{ "vcs": { "git": { "checkout_path": "{{ repo_path }}/../work/{{ branch | sanitize }}" } } }
```

## Review bot feedback

Set the GraphQL login of the GitHub review bot in `~/.config/flotilla/config.toml` if it differs from the default `claude`:

```toml
[change_request]
review_bot_login = "review-helper"
```

Only comments from that bot identity are treated as bot review feedback. Other bot comments do not wake a crew.

To observe whether review is requested from the operator, set the operator's forge login in the same section:

```toml
[change_request]
operator_login = "your-login"
```

When this is unset, `review_requested_from_owner` remains Unknown rather than assuming that the repository owner is the operator.
