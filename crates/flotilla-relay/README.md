# Flotilla event relay

This is the Cloudflare Workers reference implementation of ADR 0041. `MailboxObject` is a SQLite-backed Durable Object keyed by install id, so each install has its own credentials, mailbox, and retention. The Worker verifies GitHub's `X-Hub-Signature-256`, reduces the event to a hint, and stores only that hint. It never writes the webhook payload.

## Configuration

The Worker holds one secret, and no tenant material:

- `RELAY_OPERATOR_TOKEN_SHA256`: hex SHA-256 of the operator credential that authorizes the admin API. Without it, the admin API refuses every request.

```sh
printf %s "$OPERATOR_TOKEN" | shasum -a 256 | cut -d' ' -f1 | wrangler secret put RELAY_OPERATOR_TOKEN_SHA256
```

Optional settings (`[vars]` in `wrangler.toml`, or `--var` for `wrangler dev`):

- `RELAY_RETENTION_SECS`: how long an unchanged subject is kept. Default 604800 (7 days).
- `RELAY_SUBJECT_CAP`: the most subjects one install retains. Default 10000.

A custom domain (for example `relay.<domain>`) is declared per deployment environment in `wrangler.toml`; see the commented `[env.production]` block. Deploying is an ops step (flotilla-ops). Keep real secrets out of this repository.

## Provisioning

Each install's credentials live in that install's Durable Object. Consumer tokens are stored as SHA-256 digests. Webhook secrets are stored as-is, because HMAC verification needs them. Every admin request carries `Authorization: Bearer <operator token>`:

| Request | Effect |
| --- | --- |
| `POST /admin/installs/<install>` | Create the install; returns `{install, consumer_token: {id, token}}` once. |
| `GET /admin/installs/<install>` | Token and secret ids, creation times, latest cursor, retained subjects. No material. |
| `DELETE /admin/installs/<install>` | Delete the install's credentials and mailbox; close its websockets. |
| `POST /admin/installs/<install>/tokens` | Mint another consumer token; returns `{id, token}` once. |
| `DELETE /admin/installs/<install>/tokens/<id>` | Revoke a token and close websockets opened with it. |
| `POST /admin/installs/<install>/sources/github/secrets` | Add a webhook secret. Body `{"secret": "..."}` (at least 32 bytes) or empty to generate one; returns `{id, secret}`. |
| `DELETE /admin/installs/<install>/sources/github/secrets/<id>` | Revoke a webhook secret. |

To rotate, add the new credential, move the producer or consumer to it, then revoke the old one. Every current secret verifies deliveries, and every current token authenticates consumers. There is no public signup.

Requests for an unknown install fail exactly as requests with a bad credential do (`401 unauthorized`), so install ids cannot be enumerated. A source the relay has no adapter for returns `404` regardless of install.

## Protocol

- `POST /i/<install>/github` with GitHub's `X-Hub-Signature-256`, `X-GitHub-Event`, and `X-GitHub-Delivery` headers. The response is a JSON array of newly recorded deliveries. It has one delivery per affected pull request for check events, and it is empty for ignored events and for a redelivery of a subject's latest delivery. Bodies over 25 MiB (GitHub's maximum delivery size) get `413`; a declared `Content-Length` over the limit is refused before the body is read.
- Handled events: `pull_request`, `pull_request_review`, `pull_request_review_comment`, `pull_request_review_thread`, `check_run`, `check_suite`, `issues`, and `issue_comment`. A comment on a pull request produces a `cr/…` subject; a comment on an issue produces an `issue/…` subject. GitHub subjects are ASCII-lowercased (see `flotilla_relay_protocol::Subject`).
- `GET /i/<install>/stream?cursor=<last-processed>` with `Authorization: Bearer <consumer token>`. `Upgrade: websocket` selects the websocket stream. This is the preferred path. It sends backlog frames immediately, or a `ready` frame when the consumer is caught up, then broadcasts new hints. Without the upgrade, the request long-polls. It returns a JSON array of frames as soon as there is anything to return, or `[]` after `wait` seconds (`&wait=<0..20>`, default 20). An append wakes waiting polls; idle polls do not re-read storage. The cursor is mandatory, and `0` means a new consumer.
- Websocket consumers send `{"type":"ack","cursor":N}` and receive `{"type":"acked","cursor":N}`. Long-poll consumers send the same frame to `POST /i/<install>/stream/ack`. Acknowledgements validate progress but do not trim the mailbox; consumers persist their own cursor.
- Admin and ack bodies are small JSON objects. Bodies over 4 KiB get `413`, and a malformed ack frame gets `400`.

### Read semantics

The mailbox keeps the latest delivery per subject, one SQLite row per subject. Each new hint takes the next cursor and replaces its subject's row, so a burst of events for one pull request occupies one row. Reading from cursor `c` returns every subject whose latest cursor is greater than `c`, oldest first. A consumer resuming after a quiet period gets each changed subject once.

A subject is pruned once its latest hint is older than the retention window, or when the install exceeds its subject cap (oldest first). The relay remembers the newest pruned cursor as its horizon. A consumer whose cursor is below the horizon may have missed a pruned subject, so it gets `{"type":"gap","oldest_cursor":…,"latest_cursor":…}`. It must refresh every subject it demands, then resume from `latest_cursor`. Delivery is at-least-once.

## Development

- Host tests: `cargo test -p flotilla-relay -p flotilla-relay-protocol --locked`. They cover authentication, webhook limits and delivery handling, replay, acknowledgements, and gaps against in-process SQLite through the same `Sql` seam the Durable Object uses.
- Workers target: `cargo build -p flotilla-relay --target wasm32-unknown-unknown --locked`. This checks the runtime glue.
