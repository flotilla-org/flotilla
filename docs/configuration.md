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
The remote account needs a writable `HOME`, Git, and `cleat`
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

## Forced checkout archives

Forced checkout removal saves a Git bundle and patch under `.flotilla-archives/`
beside the base clone. A `worktree.tar.gz` is written only when the checkout has
modified or untracked files that Git does not ignore. The
`excluded-ignored.txt` manifest lists excluded ignored paths, one per line.
`flotilla events` shows the archive path in a `CheckoutArchived` event.

The hourly retention sweep removes archives older than 14 days by default.
Set the age in each host's `~/.config/flotilla/daemon.toml`:

```toml
checkout_archive_retention_days = 30
```

Each remote archive root has a five-minute sweep deadline. The remote
environment must provide GNU `timeout`; it supervises `find` and `rm` together
and kills remaining children after a five-second grace period. The client
deadline is ten seconds longer. A failed or timed-out root logs a warning and
the sweep continues with the next root. Local sweeps retain their native path.
If the environment lacks GNU `timeout`, the warning names the dependency and
asks the operator to install coreutils; no unbounded fallback sweep starts.

## Checkout removal concurrency

Checkout teardown shares a daemon-wide queue across namespaces and execution
environments. At most two removals run concurrently by default; a slot covers
archive creation and all deletions belonging to that checkout. Other removals
wait for capacity, and errors release their slot. Background archive task
admission uses the same configured limit.

Set a positive limit in each host's `~/.config/flotilla/daemon.toml`, then restart
the daemon:

```toml
checkout_removal_concurrency = 2
```

The queue limits simultaneous removals, rather than deletion bandwidth. The
operator compares filesystem I/O PSI during teardown bursts after deployment
to decide whether this limit needs further tuning.

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

## Daemon Forgejo identity

Declare each host daemon's Forgejo credential explicitly in that host's
`~/.config/flotilla/daemon.toml`. Keys are Forge IDs and values are
`CredentialSpec` names in the repository's namespace:

```toml
[credentials.forgejo]
flotilla-lab = "lab-forgejo-daemon"
```

The mapping is read whenever a repository provider bag is resolved, so new
discovery requests see the selected identity without restarting the daemon.

The named credential must have a `forgejo` consumer targeting that Forge and a
file source readable in the discovery environment. Its token file uses the
existing Forgejo provider format. Crew and governor credentials delivered by
grants are never selected implicitly, even when only one credential exists.
Without a mapping, Forgejo providers report missing authentication. A missing
or incompatible named credential reports a discovery diagnosis; startup keeps
the repository and independently discovered checkout facts observable, and
continues observing the other roots. Repository operations refuse the same
invalid declaration rather than switching identities.

Unresolved mapping keys appear as an advisory `DaemonForgejoCredentials` host
condition in fleet health. The diagnosis lists the keys and namespaces searched
for declared Forge resources, including replicated declarations. It does not
require a local checkout or block placement. Diagnosis waits for the configured
manifest source's first published pass and connected peers' Forge snapshots;
the next heartbeat clears it when a Forge declaration arrives or the mapping is
corrected or removed. Failures in the diagnostic are logged and skipped so they
do not prevent the heartbeat from publishing.

This host-local setting leaves resource schemas unchanged. Existing project-map
crew/governor declarations need no edits; enabling authenticated observation
requires a separate daemon credential declaration and this host configuration.

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
max_bytes = 52428800
generations = 9
```

The defaults shown retain ten files of 50 MiB each (500 MiB maximum). At the
incident's pre-fix rate of 10 MiB per half-hour that is roughly 25 hours;
transition-only decisions should extend this substantially. Size retention
has no age guarantee: verify a 10 MiB sample spans several hours on feta after
deployment. Existing explicit rotation settings still override these defaults.

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

The fleet charter store binds a repository, branch and path to a home host:

```toml
[manifests]
dir = "/home/alice/.local/state/flotilla/charter-stores" # private Git object cache
source = "https://github.com/example/project-map" # stable provenance identity
reconciler_root = "01J..." # stable Host resource name

[manifests.binding]
kind = "repository"
repo = "https://github.com/example/project-map"
branch = "main"
path = "flotilla"
```

Only the home host fetches and reconciles the bound branch head. It reads blobs
at the fetched commit, so an operator never needs to fast-forward a project-map
checkout. Other hosts receive authored definitions through federation. The
cache is private to the daemon and must be outside development checkouts.
`path` is relative to the repository; omit it to read the repository root.

A single laptop can bind an ordinary local directory instead:

```toml
[manifests]
dir = "/home/alice/.local/state/flotilla/charter-stores"
source = "laptop-charter"
reconciler_root = "01J..."

[manifests.binding]
kind = "local_directory"
directory = "/home/alice/flotilla-charter"
```

Local revisions are SHA-256 hashes of the ordered paths and contents actually
read. No remote or clean Git checkout is required. Source readers include
Markdown, YAML, JSON, TOML and text files; fleet manifests consume JSON/YAML
resource envelopes (`apiVersion`, `kind`, `metadata`, `spec`). Symlinked charter
inputs are refused. Local directories reject every symlink to avoid following
paths outside the source, including symlinked directories. Git ignores entries
whose names are not charter files, including gitlinks and symlinked directories;
charter-named non-regular entries are refused. Both sources require UTF-8 charter
contents, and refusal reasons identify the file.

Ops members use the same reader. Declare their source in `project.yaml`, or in
`Project.spec.repositories[].charter_store`:

```yaml
name: app
members:
  - alias: app
    url: https://github.com/example/app
    roles: [code]
  - alias: ops
    url: https://github.com/example/app-ops
    roles: [ops]
    charter_store:
      host: "01J..."
      source:
        kind: repository
        repo: https://github.com/example/app-ops
        branch: main
        path: operations
```

The designated host reconciles these ops stores periodically without an ops
checkout. Use `kind: local_directory` and an absolute `directory` for local ops
inputs. A project's ops stores must share one home host so the operational
materializer can validate their combined declarations before writing.

Every authored record carries its input revision as provenance. Run
`flotilla resource explain <kind> <name>` to inspect its source, path and commit
(or local revision). `flotilla manifest status` includes each source's last
applied revision and refusal reason; source refusals also raise fleet-health
attention. Fetch and parse failures retain the last applied revision and its
records. Pre-roll `resource validate --from-daemon` reads the same bound heads,
using raw inputs and the candidate binary's parser.

Reconciliation remains additive for fleet resource envelopes: removing a file
does not delete its object. Live drift and unmanaged objects remain subject to
the existing explicit sync/adopt controls. Repository-bound adoption refuses to
write into the private cache: commit changes to the bound branch instead.
Write-through and branch promotion remain future work; the repository/branch
binding identifies their eventual destination.

Previous-generation `[manifests]` configurations without `binding`, and ops
members without `charter_store`, remain readable for one roll under ADR 0047.
They retain their previous clean-checkout behavior. Add explicit bindings to
project-map and ops configurations when rolling this change; the new reader
then removes the manual working-tree update step. Omit `[manifests]` to disable
the fleet store.

See [bounded charter registration and additive rollout](charter-delegation.md)
for optional Project charter pointers, scope refusals, inline inputs and the
post-roll fleet-ops/project-map/wheelhouse companion sequence. Existing stores
and local placement policies can remain unchanged while this support rolls.

## Credential grant permissions

For each credential on a vessel, matching `CredentialGrant` resources must all
list that credential in `permissions`, or all omit it. Mixing the two refuses
admission: the error names the credential and both grants and asks you to make
the unlisted grant explicit. An explicit empty map requests no permissions.
Unlisted-only grants use the credential spec's maximum; explicit grants union
their permissions and are capped by the spec.

Validate the complete manifest directory before applying grant edits, so the
check can compare grants across files:

```bash
flotilla resource validate /path/to/project-map/flotilla-manifests
```

Permission maps are resolved at admission and retained in the convoy snapshot;
repository scopes re-resolve during refresh. Spec edits are read during fresh
credential preparation, while periodic token refresh retains the snapshot or
prepared request's permissions. See [ADR 0044](adr/0044-credential-grants-select-on-the-work.md)
for the absent-maximum case and the complete admission and refresh contract.

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

The crew App identity is separate from `review_bot_login`. Set `consumer.actor_login`
on the GitHub App `CredentialSpec` to its REST bot login, such as
`example-crew[bot]`. The credential adapter exports that value as
`PR_SHEPHERD_AS` to crews, and change-request observation uses the same
declaration to recognize their address markers. Project-map credential
manifests that author GitHub App credentials must add this field in the same
roll; old stored specs still decode without it.

To observe whether review is requested from the operator, set the operator's forge login in the same section:

```toml
[change_request]
operator_login = "your-login"
```

When this is unset, `review_requested_from_owner` remains Unknown rather than assuming that the repository owner is the operator.

## Windows viewers and attached terminals

A Windows viewer connects to an existing daemon over OpenSSH; it does not run a
local daemon. For example, in PowerShell:

```powershell
$env:FLOTILLA_DAEMON = "ssh://crew@kiwi"
flotilla pm connect # inside Wheelhouse, which supplies WHEELHOUSE_SOCKET
flotilla attach --host kiwi <session-or-role-reference>
```

Use Windows Terminal (ConPTY) and the native Windows OpenSSH client (`ssh.exe`
on PATH). Authenticate and accept the server's host key with an interactive
`ssh crew@kiwi` first. Both the daemon bridge and attachment use batch
authentication. The remote account must find `flotilla` in its login-shell PATH.

The daemon endpoint and terminal route are separate: the endpoint supplies
fleet metadata and resolves the session; the viewer opens SSH to the resolved
binding's host. Add routes to the **Windows viewer's** `hosts.toml` in its
Flotilla config directory (or select that directory with `--config-dir`):

```toml
[hosts.kiwi]
hostname = "kiwi" # Windows OpenSSH config alias or reachable DNS name
user = "crew"
expected_host_name = "kiwi" # Flotilla host identity, not necessarily DNS
ssh_multiplex = false
```

Add one entry per host the viewer attaches to, even when it is also the daemon
endpoint. Multiple entries for the same `expected_host_name` are ambiguous and
are refused. Windows OpenSSH does not need Unix ControlMaster sockets.
The connector uses the Windows machine's hostname for local reachability when
its daemon is remote; copying the daemon's `host_name` setting does not make
that remote host local.

Attachment inherits the real console handles. Native OpenSSH carries console
size changes to its remote PTY; Flotilla scopes raw input mode to the SSH child
and restores it after exit or a spawn/wait error. No byte-copying pipe is placed
between OpenSSH and ConPTY.

Remote-daemon attachment uses the same viewer-side routing on Linux and macOS:
SSH batch authentication avoids unattended prompts, and a login shell finds the
remote account's Flotilla executable. Attach consumes `hostname` and optional
`user` from `hosts.toml`; configure ports, identity files and jump hosts in the
viewer's native OpenSSH config under that hostname/alias. `hosts.toml` has no
port, identity-file or jump-host fields. Viewer attaches do not use daemon-side
SSH multiplex settings.

For a remote endpoint, `flotilla --json attach --host kiwi <reference>` resolves
and prints the **viewer-side SSH plan**, rather than the daemon-relative
terminal-pool command. It does not start the attachment. With a local daemon,
JSON output retains the daemon-relative plan.

## Message audit retention

The receiver authority compacts terminal Message bodies after 30 days by default,
on startup and then hourly. Set `message_audit_retention_days` in each host's
`daemon.toml`; `0` disables compaction. Restart the daemon to apply the setting.

```toml
message_audit_retention_days = 30
```

Compaction replaces body text with a SHA-256 digest. It keeps the Message identity,
addresses, references, canonical suppression pointer, receiver receipt, submission
evidence, and status. Exact producer retries still resolve to the original record.
Records are never deleted by this policy. Unresolved submissions, terminal members
of active recovery batches, and follow-ups awaiting workflow continuation keep their
body text. Follow-up references are protected across all locally stored and replicated
Convoy namespaces, using the reference's target Message namespace. Each sweep
releases inbox locks after at most 100 updates, refreshes recovery protection
between batches, and logs individual update failures for retry on the next sweep.
Compacted bodies cannot be recovered by changing the retention setting.

Active inbox reads use maintained in-memory indexes and SQLite indexes, with
receiver, reply correlation, and batch queries available over the resource API.
Historical terminal records are fetched by identity when needed for recovery;
they are excluded from delivery passes before decoding. Resource inventory and
watch bootstrap still include audit records; retention does not prune receipts.

Before rolling a generation that retires Message adoption shims, run the candidate
on **every participating host**, against that host's old daemon:

```sh
/path/to/candidate/flotilla --socket /path/to/daemon.sock resource validate --from-daemon
```

The raw-inventory gate reports the store, namespace, kind, and name of remaining
legacy terminal receipts, terminal queues, convoy authority queues, and unresolved legacy
launch witnesses. Resolve each reported record using the running adoption generation
and explicit operator decisions before retrying. Do not erase uncertain launches
or treat the absence of a terminal as proof of non-submission. All hosts must pass;
keep stale authorities stopped so they cannot republish old queues. The crew's
injected tests prove the gate and storage behavior; these host checks remain the
operator's live acceptance step. Historical golden fixtures remain unchanged.

## Continuing an existing branch or PR

New convoys refuse occupied branch names by default. To deliberately continue
existing work, opt in when starting its replacement convoy:

```bash
flotilla convoy start --project flotilla --continue-pr 2866 --workflow single-agent
flotilla convoy start --project flotilla --continue-branch fix/wip --workflow single-agent
```

Continuation fetches the existing remote branch at its current tip, sets its
upstream so crew pushes update it, and binds any existing open PR as `produces`.
The crew must update that PR rather than opening a replacement; settlement
waits for the existing PR. `--continue-pr` resolves the head and base from the
forge. `--continue-branch` requires a single-repository Project and also works
when the branch has no PR. Use `--workflow` to choose implementation work;
PR-first starts otherwise select the shepherd workflow.

Both options conflict with `--branch`, `--pr`, and each other. A live convoy
holding the branch or PR refuses continuation. Terminal or deleted convoys
release the binding. Merged/closed PRs refuse continuation; reopen a closed PR
explicitly on the forge first. Continuation requires a remote branch: push WIP
before dispatch. Git refuses an occupied physical worktree or local commits
outside the remote history; preserve that work before retiring its checkout.
See [recovery](development.md#rehydrating-work-after-host-loss).

## Crew session archives

Contained Codex and Claude crew homes live on the placement host. Environment
teardown, including forced and recovery teardown, saves session evidence before
reclaiming the backing and home:

```text
~/.local/share/flotilla/session-archive/<convoy>/<vessel>/<role>/<session-id>/
  session.json            # identity and archive time; host-local, never replicated
  identified-log.jsonl    # hook/metadata-identified transcript
  sessions/ or projects/  # identified transcript in its harness layout
  brief.md
  skills/                 # includes the frozen skills manifest
  decision-ledger.md      # if present in the private crew home
```

Launch registers a host-local manifest before starting the harness. Claude
hooks report `transcript_path`; Codex notify/hooks report `rollout_path`, and
live terminal observations also read the crew's Codex metadata index. Session
status, `crew list` and `convoy explain` expose archive references; log content
never enters resource storage or replication. Until a harness reports a native
identity, its launch ID protects its private log trees. Once identified, each
transcript is selected by its recorded path, without scanning for session files.

Archives contain an allowlist of evidence. Auth files, settings, token staging
and symlinks are excluded. If archiving fails, teardown preserves the original
home and retries. Pre-roll homes with no identity manifest are preserved and
teardown reports that operator attribution is required; it never deletes them.
`FLOTILLA_DECISION_LEDGER_DRAFT` names the private-home draft path for crews.

The existing hourly host sweep prunes archives older than 30 days by default,
only after the owning convoy no longer exists. Live or quarantined convoys are
protected across namespaces; a failed liveness lookup defers pruning. Configure
this per host in `~/.config/flotilla/daemon.toml`:

```toml
session_archive_retention_days = 30
```

### Recovering a blocked session archive

A missing identity manifest or unavailable identified transcript holds the
finalizer, including forced deletion. Force never bypasses evidence protection.
Check the diagnostic on the placement host; paths use that daemon's `HOME`
(the same `/var/lib/flotilla` fallback as agent-home provisioning when unset).
Keep this environment stable across daemon restarts.

For an identified log, restore the reported JSONL path from a trusted backup,
or correct `log_path` in the matching
`agent-homes/<environment>/.session-records/<session-id>.json` to the actual
managed host-home JSONL path. Do not point it at auth or an external file.
The finalizer retries automatically. Retry snapshots include later log bytes
and reset `archived_at`, so retention runs from the last successful snapshot.

For pre-roll homes without manifests, or irrecoverably missing logs, stop the
harness and daemon first to avoid concurrent writes. Preserve the entire home
in a private operator quarantine outside `agent-homes` (it may contain secrets):

```bash
# Substitute the exact environment name from the finalizer diagnostic.
environment=env-convoy-example-work
archive_base="$HOME/.local/share/flotilla"
quarantine=$(mktemp -d "$HOME/flotilla-session-quarantine.XXXXXX")
chmod 700 "$quarantine"
mv -- "$archive_base/agent-homes/$environment" "$quarantine/home"
```

Review the quarantine locally, attribute each transcript using harness metadata
or surviving session references, and copy only the documented evidence allowlist
into the proper session archive. Keep credentials private and outside the archive;
do not upload the quarantine. Verify retained logs and attribution before restarting
the daemon. The now-absent managed home unblocks backing reclamation without
requiring deletion of the quarantined evidence. Retention does not manage this
operator quarantine; the operator owns its eventual cleanup.
