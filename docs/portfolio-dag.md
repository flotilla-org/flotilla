# Portfolio DAG board

The portfolio board keeps tracker and fleet facts separate from operator
judgment:

- `scripts/dag-fetch` replaces only the generated JSON layer. It reads GitHub
  issues and pull requests for `flotilla-org/flotilla` and
  `flotilla-org/andamento`, plus exact issue associations from Flotilla convoy
  resources.
- `scripts/flotilla-tickets-dag.authored.json` owns the pulse, detail notes,
  display metadata for groups, stages, shapes, landed rollups and any edges
  that are judgments rather than native GitHub dependencies.
- `scripts/flotilla-tickets-dag.template.html` is the original hand-board
  renderer. Its `board-data` element is the only composition seam; keep the
  rest of the file unchanged.
- `scripts/dag-compose` combines the two JSON layers and embeds the result in
  a copy of that template. Closed tickets move to `landed` and tickets carried
  by active convoys move to `flight`; otherwise the authored stage remains the
  operator's judgment. Open PR/CI and convoy/host/phase facts are appended to
  the authored card detail.

## Refresh and view

The default paths preserve the desk-board workflow. Refreshing facts and
composing them produces one self-contained HTML file at
`~/dev/flotilla-tickets-dag.html`:

```bash
scripts/dag-fetch
scripts/dag-compose
open ~/dev/flotilla-tickets-dag.html
```

Use explicit paths to create a fixture or work entirely from the checkout:

```bash
scripts/dag-fetch --output scripts/flotilla-tickets-dag.generated.json
scripts/dag-compose \
  --generated scripts/flotilla-tickets-dag.generated.json \
  --output scripts/flotilla-tickets-dag.composed.html
```

The composer also accepts `--authored` and `--template`. Fetches intentionally
do not read or write the authored layer or template.

The authored ticket map is keyed by the exact reference, for example:

```json
{
  "tickets": {
    "flotilla-org/flotilla#1091": {
      "title": "The hand-written card title",
      "detail": "Why this ticket matters right now.",
      "statusAtAuthoring": "at-sea",
      "stage": "flight",
      "shape": "diamond"
    }
  }
}
```

Authored `title`, `detail`, `dagLabel`, and note values retain the trusted HTML
from the hand-board (`<a>`, `<br>`, and `<b>` are used intentionally). Treat
these as operator-authored presentation, not untrusted tracker text. The
composer HTML-escapes all generated tracker and convoy facts before appending
them. Tickets curated directly from another repository receive a qualified DAG
node ID and use their generated per-ticket URL rather than the board-wide repo
link.

When editing a detail, keep `stage` as the operator's intended column and set
`statusAtAuthoring` to the generated status visible at that moment (`landed`,
`at-sea`, `ready`, or `blocked`). Do not update either during a mechanical
refresh: generated `landed` and `at-sea` facts overriding that authored stage
are what make stale notes visibly move.

`dag-fetch` deliberately includes no wall-clock generation timestamp. Its
`asOf` value is the greatest stable source timestamp, and all maps and lists
are sorted before serialization. With unchanged source observations, repeated
runs therefore produce byte-identical output.
