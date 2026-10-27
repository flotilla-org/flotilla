# Access delivery store schema view

Kind tables only; the shared tables are in `access_delivery.sql`.

| Table | Role | Columns | From enum expansion | Child tables | Indexes |
|---|---|--:|---|---|---|
| `access_delivery` | root | 10 | – | `access_delivery_needs` (child) | `by_access` (access), `by_place` (place) |
| `access_delivery_needs` | child | 5 | – | – | – |

Tables wider than 30 columns:
- none

Interned column domains:

| Table | Column | Domain |
|---|---|---|
| `access_delivery` | `access` | `ra` |
| `access_delivery` | `place` | `pc` |
| `access_delivery` | `harness` | `it` |
