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
CREATE TABLE "awaiting_reply" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "message" INTEGER NOT NULL REFERENCES "resource_identity" ("id"),
  "sender" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "recipient" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "subject" INTEGER REFERENCES "strings" ("id"),
  "kind" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "awaited" TEXT NOT NULL CHECK ("awaited" IN ('ou', 're')),
  "deadline" INTEGER,
  "state" TEXT NOT NULL CHECK ("state" IN ('an', 'ex', 'op', 'wd')),
  "state_since" INTEGER NOT NULL,
  "reason" TEXT,
  CHECK (CASE WHEN "state" = 'op'
      THEN "reason" IS NULL
      END),
  CHECK (CASE WHEN "state" = 'an'
      THEN "reason" IS NULL
      END),
  CHECK (CASE WHEN "state" = 'ex'
      THEN "reason" IS NULL
      END),
  CHECK (CASE WHEN "state" = 'wd'
      THEN "reason" IS NOT NULL
      END)
) STRICT;
CREATE INDEX "awaiting_reply_by_message" ON "awaiting_reply" ("message");
CREATE INDEX "awaiting_reply_deadline" ON "awaiting_reply" ("deadline") WHERE "state" IN ('op') AND "deadline" IS NOT NULL;
CREATE INDEX "awaiting_reply_open_by_sender" ON "awaiting_reply" ("sender") WHERE "state" IN ('op');
CREATE INDEX "awaiting_reply_open_by_subject" ON "awaiting_reply" ("subject") WHERE "state" IN ('op') AND "subject" IS NOT NULL;
INSERT INTO "codes" ("kind", "field", "code", "name") VALUES
  ('ar', 'awaited', 'ou', 'Outcome'),
  ('ar', 'awaited', 're', 'Reply'),
  ('ar', 'state', 'an', 'Answered'),
  ('ar', 'state', 'ex', 'Expired'),
  ('ar', 'state', 'op', 'Open'),
  ('ar', 'state', 'wd', 'Withdrawn'),
  ('ar', 'log:edge', 'an', 'Answer'),
  ('ar', 'log:edge', 'ex', 'Expire'),
  ('ar', 'log:edge', 'wd', 'Withdraw'),
  ('ar', 'log:label', 'an', 'Answered'),
  ('ar', 'log:label', 'ex', 'Expired'),
  ('ar', 'log:label', 'op', 'Open'),
  ('ar', 'log:label', 'wd', 'Withdrawn');
CREATE VIEW "awaiting_reply_view" AS SELECT
  "awaiting_reply"."id",
  "awaiting_reply"."version",
  "awaiting_reply"."message",
  (SELECT "s" FROM "strings" WHERE "id" = "awaiting_reply"."sender") AS "sender",
  (SELECT "s" FROM "strings" WHERE "id" = "awaiting_reply"."recipient") AS "recipient",
  (SELECT "s" FROM "strings" WHERE "id" = "awaiting_reply"."subject") AS "subject",
  (SELECT "s" FROM "strings" WHERE "id" = "awaiting_reply"."kind") AS "kind",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ar' AND "field" = 'awaited' AND "code" = "awaiting_reply"."awaited") AS "awaited",
  "awaiting_reply"."deadline",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ar' AND "field" = 'state' AND "code" = "awaiting_reply"."state") AS "state",
  "awaiting_reply"."state_since",
  "awaiting_reply"."reason"
FROM "awaiting_reply";
CREATE TABLE "agent_session" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "state" TEXT NOT NULL CHECK ("state" IN ('en', 'li')),
  "state_since" INTEGER NOT NULL,
  "channel" TEXT CHECK ("channel" IN ('at', 'de')),
  "activity" TEXT CHECK ("activity" IN ('id', 'in', 'wo')),
  "activity_since" INTEGER,
  "activity_cause" TEXT,
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
  ('as', 'activity', 'id', 'Idle'),
  ('as', 'activity', 'in', 'Interrupted'),
  ('as', 'activity', 'wo', 'Working'),
  ('as', 'reason', 'co', 'Completed'),
  ('as', 'reason', 'lo', 'Lost'),
  ('as', 'log:edge', 'at', 'Attach'),
  ('as', 'log:edge', 'de', 'Detach'),
  ('as', 'log:edge', 'en', 'End'),
  ('as', 'log:edge', 'ob', 'Observe'),
  ('as', 'log:edge', 'rv', 'Receive'),
  ('as', 'log:label', 'en', 'Ended'),
  ('as', 'log:label', 'li', 'Live');
CREATE VIEW "agent_session_view" AS SELECT
  "agent_session"."id",
  "agent_session"."version",
  (SELECT "name" FROM "codes" WHERE "kind" = 'as' AND "field" = 'state' AND "code" = "agent_session"."state") AS "state",
  "agent_session"."state_since",
  (SELECT "name" FROM "codes" WHERE "kind" = 'as' AND "field" = 'channel' AND "code" = "agent_session"."channel") AS "channel",
  (SELECT "name" FROM "codes" WHERE "kind" = 'as' AND "field" = 'activity' AND "code" = "agent_session"."activity") AS "activity",
  "agent_session"."activity_since",
  "agent_session"."activity_cause",
  (SELECT "name" FROM "codes" WHERE "kind" = 'as' AND "field" = 'reason' AND "code" = "agent_session"."reason") AS "reason"
FROM "agent_session";
CREATE TABLE "convoy" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "project" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "promise_floor" INTEGER NOT NULL DEFAULT 0,
  "priority" INTEGER NOT NULL,
  "kinds" BLOB NOT NULL REFERENCES "objects" ("hash"),
  "state" TEXT NOT NULL CHECK ("state" IN ('ab', 'de', 'op')),
  "state_since" INTEGER NOT NULL,
  "reason" TEXT,
  CHECK (CASE WHEN "state" = 'op'
      THEN "reason" IS NULL
      END),
  CHECK (CASE WHEN "state" = 'de'
      THEN "reason" IS NULL
      END),
  CHECK (CASE WHEN "state" = 'ab'
      THEN "reason" IS NOT NULL
      END)
) STRICT;
CREATE TABLE "convoy_duties" (
  "convoy" INTEGER NOT NULL,
  "key" INTEGER NOT NULL,
  "owner_role" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "owner_seat" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "scope" TEXT NOT NULL CHECK ("scope" IN ('lt', 'wo')),
  "terms" BLOB NOT NULL,
  PRIMARY KEY ("convoy", "key"),
  FOREIGN KEY ("convoy") REFERENCES "convoy" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE TABLE "convoy_duties_scope_promises" (
  "convoy" INTEGER NOT NULL,
  "duties_key" INTEGER NOT NULL,
  "value" INTEGER NOT NULL,
  PRIMARY KEY ("convoy", "duties_key", "value"),
  FOREIGN KEY ("convoy", "duties_key") REFERENCES "convoy_duties" ("convoy", "key") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE TABLE "convoy_seats" (
  "convoy" INTEGER NOT NULL,
  "pos" INTEGER NOT NULL,
  "version" INTEGER NOT NULL,
  "role" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "seat" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "parent_role" INTEGER REFERENCES "strings" ("id"),
  "parent_seat" INTEGER REFERENCES "strings" ("id"),
  "supervised_by" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "retired" INTEGER NOT NULL CHECK ("retired" IN (0, 1)) DEFAULT 0,
  "replacements" INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY ("convoy", "pos"),
  FOREIGN KEY ("convoy") REFERENCES "convoy" ("id") ON DELETE CASCADE,
  CHECK (CASE WHEN "parent_role" IS NOT NULL
      THEN "parent_seat" IS NOT NULL
      ELSE "parent_seat" IS NULL
      END)
) STRICT, WITHOUT ROWID;
CREATE TABLE "convoy_promises" (
  "convoy" INTEGER NOT NULL,
  "pos" INTEGER NOT NULL,
  "version" INTEGER NOT NULL,
  "id" INTEGER NOT NULL,
  "label" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "kind" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "target" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "source" TEXT NOT NULL CHECK ("source" IN ('cr', 'dp', 'tp')),
  "owner_role" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "owner_seat" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "progress" TEXT NOT NULL CHECK ("progress" IN ('ke', 'pe', 'rt', 'su')),
  "progress_since" INTEGER NOT NULL,
  PRIMARY KEY ("convoy", "pos"),
  FOREIGN KEY ("convoy") REFERENCES "convoy" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE TABLE "convoy_promises_submissions" (
  "convoy" INTEGER NOT NULL,
  "promises_pos" INTEGER NOT NULL,
  "pos" INTEGER NOT NULL,
  "version" INTEGER NOT NULL,
  "address" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "revision" TEXT NOT NULL,
  "by_role" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "by_seat" INTEGER NOT NULL REFERENCES "strings" ("id"),
  PRIMARY KEY ("convoy", "promises_pos", "pos"),
  FOREIGN KEY ("convoy", "promises_pos") REFERENCES "convoy_promises" ("convoy", "pos") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE TABLE "convoy_quota" (
  "convoy" INTEGER NOT NULL,
  "key" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "value" INTEGER NOT NULL,
  PRIMARY KEY ("convoy", "key"),
  FOREIGN KEY ("convoy") REFERENCES "convoy" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE TABLE "convoy_retired" (
  "convoy" INTEGER NOT NULL,
  "value" INTEGER NOT NULL,
  PRIMARY KEY ("convoy", "value"),
  FOREIGN KEY ("convoy") REFERENCES "convoy" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE INDEX "convoy_open_by_project" ON "convoy" ("project") WHERE "state" IN ('op');
CREATE UNIQUE INDEX "convoy_promises_by_label" ON "convoy_promises" ("convoy", "label");
CREATE INDEX "convoy_promises_submissions_by_address" ON "convoy_promises_submissions" ("address");
INSERT INTO "codes" ("kind", "field", "code", "name") VALUES
  ('cv', 'state', 'ab', 'Abandoned'),
  ('cv', 'state', 'de', 'Delivered'),
  ('cv', 'state', 'op', 'Open'),
  ('cv', 'duties.scope', 'lt', 'Lifetime'),
  ('cv', 'duties.scope', 'wo', 'WhileOpen'),
  ('cv', 'promises.source', 'cr', 'Crew'),
  ('cv', 'promises.source', 'dp', 'Dispatch'),
  ('cv', 'promises.source', 'tp', 'Template'),
  ('cv', 'promises.progress', 'ke', 'Kept'),
  ('cv', 'promises.progress', 'pe', 'Pending'),
  ('cv', 'promises.progress', 'rt', 'Retracted'),
  ('cv', 'promises.progress', 'su', 'Submitted'),
  ('cv', 'log:edge', 'ab', 'Abandon'),
  ('cv', 'log:edge', 'as', 'AddSeat'),
  ('cv', 'log:edge', 'dc', 'Declare'),
  ('cv', 'log:edge', 'de', 'Deliver'),
  ('cv', 'log:edge', 'ke', 'Keep'),
  ('cv', 'log:edge', 'ra', 'Readmit'),
  ('cv', 'log:edge', 'rd', 'RetireDuty'),
  ('cv', 'log:edge', 'rj', 'Reject'),
  ('cv', 'log:edge', 'ro', 'Rollover'),
  ('cv', 'log:edge', 'rp', 'ReplaceSeat'),
  ('cv', 'log:edge', 'rs', 'Reassign'),
  ('cv', 'log:edge', 'rt', 'Retract'),
  ('cv', 'log:edge', 'sr', 'RetireSeat'),
  ('cv', 'log:edge', 'su', 'Submit'),
  ('cv', 'log:edge', 'sv', 'Resupervise'),
  ('cv', 'log:label', 'ab', 'Abandoned'),
  ('cv', 'log:label', 'de', 'Delivered'),
  ('cv', 'log:label', 'op', 'Open');
CREATE VIEW "convoy_view" AS SELECT
  "convoy"."id",
  "convoy"."version",
  (SELECT "s" FROM "strings" WHERE "id" = "convoy"."project") AS "project",
  "convoy"."promise_floor",
  "convoy"."priority",
  "convoy"."kinds",
  (SELECT "name" FROM "codes" WHERE "kind" = 'cv' AND "field" = 'state' AND "code" = "convoy"."state") AS "state",
  "convoy"."state_since",
  "convoy"."reason"
FROM "convoy";
CREATE VIEW "convoy_duties_view" AS SELECT
  "convoy_duties"."convoy",
  "convoy_duties"."key",
  (SELECT "s" FROM "strings" WHERE "id" = "convoy_duties"."owner_role") AS "owner_role",
  (SELECT "s" FROM "strings" WHERE "id" = "convoy_duties"."owner_seat") AS "owner_seat",
  (SELECT "name" FROM "codes" WHERE "kind" = 'cv' AND "field" = 'duties.scope' AND "code" = "convoy_duties"."scope") AS "scope",
  "convoy_duties"."terms"
FROM "convoy_duties";
CREATE VIEW "convoy_duties_scope_promises_view" AS SELECT
  "convoy_duties_scope_promises"."convoy",
  "convoy_duties_scope_promises"."duties_key",
  "convoy_duties_scope_promises"."value"
FROM "convoy_duties_scope_promises";
CREATE VIEW "convoy_seats_view" AS SELECT
  "convoy_seats"."convoy",
  "convoy_seats"."pos",
  "convoy_seats"."version",
  (SELECT "s" FROM "strings" WHERE "id" = "convoy_seats"."role") AS "role",
  (SELECT "s" FROM "strings" WHERE "id" = "convoy_seats"."seat") AS "seat",
  (SELECT "s" FROM "strings" WHERE "id" = "convoy_seats"."parent_role") AS "parent_role",
  (SELECT "s" FROM "strings" WHERE "id" = "convoy_seats"."parent_seat") AS "parent_seat",
  (SELECT "s" FROM "strings" WHERE "id" = "convoy_seats"."supervised_by") AS "supervised_by",
  "convoy_seats"."retired",
  "convoy_seats"."replacements"
FROM "convoy_seats";
CREATE VIEW "convoy_promises_view" AS SELECT
  "convoy_promises"."convoy",
  "convoy_promises"."pos",
  "convoy_promises"."version",
  "convoy_promises"."id",
  (SELECT "s" FROM "strings" WHERE "id" = "convoy_promises"."label") AS "label",
  (SELECT "s" FROM "strings" WHERE "id" = "convoy_promises"."kind") AS "kind",
  (SELECT "s" FROM "strings" WHERE "id" = "convoy_promises"."target") AS "target",
  (SELECT "name" FROM "codes" WHERE "kind" = 'cv' AND "field" = 'promises.source' AND "code" = "convoy_promises"."source") AS "source",
  (SELECT "s" FROM "strings" WHERE "id" = "convoy_promises"."owner_role") AS "owner_role",
  (SELECT "s" FROM "strings" WHERE "id" = "convoy_promises"."owner_seat") AS "owner_seat",
  (SELECT "name" FROM "codes" WHERE "kind" = 'cv' AND "field" = 'promises.progress' AND "code" = "convoy_promises"."progress") AS "progress",
  "convoy_promises"."progress_since"
FROM "convoy_promises";
CREATE VIEW "convoy_promises_submissions_view" AS SELECT
  "convoy_promises_submissions"."convoy",
  "convoy_promises_submissions"."promises_pos",
  "convoy_promises_submissions"."pos",
  "convoy_promises_submissions"."version",
  (SELECT "s" FROM "strings" WHERE "id" = "convoy_promises_submissions"."address") AS "address",
  "convoy_promises_submissions"."revision",
  (SELECT "s" FROM "strings" WHERE "id" = "convoy_promises_submissions"."by_role") AS "by_role",
  (SELECT "s" FROM "strings" WHERE "id" = "convoy_promises_submissions"."by_seat") AS "by_seat"
FROM "convoy_promises_submissions";
CREATE VIEW "convoy_quota_view" AS SELECT
  "convoy_quota"."convoy",
  (SELECT "s" FROM "strings" WHERE "id" = "convoy_quota"."key") AS "key",
  "convoy_quota"."value"
FROM "convoy_quota";
CREATE VIEW "convoy_retired_view" AS SELECT
  "convoy_retired"."convoy",
  "convoy_retired"."value"
FROM "convoy_retired";
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
  "message" INTEGER NOT NULL,
  "pos" INTEGER NOT NULL,
  "address" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "revision" TEXT NOT NULL,
  "subject" INTEGER NOT NULL CHECK ("subject" IN (0, 1)),
  PRIMARY KEY ("message", "pos"),
  FOREIGN KEY ("message") REFERENCES "message" ("id") ON DELETE CASCADE
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
  "message_references"."message",
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
