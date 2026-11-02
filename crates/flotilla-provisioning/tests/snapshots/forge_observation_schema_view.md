# Forge observation store schema view

Kind tables only; the shared tables are in `forge_observation.sql`.

| Table | Role | Columns | From enum expansion | Child tables | Indexes |
|---|---|--:|---|---|---|
| `change_request` | root | 18 | – | `change_request_closes` (child) | `by_subject` (service, scope, number), `by_branch` (service, scope, head_branch) |
| `change_request_closes` | child | 5 | – | – | `by_closed_issue` (service, scope, number) |
| `forge_read` | root | 8 | – | – | – |
| `issue` | root | 19 | – | `issue_blocked_by` (child) | `by_subject` (service, scope, number), `by_parent` (parent_service, parent_scope, parent_number) |
| `issue_blocked_by` | child | 5 | – | – | `by_blocker` (service, scope, number) |

Tables wider than 30 columns:
- none
