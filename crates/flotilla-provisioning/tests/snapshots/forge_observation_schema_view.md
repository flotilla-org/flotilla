# Forge observation store schema view

Kind tables only; the shared tables are in `forge_observation.sql`.

| Table | Role | Columns | From enum expansion | Child tables | Indexes |
|---|---|--:|---|---|---|
| `change_request` | root | 18 | – | – | `by_subject` (service, scope, number), `by_branch` (service, scope, head_branch) |
| `forge_read` | root | 8 | – | – | – |
| `issue` | root | 16 | – | – | `by_subject` (service, scope, number) |

Tables wider than 30 columns:
- none
