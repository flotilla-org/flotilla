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
CREATE TABLE "checkout" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "repository" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "environment" INTEGER NOT NULL REFERENCES "resource_identity" ("id"),
  "branch" TEXT NOT NULL,
  "base" TEXT,
  "path" TEXT NOT NULL,
  "source" TEXT NOT NULL CHECK ("source" IN ('fc', 'wt')),
  "source_url" TEXT,
  "clone" INTEGER REFERENCES "resource_identity" ("id"),
  "holder" INTEGER REFERENCES "strings" ("id"),
  "state" TEXT NOT NULL CHECK ("state" IN ('go', 'gu', 'rl', 'rq')),
  "state_since" INTEGER NOT NULL,
  "failures" INTEGER,
  "last_failure_at" INTEGER,
  "last_failure_cause_category" TEXT CHECK ("last_failure_cause_category" IN ('ex', 'mc', 'mx', 'rf', 'tr')),
  "last_failure_cause_detail" TEXT,
  "last_failure_retryable" INTEGER CHECK ("last_failure_retryable" IN (0, 1)),
  "identity" TEXT,
  "cause_category" TEXT CHECK ("cause_category" IN ('ex', 'mc', 'mx', 'rf', 'tr')),
  "cause_detail" TEXT,
  CHECK (CASE WHEN "source" = 'wt'
      THEN "source_url" IS NULL
      END),
  CHECK (CASE WHEN "source" = 'fc'
      THEN "source_url" IS NOT NULL
      END),
  CHECK (CASE WHEN "last_failure_at" IS NOT NULL
      THEN "last_failure_cause_category" IS NOT NULL
      AND "last_failure_cause_detail" IS NOT NULL
      AND "last_failure_retryable" IS NOT NULL
      ELSE "last_failure_cause_category" IS NULL
      AND "last_failure_cause_detail" IS NULL
      AND "last_failure_retryable" IS NULL
      END),
  CHECK (CASE WHEN "state" = 'rq'
      THEN "failures" IS NOT NULL
      AND "identity" IS NULL
      AND "cause_category" IS NULL
      AND "cause_detail" IS NULL
      END),
  CHECK (CASE WHEN "state" = 'rl'
      THEN "identity" IS NOT NULL
      AND "failures" IS NULL
      AND "last_failure_at" IS NULL
      AND "last_failure_cause_category" IS NULL
      AND "last_failure_cause_detail" IS NULL
      AND "last_failure_retryable" IS NULL
      AND "cause_category" IS NULL
      AND "cause_detail" IS NULL
      END),
  CHECK (CASE WHEN "state" = 'go'
      THEN "failures" IS NULL
      AND "last_failure_at" IS NULL
      AND "last_failure_cause_category" IS NULL
      AND "last_failure_cause_detail" IS NULL
      AND "last_failure_retryable" IS NULL
      AND "identity" IS NULL
      AND "cause_category" IS NULL
      AND "cause_detail" IS NULL
      END),
  CHECK (CASE WHEN "state" = 'gu'
      THEN "cause_category" IS NOT NULL
      AND "cause_detail" IS NOT NULL
      AND "failures" IS NULL
      AND "last_failure_at" IS NULL
      AND "last_failure_cause_category" IS NULL
      AND "last_failure_cause_detail" IS NULL
      AND "last_failure_retryable" IS NULL
      AND "identity" IS NULL
      END)
) STRICT;
CREATE INDEX "checkout_by_clone" ON "checkout" ("clone") WHERE "clone" IS NOT NULL;
CREATE INDEX "checkout_by_environment" ON "checkout" ("environment");
CREATE INDEX "checkout_by_holder" ON "checkout" ("holder") WHERE "holder" IS NOT NULL;
INSERT INTO "codes" ("kind", "field", "code", "name") VALUES
  ('ck', 'source', 'fc', 'FreshClone'),
  ('ck', 'source', 'wt', 'Worktree'),
  ('ck', 'state', 'go', 'Gone'),
  ('ck', 'state', 'gu', 'GaveUp'),
  ('ck', 'state', 'rl', 'Realised'),
  ('ck', 'state', 'rq', 'Requested'),
  ('ck', 'last_failure_cause_category', 'ex', 'External'),
  ('ck', 'last_failure_cause_category', 'mc', 'Misconfigured'),
  ('ck', 'last_failure_cause_category', 'mx', 'MissingCapability'),
  ('ck', 'last_failure_cause_category', 'rf', 'Refused'),
  ('ck', 'last_failure_cause_category', 'tr', 'Transient'),
  ('ck', 'cause_category', 'ex', 'External'),
  ('ck', 'cause_category', 'mc', 'Misconfigured'),
  ('ck', 'cause_category', 'mx', 'MissingCapability'),
  ('ck', 'cause_category', 'rf', 'Refused'),
  ('ck', 'cause_category', 'tr', 'Transient'),
  ('ck', 'log:edge', 'at', 'Attempted'),
  ('ck', 'log:edge', 'gu', 'GiveUp'),
  ('ck', 'log:edge', 'rl', 'Realise'),
  ('ck', 'log:edge', 'rs', 'Release'),
  ('ck', 'log:edge', 'va', 'Vanish'),
  ('ck', 'log:label', 'go', 'Gone'),
  ('ck', 'log:label', 'gu', 'GaveUp'),
  ('ck', 'log:label', 'rl', 'Realised'),
  ('ck', 'log:label', 'rq', 'Requested');
CREATE VIEW "checkout_view" AS SELECT
  "checkout"."id",
  "checkout"."version",
  (SELECT "s" FROM "strings" WHERE "id" = "checkout"."repository") AS "repository",
  "checkout"."environment",
  "checkout"."branch",
  "checkout"."base",
  "checkout"."path",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ck' AND "field" = 'source' AND "code" = "checkout"."source") AS "source",
  "checkout"."source_url",
  "checkout"."clone",
  (SELECT "s" FROM "strings" WHERE "id" = "checkout"."holder") AS "holder",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ck' AND "field" = 'state' AND "code" = "checkout"."state") AS "state",
  "checkout"."state_since",
  "checkout"."failures",
  "checkout"."last_failure_at",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ck' AND "field" = 'last_failure_cause_category' AND "code" = "checkout"."last_failure_cause_category") AS "last_failure_cause_category",
  "checkout"."last_failure_cause_detail",
  "checkout"."last_failure_retryable",
  "checkout"."identity",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ck' AND "field" = 'cause_category' AND "code" = "checkout"."cause_category") AS "cause_category",
  "checkout"."cause_detail"
FROM "checkout";
CREATE TABLE "clone" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "repository" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "url" TEXT NOT NULL,
  "environment" INTEGER NOT NULL REFERENCES "resource_identity" ("id"),
  "path" TEXT NOT NULL,
  "state" TEXT NOT NULL CHECK ("state" IN ('go', 'gu', 'rl', 'rq')),
  "state_since" INTEGER NOT NULL,
  "failures" INTEGER,
  "last_failure_at" INTEGER,
  "last_failure_cause_category" TEXT CHECK ("last_failure_cause_category" IN ('ex', 'mc', 'mx', 'rf', 'tr')),
  "last_failure_cause_detail" TEXT,
  "last_failure_retryable" INTEGER CHECK ("last_failure_retryable" IN (0, 1)),
  "identity" TEXT,
  "cause_category" TEXT CHECK ("cause_category" IN ('ex', 'mc', 'mx', 'rf', 'tr')),
  "cause_detail" TEXT,
  CHECK (CASE WHEN "last_failure_at" IS NOT NULL
      THEN "last_failure_cause_category" IS NOT NULL
      AND "last_failure_cause_detail" IS NOT NULL
      AND "last_failure_retryable" IS NOT NULL
      ELSE "last_failure_cause_category" IS NULL
      AND "last_failure_cause_detail" IS NULL
      AND "last_failure_retryable" IS NULL
      END),
  CHECK (CASE WHEN "state" = 'rq'
      THEN "failures" IS NOT NULL
      AND "identity" IS NULL
      AND "cause_category" IS NULL
      AND "cause_detail" IS NULL
      END),
  CHECK (CASE WHEN "state" = 'rl'
      THEN "identity" IS NOT NULL
      AND "failures" IS NULL
      AND "last_failure_at" IS NULL
      AND "last_failure_cause_category" IS NULL
      AND "last_failure_cause_detail" IS NULL
      AND "last_failure_retryable" IS NULL
      AND "cause_category" IS NULL
      AND "cause_detail" IS NULL
      END),
  CHECK (CASE WHEN "state" = 'go'
      THEN "failures" IS NULL
      AND "last_failure_at" IS NULL
      AND "last_failure_cause_category" IS NULL
      AND "last_failure_cause_detail" IS NULL
      AND "last_failure_retryable" IS NULL
      AND "identity" IS NULL
      AND "cause_category" IS NULL
      AND "cause_detail" IS NULL
      END),
  CHECK (CASE WHEN "state" = 'gu'
      THEN "cause_category" IS NOT NULL
      AND "cause_detail" IS NOT NULL
      AND "failures" IS NULL
      AND "last_failure_at" IS NULL
      AND "last_failure_cause_category" IS NULL
      AND "last_failure_cause_detail" IS NULL
      AND "last_failure_retryable" IS NULL
      AND "identity" IS NULL
      END)
) STRICT;
CREATE INDEX "clone_by_environment" ON "clone" ("environment", "repository");
INSERT INTO "codes" ("kind", "field", "code", "name") VALUES
  ('cl', 'state', 'go', 'Gone'),
  ('cl', 'state', 'gu', 'GaveUp'),
  ('cl', 'state', 'rl', 'Realised'),
  ('cl', 'state', 'rq', 'Requested'),
  ('cl', 'last_failure_cause_category', 'ex', 'External'),
  ('cl', 'last_failure_cause_category', 'mc', 'Misconfigured'),
  ('cl', 'last_failure_cause_category', 'mx', 'MissingCapability'),
  ('cl', 'last_failure_cause_category', 'rf', 'Refused'),
  ('cl', 'last_failure_cause_category', 'tr', 'Transient'),
  ('cl', 'cause_category', 'ex', 'External'),
  ('cl', 'cause_category', 'mc', 'Misconfigured'),
  ('cl', 'cause_category', 'mx', 'MissingCapability'),
  ('cl', 'cause_category', 'rf', 'Refused'),
  ('cl', 'cause_category', 'tr', 'Transient'),
  ('cl', 'log:edge', 'at', 'Attempted'),
  ('cl', 'log:edge', 'gu', 'GiveUp'),
  ('cl', 'log:edge', 'rl', 'Realise'),
  ('cl', 'log:edge', 'rs', 'Release'),
  ('cl', 'log:edge', 'va', 'Vanish'),
  ('cl', 'log:label', 'go', 'Gone'),
  ('cl', 'log:label', 'gu', 'GaveUp'),
  ('cl', 'log:label', 'rl', 'Realised'),
  ('cl', 'log:label', 'rq', 'Requested');
CREATE VIEW "clone_view" AS SELECT
  "clone"."id",
  "clone"."version",
  (SELECT "s" FROM "strings" WHERE "id" = "clone"."repository") AS "repository",
  "clone"."url",
  "clone"."environment",
  "clone"."path",
  (SELECT "name" FROM "codes" WHERE "kind" = 'cl' AND "field" = 'state' AND "code" = "clone"."state") AS "state",
  "clone"."state_since",
  "clone"."failures",
  "clone"."last_failure_at",
  (SELECT "name" FROM "codes" WHERE "kind" = 'cl' AND "field" = 'last_failure_cause_category' AND "code" = "clone"."last_failure_cause_category") AS "last_failure_cause_category",
  "clone"."last_failure_cause_detail",
  "clone"."last_failure_retryable",
  "clone"."identity",
  (SELECT "name" FROM "codes" WHERE "kind" = 'cl' AND "field" = 'cause_category' AND "code" = "clone"."cause_category") AS "cause_category",
  "clone"."cause_detail"
FROM "clone";
CREATE TABLE "environment" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "provider" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "image_build" INTEGER REFERENCES "resource_identity" ("id"),
  "recipe" BLOB NOT NULL REFERENCES "objects" ("hash"),
  "holder" INTEGER REFERENCES "strings" ("id"),
  "state" TEXT NOT NULL CHECK ("state" IN ('go', 'gu', 'rl', 'rq')),
  "state_since" INTEGER NOT NULL,
  "failures" INTEGER,
  "last_failure_at" INTEGER,
  "last_failure_cause_category" TEXT CHECK ("last_failure_cause_category" IN ('ex', 'mc', 'mx', 'rf', 'tr')),
  "last_failure_cause_detail" TEXT,
  "last_failure_retryable" INTEGER CHECK ("last_failure_retryable" IN (0, 1)),
  "identity" TEXT,
  "cause_category" TEXT CHECK ("cause_category" IN ('ex', 'mc', 'mx', 'rf', 'tr')),
  "cause_detail" TEXT,
  CHECK (CASE WHEN "last_failure_at" IS NOT NULL
      THEN "last_failure_cause_category" IS NOT NULL
      AND "last_failure_cause_detail" IS NOT NULL
      AND "last_failure_retryable" IS NOT NULL
      ELSE "last_failure_cause_category" IS NULL
      AND "last_failure_cause_detail" IS NULL
      AND "last_failure_retryable" IS NULL
      END),
  CHECK (CASE WHEN "state" = 'rq'
      THEN "failures" IS NOT NULL
      AND "identity" IS NULL
      AND "cause_category" IS NULL
      AND "cause_detail" IS NULL
      END),
  CHECK (CASE WHEN "state" = 'rl'
      THEN "identity" IS NOT NULL
      AND "failures" IS NULL
      AND "last_failure_at" IS NULL
      AND "last_failure_cause_category" IS NULL
      AND "last_failure_cause_detail" IS NULL
      AND "last_failure_retryable" IS NULL
      AND "cause_category" IS NULL
      AND "cause_detail" IS NULL
      END),
  CHECK (CASE WHEN "state" = 'go'
      THEN "failures" IS NULL
      AND "last_failure_at" IS NULL
      AND "last_failure_cause_category" IS NULL
      AND "last_failure_cause_detail" IS NULL
      AND "last_failure_retryable" IS NULL
      AND "identity" IS NULL
      AND "cause_category" IS NULL
      AND "cause_detail" IS NULL
      END),
  CHECK (CASE WHEN "state" = 'gu'
      THEN "cause_category" IS NOT NULL
      AND "cause_detail" IS NOT NULL
      AND "failures" IS NULL
      AND "last_failure_at" IS NULL
      AND "last_failure_cause_category" IS NULL
      AND "last_failure_cause_detail" IS NULL
      AND "last_failure_retryable" IS NULL
      AND "identity" IS NULL
      END)
) STRICT;
CREATE INDEX "environment_by_holder" ON "environment" ("holder") WHERE "holder" IS NOT NULL;
CREATE INDEX "environment_by_image_build" ON "environment" ("image_build") WHERE "image_build" IS NOT NULL;
CREATE INDEX "environment_live_by_provider" ON "environment" ("provider") WHERE "state" IN ('rq', 'rl');
INSERT INTO "codes" ("kind", "field", "code", "name") VALUES
  ('ev', 'state', 'go', 'Gone'),
  ('ev', 'state', 'gu', 'GaveUp'),
  ('ev', 'state', 'rl', 'Realised'),
  ('ev', 'state', 'rq', 'Requested'),
  ('ev', 'last_failure_cause_category', 'ex', 'External'),
  ('ev', 'last_failure_cause_category', 'mc', 'Misconfigured'),
  ('ev', 'last_failure_cause_category', 'mx', 'MissingCapability'),
  ('ev', 'last_failure_cause_category', 'rf', 'Refused'),
  ('ev', 'last_failure_cause_category', 'tr', 'Transient'),
  ('ev', 'cause_category', 'ex', 'External'),
  ('ev', 'cause_category', 'mc', 'Misconfigured'),
  ('ev', 'cause_category', 'mx', 'MissingCapability'),
  ('ev', 'cause_category', 'rf', 'Refused'),
  ('ev', 'cause_category', 'tr', 'Transient'),
  ('ev', 'log:edge', 'at', 'Attempted'),
  ('ev', 'log:edge', 'gu', 'GiveUp'),
  ('ev', 'log:edge', 'rl', 'Realise'),
  ('ev', 'log:edge', 'rs', 'Release'),
  ('ev', 'log:edge', 'va', 'Vanish'),
  ('ev', 'log:label', 'go', 'Gone'),
  ('ev', 'log:label', 'gu', 'GaveUp'),
  ('ev', 'log:label', 'rl', 'Realised'),
  ('ev', 'log:label', 'rq', 'Requested');
CREATE VIEW "environment_view" AS SELECT
  "environment"."id",
  "environment"."version",
  (SELECT "s" FROM "strings" WHERE "id" = "environment"."provider") AS "provider",
  "environment"."image_build",
  "environment"."recipe",
  (SELECT "s" FROM "strings" WHERE "id" = "environment"."holder") AS "holder",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ev' AND "field" = 'state' AND "code" = "environment"."state") AS "state",
  "environment"."state_since",
  "environment"."failures",
  "environment"."last_failure_at",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ev' AND "field" = 'last_failure_cause_category' AND "code" = "environment"."last_failure_cause_category") AS "last_failure_cause_category",
  "environment"."last_failure_cause_detail",
  "environment"."last_failure_retryable",
  "environment"."identity",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ev' AND "field" = 'cause_category' AND "code" = "environment"."cause_category") AS "cause_category",
  "environment"."cause_detail"
FROM "environment";
CREATE TABLE "image_build" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "composition" TEXT NOT NULL,
  "architecture" TEXT NOT NULL CHECK ("architecture" IN ('a6', 'r6')),
  "layers" BLOB NOT NULL REFERENCES "objects" ("hash"),
  "delivery" TEXT NOT NULL CHECK ("delivery" IN ('lo', 'rg')),
  "delivery_repository" TEXT,
  "reservation_cpu" INTEGER NOT NULL,
  "reservation_disk_bytes" INTEGER NOT NULL,
  "state" TEXT NOT NULL CHECK ("state" IN ('go', 'gu', 'rl', 'rq')),
  "state_since" INTEGER NOT NULL,
  "failures" INTEGER,
  "last_failure_at" INTEGER,
  "last_failure_cause_category" TEXT CHECK ("last_failure_cause_category" IN ('ex', 'mc', 'mx', 'rf', 'tr')),
  "last_failure_cause_detail" TEXT,
  "last_failure_retryable" INTEGER CHECK ("last_failure_retryable" IN (0, 1)),
  "identity" TEXT,
  "cause_category" TEXT CHECK ("cause_category" IN ('ex', 'mc', 'mx', 'rf', 'tr')),
  "cause_detail" TEXT,
  CHECK (CASE WHEN "delivery" = 'rg'
      THEN "delivery_repository" IS NOT NULL
      END),
  CHECK (CASE WHEN "delivery" = 'lo'
      THEN "delivery_repository" IS NULL
      END),
  CHECK (CASE WHEN "last_failure_at" IS NOT NULL
      THEN "last_failure_cause_category" IS NOT NULL
      AND "last_failure_cause_detail" IS NOT NULL
      AND "last_failure_retryable" IS NOT NULL
      ELSE "last_failure_cause_category" IS NULL
      AND "last_failure_cause_detail" IS NULL
      AND "last_failure_retryable" IS NULL
      END),
  CHECK (CASE WHEN "state" = 'rq'
      THEN "failures" IS NOT NULL
      AND "identity" IS NULL
      AND "cause_category" IS NULL
      AND "cause_detail" IS NULL
      END),
  CHECK (CASE WHEN "state" = 'rl'
      THEN "identity" IS NOT NULL
      AND "failures" IS NULL
      AND "last_failure_at" IS NULL
      AND "last_failure_cause_category" IS NULL
      AND "last_failure_cause_detail" IS NULL
      AND "last_failure_retryable" IS NULL
      AND "cause_category" IS NULL
      AND "cause_detail" IS NULL
      END),
  CHECK (CASE WHEN "state" = 'go'
      THEN "failures" IS NULL
      AND "last_failure_at" IS NULL
      AND "last_failure_cause_category" IS NULL
      AND "last_failure_cause_detail" IS NULL
      AND "last_failure_retryable" IS NULL
      AND "identity" IS NULL
      AND "cause_category" IS NULL
      AND "cause_detail" IS NULL
      END),
  CHECK (CASE WHEN "state" = 'gu'
      THEN "cause_category" IS NOT NULL
      AND "cause_detail" IS NOT NULL
      AND "failures" IS NULL
      AND "last_failure_at" IS NULL
      AND "last_failure_cause_category" IS NULL
      AND "last_failure_cause_detail" IS NULL
      AND "last_failure_retryable" IS NULL
      AND "identity" IS NULL
      END)
) STRICT;
CREATE INDEX "image_build_by_composition" ON "image_build" ("composition");
INSERT INTO "codes" ("kind", "field", "code", "name") VALUES
  ('ib', 'architecture', 'a6', 'Amd64'),
  ('ib', 'architecture', 'r6', 'Arm64'),
  ('ib', 'delivery', 'lo', 'Local'),
  ('ib', 'delivery', 'rg', 'Registry'),
  ('ib', 'state', 'go', 'Gone'),
  ('ib', 'state', 'gu', 'GaveUp'),
  ('ib', 'state', 'rl', 'Realised'),
  ('ib', 'state', 'rq', 'Requested'),
  ('ib', 'last_failure_cause_category', 'ex', 'External'),
  ('ib', 'last_failure_cause_category', 'mc', 'Misconfigured'),
  ('ib', 'last_failure_cause_category', 'mx', 'MissingCapability'),
  ('ib', 'last_failure_cause_category', 'rf', 'Refused'),
  ('ib', 'last_failure_cause_category', 'tr', 'Transient'),
  ('ib', 'cause_category', 'ex', 'External'),
  ('ib', 'cause_category', 'mc', 'Misconfigured'),
  ('ib', 'cause_category', 'mx', 'MissingCapability'),
  ('ib', 'cause_category', 'rf', 'Refused'),
  ('ib', 'cause_category', 'tr', 'Transient'),
  ('ib', 'log:edge', 'at', 'Attempted'),
  ('ib', 'log:edge', 'gu', 'GiveUp'),
  ('ib', 'log:edge', 'rl', 'Realise'),
  ('ib', 'log:edge', 'rs', 'Release'),
  ('ib', 'log:edge', 'va', 'Vanish'),
  ('ib', 'log:label', 'go', 'Gone'),
  ('ib', 'log:label', 'gu', 'GaveUp'),
  ('ib', 'log:label', 'rl', 'Realised'),
  ('ib', 'log:label', 'rq', 'Requested');
CREATE VIEW "image_build_view" AS SELECT
  "image_build"."id",
  "image_build"."version",
  "image_build"."composition",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ib' AND "field" = 'architecture' AND "code" = "image_build"."architecture") AS "architecture",
  "image_build"."layers",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ib' AND "field" = 'delivery' AND "code" = "image_build"."delivery") AS "delivery",
  "image_build"."delivery_repository",
  "image_build"."reservation_cpu",
  "image_build"."reservation_disk_bytes",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ib' AND "field" = 'state' AND "code" = "image_build"."state") AS "state",
  "image_build"."state_since",
  "image_build"."failures",
  "image_build"."last_failure_at",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ib' AND "field" = 'last_failure_cause_category' AND "code" = "image_build"."last_failure_cause_category") AS "last_failure_cause_category",
  "image_build"."last_failure_cause_detail",
  "image_build"."last_failure_retryable",
  "image_build"."identity",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ib' AND "field" = 'cause_category' AND "code" = "image_build"."cause_category") AS "cause_category",
  "image_build"."cause_detail"
FROM "image_build";
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
