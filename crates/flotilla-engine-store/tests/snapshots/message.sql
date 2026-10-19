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
CREATE TABLE "agent_session" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "state" TEXT NOT NULL CHECK ("state" IN ('en', 'li')),
  "state_since" INTEGER NOT NULL,
  "channel" TEXT CHECK ("channel" IN ('at', 'de')),
  "reason" TEXT CHECK ("reason" IN ('co', 'lo')),
  CHECK (CASE WHEN "state" = 'li'
      THEN "channel" IS NOT NULL
      AND "reason" IS NULL
      END),
  CHECK (CASE WHEN "state" = 'en'
      THEN "reason" IS NOT NULL
      AND "channel" IS NULL
      END)
) STRICT;
INSERT INTO "codes" ("kind", "field", "code", "name") VALUES
  ('as', 'state', 'en', 'Ended'),
  ('as', 'state', 'li', 'Live'),
  ('as', 'channel', 'at', 'Attached'),
  ('as', 'channel', 'de', 'Detached'),
  ('as', 'reason', 'co', 'Completed'),
  ('as', 'reason', 'lo', 'Lost'),
  ('as', 'log:edge', 'at', 'Attach'),
  ('as', 'log:edge', 'de', 'Detach'),
  ('as', 'log:edge', 'en', 'End'),
  ('as', 'log:edge', 'rv', 'Receive'),
  ('as', 'log:label', 'en', 'Ended'),
  ('as', 'log:label', 'li', 'Live');
CREATE VIEW "agent_session_view" AS SELECT
  "agent_session"."id",
  "agent_session"."version",
  (SELECT "name" FROM "codes" WHERE "kind" = 'as' AND "field" = 'state' AND "code" = "agent_session"."state") AS "state",
  "agent_session"."state_since",
  (SELECT "name" FROM "codes" WHERE "kind" = 'as' AND "field" = 'channel' AND "code" = "agent_session"."channel") AS "channel",
  (SELECT "name" FROM "codes" WHERE "kind" = 'as' AND "field" = 'reason' AND "code" = "agent_session"."reason") AS "reason"
FROM "agent_session";
CREATE TABLE "message" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "sender" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "inbox" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "admitted_seq" INTEGER NOT NULL,
  "relation" TEXT NOT NULL CHECK ("relation" IN ('de', 'dy', 'pe', 'su', 'sy')),
  "body" BLOB NOT NULL REFERENCES "objects" ("hash"),
  "in_reply_to" INTEGER REFERENCES "resource_identity" ("id"),
  "supersedes" INTEGER REFERENCES "resource_identity" ("id"),
  "expectation" TEXT NOT NULL CHECK ("expectation" IN ('no', 'ou', 're')),
  "expectation_condition" INTEGER REFERENCES "strings" ("id"),
  "deadline" INTEGER,
  "unbound_after" INTEGER NOT NULL,
  "delivery_condition" INTEGER REFERENCES "strings" ("id"),
  "state" TEXT NOT NULL CHECK ("state" IN ('aw', 'dr', 'pe', 'se')),
  "state_since" INTEGER NOT NULL,
  "receiver_kind" TEXT CHECK ("receiver_kind" IN ('as', 'co')),
  "receiver_id" INTEGER REFERENCES "strings" ("id"),
  "receiver_host" INTEGER REFERENCES "strings" ("id"),
  "reason" TEXT CHECK ("reason" IN ('cu', 'ex', 'su', 'un', 'wi')),
  "reason_by" INTEGER REFERENCES "strings" ("id"),
  "reason_authority" INTEGER REFERENCES "strings" ("id"),
  "watermark" INTEGER,
  CHECK (CASE WHEN "expectation" = 'no'
      THEN "expectation_condition" IS NULL
      END),
  CHECK (CASE WHEN "expectation" = 're'
      THEN "expectation_condition" IS NULL
      END),
  CHECK (CASE WHEN "expectation" = 'ou'
      THEN "expectation_condition" IS NOT NULL
      END),
  CHECK (CASE WHEN "receiver_kind" IS NOT NULL
      THEN "receiver_id" IS NOT NULL
      AND "receiver_host" IS NOT NULL
      ELSE "receiver_id" IS NULL
      AND "receiver_host" IS NULL
      END),
  CHECK (CASE WHEN "reason" = 'un'
      THEN "reason_by" IS NULL
      AND "reason_authority" IS NULL
      END),
  CHECK (CASE WHEN "reason" = 'ex'
      THEN "reason_by" IS NULL
      AND "reason_authority" IS NULL
      END),
  CHECK (CASE WHEN "reason" = 'su'
      THEN "reason_by" IS NOT NULL
      AND "reason_authority" IS NULL
      END),
  CHECK (CASE WHEN "reason" = 'wi'
      THEN "reason_by" IS NOT NULL
      AND "reason_authority" IS NOT NULL
      END),
  CHECK (CASE WHEN "reason" = 'cu'
      THEN "reason_by" IS NOT NULL
      AND "reason_authority" IS NOT NULL
      END),
  CHECK (CASE WHEN "state" = 'pe'
      THEN "receiver_kind" IS NULL
      AND "receiver_id" IS NULL
      AND "receiver_host" IS NULL
      AND "reason" IS NULL
      AND "reason_by" IS NULL
      AND "reason_authority" IS NULL
      AND "watermark" IS NULL
      END),
  CHECK (CASE WHEN "state" = 'aw'
      THEN "receiver_kind" IS NOT NULL
      AND "receiver_id" IS NOT NULL
      AND "receiver_host" IS NOT NULL
      AND "reason" IS NULL
      AND "reason_by" IS NULL
      AND "reason_authority" IS NULL
      AND "watermark" IS NULL
      END),
  CHECK (CASE WHEN "state" = 'se'
      THEN "reason" IS NULL
      AND "reason_by" IS NULL
      AND "reason_authority" IS NULL
      AND "watermark" IS NULL
      END),
  CHECK (CASE WHEN "state" = 'dr'
      THEN "reason" IS NOT NULL
      END)
) STRICT;
CREATE TABLE "message_references" (
  "owner" INTEGER NOT NULL,
  "pos" INTEGER NOT NULL,
  "address" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "revision" TEXT NOT NULL,
  "subject" INTEGER NOT NULL CHECK ("subject" IN (0, 1)),
  PRIMARY KEY ("owner", "pos"),
  FOREIGN KEY ("owner") REFERENCES "message" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE INDEX "message_by_reply" ON "message" ("in_reply_to") WHERE "in_reply_to" IS NOT NULL;
CREATE INDEX "message_by_supersedes" ON "message" ("supersedes") WHERE "supersedes" IS NOT NULL;
CREATE INDEX "message_deadline" ON "message" ("deadline") WHERE "state" IN ('pe', 'aw') AND "deadline" IS NOT NULL;
CREATE INDEX "message_pending_by_inbox" ON "message" ("inbox", "admitted_seq") WHERE "state" IN ('pe');
INSERT INTO "codes" ("kind", "field", "code", "name") VALUES
  ('ms', 'relation', 'de', 'Dependee'),
  ('ms', 'relation', 'dy', 'Dependency'),
  ('ms', 'relation', 'pe', 'Peer'),
  ('ms', 'relation', 'su', 'Supervisor'),
  ('ms', 'relation', 'sy', 'System'),
  ('ms', 'expectation', 'no', 'Nothing'),
  ('ms', 'expectation', 'ou', 'Outcome'),
  ('ms', 'expectation', 're', 'Reply'),
  ('ms', 'state', 'aw', 'Awaiting'),
  ('ms', 'state', 'dr', 'Dropped'),
  ('ms', 'state', 'pe', 'Pending'),
  ('ms', 'state', 'se', 'Settled'),
  ('ms', 'receiver_kind', 'as', 'AgentSession'),
  ('ms', 'receiver_kind', 'co', 'Controller'),
  ('ms', 'reason', 'cu', 'ConsumerUnreachable'),
  ('ms', 'reason', 'ex', 'Expired'),
  ('ms', 'reason', 'su', 'Superseded'),
  ('ms', 'reason', 'un', 'Undeliverable'),
  ('ms', 'reason', 'wi', 'Withdrawn'),
  ('ms', 'log:edge', 'de', 'Deliver'),
  ('ms', 'log:edge', 'dr', 'Drop'),
  ('ms', 'log:edge', 'se', 'Settle'),
  ('ms', 'log:label', 'aw', 'Awaiting'),
  ('ms', 'log:label', 'dr', 'Dropped'),
  ('ms', 'log:label', 'pe', 'Pending'),
  ('ms', 'log:label', 'se', 'Settled');
CREATE VIEW "message_view" AS SELECT
  "message"."id",
  "message"."version",
  (SELECT "s" FROM "strings" WHERE "id" = "message"."sender") AS "sender",
  (SELECT "s" FROM "strings" WHERE "id" = "message"."inbox") AS "inbox",
  "message"."admitted_seq",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ms' AND "field" = 'relation' AND "code" = "message"."relation") AS "relation",
  "message"."body",
  "message"."in_reply_to",
  "message"."supersedes",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ms' AND "field" = 'expectation' AND "code" = "message"."expectation") AS "expectation",
  (SELECT "s" FROM "strings" WHERE "id" = "message"."expectation_condition") AS "expectation_condition",
  "message"."deadline",
  "message"."unbound_after",
  (SELECT "s" FROM "strings" WHERE "id" = "message"."delivery_condition") AS "delivery_condition",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ms' AND "field" = 'state' AND "code" = "message"."state") AS "state",
  "message"."state_since",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ms' AND "field" = 'receiver_kind' AND "code" = "message"."receiver_kind") AS "receiver_kind",
  (SELECT "s" FROM "strings" WHERE "id" = "message"."receiver_id") AS "receiver_id",
  (SELECT "s" FROM "strings" WHERE "id" = "message"."receiver_host") AS "receiver_host",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ms' AND "field" = 'reason' AND "code" = "message"."reason") AS "reason",
  (SELECT "s" FROM "strings" WHERE "id" = "message"."reason_by") AS "reason_by",
  (SELECT "s" FROM "strings" WHERE "id" = "message"."reason_authority") AS "reason_authority",
  "message"."watermark"
FROM "message";
CREATE VIEW "message_references_view" AS SELECT
  "message_references"."owner",
  "message_references"."pos",
  (SELECT "s" FROM "strings" WHERE "id" = "message_references"."address") AS "address",
  "message_references"."revision",
  "message_references"."subject"
FROM "message_references";
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
