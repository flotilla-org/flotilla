# Placement store schema view

Kind tables only; the shared tables are in `placement.sql`.

| Table | Role | Columns | From enum expansion | Child tables | Indexes |
|---|---|--:|---|---|---|
| `fulfilment_kind` | root | 6 | – | `fulfilment_kind_grants` (child) | – |
| `fulfilment_kind_grants` | child | 2 | – | – | – |

Tables wider than 30 columns:
- none
