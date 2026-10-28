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
CREATE TABLE "project" (
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
  "parent" TEXT,
  "chartered" INTEGER NOT NULL CHECK ("chartered" IN (0, 1)),
  "definition" BLOB NOT NULL REFERENCES "objects" ("hash"),
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
      END)
) STRICT;
CREATE TABLE "project_repositories" (
  "project" INTEGER NOT NULL,
  "key" TEXT NOT NULL,
  "charter_host" TEXT,
  PRIMARY KEY ("project", "key"),
  FOREIGN KEY ("project") REFERENCES "project" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
INSERT INTO "codes" ("kind", "field", "code", "name") VALUES
  ('pj', 'origin', 'mf', 'Manifest'),
  ('pj', 'origin', 'op', 'Operator'),
  ('pj', 'origin', 'pd', 'ProjectDeclaration'),
  ('pj', 'origin', 'pi', 'ProjectImage'),
  ('pj', 'origin', 'po', 'ProjectOps'),
  ('pj', 'origin', 'sn', 'Snapshot'),
  ('pj', 'origin', 'wr', 'WholeRepository');
CREATE VIEW "project_view" AS SELECT
  "project"."id",
  "project"."version",
  (SELECT "name" FROM "codes" WHERE "kind" = 'pj' AND "field" = 'origin' AND "code" = "project"."origin") AS "origin",
  "project"."origin_root",
  "project"."origin_source",
  "project"."origin_path",
  "project"."origin_revision",
  "project"."origin_charter_source",
  "project"."origin_charter_revision",
  "project"."origin_charter_scope",
  "project"."origin_materialized_project",
  "project"."origin_project",
  "project"."origin_repository",
  "project"."origin_commit",
  "project"."origin_entry_path",
  "project"."origin_file",
  "project"."parent",
  "project"."chartered",
  "project"."definition"
FROM "project";
CREATE VIEW "project_repositories_view" AS SELECT
  "project_repositories"."project",
  "project_repositories"."key",
  "project_repositories"."charter_host"
FROM "project_repositories";
CREATE TABLE "project_ops" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "refused_entry_path" TEXT,
  "refused_message" TEXT,
  "refused_since" INTEGER,
  "refused_observed_at" INTEGER,
  "entries_ready" INTEGER CHECK ("entries_ready" IN (0, 1)),
  "entries_message" TEXT,
  CHECK (CASE WHEN "refused_entry_path" IS NOT NULL
      THEN "refused_message" IS NOT NULL
      AND "refused_since" IS NOT NULL
      AND "refused_observed_at" IS NOT NULL
      ELSE "refused_message" IS NULL
      AND "refused_since" IS NULL
      AND "refused_observed_at" IS NULL
      END),
  CHECK (CASE WHEN "entries_ready" IS NOT NULL
      THEN "entries_message" IS NOT NULL
      ELSE "entries_message" IS NULL
      END)
) STRICT;
CREATE VIEW "project_ops_view" AS SELECT
  "project_ops"."id",
  "project_ops"."version",
  "project_ops"."refused_entry_path",
  "project_ops"."refused_message",
  "project_ops"."refused_since",
  "project_ops"."refused_observed_at",
  "project_ops"."entries_ready",
  "project_ops"."entries_message"
FROM "project_ops";
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
