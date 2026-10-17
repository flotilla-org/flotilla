# flotilla-fake-forge

A fake Forgejo service shared by a multi-host rehearsal fleet (#3374). One
process holds one in-memory forge for the whole fleet, so pull request and
issue numbers are unique across hosts. Flotilla's Forgejo change-request and
issue providers talk to it unchanged over HTTPS. It answers only the requests
those providers make, and refuses, logs and counts every other request.

State lives in memory only. A restart starts from the seed again.

## Running

```sh
flotilla-fake-forge serve --config /etc/flotilla-fake-forge/config.toml
```

The service logs to stderr. `RUST_LOG` sets the filter (default `info`).
SIGTERM or SIGINT stops it: it stops accepting connections, lets open
requests finish, then exits.

## Configuration

```toml
public_url = "https://fake-forge.rehearsal:3443"   # execution endpoint origin: scheme, host, port; no path
listen = "0.0.0.0:3443"                             # Forgejo API (HTTPS)
admin_listen = "0.0.0.0:3444"                       # admin API (HTTPS), a separate listener
tls_cert = "/etc/flotilla-fake-forge/server.pem"    # certificate chain, PEM
tls_key = "/etc/flotilla-fake-forge/server-key.pem" # private key, PEM
admin_token_file = "/etc/flotilla-fake-forge/admin-token"
auto_merge_after_secs = 120                         # optional; see below
seed = "/etc/flotilla-fake-forge/seed.json"         # optional

[forges.github]
token_file = "/etc/flotilla-fake-forge/tokens/github"
login = "rehearsal-crew"                            # optional, default "rehearsal"

[forges.lab]
token_file = "/etc/flotilla-fake-forge/tokens/lab"
```

- Relative paths resolve against the configuration file's directory.
- Unknown fields are errors.
- Token files are trimmed and must not be empty. Each forge accepts only its
  own token, and the admin token must differ from every forge token.
- `public_url` must be an `https` origin. Every URL the fake renders (`html_url`,
  pagination `Link` headers) starts with it, so it must be the address hosts
  use to reach the fake.
- Each `[forges.<name>]` table mounts a forge at `<public_url>/<name>`.
  `login` is the user the token belongs to: `GET /user` reports it, and pull
  requests opened through the API are authored by it.
- With `auto_merge_after_secs`, the service checks every second and merges
  each open, non-draft pull request that has been open at least that long.
  This lets dummy crews open pull requests and see them merge without a
  driver. Omit the field to merge only through the admin API.

## Seed

The seed creates repositories before the first request, with the default
branch each copied project uses. Without a seed, a repository is created on
first reference, with default branch `main`.

```json
{
  "forges": {
    "github": {
      "repositories": [
        {"owner": "flotilla-org", "name": "flotilla", "default_branch": "main"},
        {"owner": "flotilla-org", "name": "cleat"}
      ]
    }
  }
}
```

`default_branch` is optional and defaults to `main`. Unknown fields are
errors. A seed that names a forge missing from the configuration fails
startup before any repository is created.

## Installing for a rehearsal

`scripts/rehearsal/install-fake-forge.sh` installs the service on one
rehearsal host and prints what every rehearsal host needs.

1. On each rehearsal host with copied stores, list its repositories:
   `flotilla resource list repositories > <host>.json`.
2. Generate the seed. Each `--forge` maps the service URL that Repository
   records name in `spec.forge.service_url` to a mount on the fake:

   ```sh
   flotilla-fake-forge seed --repositories kiwi.json --repositories feta.json --repositories udder.json \
     --forge https://github.com=github --forge https://forgejo.lab.flotilla.work=lab --output seed.json
   ```

   Replicas are merged, and each repository appears once per mount with the
   default branch its status reports. Records whose forge has no mount are
   listed as skipped.
3. On the serving host, as root:

   ```sh
   scripts/rehearsal/install-fake-forge.sh --binary ./flotilla-fake-forge \
     --host rehearsal-feta --ip 10.77.0.12 \
     --forge github=github=https://github.com \
     --forge lab=lab=https://forgejo.lab.flotilla.work \
     --seed seed.json --auto-merge-after 120
   ```

   The script creates a rehearsal-only CA (kept across re-runs), a server
   certificate for the host, one token per forge, an admin token, the config
   and a hardened systemd unit. It then waits until every forge answers its
   token over HTTPS.
4. Follow what it prints. Each rehearsal host trusts the CA, holds the token
   files and gains `[forge_overrides]` lines (and, for dummy crews,
   `[dummy_fleet.shared_forge]`). The host-local bindings under
   `apply/daemon.toml` are installed on every rehearsal daemon.

`--stage DIR` writes the same files under `DIR` without installing anything,
so the output can be reviewed first.

## Pointing a rehearsal at it

Keep the copied Forge, Repository, issue and change-request records unchanged.
Install provider substitutions in each rehearsal daemon's `daemon.toml`:

```toml
[forge_overrides."https://github.com"]
provider = "forgejo"
endpoint_url = "https://fake-forge.rehearsal:3443/github"
token_path = "/etc/flotilla-rehearsal/tokens/github"

[forge_overrides."https://forgejo.lab.flotilla.work"]
provider = "forgejo"
endpoint_url = "https://fake-forge.rehearsal:3443/lab"
token_path = "/etc/flotilla-rehearsal/tokens/lab"
```

Keys are the original canonical service URLs, including installation paths if
present, without trailing slashes. The endpoint mount belongs only to execution.
GitHub stays GitHub in stored data; its issues and PR claims keep github.com URLs.
The override selects the Forgejo adapter and its fake-only token before provider
construction, including for builtin public GitHub without a Forge record. It does
not rewrite Git remotes or change Forge ownership/admission. A missing override
token refuses provider construction rather than using the original credential.
Remove the bindings to restore ordinary provider discovery.

The installer accepts `--forge MOUNT=TOKEN_LABEL=ORIGINAL_SERVICE_URL`, including
original installation prefixes and ports. Legacy hostname lists are also
accepted; their first host supplies an unprefixed HTTPS service URL. Original
SSH aliases remain in the copied Forge record and need no extra override.

For dummy crews, select the logical forge and its fake endpoint separately:

```toml
[dummy_fleet.shared_forge]
service_url = "https://github.com"
kind = "github"
endpoint_url = "https://fake-forge.rehearsal:3443/github"
token_path = "/etc/flotilla-rehearsal/tokens/github"
```

This existing shared-crew setting selects one forge per daemon. Its logical
service and kind must match the repositories assigned to those dummy crews.
Numbers come from the fake, but submitted claims use the original service URL
and its URL syntax (`pull` for GitHub, `pulls` for Forgejo). Existing shared-forge
configs without `endpoint_url` retain their previous behavior.

Hosts must trust the rehearsal CA because providers verify TLS through the
platform trust store. Overrides are local execution settings, never replicated
resource changes. Apply them only to the isolated rehearsal fleet.

## Admin API

Rehearsal drivers use the admin API on `admin_listen` to move work along.
Every request needs `Authorization: Bearer <admin token>`, with exactly that scheme spelling; `bearer` or `token` is refused, as strictly as the Forgejo API. Bodies are JSON,
and unknown fields are refused. Unknown routes, wrong methods, bodies sent to
routes that take none, and query parameters are refused. Refusals are logged
and counted alongside the Forgejo API's.

All routes are under `/admin/v1`. Below, `REPO` is
`/admin/v1/forges/{forge}/repos/{owner}/{repo}`.

| Method and route | Body | Returns |
| --- | --- | --- |
| `GET /state` | | every forge's contents, tokens redacted |
| `GET /refusals` | | `{"refusals": [...], "total": n}`: recent refusals and the count ever refused |
| `POST REPO` | `{default_branch?}` | the repository, created if new; `default_branch` sets its default branch |
| `POST REPO/pulls` | `{head, base, title, body?, user?, draft?}` | 201, the pull request |
| `POST REPO/pulls/{n}/merge` | `{style?, user?}` (style defaults to `merge`) | the pull request |
| `POST REPO/pulls/{n}/close`, `/reopen` | | the pull request |
| `PUT REPO/pulls/{n}/checks` | `{state: pending\|success\|failure\|error}` | the pull request |
| `POST REPO/pulls/{n}/reviews` | `{user, state: APPROVED\|REQUEST_CHANGES\|REQUEST_REVIEW\|COMMENT\|PENDING, body?}` | 201, the review |
| `POST REPO/pulls/{n}/reviews/{id}/dismiss` | | the review |
| `POST REPO/pulls/{n}/push` | | the pull request with a new head commit; earlier reviews become stale |
| `POST REPO/pulls/{n}/delete-head` | | the pull request; its head now reports `refs/pull/{n}/head` |
| `POST REPO/pulls/{n}/ready`, `/draft` | | the pull request |
| `POST REPO/pulls/{n}/requested-reviewers` | `{login}` | the pull request |
| `POST REPO/issues` | `{title, body?, labels?, assignees?, user?}` | 201, the issue |
| `POST REPO/issues/{n}/close`, `/reopen` | | the issue |
| `PUT REPO/issues/{n}/labels` | `{labels}` (replaces the set) | the issue |
| `POST REPO/issues/{n}/comments` | `{user, body}` | 201, the comment |

Pull requests are issues too, as on Forgejo, so the issue routes for close,
reopen, labels and comments also accept a pull request number. Errors use
Forgejo's statuses:

- 404 for an unknown forge or item.
- 405 to merge a closed, merged or draft pull request, or to reopen a merged one.
- 409 for a second open pull request with the same head and base.
- 422 for invalid input.

Check state is stored and reported by `GET /state`. Flotilla's Forgejo
provider does not read commit statuses yet, so check state does not reach
flotilla.

```sh
ADMIN="https://fake-forge.rehearsal:3444/admin/v1"
AUTH="Authorization: Bearer $(cat /etc/flotilla-fake-forge/admin-token)"
REPO="$ADMIN/forges/github/repos/flotilla-org/flotilla"

# Open an issue a dispatch board picks up.
curl -sS -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"title": "Rehearsal task", "labels": ["ready"]}' "$REPO/issues"

# Approve and merge pull request 3.
curl -sS -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"user": "reviewer", "state": "APPROVED"}' "$REPO/pulls/3/reviews"
curl -sS -H "$AUTH" -X POST "$REPO/pulls/3/merge"

# Fail its checks, or close it unmerged.
curl -sS -H "$AUTH" -H 'Content-Type: application/json' -X PUT \
  -d '{"state": "failure"}' "$REPO/pulls/3/checks"
curl -sS -H "$AUTH" -X POST "$REPO/pulls/3/close"

# See what was refused.
curl -sS -H "$AUTH" "$ADMIN/refusals"
```
