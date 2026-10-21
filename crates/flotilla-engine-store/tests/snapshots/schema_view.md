# Message store schema view

Kind tables only; the shared tables are in `message.sql`.

| Table | Role | Columns | From enum expansion | Child tables | Indexes |
|---|---|--:|---|---|---|
| `agent_session` | root | 9 | – | – | – |
| `message` | root | 23 | 5: `expectation` 2, `reason` 3 | `message_references` (child) | `pending_by_inbox` (inbox, admitted_seq), `deadline` (deadline), `by_reply` (in_reply_to), `by_supersedes` (supersedes) |
| `message_references` | child | 5 | – | – | – |

Tables wider than 30 columns:
- none

States with data (one label column, then data shared by name across the labels):
- `agent_session`.`state`: label `state` and `state_since`; data columns `channel`, `activity`, `activity_since`, `activity_cause`, `reason`; data tables –
- `message`.`state`: label `state` and `state_since`; data columns `receiver_kind`, `receiver_id`, `receiver_host`, `reason`, `reason_by`, `reason_authority`, `watermark`; data tables –

Interned column domains:

| Table | Column | Domain |
|---|---|---|
| `message` | `sender` | `ad` |
| `message` | `inbox` | `ad` |
| `message` | `expectation_condition` | `ad` |
| `message` | `delivery_condition` | `ad` |
| `message` | `receiver_id` | `rn` |
| `message` | `receiver_host` | `hm` |
| `message` | `reason_by` | `ad` |
| `message` | `reason_authority` | `ad` |
| `message_references` | `address` | `ad` |
