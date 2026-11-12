# Provisioning store schema view

Kind tables only; the shared tables are in `provisioning.sql`.

| Table | Role | Columns | From enum expansion | Child tables | Indexes |
|---|---|--:|---|---|---|
| `clone_facts` | root | 3 | – | – | – |
| `checkout` | root | 25 | 4: `source` 2, `ending` 2 | – | `by_environment` (environment), `by_clone` (clone), `by_holder` (holder), `by_repository` (repository), `by_launch` (launch_of, launch) |
| `clone` | root | 21 | 2: `ending` 2 | – | `by_environment` (environment, repository), `by_repository` (repository), `by_launch` (launch_of, launch) |
| `environment_launch_facts` | root | 10 | – | – | – |
| `environment` | root | 18 | 2: `ending` 2 | – | `live_by_provider` (provider), `by_image_build` (image_build), `by_holder` (holder) |
| `image_build` | root | 23 | 4: `delivery` 2, `ending` 2 | – | `by_composition` (composition), `by_launch` (launch_of, launch) |
| `image_build_demand` | root | 10 | 2: `delivery` 2 | – | `by_builder` (builder), `by_composition` (composition) |
| `checkout_facts` | root | 3 | – | – | – |
| `checkout_integration` | root | 16 | – | – | – |

Tables wider than 30 columns:
- none

States with data (one label column, then data shared by name across the labels):
- `checkout`.`state`: label `state` and `state_since`; data columns `failures`, `last_failure_at`, `last_failure_cause_category`, `last_failure_cause_detail`, `last_failure_retryable`, `identity`, `ending`, `ending_reason`, `cause_category`, `cause_detail`; data tables –
- `clone`.`state`: label `state` and `state_since`; data columns `failures`, `last_failure_at`, `last_failure_cause_category`, `last_failure_cause_detail`, `last_failure_retryable`, `identity`, `ending`, `ending_reason`, `cause_category`, `cause_detail`; data tables –
- `environment`.`state`: label `state` and `state_since`; data columns `failures`, `last_failure_at`, `last_failure_cause_category`, `last_failure_cause_detail`, `last_failure_retryable`, `identity`, `ending`, `ending_reason`, `cause_category`, `cause_detail`; data tables –
- `image_build`.`state`: label `state` and `state_since`; data columns `failures`, `last_failure_at`, `last_failure_cause_category`, `last_failure_cause_detail`, `last_failure_retryable`, `identity`, `ending`, `ending_reason`, `cause_category`, `cause_detail`; data tables –

Interned column domains:

| Table | Column | Domain |
|---|---|---|
| `checkout` | `launch_of` | `ln` |
| `checkout` | `repository` | `rk` |
| `checkout` | `holder` | `hl` |
| `clone` | `launch_of` | `ln` |
| `clone` | `repository` | `rk` |
| `environment` | `provider` | `ad` |
| `environment` | `holder` | `hl` |
| `image_build` | `launch_of` | `ln` |
