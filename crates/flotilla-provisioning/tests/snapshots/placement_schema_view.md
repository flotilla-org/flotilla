# Placement store schema view

Kind tables only; the shared tables are in `placement.sql`.

| Table | Role | Columns | From enum expansion | Child tables | Indexes |
|---|---|--:|---|---|---|
| `environment_provider` | root | 32 **wide** | 28: `origin` 14, `settings` 14 | `environment_provider_settings_env` (child) | – |
| `environment_provider_settings_env` | child | 3 | – | – | – |
| `fulfilment_kind` | root | 24 | 19: `origin` 14, `realisation` 5 | `fulfilment_kind_grants` (child) | – |
| `fulfilment_kind_grants` | child | 2 | – | – | – |

Tables wider than 30 columns:
- `environment_provider`: 32 columns, 28 from enum expansion
