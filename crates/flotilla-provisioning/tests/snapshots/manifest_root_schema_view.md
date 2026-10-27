# ManifestRoot store schema view

Kind tables only; the shared tables are in `manifest_root.sql`.

| Table | Role | Columns | From enum expansion | Child tables | Indexes |
|---|---|--:|---|---|---|
| `manifest_root` | root | 14 | 5: `binding` 5 | `manifest_root_baselines` (child), `manifest_root_refusals` (child), `manifest_root_suspended` (child), `manifest_root_resolutions` (child) | `bound_by_host` (host) |
| `manifest_root_baselines` | child | 3 | – | – | – |
| `manifest_root_refusals` | child | 4 | – | – | – |
| `manifest_root_suspended` | child | 2 | – | – | – |
| `manifest_root_resolutions` | child | 7 | 2: `outcome` 2 | – | – |

Tables wider than 30 columns:
- none

States with data (one label column, then data shared by name across the labels):
- `manifest_root`.`state`: label `state` and `state_since`; data columns `binding`, `binding_repo`, `binding_branch`, `binding_path`, `binding_directory`, `applied`, `source_error`; data tables –

Interned column domains:

| Table | Column | Domain |
|---|---|---|
| `manifest_root_baselines` | `key` | `md` |
| `manifest_root_refusals` | `key` | `md` |
| `manifest_root_suspended` | `value` | `md` |
| `manifest_root_resolutions` | `key` | `md` |
