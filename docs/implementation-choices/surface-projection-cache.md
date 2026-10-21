# Surface projection lifetime and changed keys

## Workload

The root `WORKLOAD.md` sizes the fleet at 1,000 / 10,000 live convoys,
1,000 sustained / 10,000 burst change events per second, and 50 / 500 view clients
(target / ceiling). Projection and publication remain in the client process.
There are no new durable-store or network crossings. Existing named-query and
resource-watch snapshots establish the baseline, then contiguous deltas update
resident inputs. Query gaps and resource-watch gaps retain the scoped repair
policy in `pm-connect-recovery.md`.

An idle TUI frame performs zero projection calls and shares the cached table by
`Rc`. One raw table and one raw project-panel set are retained, each with at most
one filtered result; navigation/search replaces the entry. Rows, columns and
metadata still come from the existing bounded query windows, including partiality
and fetch-more actions. Result-set/state deltas and host-input events invalidate
the cache. Selection, scrolling and animation do not. Filter changes transform
the cached result once without querying or projecting again. A source-search
change selects different query inputs and replaces the raw projection.

`pm connect` retains an indexed dependency graph. A convoy, standing role,
subject observation or inbox record joins only rows sharing its actual identity
or relationship. Awareness headers and individual entries are separate keys;
a project header is not a bridge joining all its convoys. A node delta is diffed
into changed header/entry keys. Shared subject reverse links and standing-role
attempts are recomputed together. Removing a bridge retires the old component
and projects its surviving components separately. Definition changes can affect
aliases throughout the catalog and deliberately invalidate the graph. Liveness
mode changes also invalidate it. These are explicit global inputs, not ordinary
row changes.

## Candidate walk and cost

- Re-projecting and diffing the complete catalog per event costs at least
  `change_events * running_convoys`: 100 million row visits/s per client at the
  ceiling, before fields, vessels or recipes. Repeating it for idle frames adds
  work without any new input.
- A single dirty flag avoids idle work but still performs that full scan on each
  delta. Memoizing recipe minting alone leaves the row copies, joins and diff.
- Caching row fragments but re-merging every fragment per event only moves the
  full scan. Re-merging every contributor to a shared project also scans all
  siblings whenever one changes.
- The chosen graph indexes dependency -> members and node -> last component.
  Delta application and projection visit changed components. Catalog assembly
  keeps ordered contributors **per fact**, replacing only facts in retired/new
  fragments and diffing only touched targets. Shared project facts use an ordered
  lookup for their remaining contributor, not a scan of all sibling convoys.
  Namespace discovery likewise queues newly encountered namespaces instead of
  scanning every convoy on every event. Alias/forge resolution is cached per
  live source and evicted when its last input leaves the graph.

For independent rows, one changed key costs one row projection plus ordered
index/fact updates, independent of fleet size. The counter test verifies one
actual recipe mint at 1, 1,000 and 10,000 sessions; idle publication mints zero.
The 10,000-session test (including all bootstrap sizes) took 2.77 s in the
unoptimized local test build. This is a deterministic work-count assertion, not
a claim about fleet CPU or frame latency. A 1,000-convoy project test verifies
that one convoy change projects two keys with awareness enabled (convoy + entry),
and one without it. A header-state change projects one header.

Let `I` be held inputs, `J` their relationship edges and `F` published facts.
Resident storage is O(I + J + F): input rows, graph indexes, current fragments,
per-fact contributors, the assembled catalog and acknowledged catalog. The
contributor index adds a value/key per contributing fact; it retains no event
history. The scalar-field upper-bound sketch is four resident fact values of
roughly 100 bytes each per fact (fragment, contributor, assembled, acknowledged),
or about 40 MB per 100,000 facts before map/key overhead and variable-length
fact payloads. Real row sizes and relationship fanout determine this cost;
large inbox facts are not assumed to be scalar. Clients retain their existing
query cover and resource-watch lifetime, rather than adding another history
window. A future daemon-owned catalog can share this rebuildable projection
across clients without a wire or storage migration.

## Assumptions and expiry

Named-query deltas are contiguous; gaps recover snapshots before applying more
changes. The connector's existing sequence guard enforces this. Resource
snapshots remain kind/namespace scoped and applied only after their bookmark.
Project grouping is **not** a dependency edge; only a changed parent presentation
field invalidates its entries. The separate Header/Entry keys enforce that seam.
The full projector remains the oracle in differential tests, including component
splits. Landed-subject expiry has a deadline index, so an idle heartbeat visits
only due components; TTL heartbeat reasserts cached facts without re-projecting.

Components are conservative dependency closures, not a promise of constant work
for a shared subject/role with many related rows. If real relationship fanout
makes those closures dominate, refine the same node/dependency seam into
per-target joins. Do not reconnect sibling entries through their project merely
to simplify invalidation. Stored resources, Quint specifications, provider
interfaces and presentation wire formats are unchanged.
