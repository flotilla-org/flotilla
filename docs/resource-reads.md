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
