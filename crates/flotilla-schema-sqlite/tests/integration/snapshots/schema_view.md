# Fixture schema view

Kind tables only; the shared tables are in `fixtures.sql`.

| Table | Role | Columns | From enum expansion | Child tables | Indexes |
|---|---|--:|---|---|---|
| `message` | root | 79 **wide** | 60: `delivery_condition_address` 17, `expectation` 21, `subject` 22 | `message_references` (child), `message_edges` (history), `message_attempts` (history) | `open_by_inbox` (inbox, accepted_seq), `by_reply` (in_reply_to), `deadline` (deadline) |
| `message_references` | child | 24 | 22: `tag` 22 | – | – |
| `message_edges` | history | 9 | – | – | `by_edge` (edge, at), `by_time` (at) |
| `message_attempts` | history | 10 | 4: `outcome` 4 | – | – |
| `inbox` | root | 3 | – | – | – |
| `agent_session` | root | 3 | – | – | – |
| `project` | root | 3 | – | – | – |
| `convoy` | root | 10 | – | `convoy_repositories` (child), `convoy_inputs` (child), `convoy_deliverables` (records), `convoy_edges` (history) | `open_by_project` (project) |
| `convoy_repositories` | child | 7 | – | – | – |
| `convoy_inputs` | child | 6 | 4: `tag` 4 | `convoy_inputs_list_items` (child) | – |
| `convoy_inputs_list_items` | child | 4 | – | – | – |
| `convoy_deliverables` | records | 11 | 3: `source` 3 | `convoy_deliverables_evidence` (records) | – |
| `convoy_deliverables_evidence` | records | 11 | – | `convoy_deliverables_evidence_metadata` (child) | – |
| `convoy_deliverables_evidence_metadata` | child | 5 | – | – | – |
| `convoy_edges` | history | 9 | – | – | `by_edge` (edge, at), `by_time` (at) |
| `terminal_session` | root | 17 | 4: `source` 4 | `terminal_session_env` (child), `terminal_session_labels` (child), `terminal_session_events` (history), `terminal_session_attempts` (history) | `by_vessel` (vessel, ordinal), `live_by_pool` (pool) |
| `terminal_session_env` | child | 3 | – | – | – |
| `terminal_session_labels` | child | 2 | – | – | – |
| `terminal_session_events` | history | 9 | – | – | `by_edge` (edge, at), `by_time` (at) |
| `terminal_session_attempts` | history | 10 | 4: `outcome` 4 | – | – |
| `vessel` | root | 3 | – | – | – |
| `ticket` | root | 10 | – | `ticket_assignees` (state data), `ticket_notes` (state data), `ticket_edges` (history) | `open_by_queue` (queue) |
| `ticket_assignees` | state data | 2 | – | – | – |
| `ticket_notes` | state data | 3 | – | – | – |
| `ticket_edges` | history | 9 | – | – | `by_edge` (edge, at), `by_time` (at) |

Tables wider than 30 columns:
- `message`: 79 columns, 60 from enum expansion

States with data (one label column, then data shared by name across the labels):
- `ticket`.`state`: label `state` and `state_since`; data columns `priority`, `owner`, `reason`, `closed_by`; data tables `ticket_assignees`, `ticket_notes`
