# Commands on flotillad's HTTP router

The main Unix socket, per-environment sockets, and authenticated loopback TCP
listener serve these routes. They use the same command dispatcher and crew
placement authorization as framed requests. An environment caller retains its
existing allowlist and project supervision restrictions.

| Method | Path | Request | Success | Body limit |
| --- | --- | --- | --- | --- |
| POST | `/v1/commands` | `Command` | 202, `{id, command_id, url}` and Location | 1 MiB |
| GET | `/v1/commands/{id}?wait=seconds` | none | 200, command record | 0 |
| DELETE | `/v1/commands/{id}` | none | 202, cancellation requested | 0 |
| GET | `/v1/status` | none | 200, status | 0 |
| GET | `/v1/topology` | none | 200, topology | 0 |
| GET | `/v1/repos` | none | 200, repository list | 0 |
| POST | `/v1/repos` | `{path}` | 201 | 64 KiB |
| DELETE | `/v1/repos` | `{path}` | 200 | 64 KiB |
| POST | `/v1/repos/refresh` | `{repo}` | 200 | 64 KiB |
| POST | `/v1/artifacts` | `{kind, subject, summary, media_type, source_path}` | 201, artifact address/digest/URL | 64 KiB |
| GET | `/v1/artifacts` | query: `convoy`, `kind`, `subject` (optional) | 200, `{items}` | 0 |
| GET | `/v1/artifacts/content` | query: `reference`, `destination_path` | 200, size/URL | 0 |
| POST | `/v1/events/replay` | `{last_seen}` | 200, replay events | 64 KiB |
| POST | `/v1/shutdown` | none | 200 | 0 |

POST commands take the command directly, without the framed Execute envelope.
Other request bodies contain the framed request's parameters without a kind tag.
Artifact put/get keep the existing daemon-side source/destination path behavior;
TCP callers cannot use either path operation. Artifact body upload is not offered.
Repository mutations acknowledge the existing asynchronous daemon operation.

Bodies default to JSON. `Content-Type: application/cbor` selects the stack's safe
CBOR reader; `Accept: application/cbor` selects deterministic CBOR responses.
Request and response encodings are independent. Accept quality weights are
honored; unsupported types receive 415/406. JSON responses wrap integers outside
±(2^53 − 1) as `{"u64":"digits"}` / `{"i64":"-digits"}` (ADR 0063).
JSON requests accept ordinary integers. Errors carry `{message}` and use caller
errors in the 4xx range, conflicts as 409, and daemon faults as 5xx.

A command record contains `id`, `state`, all received `steps`, and `outcome`.
The HTTP resource ID is a UUID; the submission's `command_id` identifies the
underlying daemon execution for event correlation (null for a synchronous query).
Queries also return 202, with their result already retained in the record.
`wait` is between 0 and 300 seconds; it ends at completion or the deadline and
returns the current record. Concurrent waiters are supported. Cancelling a
finished command returns 409. Records survive HTTP disconnection but not daemon
restart. At most 1,024 records are retained, evicting completed records as needed;
if all capacity is occupied by active commands, submissions receive 409.
A lost broadcast stream is reported as failed progress collection, with an
explicit warning against blindly retrying the command.

Connection-owned operations (query subscriptions/recovery, fetch-more, focus,
and wait subscriptions) remain on the surface WebSocket as specified by ADR 0065.
Agent hooks have their own HTTP workstream (#3247); no hook route is added here.
