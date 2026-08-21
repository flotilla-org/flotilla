# Resource reads for scripts

`flotilla resource get`, `list`, and `watch` expose the same versioned
resource-read envelope. Use `--json` when consuming these commands from a
script.

```bash
flotilla resource get convoy implement-1286 --json
flotilla resource list convoy --json
flotilla resource watch convoy --json
flotilla resource watch convoy implement-1286 --json
flotilla resource watch convoy --host feta --json
```

The envelope is stable at `flotilla.work/v1`:

```json
{
  "apiVersion": "flotilla.work/v1",
  "resourceKind": "Convoy",
  "plural": "convoys",
  "namespace": "flotilla",
  "cursor": "eyJ2ZXJzaW9uIjoxLC4uLn0",
  "records": [
    {
      "type": "CURRENT",
      "provenance": { "source": "local", "nodeId": "01J..." },
      "object": {
        "apiVersion": "flotilla.work/v1",
        "kind": "Convoy",
        "metadata": { "name": "implement-1286" },
        "spec": {}
      }
    }
  ]
}
```

`cursor` is an opaque position in the kind's ordered mutation stream. Do not
decode or construct it. A local record has `source: "local"` and the node that
served it. A replicated record has `source: "replica"`, `originRoot`, and
`lastSyncedAt`.

`get` returns one `CURRENT` record. `list` returns the complete current slice
in `records` and a cursor immediately after that slice. An empty list still
returns an envelope and cursor with an empty `records` array.

## Watching and resuming

JSON watch output is JSON Lines: each line is one complete resource-read
envelope. A fresh watch is level-triggered:

1. one envelope contains the complete current slice as `ADDED` records;
2. a `BOOKMARK` record confirms the slice cursor;
3. later envelopes contain `ADDED`, `MODIFIED`, or `DELETED` records.

The current slice is kept in one envelope so recording its cursor cannot skip
part of bootstrap state. Persist the cursor only after processing the whole
line, then pass it back unchanged:

```bash
cursor_file=.flotilla-resource-cursor

flotilla resource watch convoy --json |
while IFS= read -r line; do
  # Process the complete envelope before advancing the durable cursor.
  jq -c '.records[] | select(.type != "BOOKMARK")' <<<"$line"
  jq -r '.cursor' <<<"$line" >"$cursor_file.tmp"
  mv "$cursor_file.tmp" "$cursor_file"
done

flotilla resource watch convoy \
  --from-cursor "$(cat "$cursor_file")" \
  --json
```

Resume delivers mutations strictly after the supplied cursor without sending
the current slice again. The stream may first emit a `BOOKMARK` at the accepted
cursor. A resource name filter applies to both the initial slice and later
mutations.

A watch that ends normally, or is stopped with Ctrl-C, exits with status 0.
Connection failures, expired/invalid cursors, lag, and daemon-reported errors
exit non-zero and write the diagnostic to stderr. Scripts should preserve the
last successfully processed cursor and resume after an error.

`--host <name>` routes all three reads to that peer. The same wire-generation
handshake used by every CLI connection rejects an incompatible daemon before
the read starts.

## One-time single-home duplicate sweep

[ADR 0033](adr/0033-homing-in-practice-creation-cascade-mutation-routing-enforcement.md)
enforces single-home authorship for new records, but fleets upgrading from the
transitional multi-author behavior may still contain Host and PlacementPolicy
records authored on several roots. With every fleet host upgraded, online, and
connected, run:

```bash
flotilla resource dedup-sweep
flotilla resource dedup-sweep --json
```

Run the sweep as the migration step immediately after deploying the ADR 0033
behavior, before relying on placement decisions. Until the standing duplicates
are removed, ordinary replica lookup may select an older authored copy and the
collision condition will remain active.

The command inventories each root's local authored rows, keeps the copy on the
host named by the Host or placement policy, and uses the raw resource-delete
path for every non-home copy. It does not delete a record if the copies cannot
establish exactly one home with an authored copy. That record is skipped,
the report marks it as `needs manual resolution` with the reason, and the sweep
continues deleting other records it can resolve. The report also names each
deletion. A second run reports any manually unresolved duplicates again while
performing no additional deletions for records already converged.

The automated sweep covers Host records and host-scoped PlacementPolicies:
`host_direct` and `docker_per_vessel`, including snapshots of those policies.
A duplicated policy with neither strategy has no natural home the tool can
prove from the record. The sweep reports that policy for manual resolution and
continues; resolve it explicitly with `resource delete --host` on the non-home
roots, then rerun the sweep.

Convoy husks mentioned by ADR 0033 are intentionally outside this migration
sweep's scope. This command only deduplicates Host and PlacementPolicy records;
resolve any standing multi-authored Convoy husks separately.
