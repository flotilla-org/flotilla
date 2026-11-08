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
CREATE TABLE snapshot_cause (
  resource INTEGER PRIMARY KEY REFERENCES resource_identity(id) ON DELETE CASCADE,
  home INTEGER NOT NULL REFERENCES strings(id),
  change_seq INTEGER NOT NULL,
  FOREIGN KEY (home, change_seq) REFERENCES change(home, seq)) STRICT, WITHOUT ROWID;
CREATE TABLE codes (
  kind TEXT NOT NULL, field TEXT NOT NULL, code TEXT NOT NULL, name TEXT NOT NULL,
  PRIMARY KEY (kind, field, code)) STRICT, WITHOUT ROWID;
CREATE TABLE "checkout" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "launch_of" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "launch" INTEGER NOT NULL,
  "repository" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "environment" INTEGER NOT NULL REFERENCES "resource_identity" ("id"),
  "branch" TEXT NOT NULL,
  "base" TEXT,
  "path" TEXT NOT NULL,
  "source" TEXT NOT NULL CHECK ("source" IN ('ad', 'fc', 'wt')),
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
  "ending" TEXT CHECK ("ending" IN ('fa', 'lo', 'rl')),
  "ending_reason" TEXT,
  "cause_category" TEXT CHECK ("cause_category" IN ('ex', 'mc', 'mx', 'rf', 'tr')),
  "cause_detail" TEXT,
  CHECK (CASE WHEN "source" = 'wt'
      THEN "source_url" IS NULL
      END),
  CHECK (CASE WHEN "source" = 'fc'
      THEN "source_url" IS NOT NULL
      END),
  CHECK (CASE WHEN "source" = 'ad'
      THEN "source_url" IS NULL
      END),
  CHECK (CASE WHEN "last_failure_at" IS NOT NULL
      THEN "last_failure_cause_category" IS NOT NULL
      AND "last_failure_cause_detail" IS NOT NULL
      AND "last_failure_retryable" IS NOT NULL
      ELSE "last_failure_cause_category" IS NULL
      AND "last_failure_cause_detail" IS NULL
      AND "last_failure_retryable" IS NULL
      END),
  CHECK (CASE WHEN "ending" = 'rl'
      THEN "ending_reason" IS NULL
      END),
  CHECK (CASE WHEN "ending" = 'lo'
      THEN "ending_reason" IS NULL
      END),
  CHECK (CASE WHEN "ending" = 'fa'
      THEN "ending_reason" IS NOT NULL
      END),
  CHECK (CASE WHEN "state" = 'rq'
      THEN "failures" IS NOT NULL
      AND "identity" IS NULL
      AND "ending" IS NULL
      AND "ending_reason" IS NULL
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
      AND "ending" IS NULL
      AND "ending_reason" IS NULL
      AND "cause_category" IS NULL
      AND "cause_detail" IS NULL
      END),
  CHECK (CASE WHEN "state" = 'go'
      THEN "ending" IS NOT NULL
      AND "failures" IS NULL
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
      AND "ending" IS NULL
      AND "ending_reason" IS NULL
      END)
) STRICT;
CREATE INDEX "checkout_by_clone" ON "checkout" ("clone") WHERE "clone" IS NOT NULL;
CREATE INDEX "checkout_by_environment" ON "checkout" ("environment");
CREATE INDEX "checkout_by_holder" ON "checkout" ("holder") WHERE "holder" IS NOT NULL;
CREATE INDEX "checkout_by_launch" ON "checkout" ("launch_of", "launch");
CREATE INDEX "checkout_by_repository" ON "checkout" ("repository");
INSERT INTO "codes" ("kind", "field", "code", "name") VALUES
  ('ck', 'source', 'ad', 'Adopted'),
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
  ('ck', 'ending', 'fa', 'Failed'),
  ('ck', 'ending', 'lo', 'Lost'),
  ('ck', 'ending', 'rl', 'Released'),
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
  (SELECT "s" FROM "strings" WHERE "id" = "checkout"."launch_of") AS "launch_of",
  "checkout"."launch",
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
  (SELECT "name" FROM "codes" WHERE "kind" = 'ck' AND "field" = 'ending' AND "code" = "checkout"."ending") AS "ending",
  "checkout"."ending_reason",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ck' AND "field" = 'cause_category' AND "code" = "checkout"."cause_category") AS "cause_category",
  "checkout"."cause_detail"
FROM "checkout";
CREATE TABLE "clone" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "launch_of" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "launch" INTEGER NOT NULL,
  "repository" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "url" TEXT NOT NULL,
  "environment" INTEGER NOT NULL REFERENCES "resource_identity" ("id"),
  "path" TEXT NOT NULL,
  "discovered" INTEGER NOT NULL CHECK ("discovered" IN (0, 1)) DEFAULT 0,
  "state" TEXT NOT NULL CHECK ("state" IN ('go', 'gu', 'rl', 'rq')),
  "state_since" INTEGER NOT NULL,
  "failures" INTEGER,
  "last_failure_at" INTEGER,
  "last_failure_cause_category" TEXT CHECK ("last_failure_cause_category" IN ('ex', 'mc', 'mx', 'rf', 'tr')),
  "last_failure_cause_detail" TEXT,
  "last_failure_retryable" INTEGER CHECK ("last_failure_retryable" IN (0, 1)),
  "identity" TEXT,
  "ending" TEXT CHECK ("ending" IN ('fa', 'lo', 'rl')),
  "ending_reason" TEXT,
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
  CHECK (CASE WHEN "ending" = 'rl'
      THEN "ending_reason" IS NULL
      END),
  CHECK (CASE WHEN "ending" = 'lo'
      THEN "ending_reason" IS NULL
      END),
  CHECK (CASE WHEN "ending" = 'fa'
      THEN "ending_reason" IS NOT NULL
      END),
  CHECK (CASE WHEN "state" = 'rq'
      THEN "failures" IS NOT NULL
      AND "identity" IS NULL
      AND "ending" IS NULL
      AND "ending_reason" IS NULL
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
      AND "ending" IS NULL
      AND "ending_reason" IS NULL
      AND "cause_category" IS NULL
      AND "cause_detail" IS NULL
      END),
  CHECK (CASE WHEN "state" = 'go'
      THEN "ending" IS NOT NULL
      AND "failures" IS NULL
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
      AND "ending" IS NULL
      AND "ending_reason" IS NULL
      END)
) STRICT;
CREATE INDEX "clone_by_environment" ON "clone" ("environment", "repository");
CREATE INDEX "clone_by_launch" ON "clone" ("launch_of", "launch");
CREATE INDEX "clone_by_repository" ON "clone" ("repository");
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
  ('cl', 'ending', 'fa', 'Failed'),
  ('cl', 'ending', 'lo', 'Lost'),
  ('cl', 'ending', 'rl', 'Released'),
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
  (SELECT "s" FROM "strings" WHERE "id" = "clone"."launch_of") AS "launch_of",
  "clone"."launch",
  (SELECT "s" FROM "strings" WHERE "id" = "clone"."repository") AS "repository",
  "clone"."url",
  "clone"."environment",
  "clone"."path",
  "clone"."discovered",
  (SELECT "name" FROM "codes" WHERE "kind" = 'cl' AND "field" = 'state' AND "code" = "clone"."state") AS "state",
  "clone"."state_since",
  "clone"."failures",
  "clone"."last_failure_at",
  (SELECT "name" FROM "codes" WHERE "kind" = 'cl' AND "field" = 'last_failure_cause_category' AND "code" = "clone"."last_failure_cause_category") AS "last_failure_cause_category",
  "clone"."last_failure_cause_detail",
  "clone"."last_failure_retryable",
  "clone"."identity",
  (SELECT "name" FROM "codes" WHERE "kind" = 'cl' AND "field" = 'ending' AND "code" = "clone"."ending") AS "ending",
  "clone"."ending_reason",
  (SELECT "name" FROM "codes" WHERE "kind" = 'cl' AND "field" = 'cause_category' AND "code" = "clone"."cause_category") AS "cause_category",
  "clone"."cause_detail"
FROM "clone";
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
