# Launch-number proposal schema view

Kind tables only; the shared tables are in `launch_proposal.sql`.

| Table | Role | Columns | From enum expansion | Child tables | Indexes |
|---|---|--:|---|---|---|
| `checkout` | root | 25 | 4: `source` 2, `ending` 2 | – | `by_environment` (environment), `by_clone` (clone), `by_holder` (holder), `by_repository` (repository), `by_launch` (launch_of, launch) |
| `clone` | root | 21 | 2: `ending` 2 | – | `by_environment` (environment, repository), `by_repository` (repository), `by_launch` (launch_of, launch) |

Tables wider than 30 columns:
- none

States with data (one label column, then data shared by name across the labels):
- `checkout`.`state`: label `state` and `state_since`; data columns `failures`, `last_failure_at`, `last_failure_cause_category`, `last_failure_cause_detail`, `last_failure_retryable`, `identity`, `ending`, `ending_reason`, `cause_category`, `cause_detail`; data tables –
- `clone`.`state`: label `state` and `state_since`; data columns `failures`, `last_failure_at`, `last_failure_cause_category`, `last_failure_cause_detail`, `last_failure_retryable`, `identity`, `ending`, `ending_reason`, `cause_category`, `cause_detail`; data tables –

Interned column domains:

| Table | Column | Domain |
|---|---|---|
| `checkout` | `launch_of` | `ln` |
| `checkout` | `repository` | `rk` |
| `checkout` | `holder` | `hl` |
| `clone` | `launch_of` | `ln` |
| `clone` | `repository` | `rk` |
