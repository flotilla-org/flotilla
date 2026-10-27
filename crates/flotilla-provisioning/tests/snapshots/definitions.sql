CREATE TABLE strings (
  id INTEGER PRIMARY KEY,
  domain TEXT NOT NULL CHECK (length(domain) > 0),
  s TEXT NOT NULL,
  UNIQUE (domain, s)
) STRICT;
CREATE TABLE resource_identity (
  id INTEGER PRIMARY KEY, kind TEXT NOT NULL,
  name INTEGER NOT NULL REFERENCES strings(id), home INTEGER NOT NULL REFERENCES strings(id),
  seq INTEGER NOT NULL DEFAULT 0,
  UNIQUE (kind, name, home)) STRICT;
CREATE INDEX resource_identity_by_home ON resource_identity (home, kind);
CREATE TABLE objects (hash BLOB PRIMARY KEY, bytes BLOB NOT NULL) STRICT;
CREATE TABLE home_sequence (home INTEGER PRIMARY KEY REFERENCES strings(id), last INTEGER NOT NULL) STRICT;
CREATE TABLE change (
  home INTEGER NOT NULL REFERENCES strings(id), seq INTEGER NOT NULL,
  writer INTEGER NOT NULL REFERENCES strings(id), at INTEGER NOT NULL,
  PRIMARY KEY (home, seq)) STRICT, WITHOUT ROWID;
CREATE TABLE change_items (
  change_home INTEGER NOT NULL, change_seq INTEGER NOT NULL, pos INTEGER NOT NULL CHECK (pos >= 0),
  record INTEGER NOT NULL REFERENCES resource_identity(id) ON DELETE CASCADE,
  revision TEXT,
  PRIMARY KEY (change_home, change_seq, pos),
  FOREIGN KEY (change_home, change_seq) REFERENCES change(home, seq)) STRICT, WITHOUT ROWID;
CREATE INDEX change_items_by_record ON change_items (record);
CREATE TABLE log (
  home INTEGER NOT NULL REFERENCES strings(id), home_seq INTEGER NOT NULL,
  change INTEGER NOT NULL,
  resource INTEGER REFERENCES resource_identity(id), prev_seq INTEGER NOT NULL,
  cause TEXT NOT NULL CHECK (cause IN ('create', 'edit', 'edge', 'delete', 'expunge')),
  edge TEXT, "from" TEXT, "to" TEXT,
  about INTEGER REFERENCES resource_identity(id) ON DELETE SET NULL,
  arguments BLOB, effect BLOB NOT NULL,
  topic TEXT NOT NULL DEFAULT 'all' CHECK (length(topic) BETWEEN 1 AND 8 AND topic NOT GLOB '*[^a-z0-9_]*' AND substr(topic, 1, 1) GLOB '[a-z]'),
  PRIMARY KEY (home, home_seq),
  FOREIGN KEY (home, change) REFERENCES change(home, seq),
  CHECK (change < home_seq),
  CHECK ((resource IS NULL) = (cause = 'expunge')),
  CHECK (CASE WHEN cause = 'edge' THEN edge IS NOT NULL AND "from" IS NOT NULL AND "to" IS NOT NULL AND arguments IS NOT NULL
    ELSE edge IS NULL AND "from" IS NULL AND "to" IS NULL AND about IS NULL AND arguments IS NULL END)) STRICT;
CREATE INDEX log_by_resource ON log (resource, home_seq);
CREATE INDEX log_about ON log (about) WHERE about IS NOT NULL;
CREATE INDEX log_expunges ON log (home, home_seq) WHERE cause = 'expunge';
CREATE TABLE stream_watermark (
  home INTEGER PRIMARY KEY REFERENCES strings(id), home_seq INTEGER NOT NULL CHECK (home_seq >= 0), applied_through INTEGER NOT NULL
) STRICT, WITHOUT ROWID;
CREATE TABLE stream_history (
  home INTEGER PRIMARY KEY REFERENCES strings(id), home_seq INTEGER NOT NULL CHECK (home_seq >= 0)
) STRICT, WITHOUT ROWID;
CREATE TABLE codes (
  kind TEXT NOT NULL, field TEXT NOT NULL, code TEXT NOT NULL, name TEXT NOT NULL,
  PRIMARY KEY (kind, field, code)) STRICT, WITHOUT ROWID;
CREATE TABLE "credential_spec" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "consumer" TEXT NOT NULL CHECK ("consumer" IN ('cl', 'co', 'cx', 'dr', 'fj', 'ga', 'gh', 'gt', 'rb')),
  "consumer_actor_login" TEXT,
  "consumer_installation_id" INTEGER,
  "consumer_installation_repository" TEXT,
  "consumer_permissions" BLOB,
  "consumer_forge_ref" TEXT,
  "consumer_username" TEXT,
  "consumer_host" TEXT,
  "consumer_account_email" TEXT,
  "consumer_registry" TEXT,
  "consumer_endpoint" TEXT,
  "consumer_bucket" TEXT,
  "consumer_region" TEXT,
  "consumer_public_base_url" TEXT,
  "consumer_allow_http" INTEGER CHECK ("consumer_allow_http" IN (0, 1)),
  "consumer_virtual_hosted_style" INTEGER CHECK ("consumer_virtual_hosted_style" IN (0, 1)),
  "source" TEXT NOT NULL CHECK ("source" IN ('en', 'fi', 'ga', 'ic')),
  "source_path" TEXT,
  "source_name" TEXT,
  "source_command" TEXT,
  "source_args" BLOB,
  "source_app_id_path" TEXT,
  "source_private_key_path" TEXT,
  "lifecycle" TEXT NOT NULL CHECK ("lifecycle" IN ('is', 'rf', 'st')),
  "authorization_epoch" INTEGER NOT NULL,
  CHECK (CASE WHEN "consumer" = 'gh'
      THEN "consumer_actor_login" IS NULL
      AND "consumer_installation_id" IS NULL
      AND "consumer_installation_repository" IS NULL
      AND "consumer_permissions" IS NULL
      AND "consumer_forge_ref" IS NULL
      AND "consumer_username" IS NULL
      AND "consumer_host" IS NULL
      AND "consumer_account_email" IS NULL
      AND "consumer_registry" IS NULL
      AND "consumer_endpoint" IS NULL
      AND "consumer_bucket" IS NULL
      AND "consumer_region" IS NULL
      AND "consumer_public_base_url" IS NULL
      AND "consumer_allow_http" IS NULL
      AND "consumer_virtual_hosted_style" IS NULL
      END),
  CHECK (CASE WHEN "consumer" = 'ga'
      THEN "consumer_permissions" IS NOT NULL
      AND "consumer_forge_ref" IS NULL
      AND "consumer_username" IS NULL
      AND "consumer_host" IS NULL
      AND "consumer_account_email" IS NULL
      AND "consumer_registry" IS NULL
      AND "consumer_endpoint" IS NULL
      AND "consumer_bucket" IS NULL
      AND "consumer_region" IS NULL
      AND "consumer_public_base_url" IS NULL
      AND "consumer_allow_http" IS NULL
      AND "consumer_virtual_hosted_style" IS NULL
      END),
  CHECK (CASE WHEN "consumer" = 'fj'
      THEN "consumer_forge_ref" IS NOT NULL
      AND "consumer_username" IS NOT NULL
      AND "consumer_actor_login" IS NULL
      AND "consumer_installation_id" IS NULL
      AND "consumer_installation_repository" IS NULL
      AND "consumer_permissions" IS NULL
      AND "consumer_host" IS NULL
      AND "consumer_account_email" IS NULL
      AND "consumer_registry" IS NULL
      AND "consumer_endpoint" IS NULL
      AND "consumer_bucket" IS NULL
      AND "consumer_region" IS NULL
      AND "consumer_public_base_url" IS NULL
      AND "consumer_allow_http" IS NULL
      AND "consumer_virtual_hosted_style" IS NULL
      END),
  CHECK (CASE WHEN "consumer" = 'gt'
      THEN "consumer_username" IS NOT NULL
      AND "consumer_host" IS NOT NULL
      AND "consumer_actor_login" IS NULL
      AND "consumer_installation_id" IS NULL
      AND "consumer_installation_repository" IS NULL
      AND "consumer_permissions" IS NULL
      AND "consumer_forge_ref" IS NULL
      AND "consumer_account_email" IS NULL
      AND "consumer_registry" IS NULL
      AND "consumer_endpoint" IS NULL
      AND "consumer_bucket" IS NULL
      AND "consumer_region" IS NULL
      AND "consumer_public_base_url" IS NULL
      AND "consumer_allow_http" IS NULL
      AND "consumer_virtual_hosted_style" IS NULL
      END),
  CHECK (CASE WHEN "consumer" = 'cl'
      THEN "consumer_actor_login" IS NULL
      AND "consumer_installation_id" IS NULL
      AND "consumer_installation_repository" IS NULL
      AND "consumer_permissions" IS NULL
      AND "consumer_forge_ref" IS NULL
      AND "consumer_username" IS NULL
      AND "consumer_host" IS NULL
      AND "consumer_account_email" IS NULL
      AND "consumer_registry" IS NULL
      AND "consumer_endpoint" IS NULL
      AND "consumer_bucket" IS NULL
      AND "consumer_region" IS NULL
      AND "consumer_public_base_url" IS NULL
      AND "consumer_allow_http" IS NULL
      AND "consumer_virtual_hosted_style" IS NULL
      END),
  CHECK (CASE WHEN "consumer" = 'co'
      THEN "consumer_account_email" IS NOT NULL
      AND "consumer_actor_login" IS NULL
      AND "consumer_installation_id" IS NULL
      AND "consumer_installation_repository" IS NULL
      AND "consumer_permissions" IS NULL
      AND "consumer_forge_ref" IS NULL
      AND "consumer_username" IS NULL
      AND "consumer_host" IS NULL
      AND "consumer_registry" IS NULL
      AND "consumer_endpoint" IS NULL
      AND "consumer_bucket" IS NULL
      AND "consumer_region" IS NULL
      AND "consumer_public_base_url" IS NULL
      AND "consumer_allow_http" IS NULL
      AND "consumer_virtual_hosted_style" IS NULL
      END),
  CHECK (CASE WHEN "consumer" = 'cx'
      THEN "consumer_actor_login" IS NULL
      AND "consumer_installation_id" IS NULL
      AND "consumer_installation_repository" IS NULL
      AND "consumer_permissions" IS NULL
      AND "consumer_forge_ref" IS NULL
      AND "consumer_username" IS NULL
      AND "consumer_host" IS NULL
      AND "consumer_account_email" IS NULL
      AND "consumer_registry" IS NULL
      AND "consumer_endpoint" IS NULL
      AND "consumer_bucket" IS NULL
      AND "consumer_region" IS NULL
      AND "consumer_public_base_url" IS NULL
      AND "consumer_allow_http" IS NULL
      AND "consumer_virtual_hosted_style" IS NULL
      END),
  CHECK (CASE WHEN "consumer" = 'dr'
      THEN "consumer_username" IS NOT NULL
      AND "consumer_registry" IS NOT NULL
      AND "consumer_actor_login" IS NULL
      AND "consumer_installation_id" IS NULL
      AND "consumer_installation_repository" IS NULL
      AND "consumer_permissions" IS NULL
      AND "consumer_forge_ref" IS NULL
      AND "consumer_host" IS NULL
      AND "consumer_account_email" IS NULL
      AND "consumer_endpoint" IS NULL
      AND "consumer_bucket" IS NULL
      AND "consumer_region" IS NULL
      AND "consumer_public_base_url" IS NULL
      AND "consumer_allow_http" IS NULL
      AND "consumer_virtual_hosted_style" IS NULL
      END),
  CHECK (CASE WHEN "consumer" = 'rb'
      THEN "consumer_endpoint" IS NOT NULL
      AND "consumer_bucket" IS NOT NULL
      AND "consumer_region" IS NOT NULL
      AND "consumer_public_base_url" IS NOT NULL
      AND "consumer_allow_http" IS NOT NULL
      AND "consumer_virtual_hosted_style" IS NOT NULL
      AND "consumer_actor_login" IS NULL
      AND "consumer_installation_id" IS NULL
      AND "consumer_installation_repository" IS NULL
      AND "consumer_permissions" IS NULL
      AND "consumer_forge_ref" IS NULL
      AND "consumer_username" IS NULL
      AND "consumer_host" IS NULL
      AND "consumer_account_email" IS NULL
      AND "consumer_registry" IS NULL
      END),
  CHECK (CASE WHEN "source" = 'fi'
      THEN "source_path" IS NOT NULL
      AND "source_name" IS NULL
      AND "source_command" IS NULL
      AND "source_args" IS NULL
      AND "source_app_id_path" IS NULL
      AND "source_private_key_path" IS NULL
      END),
  CHECK (CASE WHEN "source" = 'en'
      THEN "source_name" IS NOT NULL
      AND "source_path" IS NULL
      AND "source_command" IS NULL
      AND "source_args" IS NULL
      AND "source_app_id_path" IS NULL
      AND "source_private_key_path" IS NULL
      END),
  CHECK (CASE WHEN "source" = 'ic'
      THEN "source_command" IS NOT NULL
      AND "source_args" IS NOT NULL
      AND "source_path" IS NULL
      AND "source_name" IS NULL
      AND "source_app_id_path" IS NULL
      AND "source_private_key_path" IS NULL
      END),
  CHECK (CASE WHEN "source" = 'ga'
      THEN "source_app_id_path" IS NOT NULL
      AND "source_private_key_path" IS NOT NULL
      AND "source_path" IS NULL
      AND "source_name" IS NULL
      AND "source_command" IS NULL
      AND "source_args" IS NULL
      END)
) STRICT;
CREATE TABLE "credential_spec_binaries" (
  "credential_spec" INTEGER NOT NULL,
  "value" TEXT NOT NULL,
  PRIMARY KEY ("credential_spec", "value"),
  FOREIGN KEY ("credential_spec") REFERENCES "credential_spec" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
INSERT INTO "codes" ("kind", "field", "code", "name") VALUES
  ('cp', 'consumer', 'cl', 'Claude'),
  ('cp', 'consumer', 'co', 'ClaudeOauth'),
  ('cp', 'consumer', 'cx', 'Codex'),
  ('cp', 'consumer', 'dr', 'DockerRegistry'),
  ('cp', 'consumer', 'fj', 'Forgejo'),
  ('cp', 'consumer', 'ga', 'GithubApp'),
  ('cp', 'consumer', 'gh', 'Gh'),
  ('cp', 'consumer', 'gt', 'GitHttpToken'),
  ('cp', 'consumer', 'rb', 'ReviewBundleStore'),
  ('cp', 'source', 'en', 'Env'),
  ('cp', 'source', 'fi', 'File'),
  ('cp', 'source', 'ga', 'GithubApp'),
  ('cp', 'source', 'ic', 'IssueCommand'),
  ('cp', 'lifecycle', 'is', 'Issued'),
  ('cp', 'lifecycle', 'rf', 'Refreshable'),
  ('cp', 'lifecycle', 'st', 'Static');
CREATE VIEW "credential_spec_view" AS SELECT
  "credential_spec"."id",
  "credential_spec"."version",
  (SELECT "name" FROM "codes" WHERE "kind" = 'cp' AND "field" = 'consumer' AND "code" = "credential_spec"."consumer") AS "consumer",
  "credential_spec"."consumer_actor_login",
  "credential_spec"."consumer_installation_id",
  "credential_spec"."consumer_installation_repository",
  "credential_spec"."consumer_permissions",
  "credential_spec"."consumer_forge_ref",
  "credential_spec"."consumer_username",
  "credential_spec"."consumer_host",
  "credential_spec"."consumer_account_email",
  "credential_spec"."consumer_registry",
  "credential_spec"."consumer_endpoint",
  "credential_spec"."consumer_bucket",
  "credential_spec"."consumer_region",
  "credential_spec"."consumer_public_base_url",
  "credential_spec"."consumer_allow_http",
  "credential_spec"."consumer_virtual_hosted_style",
  (SELECT "name" FROM "codes" WHERE "kind" = 'cp' AND "field" = 'source' AND "code" = "credential_spec"."source") AS "source",
  "credential_spec"."source_path",
  "credential_spec"."source_name",
  "credential_spec"."source_command",
  "credential_spec"."source_args",
  "credential_spec"."source_app_id_path",
  "credential_spec"."source_private_key_path",
  (SELECT "name" FROM "codes" WHERE "kind" = 'cp' AND "field" = 'lifecycle' AND "code" = "credential_spec"."lifecycle") AS "lifecycle",
  "credential_spec"."authorization_epoch"
FROM "credential_spec";
CREATE VIEW "credential_spec_binaries_view" AS SELECT
  "credential_spec_binaries"."credential_spec",
  "credential_spec_binaries"."value"
FROM "credential_spec_binaries";
CREATE TABLE "crew_defaults" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "project_ref" TEXT,
  "default_workflow_ref" TEXT,
  "skills" BLOB NOT NULL,
  "image" BLOB NOT NULL
) STRICT;
CREATE TABLE "crew_defaults_roles" (
  "crew_defaults" INTEGER NOT NULL,
  "key" TEXT NOT NULL,
  "agent" TEXT,
  "model" TEXT,
  "workflow" TEXT,
  "brief_template" TEXT,
  "subscriptions" BLOB NOT NULL,
  "adoptable" INTEGER CHECK ("adoptable" IN (0, 1)),
  "principal" TEXT,
  PRIMARY KEY ("crew_defaults", "key"),
  FOREIGN KEY ("crew_defaults") REFERENCES "crew_defaults" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE VIEW "crew_defaults_view" AS SELECT
  "crew_defaults"."id",
  "crew_defaults"."version",
  "crew_defaults"."project_ref",
  "crew_defaults"."default_workflow_ref",
  "crew_defaults"."skills",
  "crew_defaults"."image"
FROM "crew_defaults";
CREATE VIEW "crew_defaults_roles_view" AS SELECT
  "crew_defaults_roles"."crew_defaults",
  "crew_defaults_roles"."key",
  "crew_defaults_roles"."agent",
  "crew_defaults_roles"."model",
  "crew_defaults_roles"."workflow",
  "crew_defaults_roles"."brief_template",
  "crew_defaults_roles"."subscriptions",
  "crew_defaults_roles"."adoptable",
  "crew_defaults_roles"."principal"
FROM "crew_defaults_roles";
CREATE TABLE "fleet_designation" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "fleet" TEXT,
  "project" TEXT NOT NULL,
  "image_cache_repository" TEXT,
  "image_cache_pull_credential" TEXT,
  "image_cache_push_credential" TEXT,
  CHECK (CASE WHEN "image_cache_repository" IS NOT NULL
      THEN "image_cache_pull_credential" IS NOT NULL
      AND "image_cache_push_credential" IS NOT NULL
      ELSE "image_cache_pull_credential" IS NULL
      AND "image_cache_push_credential" IS NULL
      END)
) STRICT;
CREATE VIEW "fleet_designation_view" AS SELECT
  "fleet_designation"."id",
  "fleet_designation"."version",
  "fleet_designation"."fleet",
  "fleet_designation"."project",
  "fleet_designation"."image_cache_repository",
  "fleet_designation"."image_cache_pull_credential",
  "fleet_designation"."image_cache_push_credential"
FROM "fleet_designation";
CREATE TABLE "forge" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "forge_id" TEXT NOT NULL,
  "forge_kind" TEXT NOT NULL CHECK ("forge_kind" IN ('fj', 'gh')),
  "https_url" TEXT NOT NULL,
  "git_ssh_host" TEXT NOT NULL
) STRICT;
CREATE TABLE "forge_hosts" (
  "forge" INTEGER NOT NULL,
  "value" TEXT NOT NULL,
  PRIMARY KEY ("forge", "value"),
  FOREIGN KEY ("forge") REFERENCES "forge" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
INSERT INTO "codes" ("kind", "field", "code", "name") VALUES
  ('fo', 'forge_kind', 'fj', 'Forgejo'),
  ('fo', 'forge_kind', 'gh', 'Github');
CREATE VIEW "forge_view" AS SELECT
  "forge"."id",
  "forge"."version",
  "forge"."forge_id",
  (SELECT "name" FROM "codes" WHERE "kind" = 'fo' AND "field" = 'forge_kind' AND "code" = "forge"."forge_kind") AS "forge_kind",
  "forge"."https_url",
  "forge"."git_ssh_host"
FROM "forge";
CREATE VIEW "forge_hosts_view" AS SELECT
  "forge_hosts"."forge",
  "forge_hosts"."value"
FROM "forge_hosts";
CREATE TABLE "credential_grant" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "host_action" TEXT CHECK ("host_action" IN ('bs', 'pl', 'ps')),
  "repository_trust" TEXT CHECK ("repository_trust" IN ('fk', 'ow')),
  "permissions" BLOB NOT NULL
) STRICT;
CREATE TABLE "credential_grant_host_action_hosts" (
  "credential_grant" INTEGER NOT NULL,
  "value" TEXT NOT NULL,
  PRIMARY KEY ("credential_grant", "value"),
  FOREIGN KEY ("credential_grant") REFERENCES "credential_grant" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE TABLE "credential_grant_projects" (
  "credential_grant" INTEGER NOT NULL,
  "value" TEXT NOT NULL,
  PRIMARY KEY ("credential_grant", "value"),
  FOREIGN KEY ("credential_grant") REFERENCES "credential_grant" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE TABLE "credential_grant_repositories" (
  "credential_grant" INTEGER NOT NULL,
  "value" TEXT NOT NULL,
  PRIMARY KEY ("credential_grant", "value"),
  FOREIGN KEY ("credential_grant") REFERENCES "credential_grant" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE TABLE "credential_grant_roles" (
  "credential_grant" INTEGER NOT NULL,
  "value" TEXT NOT NULL,
  PRIMARY KEY ("credential_grant", "value"),
  FOREIGN KEY ("credential_grant") REFERENCES "credential_grant" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE TABLE "credential_grant_credentials" (
  "credential_grant" INTEGER NOT NULL,
  "value" TEXT NOT NULL,
  PRIMARY KEY ("credential_grant", "value"),
  FOREIGN KEY ("credential_grant") REFERENCES "credential_grant" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE TABLE "credential_grant_landing_credentials" (
  "credential_grant" INTEGER NOT NULL,
  "key" TEXT NOT NULL,
  "tag" TEXT NOT NULL CHECK ("tag" IN ('br', 'tm')),
  "repository" TEXT,
  "branch" TEXT,
  PRIMARY KEY ("credential_grant", "key"),
  FOREIGN KEY ("credential_grant") REFERENCES "credential_grant" ("id") ON DELETE CASCADE,
  CHECK (CASE WHEN "tag" = 'br'
      THEN "repository" IS NOT NULL
      AND "branch" IS NOT NULL
      END),
  CHECK (CASE WHEN "tag" = 'tm'
      THEN "repository" IS NULL
      AND "branch" IS NULL
      END)
) STRICT, WITHOUT ROWID;
INSERT INTO "codes" ("kind", "field", "code", "name") VALUES
  ('gt', 'host_action', 'bs', 'ImageBuildSecret'),
  ('gt', 'host_action', 'pl', 'ImagePull'),
  ('gt', 'host_action', 'ps', 'ImagePush'),
  ('gt', 'repository_trust', 'fk', 'Fork'),
  ('gt', 'repository_trust', 'ow', 'Own'),
  ('gt', 'landing_credentials.tag', 'br', 'Branch'),
  ('gt', 'landing_credentials.tag', 'tm', 'TemporalOnly');
CREATE VIEW "credential_grant_view" AS SELECT
  "credential_grant"."id",
  "credential_grant"."version",
  (SELECT "name" FROM "codes" WHERE "kind" = 'gt' AND "field" = 'host_action' AND "code" = "credential_grant"."host_action") AS "host_action",
  (SELECT "name" FROM "codes" WHERE "kind" = 'gt' AND "field" = 'repository_trust' AND "code" = "credential_grant"."repository_trust") AS "repository_trust",
  "credential_grant"."permissions"
FROM "credential_grant";
CREATE VIEW "credential_grant_host_action_hosts_view" AS SELECT
  "credential_grant_host_action_hosts"."credential_grant",
  "credential_grant_host_action_hosts"."value"
FROM "credential_grant_host_action_hosts";
CREATE VIEW "credential_grant_projects_view" AS SELECT
  "credential_grant_projects"."credential_grant",
  "credential_grant_projects"."value"
FROM "credential_grant_projects";
CREATE VIEW "credential_grant_repositories_view" AS SELECT
  "credential_grant_repositories"."credential_grant",
  "credential_grant_repositories"."value"
FROM "credential_grant_repositories";
CREATE VIEW "credential_grant_roles_view" AS SELECT
  "credential_grant_roles"."credential_grant",
  "credential_grant_roles"."value"
FROM "credential_grant_roles";
CREATE VIEW "credential_grant_credentials_view" AS SELECT
  "credential_grant_credentials"."credential_grant",
  "credential_grant_credentials"."value"
FROM "credential_grant_credentials";
CREATE VIEW "credential_grant_landing_credentials_view" AS SELECT
  "credential_grant_landing_credentials"."credential_grant",
  "credential_grant_landing_credentials"."key",
  (SELECT "name" FROM "codes" WHERE "kind" = 'gt' AND "field" = 'landing_credentials.tag' AND "code" = "credential_grant_landing_credentials"."tag") AS "tag",
  "credential_grant_landing_credentials"."repository",
  "credential_grant_landing_credentials"."branch"
FROM "credential_grant_landing_credentials";
CREATE TABLE "image_layer" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "layer" TEXT NOT NULL,
  "layer_version" TEXT NOT NULL,
  "tier" TEXT NOT NULL CHECK ("tier" IN ('ar', 'ba', 'pr', 'sh')),
  "source" TEXT NOT NULL CHECK ("source" IN ('in', 'pr', 'sl')),
  "project" TEXT,
  "definition" BLOB NOT NULL REFERENCES "objects" ("hash")
) STRICT;
CREATE TABLE "image_layer_provides" (
  "image_layer" INTEGER NOT NULL,
  "value" TEXT NOT NULL,
  PRIMARY KEY ("image_layer", "value"),
  FOREIGN KEY ("image_layer") REFERENCES "image_layer" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
INSERT INTO "codes" ("kind", "field", "code", "name") VALUES
  ('il', 'tier', 'ar', 'AgentRuntime'),
  ('il', 'tier', 'ba', 'Base'),
  ('il', 'tier', 'pr', 'Project'),
  ('il', 'tier', 'sh', 'Shared'),
  ('il', 'source', 'in', 'Installation'),
  ('il', 'source', 'pr', 'Project'),
  ('il', 'source', 'sl', 'Stdlib');
CREATE VIEW "image_layer_view" AS SELECT
  "image_layer"."id",
  "image_layer"."version",
  "image_layer"."layer",
  "image_layer"."layer_version",
  (SELECT "name" FROM "codes" WHERE "kind" = 'il' AND "field" = 'tier' AND "code" = "image_layer"."tier") AS "tier",
  (SELECT "name" FROM "codes" WHERE "kind" = 'il' AND "field" = 'source' AND "code" = "image_layer"."source") AS "source",
  "image_layer"."project",
  "image_layer"."definition"
FROM "image_layer";
CREATE VIEW "image_layer_provides_view" AS SELECT
  "image_layer_provides"."image_layer",
  "image_layer_provides"."value"
FROM "image_layer_provides";
CREATE TABLE "workflow_template" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "project" TEXT,
  "template" BLOB NOT NULL REFERENCES "objects" ("hash")
) STRICT;
CREATE TABLE "workflow_template_roles" (
  "workflow_template" INTEGER NOT NULL,
  "value" TEXT NOT NULL,
  PRIMARY KEY ("workflow_template", "value"),
  FOREIGN KEY ("workflow_template") REFERENCES "workflow_template" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE VIEW "workflow_template_view" AS SELECT
  "workflow_template"."id",
  "workflow_template"."version",
  "workflow_template"."project",
  "workflow_template"."template"
FROM "workflow_template";
CREATE VIEW "workflow_template_roles_view" AS SELECT
  "workflow_template_roles"."workflow_template",
  "workflow_template_roles"."value"
FROM "workflow_template_roles";
CREATE VIEW "log_view" AS SELECT
  (SELECT "s" FROM "strings" WHERE "id" = "l"."home") AS "home",
  "l"."home_seq",
  "i"."kind",
  (SELECT "s" FROM "strings" WHERE "id" = "i"."name") AS "name",
  "l"."prev_seq",
  "l"."cause",
  (SELECT "name" FROM "codes" WHERE "kind" = "i"."kind" AND "field" = 'log:edge' AND "code" = "l"."edge") AS "edge",
  (SELECT "name" FROM "codes" WHERE "kind" = "i"."kind" AND "field" = 'log:label' AND "code" = "l"."from") AS "from",
  (SELECT "name" FROM "codes" WHERE "kind" = "i"."kind" AND "field" = 'log:label' AND "code" = "l"."to") AS "to",
  (SELECT "a"."kind" || ':' || (SELECT "s" FROM "strings" WHERE "id" = "a"."name") || '@' || (SELECT "s" FROM "strings" WHERE "id" = "a"."home") FROM "resource_identity" AS "a" WHERE "a"."id" = "l"."about") AS "about",
  "l"."change",
  (SELECT "s" FROM "strings" WHERE "id" = "c"."writer") AS "writer",
  "c"."at",
  "l"."arguments",
  "l"."effect",
  "l"."topic"
FROM "log" AS "l" JOIN "change" AS "c" ON "c"."home" = "l"."home" AND "c"."seq" = "l"."change"
LEFT JOIN "resource_identity" AS "i" ON "i"."id" = "l"."resource";
CREATE VIEW stream_watermark_view AS SELECT (SELECT s FROM strings WHERE id = home) AS home, home_seq, applied_through FROM stream_watermark;
CREATE VIEW stream_history_view AS SELECT (SELECT s FROM strings WHERE id = home) AS home, home_seq FROM stream_history;
