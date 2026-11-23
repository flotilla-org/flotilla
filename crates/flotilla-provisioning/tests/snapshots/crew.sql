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
  created_at INTEGER,
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
CREATE TABLE "terminal_launch_facts" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "endpoint_runtime_root" TEXT,
  "endpoint_daemon" TEXT,
  "agent_adapter" INTEGER REFERENCES "strings" ("id"),
  "agent_model" TEXT,
  "agent_stance" TEXT,
  "build_jobs" INTEGER,
  CHECK (CASE WHEN "endpoint_runtime_root" IS NOT NULL
      THEN "endpoint_daemon" IS NOT NULL
      ELSE "endpoint_daemon" IS NULL
      END),
  CHECK (CASE WHEN "agent_adapter" IS NOT NULL
      THEN "agent_stance" IS NOT NULL
      ELSE "agent_model" IS NULL
      AND "agent_stance" IS NULL
      END)
) STRICT;
CREATE VIEW "terminal_launch_facts_view" AS SELECT
  "terminal_launch_facts"."id",
  "terminal_launch_facts"."version",
  "terminal_launch_facts"."endpoint_runtime_root",
  "terminal_launch_facts"."endpoint_daemon",
  (SELECT "s" FROM "strings" WHERE "id" = "terminal_launch_facts"."agent_adapter") AS "agent_adapter",
  "terminal_launch_facts"."agent_model",
  "terminal_launch_facts"."agent_stance",
  "terminal_launch_facts"."build_jobs"
FROM "terminal_launch_facts";
CREATE TABLE "terminal_session" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "launch_of" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "launch" INTEGER NOT NULL,
  "environment" INTEGER NOT NULL REFERENCES "resource_identity" ("id"),
  "holder" INTEGER REFERENCES "strings" ("id"),
  "role" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "pool" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "cwd" TEXT NOT NULL,
  "source" TEXT NOT NULL CHECK ("source" IN ('ag', 'tl')),
  "source_command" TEXT,
  "source_capability" INTEGER REFERENCES "strings" ("id"),
  "source_adapter" TEXT,
  "source_model" TEXT,
  "source_brief" BLOB REFERENCES "objects" ("hash"),
  "agent_session" INTEGER REFERENCES "resource_identity" ("id"),
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
CREATE TABLE "terminal_session_env" (
  "terminal_session" INTEGER NOT NULL,
  "key" TEXT NOT NULL,
  "value" TEXT NOT NULL,
  PRIMARY KEY ("terminal_session", "key"),
  FOREIGN KEY ("terminal_session") REFERENCES "terminal_session" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE INDEX "terminal_session_by_agent_session" ON "terminal_session" ("agent_session") WHERE "agent_session" IS NOT NULL;
CREATE INDEX "terminal_session_by_environment" ON "terminal_session" ("environment");
CREATE INDEX "terminal_session_by_holder" ON "terminal_session" ("holder") WHERE "holder" IS NOT NULL;
CREATE INDEX "terminal_session_by_launch" ON "terminal_session" ("launch_of", "launch");
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
  ('ts', 'ending', 'fa', 'Failed'),
  ('ts', 'ending', 'lo', 'Lost'),
  ('ts', 'ending', 'rl', 'Released'),
  ('ts', 'cause_category', 'ex', 'External'),
  ('ts', 'cause_category', 'mc', 'Misconfigured'),
  ('ts', 'cause_category', 'mx', 'MissingCapability'),
  ('ts', 'cause_category', 'rf', 'Refused'),
  ('ts', 'cause_category', 'tr', 'Transient'),
  ('ts', 'log:edge', 'at', 'Attempted'),
  ('ts', 'log:edge', 'fe', 'FailedEarly'),
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
  (SELECT "s" FROM "strings" WHERE "id" = "terminal_session"."launch_of") AS "launch_of",
  "terminal_session"."launch",
  "terminal_session"."environment",
  (SELECT "s" FROM "strings" WHERE "id" = "terminal_session"."holder") AS "holder",
  (SELECT "s" FROM "strings" WHERE "id" = "terminal_session"."role") AS "role",
  (SELECT "s" FROM "strings" WHERE "id" = "terminal_session"."pool") AS "pool",
  "terminal_session"."cwd",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ts' AND "field" = 'source' AND "code" = "terminal_session"."source") AS "source",
  "terminal_session"."source_command",
  (SELECT "s" FROM "strings" WHERE "id" = "terminal_session"."source_capability") AS "source_capability",
  "terminal_session"."source_adapter",
  "terminal_session"."source_model",
  "terminal_session"."source_brief",
  "terminal_session"."agent_session",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ts' AND "field" = 'state' AND "code" = "terminal_session"."state") AS "state",
  "terminal_session"."state_since",
  "terminal_session"."failures",
  "terminal_session"."last_failure_at",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ts' AND "field" = 'last_failure_cause_category' AND "code" = "terminal_session"."last_failure_cause_category") AS "last_failure_cause_category",
  "terminal_session"."last_failure_cause_detail",
  "terminal_session"."last_failure_retryable",
  "terminal_session"."identity",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ts' AND "field" = 'ending' AND "code" = "terminal_session"."ending") AS "ending",
  "terminal_session"."ending_reason",
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
  "launch_of" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "launch" INTEGER NOT NULL,
  "convoy" INTEGER NOT NULL REFERENCES "resource_identity" ("id"),
  "class" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "priority" INTEGER NOT NULL,
  "host" INTEGER NOT NULL REFERENCES "strings" ("id"),
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
CREATE TABLE "vessel_shape" (
  "vessel" INTEGER NOT NULL,
  "key" INTEGER NOT NULL,
  "value" INTEGER NOT NULL,
  PRIMARY KEY ("vessel", "key"),
  FOREIGN KEY ("vessel") REFERENCES "vessel" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE INDEX "vessel_by_launch" ON "vessel" ("convoy", "launch_of", "launch");
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
  ('ve', 'ending', 'fa', 'Failed'),
  ('ve', 'ending', 'lo', 'Lost'),
  ('ve', 'ending', 'rl', 'Released'),
  ('ve', 'cause_category', 'ex', 'External'),
  ('ve', 'cause_category', 'mc', 'Misconfigured'),
  ('ve', 'cause_category', 'mx', 'MissingCapability'),
  ('ve', 'cause_category', 'rf', 'Refused'),
  ('ve', 'cause_category', 'tr', 'Transient'),
  ('ve', 'log:edge', 'at', 'Attempted'),
  ('ve', 'log:edge', 'fe', 'FailedEarly'),
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
  (SELECT "s" FROM "strings" WHERE "id" = "vessel"."launch_of") AS "launch_of",
  "vessel"."launch",
  "vessel"."convoy",
  (SELECT "s" FROM "strings" WHERE "id" = "vessel"."class") AS "class",
  "vessel"."priority",
  (SELECT "s" FROM "strings" WHERE "id" = "vessel"."host") AS "host",
  (SELECT "name" FROM "codes" WHERE "kind" = 've' AND "field" = 'state' AND "code" = "vessel"."state") AS "state",
  "vessel"."state_since",
  "vessel"."failures",
  "vessel"."last_failure_at",
  (SELECT "name" FROM "codes" WHERE "kind" = 've' AND "field" = 'last_failure_cause_category' AND "code" = "vessel"."last_failure_cause_category") AS "last_failure_cause_category",
  "vessel"."last_failure_cause_detail",
  "vessel"."last_failure_retryable",
  "vessel"."identity",
  (SELECT "name" FROM "codes" WHERE "kind" = 've' AND "field" = 'ending' AND "code" = "vessel"."ending") AS "ending",
  "vessel"."ending_reason",
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
