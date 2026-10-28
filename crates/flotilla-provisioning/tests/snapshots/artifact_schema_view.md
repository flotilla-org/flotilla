# Artifact store schema view

Kind tables only; the shared tables are in `artifact.sql`.

| Table | Role | Columns | From enum expansion | Child tables | Indexes |
|---|---|--:|---|---|---|
| `artifact` | root | 13 | – | – | `by_convoy` (convoy) |

Tables wider than 30 columns:
- none
