# Fixture schema view

Kind tables only; the shared tables are in `fixtures.sql`.

| Table | Role | Columns | From enum expansion | Child tables | Indexes |
|---|---|--:|---|---|---|
| `agent_session` | root | 6 | – | – | – |
| `convoy` | root | 7 | – | `convoy_promises` (child), `convoy_resolution` (state data), `convoy_quota` (state data) | `open_by_project` (project) |
| `convoy_promises` | child | 4 | – | – | – |
| `convoy_resolution` | state data | 3 | – | – | – |
| `convoy_quota` | state data | 3 | – | – | – |
| `dossier` | root | 7 | – | `dossier_inputs` (child), `dossier_deliverables` (records) | – |
| `dossier_inputs` | child | 5 | 3: `tag` 3 | `dossier_inputs_items` (child) | – |
| `dossier_inputs_items` | child | 4 | – | – | – |
| `dossier_deliverables` | records | 9 | 2: `source` 2 | `dossier_deliverables_evidence` (records) | – |
| `dossier_deliverables_evidence` | records | 9 | – | `dossier_deliverables_evidence_metadata` (child) | – |
| `dossier_deliverables_evidence_metadata` | child | 5 | – | – | – |
| `ensure` | root | 15 | – | `ensure_recorded` (state data), `ensure_created` (state data) | `by_inbox` (inbox) |
| `ensure_recorded` | state data | 2 | – | – | – |
| `ensure_created` | state data | 2 | – | – | – |
| `message` | root | 15 | 5: `expectation` 2, `reason` 3 | – | `open_by_inbox` (inbox), `deadline` (deadline) |
| `referral` | root | 24 | 12: `subject` 6, `condition_address` 6 | `referral_references` (child) | `by_session` (session, seq), `by_reply` (in_reply_to) |
| `referral_references` | child | 11 | 9: `tag` 9 | – | `repository_ref` (repository), `resource_ref` (resource) |
| `ticket` | root | 14 | – | `ticket_assignees` (state data), `ticket_notes` (state data) | `open_by_queue` (queue) |
| `ticket_assignees` | state data | 2 | – | – | – |
| `ticket_notes` | state data | 3 | – | – | – |
| `terminal_session` | root | 21 | 4: `source` 4 | `terminal_session_env` (child), `terminal_session_labels` (child) | `by_vessel` (vessel, ordinal), `live_by_pool` (pool) |
| `terminal_session_env` | child | 3 | – | – | – |
| `terminal_session_labels` | child | 2 | – | – | – |
| `vessel` | root | 16 | – | `vessel_shape` (child) | `live_by_host` (host), `by_convoy` (convoy) |
| `vessel_shape` | child | 3 | – | – | – |

Tables wider than 30 columns:
- none

States with data (one label column, then data shared by name across the labels):
- `agent_session`.`state`: label `state` and `state_since`; data columns `channel`, `reason`; data tables –
- `convoy`.`state`: label `state` and `state_since`; data columns `reason`; data tables `convoy_resolution`, `convoy_quota`
- `ensure`.`state`: label `state` and `state_since`; data columns `failures`, `last_failure_at`, `last_failure_cause_category`, `last_failure_cause_detail`, `last_failure_retryable`, `bound`; data tables `ensure_recorded`, `ensure_created`
- `message`.`state`: label `state` and `state_since`; data columns `receiver_session`, `receiver_host`, `reason`, `reason_by`, `reason_authority`, `watermark`; data tables –
- `ticket`.`state`: label `state` and `state_since`; data columns `priority`, `owner`, `hold_by`, `hold_until`, `reason`, `closed_by`, `signature_who`, `signature_at`; data tables `ticket_assignees`, `ticket_notes`
- `terminal_session`.`state`: label `state` and `state_since`; data columns `failures`, `last_failure_at`, `last_failure_cause_category`, `last_failure_cause_detail`, `last_failure_retryable`, `identity`, `cause_category`, `cause_detail`; data tables –
- `vessel`.`state`: label `state` and `state_since`; data columns `failures`, `last_failure_at`, `last_failure_cause_category`, `last_failure_cause_detail`, `last_failure_retryable`, `identity`, `cause_category`, `cause_detail`; data tables –

Interned column domains:

| Table | Column | Domain |
|---|---|---|
| `convoy` | `project` | `ad` |
| `convoy_promises` | `kind` | `pk` |
| `convoy_promises` | `target` | `ad` |
| `dossier_deliverables` | `label` | `lb` |
| `dossier_deliverables` | `source_step` | `st` |
| `dossier_deliverables_evidence` | `verdict_by` | `wr` |
| `ensure` | `inbox` | `ad` |
| `ensure` | `bound` | `rn` |
| `ensure_recorded` | `value` | `rn` |
| `ensure_created` | `value` | `rn` |
| `message` | `inbox` | `ad` |
| `message` | `expectation_condition` | `ad` |
| `message` | `receiver_session` | `sn` |
| `message` | `receiver_host` | `hm` |
| `message` | `reason_by` | `ad` |
| `message` | `reason_authority` | `ad` |
| `referral` | `sender` | `ag` |
| `referral` | `subject_service` | `sv` |
| `referral` | `condition_address_service` | `sv` |
| `referral_references` | `service` | `sv` |
| `ticket` | `queue` | `qu` |
| `ticket` | `owner` | `wr` |
| `ticket` | `closed_by` | `wr` |
| `ticket` | `signature_who` | `wr` |
| `terminal_session` | `role` | `rl` |
| `terminal_session` | `pool` | `pl` |
| `terminal_session` | `source_selector` | `sl` |
