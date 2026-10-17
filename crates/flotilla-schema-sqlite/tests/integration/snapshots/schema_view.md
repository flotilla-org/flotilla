# Fixture schema view

Kind tables only; the shared tables are in `fixtures.sql`.

| Table | Role | Columns | From enum expansion | Child tables | Indexes |
|---|---|--:|---|---|---|
| `message` | root | 22 | 8: `expectation` 2, `reason` 6 | – | `open_by_inbox` (inbox), `deadline` (deadline) |
| `agent_session` | root | 6 | – | `agent_session_receipts` (history) | – |
| `agent_session_receipts` | history | 6 | – | – | – |
| `convoy` | root | 7 | – | `convoy_promises` (child), `convoy_resolution` (state data), `convoy_quota` (state data) | `open_by_project` (project) |
| `convoy_promises` | child | 4 | – | – | – |
| `convoy_resolution` | state data | 3 | – | – | – |
| `convoy_quota` | state data | 3 | – | – | – |
| `terminal_session` | root | 16 | 4: `source` 4 | `terminal_session_env` (child), `terminal_session_labels` (child), `terminal_session_attempts` (history) | `by_vessel` (vessel, ordinal), `live_by_pool` (pool) |
| `terminal_session_env` | child | 3 | – | – | – |
| `terminal_session_labels` | child | 2 | – | – | – |
| `terminal_session_attempts` | history | 8 | 4: `outcome` 4 | – | – |
| `vessel` | root | 3 | – | – | – |
| `ticket` | root | 15 | – | `ticket_assignees` (state data), `ticket_notes` (state data) | `open_by_queue` (queue) |
| `ticket_assignees` | state data | 2 | – | – | – |
| `ticket_notes` | state data | 3 | – | – | – |
| `ensure` | root | 11 | – | `ensure_recorded` (state data), `ensure_created` (state data), `ensure_attempts` (history) | `by_inbox` (inbox) |
| `ensure_recorded` | state data | 2 | – | – | – |
| `ensure_created` | state data | 2 | – | – | – |
| `ensure_attempts` | history | 8 | 4: `outcome` 4 | – | – |
| `referral` | root | 25 | 12: `condition_address` 6, `subject` 6 | `referral_references` (child) | `by_session` (session, seq), `by_reply` (in_reply_to) |
| `referral_references` | child | 24 | 22: `tag` 22 | – | `commit_repository_ref` (commit_repository), `ref_repository_ref` (ref_repository), `artifact_resource_ref` (artifact_resource), `control_record_resource_ref` (control_record_resource) |
| `dossier` | root | 7 | – | `dossier_inputs` (child), `dossier_deliverables` (records) | – |
| `dossier_inputs` | child | 5 | 3: `tag` 3 | `dossier_inputs_list_items` (child) | – |
| `dossier_inputs_list_items` | child | 4 | – | – | – |
| `dossier_deliverables` | records | 9 | 2: `source` 2 | `dossier_deliverables_evidence` (records) | – |
| `dossier_deliverables_evidence` | records | 10 | – | `dossier_deliverables_evidence_metadata` (child) | – |
| `dossier_deliverables_evidence_metadata` | child | 5 | – | – | – |

Tables wider than 30 columns:
- none

States with data (one label column, then data shared by name across the labels):
- `message`.`state`: label `state` and `state_since`; data columns `receiver_session`, `receiver_host`, `reason`, `reason_superseded_by`, `reason_withdrawn_by`, `reason_withdrawn_authority`, `reason_consumer_unreachable_by`, `reason_consumer_unreachable_authority`, `dropped_receiver_present`, `dropped_receiver_session`, `dropped_receiver_host`, `at`, `watermark`; data tables –
- `agent_session`.`state`: label `state` and `state_since`; data columns `channel`, `reason`; data tables –
- `convoy`.`state`: label `state` and `state_since`; data columns `reason`; data tables `convoy_resolution`, `convoy_quota`
- `terminal_session`.`state`: label `state` and `state_since`; data columns `identity`, `cause_category`, `cause_detail`; data tables –
- `ticket`.`state`: label `state` and `state_since`; data columns `priority`, `owner`, `hold_present`, `hold_by`, `hold_until`, `reason`, `closed_by`, `signature_who`, `signature_at`; data tables `ticket_assignees`, `ticket_notes`
- `ensure`.`state`: label `state` and `state_since`; data columns `window`, `bound`; data tables `ensure_recorded`, `ensure_created`
