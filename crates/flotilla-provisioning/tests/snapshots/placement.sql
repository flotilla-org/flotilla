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
CREATE TABLE log (
  home INTEGER NOT NULL REFERENCES strings(id), home_seq INTEGER NOT NULL,
  resource INTEGER REFERENCES resource_identity(id), prev_seq INTEGER NOT NULL,
  cause TEXT NOT NULL CHECK (cause IN ('create', 'edit', 'edge', 'delete', 'expunge')),
  edge TEXT, "from" TEXT, "to" TEXT,
  about INTEGER REFERENCES resource_identity(id) ON DELETE SET NULL,
  writer INTEGER NOT NULL REFERENCES strings(id), at INTEGER NOT NULL,
  payload BLOB, effect BLOB NOT NULL,
  topic TEXT NOT NULL DEFAULT 'all' CHECK (length(topic) BETWEEN 1 AND 8 AND topic NOT GLOB '*[^a-z0-9_]*' AND substr(topic, 1, 1) GLOB '[a-z]'),
  PRIMARY KEY (home, home_seq),
  CHECK ((resource IS NULL) = (cause = 'expunge')),
  CHECK (CASE WHEN cause = 'edge' THEN edge IS NOT NULL AND "from" IS NOT NULL AND "to" IS NOT NULL
    ELSE edge IS NULL AND "from" IS NULL AND "to" IS NULL AND about IS NULL END)) STRICT;
CREATE INDEX log_by_resource ON log (resource, home_seq);
CREATE INDEX log_about ON log (about, at) WHERE about IS NOT NULL;
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
CREATE TABLE "environment_provider" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "origin" TEXT NOT NULL CHECK ("origin" IN ('mf', 'op', 'pd', 'pi', 'po', 'sn', 'wr')),
  "origin_root" TEXT,
  "origin_source" TEXT,
  "origin_path" TEXT,
  "origin_revision" TEXT,
  "origin_charter_source" TEXT,
  "origin_charter_revision" TEXT,
  "origin_charter_scope" TEXT,
  "origin_materialized_project" TEXT,
  "origin_project" TEXT,
  "origin_repository" TEXT,
  "origin_commit" TEXT,
  "origin_entry_path" TEXT,
  "origin_file" TEXT,
  "host_ref" TEXT NOT NULL,
  "instance" TEXT,
  "settings" TEXT NOT NULL CHECK ("settings" IN ('cs', 'dk', 'hd')),
  "settings_pull_policy" TEXT CHECK ("settings_pull_policy" IN ('al', 'ip', 'nv')),
  "settings_memory_policy_host_memory_percent" INTEGER,
  "settings_memory_policy_expected_concurrent_crews" INTEGER,
  "settings_memory_policy_swap_bytes" INTEGER,
  "settings_default_cwd" TEXT,
  "settings_checkout" TEXT CHECK ("settings_checkout" IN ('fc', 'lm', 'wm')),
  "settings_checkout_mount_path" TEXT,
  "settings_checkout_clone_path" TEXT,
  "settings_host_checkout" TEXT CHECK ("settings_host_checkout" IN ('wt')),
  "settings_size_cpus" INTEGER,
  "settings_size_memory_bytes" INTEGER,
  "settings_size_disk_bytes" INTEGER,
  "settings_sandbox_checkout" TEXT CHECK ("settings_sandbox_checkout" IN ('cv')),
  CHECK (CASE WHEN "origin_charter_source" IS NOT NULL
      THEN "origin_charter_revision" IS NOT NULL
      AND "origin_charter_scope" IS NOT NULL
      ELSE "origin_charter_revision" IS NULL
      AND "origin_charter_scope" IS NULL
      END),
  CHECK (CASE WHEN "origin" = 'op'
      THEN "origin_root" IS NULL
      AND "origin_source" IS NULL
      AND "origin_path" IS NULL
      AND "origin_revision" IS NULL
      AND "origin_charter_source" IS NULL
      AND "origin_charter_revision" IS NULL
      AND "origin_charter_scope" IS NULL
      AND "origin_materialized_project" IS NULL
      AND "origin_project" IS NULL
      AND "origin_repository" IS NULL
      AND "origin_commit" IS NULL
      AND "origin_entry_path" IS NULL
      AND "origin_file" IS NULL
      END),
  CHECK (CASE WHEN "origin" = 'mf'
      THEN "origin_root" IS NOT NULL
      AND "origin_source" IS NOT NULL
      AND "origin_path" IS NOT NULL
      AND "origin_revision" IS NOT NULL
      AND "origin_project" IS NULL
      AND "origin_repository" IS NULL
      AND "origin_commit" IS NULL
      AND "origin_entry_path" IS NULL
      AND "origin_file" IS NULL
      END),
  CHECK (CASE WHEN "origin" = 'po'
      THEN "origin_project" IS NOT NULL
      AND "origin_repository" IS NOT NULL
      AND "origin_commit" IS NOT NULL
      AND "origin_entry_path" IS NOT NULL
      AND "origin_root" IS NULL
      AND "origin_source" IS NULL
      AND "origin_path" IS NULL
      AND "origin_revision" IS NULL
      AND "origin_charter_source" IS NULL
      AND "origin_charter_revision" IS NULL
      AND "origin_charter_scope" IS NULL
      AND "origin_materialized_project" IS NULL
      AND "origin_file" IS NULL
      END),
  CHECK (CASE WHEN "origin" = 'pi'
      THEN "origin_project" IS NOT NULL
      AND "origin_commit" IS NOT NULL
      AND "origin_root" IS NULL
      AND "origin_source" IS NULL
      AND "origin_path" IS NULL
      AND "origin_revision" IS NULL
      AND "origin_charter_source" IS NULL
      AND "origin_charter_revision" IS NULL
      AND "origin_charter_scope" IS NULL
      AND "origin_materialized_project" IS NULL
      AND "origin_repository" IS NULL
      AND "origin_entry_path" IS NULL
      AND "origin_file" IS NULL
      END),
  CHECK (CASE WHEN "origin" = 'sn'
      THEN "origin_root" IS NULL
      AND "origin_source" IS NULL
      AND "origin_path" IS NULL
      AND "origin_revision" IS NULL
      AND "origin_charter_source" IS NULL
      AND "origin_charter_revision" IS NULL
      AND "origin_charter_scope" IS NULL
      AND "origin_materialized_project" IS NULL
      AND "origin_project" IS NULL
      AND "origin_repository" IS NULL
      AND "origin_commit" IS NULL
      AND "origin_entry_path" IS NULL
      AND "origin_file" IS NULL
      END),
  CHECK (CASE WHEN "origin" = 'pd'
      THEN "origin_repository" IS NOT NULL
      AND "origin_commit" IS NOT NULL
      AND "origin_file" IS NOT NULL
      AND "origin_root" IS NULL
      AND "origin_source" IS NULL
      AND "origin_path" IS NULL
      AND "origin_revision" IS NULL
      AND "origin_charter_source" IS NULL
      AND "origin_charter_revision" IS NULL
      AND "origin_charter_scope" IS NULL
      AND "origin_materialized_project" IS NULL
      AND "origin_project" IS NULL
      AND "origin_entry_path" IS NULL
      END),
  CHECK (CASE WHEN "origin" = 'wr'
      THEN "origin_root" IS NULL
      AND "origin_source" IS NULL
      AND "origin_path" IS NULL
      AND "origin_revision" IS NULL
      AND "origin_charter_source" IS NULL
      AND "origin_charter_revision" IS NULL
      AND "origin_charter_scope" IS NULL
      AND "origin_materialized_project" IS NULL
      AND "origin_project" IS NULL
      AND "origin_repository" IS NULL
      AND "origin_commit" IS NULL
      AND "origin_entry_path" IS NULL
      AND "origin_file" IS NULL
      END),
  CHECK (CASE WHEN "settings_checkout" = 'wm'
      THEN "settings_checkout_mount_path" IS NOT NULL
      AND "settings_checkout_clone_path" IS NULL
      END),
  CHECK (CASE WHEN "settings_checkout" = 'lm'
      THEN "settings_checkout_mount_path" IS NOT NULL
      AND "settings_checkout_clone_path" IS NULL
      END),
  CHECK (CASE WHEN "settings_checkout" = 'fc'
      THEN "settings_checkout_clone_path" IS NOT NULL
      AND "settings_checkout_mount_path" IS NULL
      END),
  CHECK (CASE WHEN "settings" = 'dk'
      THEN "settings_pull_policy" IS NOT NULL
      AND "settings_memory_policy_host_memory_percent" IS NOT NULL
      AND "settings_memory_policy_expected_concurrent_crews" IS NOT NULL
      AND "settings_memory_policy_swap_bytes" IS NOT NULL
      AND "settings_checkout" IS NOT NULL
      AND "settings_host_checkout" IS NULL
      AND "settings_size_cpus" IS NULL
      AND "settings_size_memory_bytes" IS NULL
      AND "settings_size_disk_bytes" IS NULL
      AND "settings_sandbox_checkout" IS NULL
      END),
  CHECK (CASE WHEN "settings" = 'hd'
      THEN "settings_host_checkout" IS NOT NULL
      AND "settings_pull_policy" IS NULL
      AND "settings_memory_policy_host_memory_percent" IS NULL
      AND "settings_memory_policy_expected_concurrent_crews" IS NULL
      AND "settings_memory_policy_swap_bytes" IS NULL
      AND "settings_default_cwd" IS NULL
      AND "settings_checkout" IS NULL
      AND "settings_checkout_mount_path" IS NULL
      AND "settings_checkout_clone_path" IS NULL
      AND "settings_size_cpus" IS NULL
      AND "settings_size_memory_bytes" IS NULL
      AND "settings_size_disk_bytes" IS NULL
      AND "settings_sandbox_checkout" IS NULL
      END),
  CHECK (CASE WHEN "settings" = 'cs'
      THEN "settings_size_cpus" IS NOT NULL
      AND "settings_size_memory_bytes" IS NOT NULL
      AND "settings_size_disk_bytes" IS NOT NULL
      AND "settings_sandbox_checkout" IS NOT NULL
      AND "settings_pull_policy" IS NULL
      AND "settings_memory_policy_host_memory_percent" IS NULL
      AND "settings_memory_policy_expected_concurrent_crews" IS NULL
      AND "settings_memory_policy_swap_bytes" IS NULL
      AND "settings_default_cwd" IS NULL
      AND "settings_checkout" IS NULL
      AND "settings_checkout_mount_path" IS NULL
      AND "settings_checkout_clone_path" IS NULL
      AND "settings_host_checkout" IS NULL
      END)
) STRICT;
CREATE TABLE "environment_provider_settings_env" (
  "environment_provider" INTEGER NOT NULL,
  "key" TEXT NOT NULL,
  "value" TEXT NOT NULL,
  PRIMARY KEY ("environment_provider", "key"),
  FOREIGN KEY ("environment_provider") REFERENCES "environment_provider" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
INSERT INTO "codes" ("kind", "field", "code", "name") VALUES
  ('ep', 'origin', 'mf', 'Manifest'),
  ('ep', 'origin', 'op', 'Operator'),
  ('ep', 'origin', 'pd', 'ProjectDeclaration'),
  ('ep', 'origin', 'pi', 'ProjectImage'),
  ('ep', 'origin', 'po', 'ProjectOps'),
  ('ep', 'origin', 'sn', 'Snapshot'),
  ('ep', 'origin', 'wr', 'WholeRepository'),
  ('ep', 'settings', 'cs', 'CloudSandboxPerVessel'),
  ('ep', 'settings', 'dk', 'DockerPerVessel'),
  ('ep', 'settings', 'hd', 'HostDirect'),
  ('ep', 'settings_pull_policy', 'al', 'Always'),
  ('ep', 'settings_pull_policy', 'ip', 'IfNotPresent'),
  ('ep', 'settings_pull_policy', 'nv', 'Never'),
  ('ep', 'settings_checkout', 'fc', 'FreshCloneInContainer'),
  ('ep', 'settings_checkout', 'lm', 'LocalCloneOnHostAndMount'),
  ('ep', 'settings_checkout', 'wm', 'WorktreeOnHostAndMount'),
  ('ep', 'settings_host_checkout', 'wt', 'Worktree'),
  ('ep', 'settings_sandbox_checkout', 'cv', 'CloneInVessel');
CREATE VIEW "environment_provider_view" AS SELECT
  "environment_provider"."id",
  "environment_provider"."version",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ep' AND "field" = 'origin' AND "code" = "environment_provider"."origin") AS "origin",
  "environment_provider"."origin_root",
  "environment_provider"."origin_source",
  "environment_provider"."origin_path",
  "environment_provider"."origin_revision",
  "environment_provider"."origin_charter_source",
  "environment_provider"."origin_charter_revision",
  "environment_provider"."origin_charter_scope",
  "environment_provider"."origin_materialized_project",
  "environment_provider"."origin_project",
  "environment_provider"."origin_repository",
  "environment_provider"."origin_commit",
  "environment_provider"."origin_entry_path",
  "environment_provider"."origin_file",
  "environment_provider"."host_ref",
  "environment_provider"."instance",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ep' AND "field" = 'settings' AND "code" = "environment_provider"."settings") AS "settings",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ep' AND "field" = 'settings_pull_policy' AND "code" = "environment_provider"."settings_pull_policy") AS "settings_pull_policy",
  "environment_provider"."settings_memory_policy_host_memory_percent",
  "environment_provider"."settings_memory_policy_expected_concurrent_crews",
  "environment_provider"."settings_memory_policy_swap_bytes",
  "environment_provider"."settings_default_cwd",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ep' AND "field" = 'settings_checkout' AND "code" = "environment_provider"."settings_checkout") AS "settings_checkout",
  "environment_provider"."settings_checkout_mount_path",
  "environment_provider"."settings_checkout_clone_path",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ep' AND "field" = 'settings_host_checkout' AND "code" = "environment_provider"."settings_host_checkout") AS "settings_host_checkout",
  "environment_provider"."settings_size_cpus",
  "environment_provider"."settings_size_memory_bytes",
  "environment_provider"."settings_size_disk_bytes",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ep' AND "field" = 'settings_sandbox_checkout' AND "code" = "environment_provider"."settings_sandbox_checkout") AS "settings_sandbox_checkout"
FROM "environment_provider";
CREATE VIEW "environment_provider_settings_env_view" AS SELECT
  "environment_provider_settings_env"."environment_provider",
  "environment_provider_settings_env"."key",
  "environment_provider_settings_env"."value"
FROM "environment_provider_settings_env";
CREATE TABLE "fulfilment_kind" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "origin" TEXT NOT NULL CHECK ("origin" IN ('mf', 'op', 'pd', 'pi', 'po', 'sn', 'wr')),
  "origin_root" TEXT,
  "origin_source" TEXT,
  "origin_path" TEXT,
  "origin_revision" TEXT,
  "origin_charter_source" TEXT,
  "origin_charter_revision" TEXT,
  "origin_charter_scope" TEXT,
  "origin_materialized_project" TEXT,
  "origin_project" TEXT,
  "origin_repository" TEXT,
  "origin_commit" TEXT,
  "origin_entry_path" TEXT,
  "origin_file" TEXT,
  "host_ref" TEXT NOT NULL,
  "pool" TEXT NOT NULL,
  "cost_class" TEXT NOT NULL CHECK ("cost_class" IN ('me', 'oi', 'si')),
  "realisation" TEXT NOT NULL CHECK ("realisation" IN ('cs', 'db', 'dk', 'dl', 'hd')),
  "realisation_sandbox_image" TEXT,
  "realisation_image" TEXT,
  "realisation_identity" TEXT,
  "realisation_image_build_ref" TEXT,
  CHECK (CASE WHEN "origin_charter_source" IS NOT NULL
      THEN "origin_charter_revision" IS NOT NULL
      AND "origin_charter_scope" IS NOT NULL
      ELSE "origin_charter_revision" IS NULL
      AND "origin_charter_scope" IS NULL
      END),
  CHECK (CASE WHEN "origin" = 'op'
      THEN "origin_root" IS NULL
      AND "origin_source" IS NULL
      AND "origin_path" IS NULL
      AND "origin_revision" IS NULL
      AND "origin_charter_source" IS NULL
      AND "origin_charter_revision" IS NULL
      AND "origin_charter_scope" IS NULL
      AND "origin_materialized_project" IS NULL
      AND "origin_project" IS NULL
      AND "origin_repository" IS NULL
      AND "origin_commit" IS NULL
      AND "origin_entry_path" IS NULL
      AND "origin_file" IS NULL
      END),
  CHECK (CASE WHEN "origin" = 'mf'
      THEN "origin_root" IS NOT NULL
      AND "origin_source" IS NOT NULL
      AND "origin_path" IS NOT NULL
      AND "origin_revision" IS NOT NULL
      AND "origin_project" IS NULL
      AND "origin_repository" IS NULL
      AND "origin_commit" IS NULL
      AND "origin_entry_path" IS NULL
      AND "origin_file" IS NULL
      END),
  CHECK (CASE WHEN "origin" = 'po'
      THEN "origin_project" IS NOT NULL
      AND "origin_repository" IS NOT NULL
      AND "origin_commit" IS NOT NULL
      AND "origin_entry_path" IS NOT NULL
      AND "origin_root" IS NULL
      AND "origin_source" IS NULL
      AND "origin_path" IS NULL
      AND "origin_revision" IS NULL
      AND "origin_charter_source" IS NULL
      AND "origin_charter_revision" IS NULL
      AND "origin_charter_scope" IS NULL
      AND "origin_materialized_project" IS NULL
      AND "origin_file" IS NULL
      END),
  CHECK (CASE WHEN "origin" = 'pi'
      THEN "origin_project" IS NOT NULL
      AND "origin_commit" IS NOT NULL
      AND "origin_root" IS NULL
      AND "origin_source" IS NULL
      AND "origin_path" IS NULL
      AND "origin_revision" IS NULL
      AND "origin_charter_source" IS NULL
      AND "origin_charter_revision" IS NULL
      AND "origin_charter_scope" IS NULL
      AND "origin_materialized_project" IS NULL
      AND "origin_repository" IS NULL
      AND "origin_entry_path" IS NULL
      AND "origin_file" IS NULL
      END),
  CHECK (CASE WHEN "origin" = 'sn'
      THEN "origin_root" IS NULL
      AND "origin_source" IS NULL
      AND "origin_path" IS NULL
      AND "origin_revision" IS NULL
      AND "origin_charter_source" IS NULL
      AND "origin_charter_revision" IS NULL
      AND "origin_charter_scope" IS NULL
      AND "origin_materialized_project" IS NULL
      AND "origin_project" IS NULL
      AND "origin_repository" IS NULL
      AND "origin_commit" IS NULL
      AND "origin_entry_path" IS NULL
      AND "origin_file" IS NULL
      END),
  CHECK (CASE WHEN "origin" = 'pd'
      THEN "origin_repository" IS NOT NULL
      AND "origin_commit" IS NOT NULL
      AND "origin_file" IS NOT NULL
      AND "origin_root" IS NULL
      AND "origin_source" IS NULL
      AND "origin_path" IS NULL
      AND "origin_revision" IS NULL
      AND "origin_charter_source" IS NULL
      AND "origin_charter_revision" IS NULL
      AND "origin_charter_scope" IS NULL
      AND "origin_materialized_project" IS NULL
      AND "origin_project" IS NULL
      AND "origin_entry_path" IS NULL
      END),
  CHECK (CASE WHEN "origin" = 'wr'
      THEN "origin_root" IS NULL
      AND "origin_source" IS NULL
      AND "origin_path" IS NULL
      AND "origin_revision" IS NULL
      AND "origin_charter_source" IS NULL
      AND "origin_charter_revision" IS NULL
      AND "origin_charter_scope" IS NULL
      AND "origin_materialized_project" IS NULL
      AND "origin_project" IS NULL
      AND "origin_repository" IS NULL
      AND "origin_commit" IS NULL
      AND "origin_entry_path" IS NULL
      AND "origin_file" IS NULL
      END),
  CHECK (CASE WHEN "realisation" = 'cs'
      THEN "realisation_sandbox_image" IS NOT NULL
      AND "realisation_image" IS NULL
      AND "realisation_identity" IS NULL
      AND "realisation_image_build_ref" IS NULL
      END),
  CHECK (CASE WHEN "realisation" = 'dk'
      THEN "realisation_sandbox_image" IS NULL
      AND "realisation_image" IS NULL
      AND "realisation_identity" IS NULL
      AND "realisation_image_build_ref" IS NULL
      END),
  CHECK (CASE WHEN "realisation" = 'dl'
      THEN "realisation_image" IS NOT NULL
      AND "realisation_sandbox_image" IS NULL
      AND "realisation_identity" IS NULL
      AND "realisation_image_build_ref" IS NULL
      END),
  CHECK (CASE WHEN "realisation" = 'db'
      THEN "realisation_identity" IS NOT NULL
      AND "realisation_image_build_ref" IS NOT NULL
      AND "realisation_sandbox_image" IS NULL
      AND "realisation_image" IS NULL
      END),
  CHECK (CASE WHEN "realisation" = 'hd'
      THEN "realisation_sandbox_image" IS NULL
      AND "realisation_image" IS NULL
      AND "realisation_identity" IS NULL
      AND "realisation_image_build_ref" IS NULL
      END)
) STRICT;
CREATE TABLE "fulfilment_kind_grants" (
  "fulfilment_kind" INTEGER NOT NULL,
  "value" TEXT NOT NULL,
  PRIMARY KEY ("fulfilment_kind", "value"),
  FOREIGN KEY ("fulfilment_kind") REFERENCES "fulfilment_kind" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
INSERT INTO "codes" ("kind", "field", "code", "name") VALUES
  ('fk', 'origin', 'mf', 'Manifest'),
  ('fk', 'origin', 'op', 'Operator'),
  ('fk', 'origin', 'pd', 'ProjectDeclaration'),
  ('fk', 'origin', 'pi', 'ProjectImage'),
  ('fk', 'origin', 'po', 'ProjectOps'),
  ('fk', 'origin', 'sn', 'Snapshot'),
  ('fk', 'origin', 'wr', 'WholeRepository'),
  ('fk', 'cost_class', 'me', 'Metered'),
  ('fk', 'cost_class', 'oi', 'OwnedIdle'),
  ('fk', 'cost_class', 'si', 'SubscriptionIncluded'),
  ('fk', 'realisation', 'cs', 'CloudSandboxPerVessel'),
  ('fk', 'realisation', 'db', 'DockerBuilt'),
  ('fk', 'realisation', 'dk', 'DockerComposed'),
  ('fk', 'realisation', 'dl', 'DockerLiteral'),
  ('fk', 'realisation', 'hd', 'HostDirect');
CREATE VIEW "fulfilment_kind_view" AS SELECT
  "fulfilment_kind"."id",
  "fulfilment_kind"."version",
  (SELECT "name" FROM "codes" WHERE "kind" = 'fk' AND "field" = 'origin' AND "code" = "fulfilment_kind"."origin") AS "origin",
  "fulfilment_kind"."origin_root",
  "fulfilment_kind"."origin_source",
  "fulfilment_kind"."origin_path",
  "fulfilment_kind"."origin_revision",
  "fulfilment_kind"."origin_charter_source",
  "fulfilment_kind"."origin_charter_revision",
  "fulfilment_kind"."origin_charter_scope",
  "fulfilment_kind"."origin_materialized_project",
  "fulfilment_kind"."origin_project",
  "fulfilment_kind"."origin_repository",
  "fulfilment_kind"."origin_commit",
  "fulfilment_kind"."origin_entry_path",
  "fulfilment_kind"."origin_file",
  "fulfilment_kind"."host_ref",
  "fulfilment_kind"."pool",
  (SELECT "name" FROM "codes" WHERE "kind" = 'fk' AND "field" = 'cost_class' AND "code" = "fulfilment_kind"."cost_class") AS "cost_class",
  (SELECT "name" FROM "codes" WHERE "kind" = 'fk' AND "field" = 'realisation' AND "code" = "fulfilment_kind"."realisation") AS "realisation",
  "fulfilment_kind"."realisation_sandbox_image",
  "fulfilment_kind"."realisation_image",
  "fulfilment_kind"."realisation_identity",
  "fulfilment_kind"."realisation_image_build_ref"
FROM "fulfilment_kind";
CREATE VIEW "fulfilment_kind_grants_view" AS SELECT
  "fulfilment_kind_grants"."fulfilment_kind",
  "fulfilment_kind_grants"."value"
FROM "fulfilment_kind_grants";
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
  (SELECT "s" FROM "strings" WHERE "id" = "l"."writer") AS "writer",
  "l"."at",
  "l"."payload",
  "l"."effect",
  "l"."topic"
FROM "log" AS "l" LEFT JOIN "resource_identity" AS "i" ON "i"."id" = "l"."resource";
CREATE VIEW stream_watermark_view AS SELECT (SELECT s FROM strings WHERE id = home) AS home, home_seq, applied_through FROM stream_watermark;
CREATE VIEW stream_history_view AS SELECT (SELECT s FROM strings WHERE id = home) AS home, home_seq FROM stream_history;
