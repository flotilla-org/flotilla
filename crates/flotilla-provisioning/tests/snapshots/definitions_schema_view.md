# Definition store schema view

Kind tables only; the shared tables are in `definitions.sql`.

| Table | Role | Columns | From enum expansion | Child tables | Indexes |
|---|---|--:|---|---|---|
| `credential_spec` | root | 40 **wide** | 36: `origin` 13, `consumer` 16, `source` 7 | `credential_spec_binaries` (child) | – |
| `credential_spec_binaries` | child | 2 | – | – | – |
| `crew_defaults` | root | 19 | 13: `origin` 13 | `crew_defaults_roles` (child) | – |
| `crew_defaults_roles` | child | 9 | – | – | – |
| `fleet_designation` | root | 20 | 13: `origin` 13 | – | – |
| `forge` | root | 19 | 13: `origin` 13 | `forge_hosts` (child) | – |
| `forge_hosts` | child | 2 | – | – | – |
| `credential_grant` | root | 18 | 13: `origin` 13 | `credential_grant_host_action_hosts` (child), `credential_grant_projects` (child), `credential_grant_repositories` (child), `credential_grant_roles` (child), `credential_grant_credentials` (child), `credential_grant_landing_credentials` (child) | – |
| `credential_grant_host_action_hosts` | child | 2 | – | – | – |
| `credential_grant_projects` | child | 2 | – | – | – |
| `credential_grant_repositories` | child | 2 | – | – | – |
| `credential_grant_roles` | child | 2 | – | – | – |
| `credential_grant_credentials` | child | 2 | – | – | – |
| `credential_grant_landing_credentials` | child | 5 | 3: `tag` 3 | – | – |
| `image_layer` | root | 21 | 13: `origin` 13 | `image_layer_provides` (child) | – |
| `image_layer_provides` | child | 2 | – | – | – |
| `workflow_template` | root | 16 | 13: `origin` 13 | `workflow_template_roles` (child) | – |
| `workflow_template_roles` | child | 2 | – | – | – |

Tables wider than 30 columns:
- `credential_spec`: 40 columns, 36 from enum expansion
