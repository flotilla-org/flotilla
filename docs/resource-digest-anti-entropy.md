# Resource digest anti-entropy

Log sync remains the primary replication path. Its contiguous-prefix and retention
horizon checks force exceptional origin snapshots when a log cannot resume.
Every 60 seconds, a direct-origin log consumer also compares key/version digests.
This catches a dropped final delete without needing another event or reconnect.

The comparison is scoped to the direct authority, store (durable or observed),
resource kind, namespace, and generation. Store selection uses the existing
separate backends and HTTP/routed observed-store paths. Relayed read views never
prove authoritative absence. The generation and the rest of the collection
identity are hashed into the root; an observed-generation restart still requires
the primary log's generation handoff.

A SHA-256 prefix assigns names to 256 buckets. Each leaf hashes sorted,
length-framed `(name, resourceVersion)` pairs. The root hashes the collection
identity and ordered bucket identities/hashes. Resource bodies and sync timestamps
are excluded. Authoritative and replica indexes use the same hash contract.

A matching root transfers only identity/position metadata and a 64-character hex
hash, with no child hashes, bodies, or replica writes. Computing a root reads at
most 256 cached leaf hashes; it never scans resource keys or serializes bodies.
On mismatch, the consumer requests the complete child-hash array and then complete
snapshots only of differing buckets. This implements the brief's partition
re-snapshot direction: unchanged bodies within a differing bucket travel with
that bucket, but unchanged buckets do not travel.

Child and snapshot requests carry the expected root. Each authoritative read
uses a consistent store lock/SQLite transaction. A concurrent write that changes
the root refuses the old request. The receiver checks the hierarchy, generation,
origin, namespace, bucket membership, duplicate keys, and the snapshot's hash
before treating an omitted key as absent. All differing snapshots are fetched
and validated before replacement, and the authority's root is confirmed again.
Failed reads, unreachable authorities, decode quarantine, truncated snapshots,
and incomplete trees do not remove replicas.

Bucket replacement compares and writes only its selected partition, preserves
unaffected timestamps, and fences removed keys against delayed relay writes.
After repair, an atomic check that the complete replica root matches the authority
proves the log cut. The consumer reopens its watch from that cut, discarding older
buffered events. A later live event therefore resumes log sync without a redundant
full snapshot. A match leaves the existing cursor alone.

In-memory writes refresh the affected leaf before publishing their event. SQLite
maintains derived key/version and leaf-hash tables with connection-local triggers
in the object-write transaction, including replica writes. These triggers cover
production backend writes without imposing custom SQL functions on administrative
connections. SQLite bootstraps preexisting rows once when adding the index. Direct
SQL object mutations bypass the backend contract; quarantine and snapshot hash
validation still refuse incomplete absence proofs.

## Sizing

Run the reproducible CPU/index microbenchmark with:

```sh
cargo test -p flotilla-resources --lib --locked benchmark_digest_index -- --ignored --nocapture
```

Measured in the vessel's unoptimized test build on 2026-10-05:

| Keys | Fanout | Root hash | One changed key: snapshot keys | One leaf refresh |
| ---: | ---: | ---: | ---: | ---: |
| 1,000 | 256 | 490 µs | 4 | 29 µs |
| 100,000 | 64 | 124 µs | 1,559 | 1,665 µs |
| 100,000 | 256 | 489 µs | 352 | 440 µs |
| 100,000 | 1,024 | 1,950 µs | 86 | 134 µs |
| 1,000,000 | 64 | 124 µs | 15,628 | 16,919 µs |
| 1,000,000 | 256 | 488 µs | 3,898 | 4,385 µs |
| 1,000,000 | 1,024 | 1,942 µs | 976 | 1,182 µs |

256 balances sparse body transfer against child-hash exchange (16 KiB of hex
hashes plus JSON/identity overhead, compared with 64 KiB at fanout 1,024). With
2 KiB average bodies, the measured sparse bucket at 100k keys transfers about
704 KiB, and at 1m about 7.6 MiB. If 20% of keys diverge uniformly, all 256 buckets
can differ and transfer the collection. That is mismatch repair, not recurring
full-data comparison.

The benchmark measures bootstrap, root hashing, and sparse/dense **batched index**
refreshes, not SQLite I/O, individual commit latency, or end-to-end network time.
A committed write rehashes its affected leaf: O(bucket keys). Dense individual
commits pay that cost repeatedly. Root work is bounded by fanout regardless of
collection size. The 60-second safety-net interval bounds quiet-store repair
latency while keeping routine exchanges small; log sync retains low-latency
updates. The benchmark is retained for later tuning rather than claiming a
production throughput result.

Full snapshot replacement currently refreshes a leaf for each inserted/removed
key in both backends. Its hashing cost can therefore reach O(N² / 256), unlike
the benchmark's once-per-touched-bucket batch. At 100k+ keys this exceptional
log-gap/horizon repair path can be substantially more expensive than the sizing
microbenchmark suggests. Periodic matching roots do not pay that cost. Deferring
leaf refresh to once per touched bucket during full replacement is a separate
optimization; the current transaction/lock still preserves digest correctness.

At 1m uniformly distributed keys, an individual write hashes about 3,900
key/version entries in its bucket. Write-heavy kinds therefore need separate
commit-throughput measurements; the batched benchmark does not predict them.
An XOR or sum of entry hashes could make leaf updates constant time, but would
replace the ordered, length-framed SHA-256 commitment with a commutative
accumulator and require a separate collision/cancellation analysis and a
versioned digest contract. The current scheme chooses the simpler commitment.
