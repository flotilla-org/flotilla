# Crew store schema view

Kind tables only; the shared tables are in `crew.sql`.

| Table | Role | Columns | From enum expansion | Child tables | Indexes |
|---|---|--:|---|---|---|
| `terminal_launch_facts` | root | 8 | – | – | – |
| `terminal_session` | root | 28 | 8: `source` 6, `ending` 2 | `terminal_session_env` (child) | `by_environment` (environment), `by_holder` (holder), `by_launch` (launch_of, launch), `by_agent_session` (agent_session) |
| `terminal_session_env` | child | 3 | – | – | – |
| `vessel` | root | 20 | 2: `ending` 2 | `vessel_shape` (child) | `live_by_host` (host), `by_launch` (convoy, launch_of, launch) |
| `vessel_shape` | child | 3 | – | – | – |

Tables wider than 30 columns:
- none

States with data (one label column, then data shared by name across the labels):
- `terminal_session`.`state`: label `state` and `state_since`; data columns `failures`, `last_failure_at`, `last_failure_cause_category`, `last_failure_cause_detail`, `last_failure_retryable`, `identity`, `ending`, `ending_reason`, `cause_category`, `cause_detail`; data tables –
- `vessel`.`state`: label `state` and `state_since`; data columns `failures`, `last_failure_at`, `last_failure_cause_category`, `last_failure_cause_detail`, `last_failure_retryable`, `identity`, `ending`, `ending_reason`, `cause_category`, `cause_detail`; data tables –

Interned column domains:

| Table | Column | Domain |
|---|---|---|
| `terminal_launch_facts` | `agent_adapter` | `ad` |
| `terminal_session` | `launch_of` | `ln` |
| `terminal_session` | `holder` | `hl` |
| `terminal_session` | `role` | `rl` |
| `terminal_session` | `pool` | `tp` |
| `terminal_session` | `source_capability` | `cp` |
| `vessel` | `launch_of` | `ln` |
| `vessel` | `class` | `vc` |
| `vessel` | `host` | `hs` |
