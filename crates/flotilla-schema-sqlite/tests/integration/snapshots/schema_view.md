# Fixture schema view

Kind tables only; the shared tables are in `fixtures.sql`.

| Table | Role | Columns | From enum expansion | Child tables | Indexes |
|---|---|--:|---|---|---|
| `agent_session` | root | 6 | – | `agent_session_edges` (history), `agent_session_receipts` (history) | – |
| `agent_session_edges` | history | 9 | – | – | `by_edge` (edge, at), `by_time` (at) |
| `agent_session_receipts` | history | 6 | – | – | – |
| `convoy` | root | 7 | – | `convoy_promises` (child), `convoy_resolution` (state data), `convoy_quota` (state data), `convoy_edges` (history) | `open_by_project` (project) |
| `convoy_promises` | child | 4 | – | – | – |
| `convoy_resolution` | state data | 3 | – | – | – |
| `convoy_quota` | state data | 3 | – | – | – |
| `convoy_edges` | history | 9 | – | – | `by_edge` (edge, at), `by_time` (at) |
| `dossier` | root | 7 | – | `dossier_inputs` (child), `dossier_deliverables` (records), `dossier_edges` (history) | – |
| `dossier_inputs` | child | 5 | 3: `tag` 3 | `dossier_inputs_items` (child) | – |
| `dossier_inputs_items` | child | 4 | – | – | – |
| `dossier_deliverables` | records | 9 | 2: `source` 2 | `dossier_deliverables_evidence` (records) | – |
| `dossier_deliverables_evidence` | records | 9 | – | `dossier_deliverables_evidence_metadata` (child) | – |
| `dossier_deliverables_evidence_metadata` | child | 5 | – | – | – |
| `dossier_edges` | history | 9 | – | – | `by_edge` (edge, at), `by_time` (at) |
| `ensure` | root | 11 | – | `ensure_recorded` (state data), `ensure_created` (state data), `ensure_edges` (history), `ensure_attempts` (history) | `by_inbox` (inbox) |
| `ensure_recorded` | state data | 2 | – | – | – |
| `ensure_created` | state data | 2 | – | – | – |
| `ensure_edges` | history | 9 | – | – | `by_edge` (edge, at), `by_time` (at) |
| `ensure_attempts` | history | 8 | 4: `outcome` 4 | – | – |
| `message` | root | 15 | 5: `expectation` 2, `reason` 3 | `message_edges` (history) | `open_by_inbox` (inbox), `deadline` (deadline) |
| `message_edges` | history | 9 | – | – | `by_edge` (edge, at), `by_time` (at) |
| `referral` | root | 24 | 12: `subject` 6, `condition_address` 6 | `referral_references` (child), `referral_edges` (history) | `by_session` (session, seq), `by_reply` (in_reply_to) |
| `referral_references` | child | 11 | 9: `tag` 9 | – | – |
| `referral_edges` | history | 9 | – | – | `by_edge` (edge, at), `by_time` (at) |
| `ticket` | root | 14 | – | `ticket_assignees` (state data), `ticket_notes` (state data), `ticket_edges` (history) | `open_by_queue` (queue) |
| `ticket_assignees` | state data | 2 | – | – | – |
| `ticket_notes` | state data | 3 | – | – | – |
| `ticket_edges` | history | 9 | – | – | `by_edge` (edge, at), `by_time` (at) |
| `terminal_session` | root | 16 | 4: `source` 4 | `terminal_session_env` (child), `terminal_session_labels` (child), `terminal_session_events` (history), `terminal_session_attempts` (history) | `by_vessel` (vessel, ordinal), `live_by_pool` (pool) |
| `terminal_session_env` | child | 3 | – | – | – |
| `terminal_session_labels` | child | 2 | – | – | – |
| `terminal_session_events` | history | 9 | – | – | `by_edge` (edge, at), `by_time` (at) |
| `terminal_session_attempts` | history | 8 | 4: `outcome` 4 | – | – |
| `vessel` | root | 3 | – | – | – |

Tables wider than 30 columns:
- none

States with data (one label column, then data shared by name across the labels):
- `agent_session`.`state`: label `state` and `state_since`; data columns `channel`, `reason`; data tables –
- `convoy`.`state`: label `state` and `state_since`; data columns `reason`; data tables `convoy_resolution`, `convoy_quota`
- `ensure`.`state`: label `state` and `state_since`; data columns `window`, `bound`; data tables `ensure_recorded`, `ensure_created`
- `message`.`state`: label `state` and `state_since`; data columns `receiver_session`, `receiver_host`, `reason`, `reason_by`, `reason_authority`, `watermark`; data tables –
- `ticket`.`state`: label `state` and `state_since`; data columns `priority`, `owner`, `hold_by`, `hold_until`, `reason`, `closed_by`, `signature_who`, `signature_at`; data tables `ticket_assignees`, `ticket_notes`
- `terminal_session`.`state`: label `state` and `state_since`; data columns `identity`, `cause_category`, `cause_detail`; data tables –
