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
CREATE TABLE "manifest_root" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "host" TEXT NOT NULL,
  "path" TEXT NOT NULL,
  "source" TEXT NOT NULL,
  "state" TEXT NOT NULL CHECK ("state" IN ('bd', 'rt')),
  "state_since" INTEGER NOT NULL,
  "binding" TEXT CHECK ("binding" IN ('ld', 'rp')),
  "binding_repo" TEXT,
  "binding_branch" TEXT,
  "binding_path" TEXT,
  "binding_directory" TEXT,
  "applied" TEXT,
  "source_error" TEXT,
  CHECK (CASE WHEN "binding" = 'rp'
      THEN "binding_repo" IS NOT NULL
      AND "binding_branch" IS NOT NULL
      AND "binding_path" IS NOT NULL
      AND "binding_directory" IS NULL
      END),
  CHECK (CASE WHEN "binding" = 'ld'
      THEN "binding_directory" IS NOT NULL
      AND "binding_repo" IS NULL
      AND "binding_branch" IS NULL
      AND "binding_path" IS NULL
      END),
  CHECK (CASE WHEN "binding" IS NOT NULL
      THEN 1
      ELSE "binding_repo" IS NULL
      AND "binding_branch" IS NULL
      AND "binding_path" IS NULL
      AND "binding_directory" IS NULL
      END)
) STRICT;
CREATE TABLE "manifest_root_baselines" (
  "manifest_root" INTEGER NOT NULL,
  "key" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "value" TEXT NOT NULL,
  PRIMARY KEY ("manifest_root", "key"),
  FOREIGN KEY ("manifest_root") REFERENCES "manifest_root" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE TABLE "manifest_root_refusals" (
  "manifest_root" INTEGER NOT NULL,
  "key" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "reason" TEXT NOT NULL,
  "since" INTEGER NOT NULL,
  PRIMARY KEY ("manifest_root", "key"),
  FOREIGN KEY ("manifest_root") REFERENCES "manifest_root" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE TABLE "manifest_root_suspended" (
  "manifest_root" INTEGER NOT NULL,
  "value" INTEGER NOT NULL REFERENCES "strings" ("id"),
  PRIMARY KEY ("manifest_root", "value"),
  FOREIGN KEY ("manifest_root") REFERENCES "manifest_root" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE TABLE "manifest_root_resolutions" (
  "manifest_root" INTEGER NOT NULL,
  "key" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "act" TEXT NOT NULL CHECK ("act" IN ('ad', 'sy')),
  "token" TEXT NOT NULL,
  "by" TEXT NOT NULL,
  "outcome" TEXT NOT NULL CHECK ("outcome" IN ('ad', 'fl', 'rq', 'st', 'sy')),
  "outcome_reason" TEXT,
  PRIMARY KEY ("manifest_root", "key"),
  FOREIGN KEY ("manifest_root") REFERENCES "manifest_root" ("id") ON DELETE CASCADE,
  CHECK (CASE WHEN "outcome" = 'rq'
      THEN "outcome_reason" IS NULL
      END),
  CHECK (CASE WHEN "outcome" = 'st'
      THEN "outcome_reason" IS NULL
      END),
  CHECK (CASE WHEN "outcome" = 'sy'
      THEN "outcome_reason" IS NULL
      END),
  CHECK (CASE WHEN "outcome" = 'ad'
      THEN "outcome_reason" IS NULL
      END),
  CHECK (CASE WHEN "outcome" = 'fl'
      THEN "outcome_reason" IS NOT NULL
      END)
) STRICT, WITHOUT ROWID;
CREATE INDEX "manifest_root_bound_by_host" ON "manifest_root" ("host") WHERE "state" IN ('bd');
INSERT INTO "codes" ("kind", "field", "code", "name") VALUES
  ('mr', 'state', 'bd', 'Bound'),
  ('mr', 'state', 'rt', 'Retired'),
  ('mr', 'binding', 'ld', 'LocalDirectory'),
  ('mr', 'binding', 'rp', 'Repository'),
  ('mr', 'resolutions.act', 'ad', 'Adopt'),
  ('mr', 'resolutions.act', 'sy', 'Sync'),
  ('mr', 'resolutions.outcome', 'ad', 'Adopted'),
  ('mr', 'resolutions.outcome', 'fl', 'Failed'),
  ('mr', 'resolutions.outcome', 'rq', 'Requested'),
  ('mr', 'resolutions.outcome', 'st', 'Started'),
  ('mr', 'resolutions.outcome', 'sy', 'Synced'),
  ('mr', 'log:edge', 'ap', 'Applied'),
  ('mr', 'log:edge', 'rb', 'Rebind'),
  ('mr', 'log:edge', 're', 'Retire'),
  ('mr', 'log:edge', 'rr', 'RequestResolution'),
  ('mr', 'log:edge', 'rs', 'Resolve'),
  ('mr', 'log:edge', 'sf', 'SourceFailed'),
  ('mr', 'log:edge', 'su', 'Suspend'),
  ('mr', 'log:edge', 'us', 'Unsuspend'),
  ('mr', 'log:label', 'bd', 'Bound'),
  ('mr', 'log:label', 'rt', 'Retired');
CREATE VIEW "manifest_root_view" AS SELECT
  "manifest_root"."id",
  "manifest_root"."version",
  "manifest_root"."host",
  "manifest_root"."path",
  "manifest_root"."source",
  (SELECT "name" FROM "codes" WHERE "kind" = 'mr' AND "field" = 'state' AND "code" = "manifest_root"."state") AS "state",
  "manifest_root"."state_since",
  (SELECT "name" FROM "codes" WHERE "kind" = 'mr' AND "field" = 'binding' AND "code" = "manifest_root"."binding") AS "binding",
  "manifest_root"."binding_repo",
  "manifest_root"."binding_branch",
  "manifest_root"."binding_path",
  "manifest_root"."binding_directory",
  "manifest_root"."applied",
  "manifest_root"."source_error"
FROM "manifest_root";
CREATE VIEW "manifest_root_baselines_view" AS SELECT
  "manifest_root_baselines"."manifest_root",
  (SELECT "s" FROM "strings" WHERE "id" = "manifest_root_baselines"."key") AS "key",
  "manifest_root_baselines"."value"
FROM "manifest_root_baselines";
CREATE VIEW "manifest_root_refusals_view" AS SELECT
  "manifest_root_refusals"."manifest_root",
  (SELECT "s" FROM "strings" WHERE "id" = "manifest_root_refusals"."key") AS "key",
  "manifest_root_refusals"."reason",
  "manifest_root_refusals"."since"
FROM "manifest_root_refusals";
CREATE VIEW "manifest_root_suspended_view" AS SELECT
  "manifest_root_suspended"."manifest_root",
  (SELECT "s" FROM "strings" WHERE "id" = "manifest_root_suspended"."value") AS "value"
FROM "manifest_root_suspended";
CREATE VIEW "manifest_root_resolutions_view" AS SELECT
  "manifest_root_resolutions"."manifest_root",
  (SELECT "s" FROM "strings" WHERE "id" = "manifest_root_resolutions"."key") AS "key",
  (SELECT "name" FROM "codes" WHERE "kind" = 'mr' AND "field" = 'resolutions.act' AND "code" = "manifest_root_resolutions"."act") AS "act",
  "manifest_root_resolutions"."token",
  "manifest_root_resolutions"."by",
  (SELECT "name" FROM "codes" WHERE "kind" = 'mr' AND "field" = 'resolutions.outcome' AND "code" = "manifest_root_resolutions"."outcome") AS "outcome",
  "manifest_root_resolutions"."outcome_reason"
FROM "manifest_root_resolutions";
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
