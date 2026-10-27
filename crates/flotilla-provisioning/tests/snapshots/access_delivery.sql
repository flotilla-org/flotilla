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
CREATE TABLE "access_delivery" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "access" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "place" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "harness" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "home" TEXT NOT NULL,
  "retry_need" TEXT,
  "retry_attempts" INTEGER,
  "retry_error" TEXT,
  "retry_next_at" INTEGER,
  CHECK (CASE WHEN "retry_need" IS NOT NULL
      THEN "retry_attempts" IS NOT NULL
      AND "retry_error" IS NOT NULL
      AND "retry_next_at" IS NOT NULL
      ELSE "retry_attempts" IS NULL
      AND "retry_error" IS NULL
      AND "retry_next_at" IS NULL
      END)
) STRICT;
CREATE TABLE "access_delivery_needs" (
  "access_delivery" INTEGER NOT NULL,
  "key" TEXT NOT NULL,
  "mechanism" TEXT NOT NULL CHECK ("mechanism" IN ('fu', 'hrl', 'hrs', 'hwl', 'hws', 'pu')),
  "digest" TEXT NOT NULL,
  "at" INTEGER NOT NULL,
  PRIMARY KEY ("access_delivery", "key"),
  FOREIGN KEY ("access_delivery") REFERENCES "access_delivery" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE INDEX "access_delivery_by_access" ON "access_delivery" ("access");
CREATE INDEX "access_delivery_by_place" ON "access_delivery" ("place");
INSERT INTO "codes" ("kind", "field", "code", "name") VALUES
  ('ad', 'needs.mechanism', 'fu', 'AgentFetchUrl'),
  ('ad', 'needs.mechanism', 'hrl', 'HostPathReadOnlyLive'),
  ('ad', 'needs.mechanism', 'hrs', 'HostPathReadOnlySnapshot'),
  ('ad', 'needs.mechanism', 'hwl', 'HostPathReadWriteLive'),
  ('ad', 'needs.mechanism', 'hws', 'HostPathReadWriteSnapshot'),
  ('ad', 'needs.mechanism', 'pu', 'AgentPush');
CREATE VIEW "access_delivery_view" AS SELECT
  "access_delivery"."id",
  "access_delivery"."version",
  (SELECT "s" FROM "strings" WHERE "id" = "access_delivery"."access") AS "access",
  (SELECT "s" FROM "strings" WHERE "id" = "access_delivery"."place") AS "place",
  (SELECT "s" FROM "strings" WHERE "id" = "access_delivery"."harness") AS "harness",
  "access_delivery"."home",
  "access_delivery"."retry_need",
  "access_delivery"."retry_attempts",
  "access_delivery"."retry_error",
  "access_delivery"."retry_next_at"
FROM "access_delivery";
CREATE VIEW "access_delivery_needs_view" AS SELECT
  "access_delivery_needs"."access_delivery",
  "access_delivery_needs"."key",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ad' AND "field" = 'needs.mechanism' AND "code" = "access_delivery_needs"."mechanism") AS "mechanism",
  "access_delivery_needs"."digest",
  "access_delivery_needs"."at"
FROM "access_delivery_needs";
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
