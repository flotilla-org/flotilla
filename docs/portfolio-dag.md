# Portfolio DAG board

The portfolio board keeps tracker and fleet facts separate from operator
judgment:

- `scripts/dag-fetch` replaces only the generated JSON layer. It reads GitHub
  issues and pull requests for `flotilla-org/flotilla` and
  `flotilla-org/andamento`, plus exact issue associations from Flotilla convoy
  resources.
- `scripts/flotilla-tickets-dag.authored.json` owns the pulse, detail notes,
  display metadata for groups and shapes, and any edges that are judgments
  rather than native GitHub dependencies.
- `scripts/flotilla-tickets-dag.html` merges both layers in the browser.
  Generated status always wins. A detail note with a different
  `statusAtAuthoring` is rendered with a prominent stale warning.

## Refresh and view

The default generated output preserves the original desk-board location. Copy
the board assets only after moving the legacy HTML's `pulse`, ticket `detail`
cards, shapes, group presentation, and non-native edges into
`flotilla-tickets-dag.authored.json`:

```bash
scripts/dag-fetch
cp scripts/flotilla-tickets-dag.html scripts/dag-board.mjs ~/dev/
python3 -m http.server --directory ~/dev 8000
```

Then open `http://localhost:8000/flotilla-tickets-dag.html`. To work entirely
from the checkout instead:

```bash
scripts/dag-fetch --output scripts/flotilla-tickets-dag.generated.json
python3 -m http.server 8000
```

Open `http://localhost:8000/scripts/flotilla-tickets-dag.html`. The page also
accepts `?generated=URL&authored=URL` for alternate layer locations.

On a new board, the empty authored-layer template can be copied explicitly:

```bash
cp scripts/flotilla-tickets-dag.authored.json ~/dev/
```

Never run that command over a populated authored layer: fetches intentionally
do not read or write authored JSON.

The authored ticket map is keyed by the exact reference, for example:

```json
{
  "tickets": {
    "flotilla-org/flotilla#1091": {
      "detail": "Why this ticket matters right now.",
      "statusAtAuthoring": "at-sea",
      "shape": "diamond"
    }
  }
}
```

When editing a detail, set `statusAtAuthoring` to the status visible at that
moment (`landed`, `at-sea`, `ready`, or `blocked`). Do not update it during a
mechanical refresh: the mismatch is what exposes drift.

`dag-fetch` deliberately includes no wall-clock generation timestamp. Its
`asOf` value is the greatest stable source timestamp, and all maps and lists
are sorted before serialization. With unchanged source observations, repeated
runs therefore produce byte-identical output.
