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
CREATE TABLE "host" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "display_name" TEXT NOT NULL,
  "connection" TEXT NOT NULL CHECK ("connection" IN ('dm', 'ss')),
  "connection_owning_daemon" TEXT,
  "image_builds" TEXT CHECK ("image_builds" IN ('bu', 'no')),
  "image_builds_architecture" TEXT,
  "image_builds_slots" INTEGER,
  "image_builds_reservation_cpu" INTEGER,
  "image_builds_reservation_disk_bytes" INTEGER,
  "max_vessels" INTEGER,
  "single_tenant" INTEGER NOT NULL CHECK ("single_tenant" IN (0, 1)),
  CHECK (CASE WHEN "connection" = 'dm'
      THEN "connection_owning_daemon" IS NULL
      END),
  CHECK (CASE WHEN "connection" = 'ss'
      THEN "connection_owning_daemon" IS NOT NULL
      END),
  CHECK (CASE WHEN "image_builds" = 'no'
      THEN "image_builds_architecture" IS NULL
      AND "image_builds_slots" IS NULL
      AND "image_builds_reservation_cpu" IS NULL
      AND "image_builds_reservation_disk_bytes" IS NULL
      END),
  CHECK (CASE WHEN "image_builds" = 'bu'
      THEN "image_builds_architecture" IS NOT NULL
      AND "image_builds_slots" IS NOT NULL
      AND "image_builds_reservation_cpu" IS NOT NULL
      AND "image_builds_reservation_disk_bytes" IS NOT NULL
      END),
  CHECK (CASE WHEN "image_builds" IS NOT NULL
      THEN 1
      ELSE "image_builds_architecture" IS NULL
      AND "image_builds_slots" IS NULL
      AND "image_builds_reservation_cpu" IS NULL
      AND "image_builds_reservation_disk_bytes" IS NULL
      END)
) STRICT;
INSERT INTO "codes" ("kind", "field", "code", "name") VALUES
  ('ho', 'connection', 'dm', 'Daemon'),
  ('ho', 'connection', 'ss', 'SshHost'),
  ('ho', 'image_builds', 'bu', 'Builder'),
  ('ho', 'image_builds', 'no', 'None');
CREATE VIEW "host_view" AS SELECT
  "host"."id",
  "host"."version",
  "host"."display_name",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ho' AND "field" = 'connection' AND "code" = "host"."connection") AS "connection",
  "host"."connection_owning_daemon",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ho' AND "field" = 'image_builds' AND "code" = "host"."image_builds") AS "image_builds",
  "host"."image_builds_architecture",
  "host"."image_builds_slots",
  "host"."image_builds_reservation_cpu",
  "host"."image_builds_reservation_disk_bytes",
  "host"."max_vessels",
  "host"."single_tenant"
FROM "host";
CREATE TABLE "host_observation" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "daemon_generation" TEXT,
  "architecture" TEXT,
  "below_free_space_floor" INTEGER NOT NULL CHECK ("below_free_space_floor" IN (0, 1)),
  "codex_central_login" INTEGER NOT NULL CHECK ("codex_central_login" IN (0, 1))
) STRICT;
CREATE TABLE "host_observation_conditions" (
  "host_observation" INTEGER NOT NULL,
  "key" TEXT NOT NULL,
  "value" TEXT NOT NULL CHECK ("value" IN ('f', 't', 'u')),
  "reason" TEXT NOT NULL,
  "message" TEXT NOT NULL,
  "blocks_readiness" INTEGER NOT NULL CHECK ("blocks_readiness" IN (0, 1)),
  "blocks_forge_observation" INTEGER NOT NULL CHECK ("blocks_forge_observation" IN (0, 1)),
  PRIMARY KEY ("host_observation", "key"),
  FOREIGN KEY ("host_observation") REFERENCES "host_observation" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE TABLE "host_observation_providers" (
  "host_observation" INTEGER NOT NULL,
  "key" TEXT NOT NULL,
  "class" TEXT CHECK ("class" IN ('cs', 'ct', 'dy', 'hd', 'mv')),
  "readiness" TEXT NOT NULL CHECK ("readiness" IN ('mc', 'mt', 'rd', 'un')),
  "readiness_reason" TEXT,
  "workspace_root" TEXT NOT NULL,
  "material_modes" BLOB NOT NULL,
  "checkout_strategies" BLOB NOT NULL,
  PRIMARY KEY ("host_observation", "key"),
  FOREIGN KEY ("host_observation") REFERENCES "host_observation" ("id") ON DELETE CASCADE,
  CHECK (CASE WHEN "readiness" = 'rd'
      THEN "readiness_reason" IS NULL
      END),
  CHECK (CASE WHEN "readiness" = 'mt'
      THEN "readiness_reason" IS NOT NULL
      END),
  CHECK (CASE WHEN "readiness" = 'mc'
      THEN "readiness_reason" IS NOT NULL
      END),
  CHECK (CASE WHEN "readiness" = 'un'
      THEN "readiness_reason" IS NOT NULL
      END)
) STRICT, WITHOUT ROWID;
CREATE TABLE "host_observation_agent_adapters" (
  "host_observation" INTEGER NOT NULL,
  "value" TEXT NOT NULL,
  PRIMARY KEY ("host_observation", "value"),
  FOREIGN KEY ("host_observation") REFERENCES "host_observation" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE TABLE "host_observation_agent_adapter_baseline" (
  "host_observation" INTEGER NOT NULL,
  "value" TEXT NOT NULL,
  PRIMARY KEY ("host_observation", "value"),
  FOREIGN KEY ("host_observation") REFERENCES "host_observation" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE TABLE "host_observation_held_credentials" (
  "host_observation" INTEGER NOT NULL,
  "value" TEXT NOT NULL,
  PRIMARY KEY ("host_observation", "value"),
  FOREIGN KEY ("host_observation") REFERENCES "host_observation" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE TABLE "host_observation_credential_expiry" (
  "host_observation" INTEGER NOT NULL,
  "key" TEXT NOT NULL,
  "expires_at" INTEGER,
  "refresh_expires_at" INTEGER,
  PRIMARY KEY ("host_observation", "key"),
  FOREIGN KEY ("host_observation") REFERENCES "host_observation" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE TABLE "host_observation_image_digests" (
  "host_observation" INTEGER NOT NULL,
  "key" TEXT NOT NULL,
  PRIMARY KEY ("host_observation", "key"),
  FOREIGN KEY ("host_observation") REFERENCES "host_observation" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE TABLE "host_observation_image_digests_images" (
  "host_observation" INTEGER NOT NULL,
  "image_digests_key" TEXT NOT NULL,
  "value" TEXT NOT NULL,
  PRIMARY KEY ("host_observation", "image_digests_key", "value"),
  FOREIGN KEY ("host_observation", "image_digests_key") REFERENCES "host_observation_image_digests" ("host_observation", "key") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE TABLE "host_observation_facts" (
  "host_observation" INTEGER NOT NULL,
  "key" TEXT NOT NULL,
  "crew_pool_missing" INTEGER NOT NULL CHECK ("crew_pool_missing" IN (0, 1)),
  "gui_session_logged_in" INTEGER NOT NULL CHECK ("gui_session_logged_in" IN (0, 1)),
  PRIMARY KEY ("host_observation", "key"),
  FOREIGN KEY ("host_observation") REFERENCES "host_observation" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE TABLE "host_observation_facts_harnesses" (
  "host_observation" INTEGER NOT NULL,
  "facts_key" TEXT NOT NULL,
  "key" TEXT NOT NULL,
  "value" TEXT NOT NULL,
  PRIMARY KEY ("host_observation", "facts_key", "key"),
  FOREIGN KEY ("host_observation", "facts_key") REFERENCES "host_observation_facts" ("host_observation", "key") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE TABLE "host_observation_facts_toolchains" (
  "host_observation" INTEGER NOT NULL,
  "facts_key" TEXT NOT NULL,
  "key" TEXT NOT NULL,
  "value" TEXT NOT NULL,
  PRIMARY KEY ("host_observation", "facts_key", "key"),
  FOREIGN KEY ("host_observation", "facts_key") REFERENCES "host_observation_facts" ("host_observation", "key") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
INSERT INTO "codes" ("kind", "field", "code", "name") VALUES
  ('ob', 'conditions.value', 'f', 'False'),
  ('ob', 'conditions.value', 't', 'True'),
  ('ob', 'conditions.value', 'u', 'Unknown'),
  ('ob', 'providers.class', 'cs', 'CloudSandbox'),
  ('ob', 'providers.class', 'ct', 'Container'),
  ('ob', 'providers.class', 'dy', 'Dummy'),
  ('ob', 'providers.class', 'hd', 'HostDirect'),
  ('ob', 'providers.class', 'mv', 'MicroVm'),
  ('ob', 'providers.readiness', 'mc', 'MissingCredential'),
  ('ob', 'providers.readiness', 'mt', 'MissingTool'),
  ('ob', 'providers.readiness', 'rd', 'Ready'),
  ('ob', 'providers.readiness', 'un', 'Unknown');
CREATE VIEW "host_observation_view" AS SELECT
  "host_observation"."id",
  "host_observation"."version",
  "host_observation"."daemon_generation",
  "host_observation"."architecture",
  "host_observation"."below_free_space_floor",
  "host_observation"."codex_central_login"
FROM "host_observation";
CREATE VIEW "host_observation_conditions_view" AS SELECT
  "host_observation_conditions"."host_observation",
  "host_observation_conditions"."key",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ob' AND "field" = 'conditions.value' AND "code" = "host_observation_conditions"."value") AS "value",
  "host_observation_conditions"."reason",
  "host_observation_conditions"."message",
  "host_observation_conditions"."blocks_readiness",
  "host_observation_conditions"."blocks_forge_observation"
FROM "host_observation_conditions";
CREATE VIEW "host_observation_providers_view" AS SELECT
  "host_observation_providers"."host_observation",
  "host_observation_providers"."key",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ob' AND "field" = 'providers.class' AND "code" = "host_observation_providers"."class") AS "class",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ob' AND "field" = 'providers.readiness' AND "code" = "host_observation_providers"."readiness") AS "readiness",
  "host_observation_providers"."readiness_reason",
  "host_observation_providers"."workspace_root",
  "host_observation_providers"."material_modes",
  "host_observation_providers"."checkout_strategies"
FROM "host_observation_providers";
CREATE VIEW "host_observation_agent_adapters_view" AS SELECT
  "host_observation_agent_adapters"."host_observation",
  "host_observation_agent_adapters"."value"
FROM "host_observation_agent_adapters";
CREATE VIEW "host_observation_agent_adapter_baseline_view" AS SELECT
  "host_observation_agent_adapter_baseline"."host_observation",
  "host_observation_agent_adapter_baseline"."value"
FROM "host_observation_agent_adapter_baseline";
CREATE VIEW "host_observation_held_credentials_view" AS SELECT
  "host_observation_held_credentials"."host_observation",
  "host_observation_held_credentials"."value"
FROM "host_observation_held_credentials";
CREATE VIEW "host_observation_credential_expiry_view" AS SELECT
  "host_observation_credential_expiry"."host_observation",
  "host_observation_credential_expiry"."key",
  "host_observation_credential_expiry"."expires_at",
  "host_observation_credential_expiry"."refresh_expires_at"
FROM "host_observation_credential_expiry";
CREATE VIEW "host_observation_image_digests_view" AS SELECT
  "host_observation_image_digests"."host_observation",
  "host_observation_image_digests"."key"
FROM "host_observation_image_digests";
CREATE VIEW "host_observation_image_digests_images_view" AS SELECT
  "host_observation_image_digests_images"."host_observation",
  "host_observation_image_digests_images"."image_digests_key",
  "host_observation_image_digests_images"."value"
FROM "host_observation_image_digests_images";
CREATE VIEW "host_observation_facts_view" AS SELECT
  "host_observation_facts"."host_observation",
  "host_observation_facts"."key",
  "host_observation_facts"."crew_pool_missing",
  "host_observation_facts"."gui_session_logged_in"
FROM "host_observation_facts";
CREATE VIEW "host_observation_facts_harnesses_view" AS SELECT
  "host_observation_facts_harnesses"."host_observation",
  "host_observation_facts_harnesses"."facts_key",
  "host_observation_facts_harnesses"."key",
  "host_observation_facts_harnesses"."value"
FROM "host_observation_facts_harnesses";
CREATE VIEW "host_observation_facts_toolchains_view" AS SELECT
  "host_observation_facts_toolchains"."host_observation",
  "host_observation_facts_toolchains"."facts_key",
  "host_observation_facts_toolchains"."key",
  "host_observation_facts_toolchains"."value"
FROM "host_observation_facts_toolchains";
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
