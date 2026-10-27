# Repository store schema view

Kind tables only; the shared tables are in `repository.sql`.

| Table | Role | Columns | From enum expansion | Child tables | Indexes |
|---|---|--:|---|---|---|
| `repository` | root | 8 | – | – | `by_verification_project` (verification_project) |

Tables wider than 30 columns:
- none
