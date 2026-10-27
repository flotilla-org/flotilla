# Project store schema view

Kind tables only; the shared tables are in `project.sql`.

| Table | Role | Columns | From enum expansion | Child tables | Indexes |
|---|---|--:|---|---|---|
| `project` | root | 5 | – | `project_repositories` (child) | – |
| `project_repositories` | child | 3 | – | – | – |
| `project_ops` | root | 8 | – | – | – |

Tables wider than 30 columns:
- none
