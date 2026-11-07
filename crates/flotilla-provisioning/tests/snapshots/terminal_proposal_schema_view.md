# TerminalSession proposal schema view

Kind tables only; the shared tables are in `terminal_proposal.sql`.

| Table | Role | Columns | From enum expansion | Child tables | Indexes |
|---|---|--:|---|---|---|
| `environment_health` | root | 3 | – | – | – |
| `terminal_drafts` | root | 2 | – | `terminal_drafts_drafts` (records) | – |
| `terminal_drafts_drafts` | records | 6 | – | – | – |
| `terminal_launch_facts` | root | 12 | – | – | – |
| `terminal_session` | root | 25 | 8: `source` 6, `ending` 2 | `terminal_session_env` (child) | `by_environment` (environment), `by_holder` (holder) |
| `terminal_session_env` | child | 3 | – | – | – |

Tables wider than 30 columns:
- none

States with data (one label column, then data shared by name across the labels):
- `terminal_session`.`state`: label `state` and `state_since`; data columns `failures`, `last_failure_at`, `last_failure_cause_category`, `last_failure_cause_detail`, `last_failure_retryable`, `identity`, `ending`, `ending_reason`, `cause_category`, `cause_detail`; data tables –

Interned column domains:

| Table | Column | Domain |
|---|---|---|
| `terminal_launch_facts` | `agent_adapter` | `ad` |
| `terminal_session` | `holder` | `hl` |
| `terminal_session` | `role` | `rl` |
| `terminal_session` | `pool` | `tp` |
| `terminal_session` | `source_capability` | `cp` |
