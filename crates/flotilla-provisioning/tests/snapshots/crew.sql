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
CREATE TABLE "ensure" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "inbox" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "policy_max_failures" INTEGER NOT NULL,
  "policy_base" INTEGER NOT NULL,
  "policy_factor" INTEGER NOT NULL,
  "policy_cap" INTEGER NOT NULL,
  "state" TEXT NOT NULL CHECK ("state" IN ('fu', 'he', 'rt', 'un')),
  "state_since" INTEGER NOT NULL,
  "failures" INTEGER,
  "last_failure_at" INTEGER,
  "last_failure_cause_category" TEXT CHECK ("last_failure_cause_category" IN ('ex', 'mc', 'mx', 'rf', 'tr')),
  "last_failure_cause_detail" TEXT,
  "last_failure_retryable" INTEGER CHECK ("last_failure_retryable" IN (0, 1)),
  "bound" INTEGER REFERENCES "strings" ("id"),
  CHECK (CASE WHEN "last_failure_at" IS NOT NULL
      THEN "last_failure_cause_category" IS NOT NULL
      AND "last_failure_cause_detail" IS NOT NULL
      AND "last_failure_retryable" IS NOT NULL
      ELSE "last_failure_cause_category" IS NULL
      AND "last_failure_cause_detail" IS NULL
      AND "last_failure_retryable" IS NULL
      END),
  CHECK (CASE WHEN "state" = 'un'
      THEN "failures" IS NOT NULL
      AND "bound" IS NULL
      END),
  CHECK (CASE WHEN "state" = 'fu'
      THEN "failures" IS NOT NULL
      AND "bound" IS NOT NULL
      END),
  CHECK (CASE WHEN "state" = 'he'
      THEN "failures" IS NOT NULL
      AND "bound" IS NULL
      END),
  CHECK (CASE WHEN "state" = 'rt'
      THEN "failures" IS NOT NULL
      AND "bound" IS NULL
      END)
) STRICT;
CREATE TABLE "ensure_recorded" (
  "ensure" INTEGER NOT NULL,
  "value" INTEGER NOT NULL REFERENCES "strings" ("id"),
  PRIMARY KEY ("ensure", "value"),
  FOREIGN KEY ("ensure") REFERENCES "ensure" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE TABLE "ensure_created" (
  "ensure" INTEGER NOT NULL,
  "value" INTEGER NOT NULL REFERENCES "strings" ("id"),
  PRIMARY KEY ("ensure", "value"),
  FOREIGN KEY ("ensure") REFERENCES "ensure" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE INDEX "ensure_by_inbox" ON "ensure" ("inbox");
INSERT INTO "codes" ("kind", "field", "code", "name") VALUES
  ('en', 'state', 'fu', 'Fulfilled'),
  ('en', 'state', 'he', 'Held'),
  ('en', 'state', 'rt', 'Retired'),
  ('en', 'state', 'un', 'Unfulfilled'),
  ('en', 'last_failure_cause_category', 'ex', 'External'),
  ('en', 'last_failure_cause_category', 'mc', 'Misconfigured'),
  ('en', 'last_failure_cause_category', 'mx', 'MissingCapability'),
  ('en', 'last_failure_cause_category', 'rf', 'Refused'),
  ('en', 'last_failure_cause_category', 'tr', 'Transient'),
  ('en', 'log:edge', 'at', 'Attempted'),
  ('en', 'log:edge', 'bi', 'Bind'),
  ('en', 'log:edge', 'cr', 'Create'),
  ('en', 'log:edge', 'ho', 'Hold'),
  ('en', 'log:edge', 'rc', 'Record'),
  ('en', 'log:edge', 'rs', 'Resume'),
  ('en', 'log:edge', 'rt', 'Retire'),
  ('en', 'log:edge', 'ub', 'Unbind'),
  ('en', 'log:label', 'fu', 'Fulfilled'),
  ('en', 'log:label', 'he', 'Held'),
  ('en', 'log:label', 'rt', 'Retired'),
  ('en', 'log:label', 'un', 'Unfulfilled');
CREATE VIEW "ensure_view" AS SELECT
  "ensure"."id",
  "ensure"."version",
  (SELECT "s" FROM "strings" WHERE "id" = "ensure"."inbox") AS "inbox",
  "ensure"."policy_max_failures",
  "ensure"."policy_base",
  "ensure"."policy_factor",
  "ensure"."policy_cap",
  (SELECT "name" FROM "codes" WHERE "kind" = 'en' AND "field" = 'state' AND "code" = "ensure"."state") AS "state",
  "ensure"."state_since",
  "ensure"."failures",
  "ensure"."last_failure_at",
  (SELECT "name" FROM "codes" WHERE "kind" = 'en' AND "field" = 'last_failure_cause_category' AND "code" = "ensure"."last_failure_cause_category") AS "last_failure_cause_category",
  "ensure"."last_failure_cause_detail",
  "ensure"."last_failure_retryable",
  (SELECT "s" FROM "strings" WHERE "id" = "ensure"."bound") AS "bound"
FROM "ensure";
CREATE VIEW "ensure_recorded_view" AS SELECT
  "ensure_recorded"."ensure",
  (SELECT "s" FROM "strings" WHERE "id" = "ensure_recorded"."value") AS "value"
FROM "ensure_recorded";
CREATE VIEW "ensure_created_view" AS SELECT
  "ensure_created"."ensure",
  (SELECT "s" FROM "strings" WHERE "id" = "ensure_created"."value") AS "value"
FROM "ensure_created";
CREATE TABLE "terminal_session" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "environment" INTEGER NOT NULL REFERENCES "resource_identity" ("id"),
  "vessel" INTEGER REFERENCES "resource_identity" ("id"),
  "convoy" INTEGER REFERENCES "resource_identity" ("id"),
  "role" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "pool" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "cwd" TEXT NOT NULL,
  "source" TEXT NOT NULL CHECK ("source" IN ('ag', 'tl')),
  "source_command" TEXT,
  "source_capability" INTEGER REFERENCES "strings" ("id"),
  "source_adapter" TEXT,
  "source_model" TEXT,
  "source_brief" BLOB REFERENCES "objects" ("hash"),
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
  CHECK (CASE WHEN "source" = 'tl'
      THEN "source_command" IS NOT NULL
      AND "source_capability" IS NULL
      AND "source_adapter" IS NULL
      AND "source_model" IS NULL
      AND "source_brief" IS NULL
      END),
  CHECK (CASE WHEN "source" = 'ag'
      THEN "source_capability" IS NOT NULL
      AND "source_brief" IS NOT NULL
      AND "source_command" IS NULL
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
CREATE TABLE "terminal_session_env" (
  "terminal_session" INTEGER NOT NULL,
  "key" TEXT NOT NULL,
  "value" TEXT NOT NULL,
  PRIMARY KEY ("terminal_session", "key"),
  FOREIGN KEY ("terminal_session") REFERENCES "terminal_session" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE INDEX "terminal_session_by_convoy" ON "terminal_session" ("convoy") WHERE "convoy" IS NOT NULL;
CREATE INDEX "terminal_session_by_environment" ON "terminal_session" ("environment");
CREATE INDEX "terminal_session_by_vessel" ON "terminal_session" ("vessel") WHERE "vessel" IS NOT NULL;
INSERT INTO "codes" ("kind", "field", "code", "name") VALUES
  ('ts', 'source', 'ag', 'Agent'),
  ('ts', 'source', 'tl', 'Tool'),
  ('ts', 'state', 'go', 'Gone'),
  ('ts', 'state', 'gu', 'GaveUp'),
  ('ts', 'state', 'rl', 'Realised'),
  ('ts', 'state', 'rq', 'Requested'),
  ('ts', 'last_failure_cause_category', 'ex', 'External'),
  ('ts', 'last_failure_cause_category', 'mc', 'Misconfigured'),
  ('ts', 'last_failure_cause_category', 'mx', 'MissingCapability'),
  ('ts', 'last_failure_cause_category', 'rf', 'Refused'),
  ('ts', 'last_failure_cause_category', 'tr', 'Transient'),
  ('ts', 'cause_category', 'ex', 'External'),
  ('ts', 'cause_category', 'mc', 'Misconfigured'),
  ('ts', 'cause_category', 'mx', 'MissingCapability'),
  ('ts', 'cause_category', 'rf', 'Refused'),
  ('ts', 'cause_category', 'tr', 'Transient'),
  ('ts', 'log:edge', 'at', 'Attempted'),
  ('ts', 'log:edge', 'gu', 'GiveUp'),
  ('ts', 'log:edge', 'rl', 'Realise'),
  ('ts', 'log:edge', 'rs', 'Release'),
  ('ts', 'log:edge', 'va', 'Vanish'),
  ('ts', 'log:label', 'go', 'Gone'),
  ('ts', 'log:label', 'gu', 'GaveUp'),
  ('ts', 'log:label', 'rl', 'Realised'),
  ('ts', 'log:label', 'rq', 'Requested');
CREATE VIEW "terminal_session_view" AS SELECT
  "terminal_session"."id",
  "terminal_session"."version",
  "terminal_session"."environment",
  "terminal_session"."vessel",
  "terminal_session"."convoy",
  (SELECT "s" FROM "strings" WHERE "id" = "terminal_session"."role") AS "role",
  (SELECT "s" FROM "strings" WHERE "id" = "terminal_session"."pool") AS "pool",
  "terminal_session"."cwd",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ts' AND "field" = 'source' AND "code" = "terminal_session"."source") AS "source",
  "terminal_session"."source_command",
  (SELECT "s" FROM "strings" WHERE "id" = "terminal_session"."source_capability") AS "source_capability",
  "terminal_session"."source_adapter",
  "terminal_session"."source_model",
  "terminal_session"."source_brief",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ts' AND "field" = 'state' AND "code" = "terminal_session"."state") AS "state",
  "terminal_session"."state_since",
  "terminal_session"."failures",
  "terminal_session"."last_failure_at",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ts' AND "field" = 'last_failure_cause_category' AND "code" = "terminal_session"."last_failure_cause_category") AS "last_failure_cause_category",
  "terminal_session"."last_failure_cause_detail",
  "terminal_session"."last_failure_retryable",
  "terminal_session"."identity",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ts' AND "field" = 'cause_category' AND "code" = "terminal_session"."cause_category") AS "cause_category",
  "terminal_session"."cause_detail"
FROM "terminal_session";
CREATE VIEW "terminal_session_env_view" AS SELECT
  "terminal_session_env"."terminal_session",
  "terminal_session_env"."key",
  "terminal_session_env"."value"
FROM "terminal_session_env";
CREATE TABLE "vessel" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "convoy" INTEGER NOT NULL REFERENCES "resource_identity" ("id"),
  "class" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "priority" INTEGER NOT NULL,
  "host" INTEGER NOT NULL,
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
CREATE TABLE "vessel_shape" (
  "vessel" INTEGER NOT NULL,
  "key" INTEGER NOT NULL,
  "value" INTEGER NOT NULL,
  PRIMARY KEY ("vessel", "key"),
  FOREIGN KEY ("vessel") REFERENCES "vessel" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE INDEX "vessel_by_convoy" ON "vessel" ("convoy");
CREATE INDEX "vessel_live_by_host" ON "vessel" ("host") WHERE "state" IN ('rq', 'rl');
INSERT INTO "codes" ("kind", "field", "code", "name") VALUES
  ('ve', 'state', 'go', 'Gone'),
  ('ve', 'state', 'gu', 'GaveUp'),
  ('ve', 'state', 'rl', 'Realised'),
  ('ve', 'state', 'rq', 'Requested'),
  ('ve', 'last_failure_cause_category', 'ex', 'External'),
  ('ve', 'last_failure_cause_category', 'mc', 'Misconfigured'),
  ('ve', 'last_failure_cause_category', 'mx', 'MissingCapability'),
  ('ve', 'last_failure_cause_category', 'rf', 'Refused'),
  ('ve', 'last_failure_cause_category', 'tr', 'Transient'),
  ('ve', 'cause_category', 'ex', 'External'),
  ('ve', 'cause_category', 'mc', 'Misconfigured'),
  ('ve', 'cause_category', 'mx', 'MissingCapability'),
  ('ve', 'cause_category', 'rf', 'Refused'),
  ('ve', 'cause_category', 'tr', 'Transient'),
  ('ve', 'log:edge', 'at', 'Attempted'),
  ('ve', 'log:edge', 'gu', 'GiveUp'),
  ('ve', 'log:edge', 'rl', 'Realise'),
  ('ve', 'log:edge', 'rs', 'Release'),
  ('ve', 'log:edge', 'va', 'Vanish'),
  ('ve', 'log:label', 'go', 'Gone'),
  ('ve', 'log:label', 'gu', 'GaveUp'),
  ('ve', 'log:label', 'rl', 'Realised'),
  ('ve', 'log:label', 'rq', 'Requested');
CREATE VIEW "vessel_view" AS SELECT
  "vessel"."id",
  "vessel"."version",
  "vessel"."convoy",
  (SELECT "s" FROM "strings" WHERE "id" = "vessel"."class") AS "class",
  "vessel"."priority",
  "vessel"."host",
  (SELECT "name" FROM "codes" WHERE "kind" = 've' AND "field" = 'state' AND "code" = "vessel"."state") AS "state",
  "vessel"."state_since",
  "vessel"."failures",
  "vessel"."last_failure_at",
  (SELECT "name" FROM "codes" WHERE "kind" = 've' AND "field" = 'last_failure_cause_category' AND "code" = "vessel"."last_failure_cause_category") AS "last_failure_cause_category",
  "vessel"."last_failure_cause_detail",
  "vessel"."last_failure_retryable",
  "vessel"."identity",
  (SELECT "name" FROM "codes" WHERE "kind" = 've' AND "field" = 'cause_category' AND "code" = "vessel"."cause_category") AS "cause_category",
  "vessel"."cause_detail"
FROM "vessel";
CREATE VIEW "vessel_shape_view" AS SELECT
  "vessel_shape"."vessel",
  "vessel_shape"."key",
  "vessel_shape"."value"
FROM "vessel_shape";
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
