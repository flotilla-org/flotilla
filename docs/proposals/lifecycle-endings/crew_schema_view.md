# Candidate crew store schema view

Generated from typed review declarations; not registered by the runtime.

| Table | Role | Columns | From enum expansion | Child tables | Indexes |
|---|---|--:|---|---|---|
| `ensure` | root | 15 | – | `ensure_recorded` (state data), `ensure_created` (state data) | `by_inbox` (inbox) |
| `ensure_recorded` | state data | 2 | – | – | – |
| `ensure_created` | state data | 2 | – | – | – |
| `terminal_session` | root | 26 | 9: `source` 6, `ending` 3 | `terminal_session_env` (child) | `by_environment` (environment), `by_holder` (holder) |
| `terminal_session_env` | child | 3 | – | – | – |
| `vessel` | root | 19 | 3: `ending` 3 | `vessel_shape` (child) | `live_by_host` (host), `by_convoy` (convoy) |
| `vessel_shape` | child | 3 | – | – | – |

Tables wider than 30 columns:
- none

States with data (one label column, then data shared by name across the labels):
- `ensure`.`state`: label `state` and `state_since`; data columns `failures`, `last_failure_at`, `last_failure_cause_category`, `last_failure_cause_detail`, `last_failure_retryable`, `bound`; data tables `ensure_recorded`, `ensure_created`
- `terminal_session`.`state`: label `state` and `state_since`; data columns `failures`, `last_failure_at`, `last_failure_cause_category`, `last_failure_cause_detail`, `last_failure_retryable`, `identity`, `ending`, `ending_cause`, `ending_cause_reason`, `cause_category`, `cause_detail`; data tables –
- `vessel`.`state`: label `state` and `state_since`; data columns `failures`, `last_failure_at`, `last_failure_cause_category`, `last_failure_cause_detail`, `last_failure_retryable`, `identity`, `ending`, `ending_cause`, `ending_cause_reason`, `cause_category`, `cause_detail`; data tables –

Interned column domains:

| Table | Column | Domain |
|---|---|---|
| `ensure` | `inbox` | `ad` |
| `ensure` | `bound` | `rn` |
| `ensure_recorded` | `value` | `rn` |
| `ensure_created` | `value` | `rn` |
| `terminal_session` | `holder` | `hl` |
| `terminal_session` | `role` | `rl` |
| `terminal_session` | `pool` | `tp` |
| `terminal_session` | `source_capability` | `cp` |
| `vessel` | `class` | `vc` |
| `vessel` | `host` | `hs` |
