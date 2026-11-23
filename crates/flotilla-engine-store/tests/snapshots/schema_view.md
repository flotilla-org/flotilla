# Message store schema view

Kind tables only; the shared tables are in `message.sql`.

| Table | Role | Columns | From enum expansion | Child tables | Indexes |
|---|---|--:|---|---|---|
| `awaiting_reply` | root | 15 | – | – | `open_by_sender` (sender), `open_by_subject` (subject), `by_message` (message), `deadline` (deadline) |
| `agent_session` | root | 15 | – | – | `by_convoy` (convoy), `by_vessel` (vessel) |
| `convoy` | root | 15 | – | `convoy_subjects` (records), `convoy_duties` (child), `convoy_seats` (records), `convoy_promises` (records), `convoy_retired_duties` (state data) | `open_by_project` (project), `open_by_role` (project, role), `by_launch` (project, launch_of, launch) |
| `convoy_subjects` | records | 6 | – | – | `by_subject` (subject) |
| `convoy_duties` | child | 6 | 1: `scope` 1 | `convoy_duties_scope_promises` (child) | – |
| `convoy_duties_scope_promises` | child | 3 | – | – | – |
| `convoy_seats` | records | 11 | – | – | – |
| `convoy_promises` | records | 12 | – | `convoy_promises_submissions` (records) | `by_label` (unique convoy, label) |
| `convoy_promises_submissions` | records | 8 | – | – | `by_address` (address) |
| `convoy_retired_duties` | state data | 2 | – | – | – |
| `message` | root | 24 | 5: `expectation` 2, `reason` 3 | `message_references` (child) | `by_inbox` (inbox, state, admitted_seq), `delivered_by_inbox` (inbox, delivered_at), `settled_by_inbox` (inbox, state_since), `deadline` (deadline), `by_reply` (in_reply_to), `by_supersedes` (supersedes) |
| `message_references` | child | 5 | – | – | – |
| `person` | root | 4 | – | – | – |
| `service` | root | 4 | – | – | – |

Tables wider than 30 columns:
- none

States with data (one label column, then data shared by name across the labels):
- `awaiting_reply`.`state`: label `state` and `state_since`; data columns `reason`; data tables –
- `agent_session`.`state`: label `state` and `state_since`; data columns `channel`, `activity`, `activity_since`, `activity_cause`, `reason`; data tables –
- `convoy`.`state`: label `state` and `state_since`; data columns `disposition`, `cause`, `reason`; data tables `convoy_retired_duties`
- `message`.`state`: label `state` and `state_since`; data columns `receiver_kind`, `receiver_id`, `receiver_host`, `delivered_at`, `reason`, `reason_by`, `reason_authority`, `watermark`; data tables –

Interned column domains:

| Table | Column | Domain |
|---|---|---|
| `awaiting_reply` | `sender` | `ad` |
| `awaiting_reply` | `recipient` | `ad` |
| `awaiting_reply` | `subject` | `ad` |
| `awaiting_reply` | `kind` | `ak` |
| `agent_session` | `role` | `rl` |
| `convoy` | `launch_of` | `ln` |
| `convoy` | `role` | `rl` |
| `convoy` | `workflow_ref` | `wf` |
| `convoy` | `project` | `ad` |
| `convoy_subjects` | `subject` | `ad` |
| `convoy_duties` | `owner_role` | `rl` |
| `convoy_duties` | `owner_seat` | `st` |
| `convoy_seats` | `role` | `rl` |
| `convoy_seats` | `seat` | `st` |
| `convoy_seats` | `parent_role` | `rl` |
| `convoy_seats` | `parent_seat` | `st` |
| `convoy_seats` | `supervised_by` | `ad` |
| `convoy_promises` | `label` | `pl` |
| `convoy_promises` | `kind` | `pk` |
| `convoy_promises` | `target` | `ad` |
| `convoy_promises` | `owner_role` | `rl` |
| `convoy_promises` | `owner_seat` | `st` |
| `convoy_promises_submissions` | `address` | `ad` |
| `convoy_promises_submissions` | `by_role` | `rl` |
| `convoy_promises_submissions` | `by_seat` | `st` |
| `message` | `sender` | `ad` |
| `message` | `inbox` | `ad` |
| `message` | `expectation_condition` | `ad` |
| `message` | `delivery_condition` | `ad` |
| `message` | `receiver_id` | `rn` |
| `message` | `receiver_host` | `hm` |
| `message` | `reason_by` | `ad` |
| `message` | `reason_authority` | `ad` |
| `message_references` | `address` | `ad` |
