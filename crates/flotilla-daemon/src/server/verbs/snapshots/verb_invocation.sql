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
CREATE TABLE "verb_invocation" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "caller" TEXT NOT NULL,
  "invocation" TEXT NOT NULL,
  "verb" TEXT NOT NULL,
  "params" BLOB NOT NULL,
  "at" INTEGER NOT NULL
) STRICT;
CREATE TABLE "verb_invocation_subjects" (
  "verb_invocation" INTEGER NOT NULL,
  "pos" INTEGER NOT NULL,
  "value" TEXT NOT NULL,
  PRIMARY KEY ("verb_invocation", "pos"),
  FOREIGN KEY ("verb_invocation") REFERENCES "verb_invocation" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE TABLE "verb_invocation_outcomes" (
  "verb_invocation" INTEGER NOT NULL,
  "pos" INTEGER NOT NULL,
  "subject" TEXT NOT NULL,
  "status" TEXT NOT NULL CHECK ("status" IN ('dn', 'fl', 'na', 'rf')),
  "detail" TEXT,
  PRIMARY KEY ("verb_invocation", "pos"),
  FOREIGN KEY ("verb_invocation") REFERENCES "verb_invocation" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
INSERT INTO "codes" ("kind", "field", "code", "name") VALUES
  ('vi', 'outcomes.status', 'dn', 'Done'),
  ('vi', 'outcomes.status', 'fl', 'Failed'),
  ('vi', 'outcomes.status', 'na', 'NotApplicable'),
  ('vi', 'outcomes.status', 'rf', 'Refused');
CREATE VIEW "verb_invocation_view" AS SELECT
  "verb_invocation"."id",
  "verb_invocation"."version",
  "verb_invocation"."caller",
  "verb_invocation"."invocation",
  "verb_invocation"."verb",
  "verb_invocation"."params",
  "verb_invocation"."at"
FROM "verb_invocation";
CREATE VIEW "verb_invocation_subjects_view" AS SELECT
  "verb_invocation_subjects"."verb_invocation",
  "verb_invocation_subjects"."pos",
  "verb_invocation_subjects"."value"
FROM "verb_invocation_subjects";
CREATE VIEW "verb_invocation_outcomes_view" AS SELECT
  "verb_invocation_outcomes"."verb_invocation",
  "verb_invocation_outcomes"."pos",
  "verb_invocation_outcomes"."subject",
  (SELECT "name" FROM "codes" WHERE "kind" = 'vi' AND "field" = 'outcomes.status' AND "code" = "verb_invocation_outcomes"."status") AS "status",
  "verb_invocation_outcomes"."detail"
FROM "verb_invocation_outcomes";
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
