# Commands on flotillad's HTTP router

The main Unix socket, per-environment sockets, and authenticated loopback TCP
listener serve these routes. They use the same command dispatcher and crew
placement authorization as framed requests. An environment caller retains its
existing allowlist and project supervision restrictions. Bearer tokens and browser
sessions authenticate the same single daemon operator as the main Unix socket;
they can administer repositories and shut down the daemon. All non-crew callers
share that operator's command records, including read and cancel access across
transports and browser sessions. Crew callers can read only their own records;
they cannot cancel commands. Transport-bound callers receive scoped completion
outcomes and no step events, matching the framed connection. Main-socket callers
without kernel peer credentials and hook-token callers are refused.
The TCP listener's Host/Origin policy still applies before routing; artifact path
operations remain restricted to Unix sockets.

| Method | Path | Request | Success | Body limit |
| --- | --- | --- | --- | --- |
| POST | `/v1/commands` | `Command` | 202, `{id, command_id, url}` and Location | 1 MiB |
| GET | `/v1/commands/{id}?wait=seconds` | none | 200, command record | 0 |
| DELETE | `/v1/commands/{id}` | none | 202, cancellation requested | 0 |
| GET | `/v1/status` | none | 200, status | 0 |
| GET | `/v1/topology` | none | 200, topology | 0 |
| GET | `/v1/repos` | none | 200, repository list | 0 |
| POST | `/v1/repos` | `{path}` | 201 | 64 KiB |
| DELETE | `/v1/repos` | `{path}` or query `path` | 200 | 64 KiB |
| POST | `/v1/repos/refresh` | `{repo}` | 200 | 64 KiB |
| POST | `/v1/artifacts` | `{kind, subject, summary, media_type, source_path}` | 201, artifact address/digest/URL | 64 KiB |
| GET | `/v1/artifacts` | query: `convoy`, `kind`, `subject` (optional) | 200, `{items}` | 0 |
| GET | `/v1/artifacts/content` | query: `reference`, `destination_path` | 200, size/URL | 0 |
| POST | `/v1/shutdown` | none | 200 | 0 |

POST commands take the command directly, without the framed Execute envelope.
Other request bodies contain the framed request's parameters without a kind tag.
Artifact put/get keep the existing daemon-side source/destination path behavior;
TCP callers cannot use either path operation. Unix user downloads rely on router
authentication and need no surface connection. Artifact body upload is not offered.
Repository mutations acknowledge the existing asynchronous daemon operation.

When `Content-Type` is absent, request bodies default to JSON.
`Content-Type: application/cbor` selects the stack's safe
CBOR reader; `Accept: application/cbor` selects deterministic CBOR responses.
Request and response encodings are independent. Accept quality weights are
honored, media types ignore case, and quality parameters allow surrounding
whitespace; unsupported types receive 415/406. A malformed Accept that names CBOR
gets a CBOR error body; an uninterpretable/unsupported Accept defaults to a JSON
error because no supported response type was selected. JSON responses wrap
integers outside
±(2^53 − 1) as `{"u64":"digits"}` / `{"i64":"-digits"}` (ADR 0063).
JSON requests accept ordinary integers. Errors carry `{message}` and use caller
errors in the 4xx range, conflicts as 409, and daemon faults as 5xx. Legacy
dispatcher message prefixes currently identify known error classes; unknown
messages default to 500. [#3584](https://github.com/flotilla-org/flotilla/issues/3584)
tracks replacing that inference with typed errors.

A command record contains `id`, `state`, retained `steps`, `outcome`,
`progress_truncated`, and `observation_error`. Progress retains the newest 256
updates within 256 KiB of JSON-encoded event data. Older or individually oversized
updates are dropped and `progress_truncated` becomes true. GET therefore clones
only a bounded progress history. The internal deque caches each update's encoded
size, so trimming neither shifts the entire history nor re-encodes old events. JSON clients must handle ADR 0063's integer
wrappers wherever numeric fields occur; clients wanting native integers can
negotiate CBOR.

The HTTP resource ID is a UUID; the submission's `command_id` identifies the
underlying daemon execution for event correlation (null for a synchronous query).
Queries also return 202, with their result already retained in the record.
`wait` is between 0 and 300 seconds and returns the current record. Concurrent waiters are supported. Cancelling a
finished command returns 409. Records survive HTTP disconnection but not daemon
restart. At most 1,024 records are retained. Successful command dispatch evicts the oldest eligible record
as needed. Completed records become eligible in completion order, including a
recovered unknown record: its finish event assigns a fresh completion sequence. An unknown
record with a closed collector is immediately
evictable. An unknown record whose collector can still recover becomes eligible
for capacity eviction 300 seconds after its first lost observation; repeated lag
does not extend that window. Eviction stops the event collector and releases its
record slot, but never cancels the underlying execution or invents its outcome.
Failed dispatch preserves existing outcomes. Admissions are serialized independently
of record reads and cancellation. If active commands occupy all capacity, submissions receive 409.
Lost observations report `state: "unknown"` and an `observation_error`, without
fabricating an execution outcome. A lagged receiver continues consuming; a later
finish event recovers the real outcome. A closed receiver leaves the outcome
unknown. Unknown is non-terminal while its collector remains live: waits stay
open for a recovered outcome until their deadline or collector closure. Recovery
clears the stale observation error and preserves the progress-truncation flag.
Wait expiry does not terminate execution; later requests can still see its outcome
while the record remains retained. Never blindly retry an execution whose observations were lost.
Cancellation of an existing record whose execution just finished returns 409,
even if the HTTP collector has not observed that completion yet.

Connection-owned operations (query subscriptions/recovery, fetch-more, focus,
replay recovery and wait subscriptions) remain on the surface WebSocket as specified by ADR 0065.
Agent hooks have their own HTTP workstream (#3247); no hook route is added here.
