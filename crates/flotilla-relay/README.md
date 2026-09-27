# Flotilla event relay

This is the Cloudflare Workers reference implementation of ADR 0041. `MailboxObject` is keyed by the install id, so each install has an independent Durable Object and retention window. The Worker accepts GitHub webhook payloads, verifies `X-Hub-Signature-256`, reduces the event to a hint, and forwards only that hint to the Durable Object. It never writes the webhook payload.

## Configuration

Bind a Worker secret named `RELAY_INSTALLS` containing a JSON object keyed by install id:

```json
{
  "my-install": {
    "consumer_token": "a-long-random-consumer-token",
    "sources": { "github": "github-webhook-secret-for-this-install" }
  }
}
```

Set secrets with Wrangler in the deployment environment; do not put real tokens in `wrangler.toml`. There is no signup or provisioning API. Installs are added by changing the secret. Each `sources` entry is scoped to its install. The current adapter supports `github`.

## Protocol

- `POST /i/<install>/github` with GitHub's `X-Hub-Signature-256`, `X-GitHub-Event`, and `X-GitHub-Delivery` headers. Valid known events append hints and return their deliveries (one per affected pull request for check events). Unknown events return an empty success response.
- `GET /i/<install>/stream?cursor=<last-processed>` with `Authorization: Bearer <consumer_token>`. `Upgrade: websocket` selects the websocket stream; otherwise the request long polls for up to 20 seconds and returns a JSON array of frames. The cursor is mandatory, with `0` meaning a new consumer. A websocket sends replay frames immediately, or a `ready` frame if caught up, then broadcasts new hints.
- Websocket consumers send `{"type":"ack","cursor":N}` and receive `{"type":"acked","cursor":N}`. Long-poll consumers send the same frame to `POST /i/<install>/stream/ack`. Consumers persist their own acknowledged cursor; acknowledgements validate progress but do not trim shared mailbox history. A cursor older than retained history receives `gap` with the oldest available cursor (or `null` when none remain) and the latest issued cursor. Consumers must refresh their demanded subjects after a gap, then resume from `latest_cursor`.

The mailbox keeps at most 256 hints and 24 hours of history. Once either bound removes a hint, a consumer behind that hint gets an explicit gap. Hints have at-least-once semantics. The `flotilla-relay-protocol` crate defines the frames and adapter behavior for the future daemon client.

Local verification: `cargo test -p flotilla-relay-protocol --locked` and `cargo build -p flotilla-relay --target wasm32-unknown-unknown --locked`. A local Workers runtime can use `wrangler dev` after `worker-build` and the wasm target are installed and `RELAY_INSTALLS` is provided as a local secret.
