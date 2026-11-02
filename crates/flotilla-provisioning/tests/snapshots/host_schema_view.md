# Host store schema view

Kind tables only; the shared tables are in `host.sql`.

| Table | Role | Columns | From enum expansion | Child tables | Indexes |
|---|---|--:|---|---|---|
| `host` | root | 12 | 7: `connection` 2, `image_builds` 5 | – | – |
| `host_observation` | root | 6 | – | `host_observation_conditions` (child), `host_observation_providers` (child), `host_observation_agent_adapters` (child), `host_observation_agent_adapter_baseline` (child), `host_observation_held_credentials` (child), `host_observation_credential_expiry` (child), `host_observation_image_digests` (child), `host_observation_facts` (child) | – |
| `host_observation_conditions` | child | 7 | – | – | – |
| `host_observation_providers` | child | 8 | 2: `readiness` 2 | – | – |
| `host_observation_agent_adapters` | child | 2 | – | – | – |
| `host_observation_agent_adapter_baseline` | child | 2 | – | – | – |
| `host_observation_held_credentials` | child | 2 | – | – | – |
| `host_observation_credential_expiry` | child | 4 | – | – | – |
| `host_observation_image_digests` | child | 2 | – | `host_observation_image_digests_images` (child) | – |
| `host_observation_image_digests_images` | child | 3 | – | – | – |
| `host_observation_facts` | child | 4 | – | `host_observation_facts_harnesses` (child), `host_observation_facts_toolchains` (child) | – |
| `host_observation_facts_harnesses` | child | 4 | – | – | – |
| `host_observation_facts_toolchains` | child | 4 | – | – | – |

Tables wider than 30 columns:
- none
