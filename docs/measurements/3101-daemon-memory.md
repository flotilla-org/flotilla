# #3101: shared daemon inventories and maintained snapshots

Measured on 2026-10-12, host-side Linux/glibc, System allocator. Baseline is
`3a5ab934a`; the same fixture and daemon/Aggregator composition runs before and
after the production patch. No Docker, live hosts, or modified TMPDIR.

## Reproduce

```sh
cargo test -p flotilla-daemon --locked --test daemon_allocations -- --nocapture
MALLOC_ARENA_MAX=1 cargo test -p flotilla-daemon --locked --test daemon_allocations -- --nocapture
```

The target installs a process-wide System-delegating allocator. It reports Rust
requested live bytes, live allocation counts, allocation/reallocation calls, and
requested allocation bytes. On Linux/glibc it also reports `mallinfo2` arena
used/free and mmap bytes. These are not RSS or a cross-platform fragmentation
estimate: C allocations and allocator size rounding are outside the Rust counter.

Each N gets a separate current-thread runtime, whose teardown cancels all daemon
tasks. The real Aggregator's initial convoy ResultSet is the bootstrap barrier,
including N=0. The fixture contains 0/32/224 convoys, one quiet active convoy in
eight and the remainder landed, each with a 16,500-byte diagnostic and frozen
single-agent workflow vessels. It varies 1/8/32 query subscribers and 1/8/32 real
leaf/stall passes. Returned query events are consumed and dropped; subscription
tokens remain live. A separate retaining-owner probe holds 32 watched-generation
readers. Active stall/queued-turn behavior is additionally covered by existing
leaf scenarios, rather than inducing continuous writes in this steady-state test.

## Matched daemon measurements

All byte columns below are raw bytes. Rows use M=32, K=1 after bootstrap.

| Convoys | Before calls/pass | After calls/pass | Before requested/pass | After requested/pass | Before live | After live |
|---:|---:|---:|---:|---:|---:|---:|
| 0 | 68 | 38 | 4,825 | 1,900 | 1,258,587 | 1,259,283 |
| 32 | 15,156 | 54 | 7,002,141 | 2,560 | 5,742,504 | 7,124,464 |
| 224 | 105,501 | 150 | 48,696,637 | 6,520 | 31,901,041 | 41,565,166 |

At N=224, M=32, K=32, the baseline requests 1,558,370,186 bytes in
3,376,586 calls; the fix requests 208,640 bytes in 4,800 calls. Both runs have
zero live-byte growth across this batch. The churn assertion fails against the
unpatched code and passes after the fix. The full baseline target fails in 6.21 s;
the patched target passes in about 0.6 s (timing is descriptive, not a guard).

## Retaining owners and scaling

The daemon/Aggregator startup phase is reported separately from leaf-view startup.
The Aggregator keeps its source inventories and shared result projection; adding
query subscribers in this fixture does not keep a full inventory per subscriber.
The final 24 added query subscribers retain 22,464 bytes for every N, before and
after the fix. Existing query/read-budget tests remain the source of truth for
selector behavior; this scenario adds allocation/lifetime evidence.

The leaf engine's formerly owned local-convoy map now holds indices into one
shared typed generation. Charter reconciliation reuses the same convoy/session/
ensure evidence instead of loading and decoding another inventory from its Store
adapter. Empty queued-turn evidence no longer clones each convoy status.

Direct probes of an additional independent convoy WatchView identify its owners:

| N | Indexed watch state | Shared typed generation | Maintained serialized partition | 32 additional readers |
|---:|---:|---:|---:|---:|
| 0 | 10,367 | 40 | 718 | 24,512 |
| 32 | 741,570 | 628,740 | 750,546 | 24,512 |
| 224 | 5,114,201 | 4,401,139 | 5,250,625 | 24,512 |

These three representations are a bounded O(N) cost per watch view/namespace,
created on demand. Their maps/indexes are shared between snapshots. Current
readers add O(M) handles; steady passes add no O(K) retained state. A watch event
changes one serialized body and its index entries. An outstanding old reader
can retain its generation and trigger copy-on-write of map/index bookkeeping;
unrelated object bodies stay shared. The shared typed generation is invalidated
on change and rebuilt on its next read, rather than cloned on every steady pass.

This deliberately trades about 9.7 MB of fixed daemon live bytes for removing
48.7 MB of allocation churn per pass. It does **not** demonstrate a reduction in
live bytes, identify the owners of the reported 76 live inventory arrays, or
reproduce the 20-hour gigabyte footprint. It establishes bounded ownership for
this measured daemon topology; it does not model peer counts or finished-convoy
retention policy (#3081).

## Allocator decision

At N=224, M=32, after the same pass matrix:

| System/glibc metric | Before | After | After, MALLOC_ARENA_MAX=1 |
|---|---:|---:|---:|
| Arena bytes | 56,283,136 | 48,795,648 | 48,668,672 |
| Used arena bytes | 33,973,424 | 43,867,808 | 43,870,432 |
| Free arena bytes | 22,309,712 | 4,927,840 | 4,798,240 |

The arena footprint decreases about 13.3% despite the fixed cache cost.
Remaining free arena memory is about 4.70 MiB; limiting arenas saves only about
0.12 MiB. Keep System and existing arena settings. A stock allocator is not
adopted from this short, single-thread scenario, which cannot establish Linux
thread-arena or macOS long-lived fragmentation. Follow-up #3104 adds cheap
production health diagnostics so remaining fleet retention can be measured.

## Guard rails and mutation evidence

- One fixed 64 KiB live-growth budget across all K-pass batches at each M.
- Fixed 256 KiB/pass allocation budget, independent of N/M, catching a full
  inventory clone or JSON rebuild even when all temporary allocations are freed.
- Bounded extra query subscriber memory and 64 KiB total retention for 32
  snapshot readers, plus pointer identity of their shared typed generation.
- Generated create/update/delete and duplicate-name histories compare maintained
  snapshots to view queries and verify every saved generation stays unchanged.
- Existing memory/SQLite/HTTP view contracts now exercise maintained snapshots,
  self-origin shadowing, selectors, and recovery after lost watch events.
- Existing counted-definition property verifies unrelated definitions are not
  serialized when the maintained snapshot receives a change.
- Borrowed freshest-observation selection is differential against the existing
  owned selection, including exact replica ties and local precedence.

Targeted mutants remove typed-generation invalidation and skip serialized-object
updates. Each fails the generated snapshot scenario and is reverted. A third
mutant changes the replica tie comparison from `>=` to `>` and fails
the differential freshness property; it is reverted.

Stored shapes and wire shapes are unchanged. No outside resource authors or
manifests need updates. The supplied issue snapshot is the only out-of-repository
input; no external source repositories or live fleet stores were used.
