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
public_url = "https://fake-forge.rehearsal:3443"   # origin the Forge records name: scheme, host, port; no path
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

## Pointing a rehearsal at it

The rehearsal fleet's Forge records point at the fake with the Forgejo kind.
For github.com projects:

```yaml
forge_id: github-rehearsal
kind: forgejo
hosts: [github.com]
https_url: https://fake-forge.rehearsal:3443/github
```

The record cannot use the id `github`: ADR 0066 reserves it for public GitHub
(kind `github`, `https://github.com`), and admission refuses any other forge
under that id. The rehearsal record claims the `github.com` host instead, so
github.com remotes resolve to it, and the Forgejo providers serve them. The
mount name (`github` above) is only a path on the fake and need not match the
Forge id.

The copied `lab` Forge record changes its `https_url` to
`https://fake-forge.rehearsal:3443/lab` in the same way. Each host's daemon
config maps the forge to a credential holding that forge's token through
`[credentials.forgejo]`. Hosts must trust the CA that issued the fake's
certificate, because the providers verify TLS through the platform trust
store.

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
