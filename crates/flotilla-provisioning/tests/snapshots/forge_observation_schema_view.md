# Forge observation store schema view

Kind tables only; the shared tables are in `forge_observation.sql`.

| Table | Role | Columns | From enum expansion | Child tables | Indexes |
|---|---|--:|---|---|---|
| `change_request` | root | 15 | – | – | `by_subject` (service, scope, number) |
| `forge_read` | root | 8 | – | – | – |
| `issue` | root | 11 | – | – | `by_subject` (service, scope, number) |

Tables wider than 30 columns:
- none
