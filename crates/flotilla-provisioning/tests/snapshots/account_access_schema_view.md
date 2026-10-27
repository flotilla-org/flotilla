# Account access store schema view

Kind tables only; the shared tables are in `account_access.sql`.

| Table | Role | Columns | From enum expansion | Child tables | Indexes |
|---|---|--:|---|---|---|
| `account_binding` | root | 16 | 3: `selection` 3 | `account_binding_grants` (child), `account_binding_candidates` (child), `account_binding_exclusions` (child), `account_binding_epochs` (records) | `by_requester` (requester), `open_by_credential` (credential) |
| `account_binding_grants` | child | 3 | – | – | – |
| `account_binding_candidates` | child | 2 | – | – | – |
| `account_binding_exclusions` | child | 3 | – | – | – |
| `account_binding_epochs` | records | 5 | – | – | – |
| `account_provision` | root | 13 | – | – | `by_binding` (binding), `by_place` (place) |

Tables wider than 30 columns:
- none

States with data (one label column, then data shared by name across the labels):
- `account_binding`.`state`: label `state` and `state_since`; data columns `reason`, `delivered_until`; data tables –

Interned column domains:

| Table | Column | Domain |
|---|---|---|
| `account_binding` | `requester` | `ad` |
| `account_binding` | `item` | `it` |
| `account_binding` | `requirement` | `aq` |
| `account_binding` | `credential` | `cr` |
| `account_binding_grants` | `key` | `gr` |
| `account_binding_candidates` | `value` | `cr` |
| `account_binding_exclusions` | `key` | `cr` |
| `account_provision` | `place` | `pc` |
| `account_provision` | `environment_kind` | `ek` |
| `account_provision` | `plan_implementation` | `pi` |
| `account_provision` | `plan_mechanism` | `pm` |
