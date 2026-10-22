# Provisioning store schema view

Kind tables only; the shared tables are in `provisioning.sql`.

| Table | Role | Columns | From enum expansion | Child tables | Indexes |
|---|---|--:|---|---|---|
| `checkout` | root | 21 | 2: `source` 2 | – | `by_environment` (environment), `by_clone` (clone), `by_holder` (holder) |
| `clone` | root | 16 | – | – | `by_environment` (environment, repository) |
| `environment` | root | 16 | – | – | `live_by_provider` (provider), `by_image_build` (image_build), `by_holder` (holder) |
| `image_build` | root | 19 | 2: `delivery` 2 | – | `by_composition` (composition) |

Tables wider than 30 columns:
- none

States with data (one label column, then data shared by name across the labels):
- `checkout`.`state`: label `state` and `state_since`; data columns `failures`, `last_failure_at`, `last_failure_cause_category`, `last_failure_cause_detail`, `last_failure_retryable`, `identity`, `cause_category`, `cause_detail`; data tables –
- `clone`.`state`: label `state` and `state_since`; data columns `failures`, `last_failure_at`, `last_failure_cause_category`, `last_failure_cause_detail`, `last_failure_retryable`, `identity`, `cause_category`, `cause_detail`; data tables –
- `environment`.`state`: label `state` and `state_since`; data columns `failures`, `last_failure_at`, `last_failure_cause_category`, `last_failure_cause_detail`, `last_failure_retryable`, `identity`, `cause_category`, `cause_detail`; data tables –
- `image_build`.`state`: label `state` and `state_since`; data columns `failures`, `last_failure_at`, `last_failure_cause_category`, `last_failure_cause_detail`, `last_failure_retryable`, `identity`, `cause_category`, `cause_detail`; data tables –

Interned column domains:

| Table | Column | Domain |
|---|---|---|
| `checkout` | `repository` | `rk` |
| `checkout` | `holder` | `hl` |
| `clone` | `repository` | `rk` |
| `environment` | `provider` | `ad` |
| `environment` | `holder` | `hl` |
