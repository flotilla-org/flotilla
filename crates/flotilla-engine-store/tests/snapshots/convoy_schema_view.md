# Convoy store schema view

Kind tables only; the shared tables are in `convoy.sql`.

| Table | Role | Columns | From enum expansion | Child tables | Indexes |
|---|---|--:|---|---|---|
| `awaiting_reply` | root | 12 | – | – | `open_by_sender` (sender), `open_by_subject` (subject), `by_message` (message), `deadline` (deadline) |
| `convoy` | root | 8 | – | `convoy_duties` (child), `convoy_seats` (records), `convoy_promises` (records), `convoy_quota` (state data), `convoy_retired` (state data) | `open_by_project` (project) |
| `convoy_duties` | child | 6 | 1: `scope` 1 | `convoy_duties_scope_promises` (child) | – |
| `convoy_duties_scope_promises` | child | 3 | – | – | – |
| `convoy_seats` | records | 8 | – | – | – |
| `convoy_promises` | records | 11 | – | `convoy_promises_submissions` (records) | – |
| `convoy_promises_submissions` | records | 8 | – | – | – |
| `convoy_quota` | state data | 3 | – | – | – |
| `convoy_retired` | state data | 2 | – | – | – |

Tables wider than 30 columns:
- none

States with data (one label column, then data shared by name across the labels):
- `awaiting_reply`.`state`: label `state` and `state_since`; data columns `reason`; data tables –
- `convoy`.`state`: label `state` and `state_since`; data columns `reason`; data tables `convoy_quota`, `convoy_retired`

Interned column domains:

| Table | Column | Domain |
|---|---|---|
| `awaiting_reply` | `sender` | `ad` |
| `awaiting_reply` | `recipient` | `ad` |
| `awaiting_reply` | `subject` | `ad` |
| `awaiting_reply` | `kind` | `ak` |
| `convoy` | `project` | `ad` |
| `convoy_duties` | `owner_role` | `rl` |
| `convoy_duties` | `owner_seat` | `st` |
| `convoy_seats` | `role` | `rl` |
| `convoy_seats` | `seat` | `st` |
| `convoy_seats` | `parent_role` | `rl` |
| `convoy_seats` | `parent_seat` | `st` |
| `convoy_seats` | `supervised_by` | `ad` |
| `convoy_promises` | `kind` | `pk` |
| `convoy_promises` | `target` | `ad` |
| `convoy_promises` | `owner_role` | `rl` |
| `convoy_promises` | `owner_seat` | `st` |
| `convoy_promises_submissions` | `address` | `ad` |
| `convoy_promises_submissions` | `by_role` | `rl` |
| `convoy_promises_submissions` | `by_seat` | `st` |
