# Research: streaming growing session logs into S3-compatible storage

Ticket: flotilla-org/flotilla#3561 (map #3558). Researched 2026-10-20.

## Summary

S3-compatible stores have no portable append, so a growing log becomes a sequence of immutable objects. The write that makes this safe is the same on all three target stores: `PutObject` with `If-None-Match: *` on a sequence-numbered key. AWS S3 documents it, R2 documents it, and RustFS implements it (verified in source, v1.0.2 at HEAD). Every chunk key is then write-once. A restarted or zombie writer cannot overwrite chunk N; it gets `412` and must compare.

Multipart upload does not work as a live append channel. At 1–20 KB/s with 5–30 s flushes, a chunk is 5–600 KB. Every part except the last must be at least 5 MiB on AWS, R2 and RustFS, and R2 also requires equal-sized parts. Multipart belongs, at most, in an offline consolidation step. S3 Express One Zone has a true append (`x-amz-write-offset-bytes`), but only in AWS directory buckets, so it is lock-in.

Both designs end with the same history tier: coarse per-session segments that the mining pass and viewers read cheaply. They differ only in the **live tier**:

- **(a)** per-session live chunks: simple, with one recovery story, and the reader downloads exactly its session's bytes.
- **(b)** per-writer timeslices plus a reslice step: fewer write requests, by the sessions-per-writer ratio. In exchange, the reslice step must be idempotent, there are two read forms, and a live reader downloads, or range-reads, other sessions' bytes.

At the stated scale, the dollar difference is small. R2 class A requests cost about $13–71 a month for (a) and $4–15 for (b), compaction included, with every session active around the clock. At 10× scale it reaches about $128–711 against $24–83. The lab store (RustFS) has no request billing; its cost there is small-object count and IOPS.

**Recommendation.** Fix the consumer contract as a per-session, byte-offset-addressed stream with a close marker. Build the live tier as (a), shipped by a host-level process from the bind-mounted log directories. Add segment compaction. Keep (b) as a drop-in replacement for the live tier if request volume grows. The open questions at the end need owner rulings.

## Store semantics table

The **Source** row says where each column's facts come from. RustFS facts come from source at `rustfs/rustfs@10a9b158` (2026-10-20, `version = "1.0.2"`); which release the lab runs is unknown.

| Operation / property | AWS S3 (general purpose) | Cloudflare R2 | RustFS (lab) |
|---|---|---|---|
| Read-after-write and list-after-write | Strong for PUT/DELETE and LIST; single-key updates atomic; no cross-key atomicity; concurrent PUTs are last-writer-wins | Strong read-after-write and list; last-writer-wins. Custom-domain caching breaks the guarantees for that domain; the S3 API path is unaffected | Not stated in pages read. Writes take a namespace write lock and re-check preconditions at commit (`set_disk/ops/object.rs`) |
| `PutObject If-None-Match: *` | Yes. First to finish wins, the rest get `412`. A racing DELETE gives `409` (PutObject may retry) | Yes, checked atomically at commit; at most one of concurrent creators succeeds | Yes. Only the wildcard is accepted on writes; an ETag value gets `NotImplemented` (`reject_non_wildcard_write_if_none_match`). Authoritative check under the commit lock |
| `PutObject If-Match: <etag>` | Yes (`412` on mismatch, `404` if deleted) | Yes, plus `If-(Un)Modified-Since` | Yes (`should_prevent_write`) |
| Conditionals on `CompleteMultipartUpload` | `If-None-Match`, `If-Match` | **Not documented** in the compatibility table | Yes (`multipart_usecase.rs`, with tests) |
| Conditionals on `CopyObject` | Destination `If-None-Match`/`If-Match` | Copy-source conditionals only | Copy-source conditionals; wildcard-only destination `If-None-Match` |
| Multipart part size | 5 MiB–5 GiB, last part exempt; up to 10,000 parts | 5 MiB minimum, 5 GiB maximum, 10,000 parts; **all parts except the last equal-sized** | 5 MiB minimum (`GLOBAL_MIN_PART_SIZE`) |
| Incomplete multipart uploads | Persist until a lifecycle `AbortIncompleteMultipartUpload` rule removes them | Auto-aborted after 7 days by default | Not checked |
| True append | Only in S3 Express One Zone directory buckets (`x-amz-write-offset-bytes`, at most 10,000 appends) | Not listed in the compatibility table | Not checked |
| `ListObjectsV2 start-after` | Yes | Yes | Yes (`list_objects_v2(..., start_after, ...)` in source) |
| Versioning | Yes | **No** (`Get/PutBucketVersioning` unimplemented) | Yes (docs) |
| Object lock | Yes | **No** | Not checked |
| Request pricing | GET $0.0004 per 1,000 (confirmed on the pricing page). The PUT/LIST rate was **not confirmed** from the page render | Class A $4.50/M (PUT, LIST, Copy, all multipart calls); class B $0.36/M (GET, HEAD); DELETE and Abort free. Free tier: 1M A + 10M B a month | Self-hosted: no request billing; cost is disk and IOPS |
| Storage | Not confirmed from the page render | $0.015/GB-month standard; 10 GB free | Lab disks |
| Egress | First 100 GB a month free (page); metered beyond | Free | LAN |
| Source | docs.aws.amazon.com conditional-writes, qfacts, Welcome#ConsistencyModel, directory-buckets-objects-append; aws.amazon.com/s3/pricing | developers.cloudflare.com/r2/api/s3/api, /r2/pricing, /r2/objects/multipart-objects, /r2/reference/consistency | github.com/rustfs/rustfs source; docs.rustfs.com/features/versioning |

**Portable subset.** Every design below uses only these operations, which all three stores support:

- `PutObject`, plain or with `If-None-Match: *` or `If-Match`;
- `GetObject`, including ranges and conditional GET;
- `HeadObject`;
- `DeleteObject`;
- `ListObjectsV2` with a prefix and `start-after`.

Avoid the following:

- versioning (absent on R2);
- object lock;
- conditional `CompleteMultipartUpload` (undocumented on R2);
- S3 Express append (AWS-only);
- store event notifications, which have a different mechanism per vendor.

## The two designs compared

### Common ground

These points hold for both designs:

- **The local log file is the durable source.** Owner direction on #2872 puts agent homes and log directories on bind-mounted host directories, and Codex homes are already there. The object store is a copy that may lag. A crash of the shipper loses nothing that is still on disk. Losing the host loses at most the unflushed tail, up to one flush interval plus anything queued.
- **The resource store holds only references.** Owner ruling on #2872 and map #3558: log bytes are never replicated resources. A session's resource records its stream location (key prefix, writer id and epoch) and, once closed, the final sequence, byte count and hash. It never records per-chunk progress.
- **Chunks are cut at JSONL line boundaries.** CodexBar and codeburn both resume parsing at a complete-line offset (`parsedBytes`; `endsLineAt`). If chunks never split a line, a reader can parse each chunk on its own.
- **The source file is identified by a fingerprint.** codeburn keys its cache on `(dev, ino, mtimeMs, sizeBytes)`. If the file shrinks or its inode changes, the shipper starts a new stream generation. It never writes bytes that contradict already-shipped offsets.
- **Each chunk carries a header** of session id, stream generation, sequence number, the source byte range `[start, end)`, the previous chunk's SHA-256, and the writer id and epoch. A JSON first line or a small CBOR frame (`flotilla-cbor` framing) carries it in the body, which keeps it portable. User metadata headers would also work but are harder to read in a range read.
- **Each stream gets one close marker**: `close.json` with the final sequence, total bytes and final hash, written with `If-None-Match: *`. If there is no marker, the stream is open or its writer crashed. Use no per-chunk manifest; it would double the class A writes and add nothing.
- **Staging matches owner direction** ("end-of-session first, live later"). The first increment uploads the whole file at session end as segment(s) plus the close marker. The live tier is added afterwards under the same keys and contract.

### Design (a): one chunk stream per session

**Keys:**

```
logs/<installation>/<session-id>/<gen>/live/<seq:010>      write-once chunk
logs/<installation>/<session-id>/<gen>/seg/<first:010>-<last:010>   compacted segment (history tier)
logs/<installation>/<session-id>/<gen>/close.json          close marker
```

**Write path.** A shipper tails each session file. It flushes when the buffer reaches a size threshold (for example 256 KB), when T seconds pass with buffered bytes, or when a harness hook fires at turn end, whichever comes first. A session with nothing to send costs nothing. The commit is a single `PutObject If-None-Match: *` on the next seq key.

**Live consumers.** A reader bootstraps once. It calls `ListObjectsV2 start-after` on `live/` and `seg/`, which is one class A request, or it starts from the close marker. It then polls `GET live/<next>`; a `404` means "not yet". Readers must not poll with LIST. On R2, LIST is class A ($4.50/M) and GET or HEAD is class B ($0.36/M), so a LIST-polling viewer costs more than the writer does. A better wake-up is a "seq N committed" notice over flotilla's own transport, which is not replicated. The reader then issues one GET per real chunk.

**Ordering and completeness.** Sequence keys plus write-once creation give a total order with no duplicates. Contiguous source offsets show there are no gaps. The hash chain shows there was no rewrite. The close marker's final seq, bytes and hash prove a closed stream is complete. An open stream is provably a complete prefix.

**Crash recovery.** The shipper persists `(fingerprint, offset, next seq, last hash)` locally. It can also rebuild that state from the store with one LIST and a GET of the last chunk's header. If a PUT gets `412`, the shipper reads the winning chunk's header. Its own retry and a zombie or restarted shipper both end up here, and either may have cut a different flush boundary. If the winner's start offset equals the expected one, and its bytes match the local file over its range, the shipper **adopts the winner**: it takes the winner's end offset and hash as the new tail and re-cuts its buffer from there. Only a mismatched start offset, or bytes that differ from the local file, count as divergence. In that case the shipper opens a new generation and flags it. R2's consistency page recommends retrying with `If-None-Match: *` for an in-flight PUT whose outcome is unknown, and this rule makes that retry safe. With adoption, competing writers converge on whoever won each seq, so no epoch fencing is needed.

**Compaction to the history tier.** On close, or every N minutes, the shipper concatenates live chunks into a segment of at most a few MB. It PUTs the segment write-once and then deletes the live chunks; deletes are free on R2. Deleting live chunks races live readers, as slice deletion does in (b). Compaction keeps live chunks for a grace period after the segment lands. Readers take history from `seg/`, poll `live/` only past the last segment, and re-list on an unexpected `404`. Compaction is required, not optional. Reading a month of 5-second chunks costs 15.5M GETs for the mining pass, against 0.26M GETs for 5-minute segments.

### Design (b): writer timeslices plus reslice

**Keys:**

```
slices/<installation>/<writer-id>/<epoch>/<seq:010>        write-once timeslice, all sessions
slices/<installation>/<writer-id>/<epoch>/watermark        reslice progress, advanced with If-Match (CAS)
logs/<installation>/<session-id>/<gen>/seg/...             resliced per-session chunks (history tier)
logs/<installation>/<session-id>/<gen>/close.json
```

**Write path.** Every T seconds the writer, which runs per vessel, per host or as a collector, PUTs one slice that holds frames for all its active sessions. The slice starts with an index of `session → (byte range inside slice, session source offsets [start,end))`. The writer claims an epoch with `If-None-Match: *` on `epochs/<n>` when it starts.

**Reslice.** A reslicer reads slices in order and emits per-session chunks write-once, then advances the watermark with `If-Match`. It deletes slices only after the watermark covers them, plus a grace period of at least the largest reader lag it supports. The hard part is idempotency. A reslice re-run after a crash must produce **byte-identical** objects under the **same keys**. Chunk boundaries must therefore be a pure function of the inputs, for example "segment k of session S = S's frames from slices [k·W, (k+1)·W)", or boundaries derived from session source offsets alone. A `412` on re-emit counts as success only if the stored hash equals the recomputed one. Several things break this:

- boundaries that depend on wall clock or batch timing;
- a writer epoch change in the middle of a session, which leaves frames for one session in two epochs that must be merged by source offset;
- a collector that re-sends an overlapping range after a crash, which must be deduplicated by `(session, offset, hash)`.

**This is the main open design question for (b).** It is not an implementation detail.

**Live consumers.** A session's live bytes live in its writer's slices, so the session resource must name the writer and epoch. The reader takes resliced segments up to the watermark, then polls `GET slices/.../<next>`. It reads the slice index, which a range GET of a fixed-size head can fetch, and range-GETs its own frames. That is two class B requests per slice, or one if it downloads the whole slice and accepts S/H times the bytes. The handover between tiers needs care. A reader that sees a slice vanish re-reads the watermark and switches to segments. Its per-session source offsets make the stitch exact and duplicate-free.

**Ordering and completeness.** Per writer, slice sequence numbers plus write-once creation give the order. Per session, contiguous source offsets across slices and then segments give completeness, and the close marker ends the stream. There are two proof obligations instead of one: per-writer slice continuity, and per-session offset continuity across tiers.

**Crash recovery.** A writer crash means a new epoch. The writer re-ships each session from its last shipped offset, recovered locally or from the last slice index. Duplicates are resolved by offset at reslice time. A reslicer crash means a re-run from the watermark, which is safe only if the step is idempotent as above. When one writer stalls, every session on it stalls. In (a), each session stalls independently.

### Arithmetic

Assumptions:

- S = 30 concurrent sessions, one per crew (Claude subagent sidechain files would add more);
- every session active 24/7, so this is an upper bound that scales linearly with duty cycle;
- a month of 2,592,000 s;
- for (b), H = 6 writers (per host), about 5 sessions per writer;
- reslice/compaction to 5-minute segments.

Prices are R2 standard. AWS PUT/LIST rates were not confirmed from source; the request counts carry over directly.

| Item | Formula | T = 5 s | T = 30 s |
|---|---|---|---|
| Chunk size at 1–20 KB/s | rate × T | 5–100 KB | 30–600 KB |
| (a) live PUTs/month | S × M / T | 15.55M | 2.59M |
| (a) + compaction PUTs | + S × M / 300 | +0.26M | +0.26M |
| **(a) class A, gross** | | **15.8M ≈ $71** | **2.85M ≈ $13** |
| (b) slice PUTs/month | H × M / T | 3.11M | 0.52M |
| (b) reslice output + watermark PUTs | S × M/300 + H × M/300 | 0.31M | 0.31M |
| **(b) class A, gross** | | **3.42M ≈ $15** | **0.83M ≈ $4** (inside free tier) |
| (b) reslice GETs (class B) | H × M / T | 3.11M ≈ $1.1 | 0.52M ≈ $0.2 |
| 10× scale: 300 sessions, 30 writers, (a) | | 158M ≈ $711 | 28.5M ≈ $128 |
| 10× scale, (b) | | 18.4M ≈ $83 | 5.4M ≈ $24 |
| Data volume, 30 sessions | S × rate × M | 78 GB/month at 1 KB/s; 389 GB at 5 KB/s; 1.55 TB at 20 KB/s | same |
| R2 storage for 389 GB retained | × $0.015 | ≈ $5.8 per month of history kept, accumulating (before compression; JSONL compresses well, so measure) | same |
| One live tail, GET every 2 s | M / 2 | 1.3M GETs ≈ $0.47 (class B); as LIST polling ≈ $5.8 (class A) | same |
| Mining pass over one month, all sessions | GETs | 15.5M live chunks ≈ $5.6 and slow; 0.26M segments ≈ $0.09 | |

How to read the table:

- Request savings in (b) equal the sessions-per-writer ratio on the live tier only. Both designs pay the same history-tier writes.
- (b) saves requests and (a) saves bytes. A live tailer in (b) moves S/H times the bytes unless it range-reads. Egress is free on R2 and the LAN, and metered on AWS beyond 100 GB.
- RustFS has no request billing. Object count matters, though. With compaction, (a)'s live chunks are transient, so the steady object count of the two designs is similar. The extra write IOPS from small objects in (a), at 1–6 PUT/s fleet-wide, have not been measured on the lab.
- A per-vessel writer in (b) has H = S, which makes it the same as (a) with extra steps. (b) only pays off with a per-host writer or a collector.

### Lock-in

Both designs use only the portable subset, so neither carries store lock-in. The lock-in risks sit beside the designs:

- relying on R2 or S3 event notifications to wake readers;
- using S3 Express append;
- using versioning to recover from overwrites;
- using conditional `CompleteMultipartUpload` for consolidation, which R2 does not document.

RustFS also quietly rejects non-wildcard `If-None-Match` on writes, so ETag-valued `If-None-Match` writes must be avoided.

## Prior art

- **Vector `aws_s3` sink.** It batches up to 10 MB or 300 s by default and keys objects `date=%F/<unix-epoch>-<uuidv4>.log.gz`. Keys are time-plus-random, not sequenced, so order comes from timestamps and the sink does not prove completeness. The memory buffer is the default and loses data on a crash; a disk buffer syncs every 500 ms. The page does not say whether it uses multipart. The lesson: log shippers trade latency (minutes) for request count, and leave ordering and completeness to the consumer.
- **Fluent Bit `s3` output.** It uses multipart with 5 MiB parts by default and uploads when it reaches `total_file_size` (100 MB) or `upload_timeout` (10 min). An object is invisible until `CompleteMultipartUpload`. The default key is `/fluent-bit-logs/$TAG/%Y/%m/%d/%H/%M/%S`. Its **`$INDEX`** counter, persisted in `store_dir` and resumed after restart, is the closest analogue to a per-stream sequence key. Multipart state in `store_dir` lets uploads finish after a restart, and leftover buffers go up with `PutObject`. This confirms that multipart suits only large, delayed objects.
- **OpenTelemetry GenAI semantic conventions.** These now live in `open-telemetry/semantic-conventions-genai` and have **Development** status. Agent spans carry `gen_ai.conversation.id` (session or thread) and `gen_ai.agent.id`. Content should not be captured by default, and the production pattern they recommend is to store content externally and record references on the spans, through an optional in-process upload hook. This matches the hard constraint here: bytes in the object store, references in the resource store and traces. `gen_ai.conversation.id` is the natural join key for #3550 tracing.
- **CodexBar** (`~/dev/CodexBar`, `Vendored/CostUsage`) reads `~/.claude/projects` and `~/.codex/sessions` JSONL. Its per-file cache is keyed on mtime and size and resumes incrementally from `parsedBytes`. Claude streaming chunks repeat `message.id` and `requestId`, so it deduplicates on `messageId:requestId` and, across files, `sessionId:messageId:requestId`.
- **codeburn** (`getagentseal/codeburn`). Its cache fingerprints `(dev, ino, mtimeMs, sizeBytes)` and resumes at a byte offset only if that offset ends a line (`endsLineAt`). It detects Codex fork replay: a fork copies the parent's history into the new file, so readers must not count it twice. For the shipper this means the raw-byte stream is right, ship verbatim. Deduplication and fork handling belong to readers and the mining pass, so they must not be baked into chunk boundaries.

## Reuse in flotilla

- **`object_store` 0.12.5**, already a dependency of `flotilla-review-bundle-object-store`, has everything the write and read paths need:
  - `PutMode::Create`, which sends `If-None-Match: *` under the default `S3ConditionalPut::ETagMatch`;
  - `PutMode::Update`, which sends `If-Match`, for the (b) watermark CAS;
  - `get_range` and `get_opts` with `if_none_match` (`304`) for cheap re-polls;
  - `head`;
  - `list_with_offset`, which sends `start-after`;
  - `delete`;
  - `put_multipart`, for consolidation only.

  `InMemory` honours `Create` and `Update`, so conditional semantics can be tested in-process. `LocalFileSystem` supports `Create` but not `Update`.
- **`connect_review_bundle`** (`crates/flotilla-review-bundle-object-store/src/lib.rs`) already holds the reusable `AmazonS3Builder` wiring: endpoint, bucket, region, path-style by default, `allow_http`, and session token. **`ReviewBundleStoreConfig`** (`crates/flotilla-resources/src/review_bundle.rs`) is the config shape to generalise. Its name is review-specific, and generalising it touches stored daemon config, so the N→N+1 rules apply.
- **The `ReviewBundleObjects` port** (`crates/flotilla-crew/src/review_bundle.rs`) is too narrow: it has only overwrite `put` and `get`. Session streaming needs a new narrow port with create-only put, `If-Match` put, range get, head, list-after and delete, implemented once over `object_store`. Its **publication-fence pattern**, artifacts first and the well-known `index.json` last, read back for admissibility, is the same idea as chunks first and close marker last.
- **`crates/flotilla-daemon/src/blob_store.rs`** is a second, hand-rolled S3 client: SigV4 over `HttpClient`, content-addressed keys, and `TieredBlobStore` as a local durable store plus a fleet cache. Its keys cannot express sequenced streams, so it is not reusable for streaming. It duplicates `object_store` and is a consolidation candidate. Two things from it are worth keeping:
  - its framing, "fleet stores are caches of the durable local write path", fits session logs exactly;
  - its recorded `s3_blob_contract.yaml` is the model for the HTTP-contract coverage the testing rules require. The current review-bundle adapter is tested only against `InMemory`.
- **Credentials.** `ReviewBundleWriteCredential` is staged into vessels. A host-level shipper reading bind-mounted logs keeps object-store write credentials out of vessels entirely. That is a reason to put the writer on the host, for either layout.
- **Log locations.** `flotilla-agent` hooks already parse `transcript_path` (`crates/flotilla-agent/src/hooks.rs`). The Codex rollout path and the adapter-declared location (#2836) are the other sources the owner named on #2872.

## Recommendation and open questions

**Recommendation:**

1. **Fix the consumer contract first.** A session log is a per-session stream of `(generation, source offset range, bytes)`, in a write-once history tier of segments plus a close marker. Live reads go through whichever live tier exists. Viewers and the mining pass code against this contract, not against key layouts.
2. **Make the live tier per-session chunks, design (a),** written by a host-level shipper: the daemon or a sibling process tails the bind-mounted log directories. Each flush is a single `If-None-Match: *` PUT, with an adaptive flush on size, T seconds, or a turn-end hook. Compaction to segments runs on close and every few minutes.
3. **Ship in stages,** as owner direction asks: whole-file upload at session end as segments plus close marker, then the live tier.
4. **Treat (b) as a replacement live tier, not a rival layout.** It matters when sessions per writer and request prices are both high. At 10× scale on R2 with 5-second flushes it saves about $630 a month. At today's scale it saves $9–56, and RustFS has no request billing. If the owner prefers (b) now, the reslice idempotency rule above is the part to design and test first, with generated crash and re-run interleavings.

**Open questions:**

- Which RustFS release does the lab run? Does it match v1.0.2's conditional-write behaviour? Run a probe with the scoped, non-secret config, without reading credentials.
- What should the flush latency target be? 5 s versus 30 s is a 6× swing in requests.
- Live wake-ups: a flotilla-transport notice of the form "session S committed seq N", which is not replicated, or reader polling? Who may subscribe?
- What is the writer granularity for (b), if adopted: per host, or a collector? How does it interact with agentless SSH hosts and hosted VMs?
- Should compression (zstd) go per chunk, or only on segments? Per-chunk compression helps transfer but complicates byte-range reads.
- What segment size and cadence should the history tier use? Retention ties to #3081, and deletion on expunge only.
- What are the real per-session byte rates, and how many concurrent files (subagent sidechains) does each session have? Measure from existing agent homes before fixing T and the segment size.
- Should `blob_store.rs` and the review-bundle adapter consolidate onto one `object_store`-based client with an HTTP-contract fixture?

Sources: AWS S3 [conditional writes](https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.html), [multipart limits](https://docs.aws.amazon.com/AmazonS3/latest/userguide/qfacts.html), [consistency model](https://docs.aws.amazon.com/AmazonS3/latest/userguide/Welcome.html#ConsistencyModel), [directory bucket append](https://docs.aws.amazon.com/AmazonS3/latest/userguide/directory-buckets-objects-append.html), [pricing](https://aws.amazon.com/s3/pricing/); Cloudflare R2 [S3 API compatibility](https://developers.cloudflare.com/r2/api/s3/api/), [pricing](https://developers.cloudflare.com/r2/pricing/), [multipart](https://developers.cloudflare.com/r2/objects/multipart-objects/), [consistency](https://developers.cloudflare.com/r2/reference/consistency/); RustFS [source](https://github.com/rustfs/rustfs) and [versioning docs](https://docs.rustfs.com/features/versioning); [Vector aws_s3 sink](https://vector.dev/docs/reference/configuration/sinks/aws_s3/); [Fluent Bit S3 output](https://docs.fluentbit.io/manual/pipeline/outputs/s3); [OTel GenAI semantic conventions](https://github.com/open-telemetry/semantic-conventions-genai); [codeburn](https://github.com/getagentseal/codeburn); CodexBar (local clone).
