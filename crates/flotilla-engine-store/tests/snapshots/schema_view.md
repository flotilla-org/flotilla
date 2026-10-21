# Message store schema view

Kind tables only; the shared tables are in `message.sql`.

| Table | Role | Columns | From enum expansion | Child tables | Indexes |
|---|---|--:|---|---|---|
| `agent_session` | root | 9 | – | – | – |
| `convoy` | root | 8 | – | `convoy_duties` (child), `convoy_seats` (records), `convoy_promises` (records), `convoy_quota` (state data), `convoy_retired` (state data) | `open_by_project` (project) |
| `convoy_duties` | child | 6 | 1: `scope` 1 | `convoy_duties_scope_promises` (child) | – |
| `convoy_duties_scope_promises` | child | 3 | – | – | – |
| `convoy_seats` | records | 8 | – | – | – |
| `convoy_promises` | records | 11 | – | `convoy_promises_submissions` (records) | – |
| `convoy_promises_submissions` | records | 8 | – | – | `by_address` (address) |
| `convoy_quota` | state data | 3 | – | – | – |
| `convoy_retired` | state data | 2 | – | – | – |
| `message` | root | 23 | 5: `expectation` 2, `reason` 3 | `message_references` (child) | `pending_by_inbox` (inbox, admitted_seq), `deadline` (deadline), `by_reply` (in_reply_to), `by_supersedes` (supersedes) |
| `message_references` | child | 5 | – | – | – |

Tables wider than 30 columns:
- none

States with data (one label column, then data shared by name across the labels):
- `agent_session`.`state`: label `state` and `state_since`; data columns `channel`, `activity`, `activity_since`, `activity_cause`, `reason`; data tables –
- `convoy`.`state`: label `state` and `state_since`; data columns `reason`; data tables `convoy_quota`, `convoy_retired`
- `message`.`state`: label `state` and `state_since`; data columns `receiver_kind`, `receiver_id`, `receiver_host`, `reason`, `reason_by`, `reason_authority`, `watermark`; data tables –

Interned column domains:

| Table | Column | Domain |
|---|---|---|
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
| `convoy_quota` | `key` | `vc` |
| `message` | `sender` | `ad` |
| `message` | `inbox` | `ad` |
| `message` | `expectation_condition` | `ad` |
| `message` | `delivery_condition` | `ad` |
| `message` | `receiver_id` | `rn` |
| `message` | `receiver_host` | `hm` |
| `message` | `reason_by` | `ad` |
| `message` | `reason_authority` | `ad` |
| `message_references` | `address` | `ad` |
