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
CREATE TABLE "dossier" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "workflow" BLOB NOT NULL REFERENCES "objects" ("hash"),
  "placement" BLOB REFERENCES "objects" ("hash"),
  "instruction" TEXT,
  "state" TEXT NOT NULL CHECK ("state" IN ('c1', 'c2', 'c3')),
  "state_since" INTEGER NOT NULL
) STRICT;
CREATE TABLE "dossier_inputs" (
  "dossier" INTEGER NOT NULL,
  "key" TEXT NOT NULL,
  "tag" TEXT NOT NULL CHECK ("tag" IN ('c1', 'c2', 'c3', 'c4')),
  "value" TEXT,
  "number" INTEGER,
  PRIMARY KEY ("dossier", "key"),
  FOREIGN KEY ("dossier") REFERENCES "dossier" ("id") ON DELETE CASCADE,
  CHECK (CASE WHEN "tag" = 'c1'
      THEN "value" IS NOT NULL
      AND "number" IS NULL
      END),
  CHECK (CASE WHEN "tag" = 'c2'
      THEN "number" IS NOT NULL
      AND "value" IS NULL
      END),
  CHECK (CASE WHEN "tag" = 'c3'
      THEN "value" IS NULL
      AND "number" IS NULL
      END),
  CHECK (CASE WHEN "tag" = 'c4'
      THEN "value" IS NULL
      AND "number" IS NULL
      END)
) STRICT, WITHOUT ROWID;
CREATE TABLE "dossier_inputs_items" (
  "dossier" INTEGER NOT NULL,
  "inputs_key" TEXT NOT NULL,
  "pos" INTEGER NOT NULL,
  "value" TEXT NOT NULL,
  PRIMARY KEY ("dossier", "inputs_key", "pos"),
  FOREIGN KEY ("dossier", "inputs_key") REFERENCES "dossier_inputs" ("dossier", "key") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE TABLE "dossier_deliverables" (
  "dossier" INTEGER NOT NULL,
  "pos" INTEGER NOT NULL,
  "version" INTEGER NOT NULL,
  "label" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "kind" TEXT NOT NULL CHECK ("kind" IN ('c1', 'c2')),
  "source" TEXT NOT NULL CHECK ("source" IN ('c1', 'c2')),
  "source_step" INTEGER REFERENCES "strings" ("id"),
  "state" TEXT NOT NULL CHECK ("state" IN ('c1', 'c2')),
  "state_since" INTEGER NOT NULL,
  PRIMARY KEY ("dossier", "pos"),
  FOREIGN KEY ("dossier") REFERENCES "dossier" ("id") ON DELETE CASCADE,
  CHECK (CASE WHEN "source" = 'c1'
      THEN "source_step" IS NOT NULL
      END),
  CHECK (CASE WHEN "source" = 'c2'
      THEN "source_step" IS NULL
      END)
) STRICT, WITHOUT ROWID;
CREATE TABLE "dossier_deliverables_evidence" (
  "dossier" INTEGER NOT NULL,
  "deliverables_pos" INTEGER NOT NULL,
  "pos" INTEGER NOT NULL,
  "version" INTEGER NOT NULL,
  "reference" TEXT NOT NULL,
  "submitted_at" INTEGER NOT NULL,
  "verdict_accepted" INTEGER CHECK ("verdict_accepted" IN (0, 1)),
  "verdict_by" INTEGER REFERENCES "strings" ("id"),
  "verdict_at" INTEGER,
  PRIMARY KEY ("dossier", "deliverables_pos", "pos"),
  FOREIGN KEY ("dossier", "deliverables_pos") REFERENCES "dossier_deliverables" ("dossier", "pos") ON DELETE CASCADE,
  CHECK (CASE WHEN "verdict_accepted" IS NOT NULL
      THEN "verdict_by" IS NOT NULL
      AND "verdict_at" IS NOT NULL
      ELSE "verdict_by" IS NULL
      AND "verdict_at" IS NULL
      END)
) STRICT, WITHOUT ROWID;
CREATE TABLE "dossier_deliverables_evidence_metadata" (
  "dossier" INTEGER NOT NULL,
  "deliverables_pos" INTEGER NOT NULL,
  "evidence_pos" INTEGER NOT NULL,
  "key" TEXT NOT NULL,
  "value" TEXT NOT NULL,
  PRIMARY KEY ("dossier", "deliverables_pos", "evidence_pos", "key"),
  FOREIGN KEY ("dossier", "deliverables_pos", "evidence_pos") REFERENCES "dossier_deliverables_evidence" ("dossier", "deliverables_pos", "pos") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
INSERT INTO "codes" ("kind", "field", "code", "name") VALUES
  ('do', 'state', 'c1', 'Open'),
  ('do', 'state', 'c2', 'Delivered'),
  ('do', 'state', 'c3', 'Abandoned'),
  ('do', 'inputs.tag', 'c1', 'Text'),
  ('do', 'inputs.tag', 'c2', 'Number'),
  ('do', 'inputs.tag', 'c3', 'List'),
  ('do', 'inputs.tag', 'c4', 'Unset'),
  ('do', 'deliverables.kind', 'c1', 'ChangeRequest'),
  ('do', 'deliverables.kind', 'c2', 'Artifact'),
  ('do', 'deliverables.source', 'c1', 'Workflow'),
  ('do', 'deliverables.source', 'c2', 'Dispatcher'),
  ('do', 'deliverables.state', 'c1', 'Open'),
  ('do', 'deliverables.state', 'c2', 'Retracted'),
  ('do', 'log:edge', 'c1', 'Deliver'),
  ('do', 'log:edge', 'c2', 'Abandon'),
  ('do', 'log:edge', 'c3', 'Promise'),
  ('do', 'log:edge', 'c4', 'Submit'),
  ('do', 'log:edge', 'c5', 'Retract'),
  ('do', 'log:label', 'c1', 'Open'),
  ('do', 'log:label', 'c2', 'Delivered'),
  ('do', 'log:label', 'c3', 'Abandoned');
CREATE VIEW "dossier_view" AS SELECT
  "dossier"."id",
  "dossier"."version",
  "dossier"."workflow",
  "dossier"."placement",
  "dossier"."instruction",
  (SELECT "name" FROM "codes" WHERE "kind" = 'do' AND "field" = 'state' AND "code" = "dossier"."state") AS "state",
  "dossier"."state_since"
FROM "dossier";
CREATE VIEW "dossier_inputs_view" AS SELECT
  "dossier_inputs"."dossier",
  "dossier_inputs"."key",
  (SELECT "name" FROM "codes" WHERE "kind" = 'do' AND "field" = 'inputs.tag' AND "code" = "dossier_inputs"."tag") AS "tag",
  "dossier_inputs"."value",
  "dossier_inputs"."number"
FROM "dossier_inputs";
CREATE VIEW "dossier_inputs_items_view" AS SELECT
  "dossier_inputs_items"."dossier",
  "dossier_inputs_items"."inputs_key",
  "dossier_inputs_items"."pos",
  "dossier_inputs_items"."value"
FROM "dossier_inputs_items";
CREATE VIEW "dossier_deliverables_view" AS SELECT
  "dossier_deliverables"."dossier",
  "dossier_deliverables"."pos",
  "dossier_deliverables"."version",
  (SELECT "s" FROM "strings" WHERE "id" = "dossier_deliverables"."label") AS "label",
  (SELECT "name" FROM "codes" WHERE "kind" = 'do' AND "field" = 'deliverables.kind' AND "code" = "dossier_deliverables"."kind") AS "kind",
  (SELECT "name" FROM "codes" WHERE "kind" = 'do' AND "field" = 'deliverables.source' AND "code" = "dossier_deliverables"."source") AS "source",
  (SELECT "s" FROM "strings" WHERE "id" = "dossier_deliverables"."source_step") AS "source_step",
  (SELECT "name" FROM "codes" WHERE "kind" = 'do' AND "field" = 'deliverables.state' AND "code" = "dossier_deliverables"."state") AS "state",
  "dossier_deliverables"."state_since"
FROM "dossier_deliverables";
CREATE VIEW "dossier_deliverables_evidence_view" AS SELECT
  "dossier_deliverables_evidence"."dossier",
  "dossier_deliverables_evidence"."deliverables_pos",
  "dossier_deliverables_evidence"."pos",
  "dossier_deliverables_evidence"."version",
  "dossier_deliverables_evidence"."reference",
  "dossier_deliverables_evidence"."submitted_at",
  "dossier_deliverables_evidence"."verdict_accepted",
  (SELECT "s" FROM "strings" WHERE "id" = "dossier_deliverables_evidence"."verdict_by") AS "verdict_by",
  "dossier_deliverables_evidence"."verdict_at"
FROM "dossier_deliverables_evidence";
CREATE VIEW "dossier_deliverables_evidence_metadata_view" AS SELECT
  "dossier_deliverables_evidence_metadata"."dossier",
  "dossier_deliverables_evidence_metadata"."deliverables_pos",
  "dossier_deliverables_evidence_metadata"."evidence_pos",
  "dossier_deliverables_evidence_metadata"."key",
  "dossier_deliverables_evidence_metadata"."value"
FROM "dossier_deliverables_evidence_metadata";
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
  "receiver_kind" TEXT CHECK ("receiver_kind" IN ('as', 'co', 'pe')),
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
  ('ms', 'receiver_kind', 'pe', 'Person'),
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
CREATE TABLE "person" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "state" TEXT NOT NULL CHECK ("state" IN ('ac')),
  "state_since" INTEGER NOT NULL
) STRICT;
INSERT INTO "codes" ("kind", "field", "code", "name") VALUES
  ('pn', 'state', 'ac', 'Active'),
  ('pn', 'log:edge', 'rv', 'Receive'),
  ('pn', 'log:label', 'ac', 'Active');
CREATE VIEW "person_view" AS SELECT
  "person"."id",
  "person"."version",
  (SELECT "name" FROM "codes" WHERE "kind" = 'pn' AND "field" = 'state' AND "code" = "person"."state") AS "state",
  "person"."state_since"
FROM "person";
CREATE TABLE "referral" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "sender" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "session" INTEGER NOT NULL REFERENCES "resource_identity" ("id"),
  "in_reply_to" INTEGER REFERENCES "resource_identity" ("id"),
  "subject" TEXT CHECK ("subject" IN ('c1', 'c2', 'c3')),
  "subject_name" TEXT,
  "subject_service" INTEGER REFERENCES "strings" ("id"),
  "subject_number" INTEGER,
  "subject_provider" TEXT,
  "subject_account" TEXT,
  "condition_address" TEXT CHECK ("condition_address" IN ('c1', 'c2', 'c3')),
  "condition_address_name" TEXT,
  "condition_address_service" INTEGER REFERENCES "strings" ("id"),
  "condition_address_number" INTEGER,
  "condition_address_provider" TEXT,
  "condition_address_account" TEXT,
  "condition_operator" TEXT CHECK ("condition_operator" IN ('c1', 'c2', 'c3')),
  "condition_literal" TEXT,
  "sent_at" INTEGER,
  "seq" INTEGER NOT NULL,
  "ordinal" INTEGER NOT NULL,
  "phase" TEXT NOT NULL CHECK ("phase" IN ('c1', 'c2', 'c3')),
  "phase_since" INTEGER NOT NULL,
  CHECK (CASE WHEN "subject" = 'c1'
      THEN "subject_name" IS NOT NULL
      AND "subject_service" IS NULL
      AND "subject_number" IS NULL
      AND "subject_provider" IS NULL
      AND "subject_account" IS NULL
      END),
  CHECK (CASE WHEN "subject" = 'c2'
      THEN "subject_service" IS NOT NULL
      AND "subject_number" IS NOT NULL
      AND "subject_name" IS NULL
      AND "subject_provider" IS NULL
      AND "subject_account" IS NULL
      END),
  CHECK (CASE WHEN "subject" = 'c3'
      THEN "subject_provider" IS NOT NULL
      AND "subject_account" IS NOT NULL
      AND "subject_name" IS NULL
      AND "subject_service" IS NULL
      AND "subject_number" IS NULL
      END),
  CHECK (CASE WHEN "subject" IS NOT NULL
      THEN 1
      ELSE "subject_name" IS NULL
      AND "subject_service" IS NULL
      AND "subject_number" IS NULL
      AND "subject_provider" IS NULL
      AND "subject_account" IS NULL
      END),
  CHECK (CASE WHEN "condition_address" = 'c1'
      THEN "condition_address_name" IS NOT NULL
      AND "condition_address_service" IS NULL
      AND "condition_address_number" IS NULL
      AND "condition_address_provider" IS NULL
      AND "condition_address_account" IS NULL
      END),
  CHECK (CASE WHEN "condition_address" = 'c2'
      THEN "condition_address_service" IS NOT NULL
      AND "condition_address_number" IS NOT NULL
      AND "condition_address_name" IS NULL
      AND "condition_address_provider" IS NULL
      AND "condition_address_account" IS NULL
      END),
  CHECK (CASE WHEN "condition_address" = 'c3'
      THEN "condition_address_provider" IS NOT NULL
      AND "condition_address_account" IS NOT NULL
      AND "condition_address_name" IS NULL
      AND "condition_address_service" IS NULL
      AND "condition_address_number" IS NULL
      END),
  CHECK (CASE WHEN "condition_address" IS NOT NULL
      THEN "condition_operator" IS NOT NULL
      AND "condition_literal" IS NOT NULL
      ELSE "condition_address_name" IS NULL
      AND "condition_address_service" IS NULL
      AND "condition_address_number" IS NULL
      AND "condition_address_provider" IS NULL
      AND "condition_address_account" IS NULL
      AND "condition_operator" IS NULL
      AND "condition_literal" IS NULL
      END)
) STRICT;
CREATE TABLE "referral_references" (
  "referral" INTEGER NOT NULL,
  "pos" INTEGER NOT NULL,
  "tag" TEXT NOT NULL CHECK ("tag" IN ('c1', 'c2', 'c3', 'c4', 'c5', 'c6', 'c7')),
  "service" INTEGER REFERENCES "strings" ("id"),
  "scope" TEXT,
  "number" INTEGER,
  "revision" TEXT,
  "repository" INTEGER REFERENCES "resource_identity" ("id"),
  "name" TEXT,
  "resource" INTEGER REFERENCES "resource_identity" ("id"),
  "id" TEXT,
  PRIMARY KEY ("referral", "pos"),
  FOREIGN KEY ("referral") REFERENCES "referral" ("id") ON DELETE CASCADE,
  CHECK (CASE WHEN "tag" = 'c1'
      THEN "service" IS NOT NULL
      AND "scope" IS NOT NULL
      AND "number" IS NOT NULL
      AND "revision" IS NOT NULL
      AND "repository" IS NULL
      AND "name" IS NULL
      AND "resource" IS NULL
      AND "id" IS NULL
      END),
  CHECK (CASE WHEN "tag" = 'c2'
      THEN "revision" IS NOT NULL
      AND "repository" IS NOT NULL
      AND "service" IS NULL
      AND "scope" IS NULL
      AND "number" IS NULL
      AND "name" IS NULL
      AND "resource" IS NULL
      AND "id" IS NULL
      END),
  CHECK (CASE WHEN "tag" = 'c3'
      THEN "revision" IS NOT NULL
      AND "repository" IS NOT NULL
      AND "name" IS NOT NULL
      AND "service" IS NULL
      AND "scope" IS NULL
      AND "number" IS NULL
      AND "resource" IS NULL
      AND "id" IS NULL
      END),
  CHECK (CASE WHEN "tag" = 'c4'
      THEN "revision" IS NOT NULL
      AND "resource" IS NOT NULL
      AND "service" IS NULL
      AND "scope" IS NULL
      AND "number" IS NULL
      AND "repository" IS NULL
      AND "name" IS NULL
      AND "id" IS NULL
      END),
  CHECK (CASE WHEN "tag" = 'c5'
      THEN "service" IS NOT NULL
      AND "scope" IS NOT NULL
      AND "number" IS NOT NULL
      AND "revision" IS NOT NULL
      AND "repository" IS NULL
      AND "name" IS NULL
      AND "resource" IS NULL
      AND "id" IS NULL
      END),
  CHECK (CASE WHEN "tag" = 'c6'
      THEN "service" IS NOT NULL
      AND "scope" IS NOT NULL
      AND "revision" IS NOT NULL
      AND "id" IS NOT NULL
      AND "number" IS NULL
      AND "repository" IS NULL
      AND "name" IS NULL
      AND "resource" IS NULL
      END),
  CHECK (CASE WHEN "tag" = 'c7'
      THEN "revision" IS NOT NULL
      AND "resource" IS NOT NULL
      AND "service" IS NULL
      AND "scope" IS NULL
      AND "number" IS NULL
      AND "repository" IS NULL
      AND "name" IS NULL
      AND "id" IS NULL
      END)
) STRICT, WITHOUT ROWID;
CREATE INDEX "referral_by_reply" ON "referral" ("in_reply_to") WHERE "in_reply_to" IS NOT NULL;
CREATE INDEX "referral_by_session" ON "referral" ("session", "seq");
CREATE INDEX "referral_references_repository_ref" ON "referral_references" ("repository") WHERE "repository" IS NOT NULL;
CREATE INDEX "referral_references_resource_ref" ON "referral_references" ("resource") WHERE "resource" IS NOT NULL;
INSERT INTO "codes" ("kind", "field", "code", "name") VALUES
  ('rf', 'subject', 'c1', 'Convoy'),
  ('rf', 'subject', 'c2', 'ChangeRequest'),
  ('rf', 'subject', 'c3', 'Usage'),
  ('rf', 'condition_address', 'c1', 'Convoy'),
  ('rf', 'condition_address', 'c2', 'ChangeRequest'),
  ('rf', 'condition_address', 'c3', 'Usage'),
  ('rf', 'condition_operator', 'c1', 'Equal'),
  ('rf', 'condition_operator', 'c2', 'LessThan'),
  ('rf', 'condition_operator', 'c3', 'GreaterThan'),
  ('rf', 'phase', 'c1', 'Draft'),
  ('rf', 'phase', 'c2', 'Sent'),
  ('rf', 'phase', 'c3', 'Read'),
  ('rf', 'references.tag', 'c1', 'ChangeRequest'),
  ('rf', 'references.tag', 'c2', 'Commit'),
  ('rf', 'references.tag', 'c3', 'Ref'),
  ('rf', 'references.tag', 'c4', 'Artifact'),
  ('rf', 'references.tag', 'c5', 'Issue'),
  ('rf', 'references.tag', 'c6', 'Comment'),
  ('rf', 'references.tag', 'c7', 'ControlRecord'),
  ('rf', 'log:edge', 'c1', 'Send'),
  ('rf', 'log:edge', 'c2', 'Read'),
  ('rf', 'log:edge', 'c3', 'Amend'),
  ('rf', 'log:label', 'c1', 'Draft'),
  ('rf', 'log:label', 'c2', 'Sent'),
  ('rf', 'log:label', 'c3', 'Read');
CREATE VIEW "referral_view" AS SELECT
  "referral"."id",
  "referral"."version",
  (SELECT "s" FROM "strings" WHERE "id" = "referral"."sender") AS "sender",
  "referral"."session",
  "referral"."in_reply_to",
  (SELECT "name" FROM "codes" WHERE "kind" = 'rf' AND "field" = 'subject' AND "code" = "referral"."subject") AS "subject",
  "referral"."subject_name",
  (SELECT "s" FROM "strings" WHERE "id" = "referral"."subject_service") AS "subject_service",
  "referral"."subject_number",
  "referral"."subject_provider",
  "referral"."subject_account",
  (SELECT "name" FROM "codes" WHERE "kind" = 'rf' AND "field" = 'condition_address' AND "code" = "referral"."condition_address") AS "condition_address",
  "referral"."condition_address_name",
  (SELECT "s" FROM "strings" WHERE "id" = "referral"."condition_address_service") AS "condition_address_service",
  "referral"."condition_address_number",
  "referral"."condition_address_provider",
  "referral"."condition_address_account",
  (SELECT "name" FROM "codes" WHERE "kind" = 'rf' AND "field" = 'condition_operator' AND "code" = "referral"."condition_operator") AS "condition_operator",
  "referral"."condition_literal",
  "referral"."sent_at",
  "referral"."seq",
  "referral"."ordinal",
  (SELECT "name" FROM "codes" WHERE "kind" = 'rf' AND "field" = 'phase' AND "code" = "referral"."phase") AS "phase",
  "referral"."phase_since"
FROM "referral";
CREATE VIEW "referral_references_view" AS SELECT
  "referral_references"."referral",
  "referral_references"."pos",
  (SELECT "name" FROM "codes" WHERE "kind" = 'rf' AND "field" = 'references.tag' AND "code" = "referral_references"."tag") AS "tag",
  (SELECT "s" FROM "strings" WHERE "id" = "referral_references"."service") AS "service",
  "referral_references"."scope",
  "referral_references"."number",
  "referral_references"."revision",
  "referral_references"."repository",
  "referral_references"."name",
  "referral_references"."resource",
  "referral_references"."id"
FROM "referral_references";
CREATE TABLE "ticket" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "queue" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "title" TEXT NOT NULL,
  "state" TEXT NOT NULL CHECK ("state" IN ('c1', 'c2', 'c3')),
  "state_since" INTEGER NOT NULL,
  "priority" INTEGER,
  "owner" INTEGER REFERENCES "strings" ("id"),
  "hold_by" TEXT,
  "hold_until" INTEGER,
  "reason" TEXT,
  "closed_by" INTEGER REFERENCES "strings" ("id"),
  "signature_who" INTEGER REFERENCES "strings" ("id"),
  "signature_at" INTEGER,
  CHECK (CASE WHEN "hold_by" IS NOT NULL
      THEN "hold_until" IS NOT NULL
      ELSE "hold_until" IS NULL
      END),
  CHECK (CASE WHEN "state" = 'c1'
      THEN "priority" IS NULL
      AND "owner" IS NULL
      AND "hold_by" IS NULL
      AND "hold_until" IS NULL
      AND "reason" IS NULL
      AND "closed_by" IS NULL
      AND "signature_who" IS NULL
      AND "signature_at" IS NULL
      END),
  CHECK (CASE WHEN "state" = 'c2'
      THEN "priority" IS NOT NULL
      AND "reason" IS NULL
      AND "closed_by" IS NULL
      AND "signature_who" IS NULL
      AND "signature_at" IS NULL
      END),
  CHECK (CASE WHEN "state" = 'c3'
      THEN "priority" IS NOT NULL
      AND "reason" IS NOT NULL
      AND "closed_by" IS NOT NULL
      AND "signature_who" IS NOT NULL
      AND "signature_at" IS NOT NULL
      AND "owner" IS NULL
      AND "hold_by" IS NULL
      AND "hold_until" IS NULL
      END)
) STRICT;
CREATE TABLE "ticket_assignees" (
  "ticket" INTEGER NOT NULL,
  "value" TEXT NOT NULL,
  PRIMARY KEY ("ticket", "value"),
  FOREIGN KEY ("ticket") REFERENCES "ticket" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE TABLE "ticket_notes" (
  "ticket" INTEGER NOT NULL,
  "key" TEXT NOT NULL,
  "value" TEXT NOT NULL,
  PRIMARY KEY ("ticket", "key"),
  FOREIGN KEY ("ticket") REFERENCES "ticket" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE INDEX "ticket_open_by_queue" ON "ticket" ("queue") WHERE "state" IN ('c2');
INSERT INTO "codes" ("kind", "field", "code", "name") VALUES
  ('tk', 'state', 'c1', 'Draft'),
  ('tk', 'state', 'c2', 'Open'),
  ('tk', 'state', 'c3', 'Closed'),
  ('tk', 'log:edge', 'c1', 'Edit'),
  ('tk', 'log:edge', 'c2', 'Move'),
  ('tk', 'log:label', 'c1', 'Draft'),
  ('tk', 'log:label', 'c2', 'Open'),
  ('tk', 'log:label', 'c3', 'Closed');
CREATE VIEW "ticket_view" AS SELECT
  "ticket"."id",
  "ticket"."version",
  (SELECT "s" FROM "strings" WHERE "id" = "ticket"."queue") AS "queue",
  "ticket"."title",
  (SELECT "name" FROM "codes" WHERE "kind" = 'tk' AND "field" = 'state' AND "code" = "ticket"."state") AS "state",
  "ticket"."state_since",
  "ticket"."priority",
  (SELECT "s" FROM "strings" WHERE "id" = "ticket"."owner") AS "owner",
  "ticket"."hold_by",
  "ticket"."hold_until",
  "ticket"."reason",
  (SELECT "s" FROM "strings" WHERE "id" = "ticket"."closed_by") AS "closed_by",
  (SELECT "s" FROM "strings" WHERE "id" = "ticket"."signature_who") AS "signature_who",
  "ticket"."signature_at"
FROM "ticket";
CREATE VIEW "ticket_assignees_view" AS SELECT
  "ticket_assignees"."ticket",
  "ticket_assignees"."value"
FROM "ticket_assignees";
CREATE VIEW "ticket_notes_view" AS SELECT
  "ticket_notes"."ticket",
  "ticket_notes"."key",
  "ticket_notes"."value"
FROM "ticket_notes";
CREATE TABLE "terminal_session" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "vessel" INTEGER NOT NULL REFERENCES "resource_identity" ("id"),
  "role" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "ordinal" INTEGER NOT NULL,
  "pool" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "cwd" TEXT NOT NULL,
  "source" TEXT NOT NULL CHECK ("source" IN ('c1', 'c2')),
  "source_command" TEXT,
  "source_selector" INTEGER REFERENCES "strings" ("id"),
  "source_brief" BLOB REFERENCES "objects" ("hash"),
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
  CHECK (CASE WHEN "source" = 'c1'
      THEN "source_command" IS NOT NULL
      AND "source_selector" IS NULL
      AND "source_brief" IS NULL
      END),
  CHECK (CASE WHEN "source" = 'c2'
      THEN "source_selector" IS NOT NULL
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
CREATE TABLE "terminal_session_labels" (
  "terminal_session" INTEGER NOT NULL,
  "value" TEXT NOT NULL,
  PRIMARY KEY ("terminal_session", "value"),
  FOREIGN KEY ("terminal_session") REFERENCES "terminal_session" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE INDEX "terminal_session_by_vessel" ON "terminal_session" ("vessel", "ordinal");
CREATE INDEX "terminal_session_live_by_pool" ON "terminal_session" ("pool") WHERE "state" IN ('rq', 'rl');
INSERT INTO "codes" ("kind", "field", "code", "name") VALUES
  ('ts', 'source', 'c1', 'Tool'),
  ('ts', 'source', 'c2', 'Agent'),
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
  "terminal_session"."vessel",
  (SELECT "s" FROM "strings" WHERE "id" = "terminal_session"."role") AS "role",
  "terminal_session"."ordinal",
  (SELECT "s" FROM "strings" WHERE "id" = "terminal_session"."pool") AS "pool",
  "terminal_session"."cwd",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ts' AND "field" = 'source' AND "code" = "terminal_session"."source") AS "source",
  "terminal_session"."source_command",
  (SELECT "s" FROM "strings" WHERE "id" = "terminal_session"."source_selector") AS "source_selector",
  "terminal_session"."source_brief",
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
CREATE VIEW "terminal_session_labels_view" AS SELECT
  "terminal_session_labels"."terminal_session",
  "terminal_session_labels"."value"
FROM "terminal_session_labels";
CREATE TABLE "vessel" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
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
  ('ve', 'ending', 'fa', 'Failed'),
  ('ve', 'ending', 'lo', 'Lost'),
  ('ve', 'ending', 'rl', 'Released'),
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
  (SELECT "s" FROM "strings" WHERE "id" = "l"."writer") AS "writer",
  "l"."at",
  "l"."payload",
  "l"."effect",
  "l"."topic"
FROM "log" AS "l" LEFT JOIN "resource_identity" AS "i" ON "i"."id" = "l"."resource";
CREATE VIEW stream_watermark_view AS SELECT (SELECT s FROM strings WHERE id = home) AS home, home_seq, applied_through FROM stream_watermark;
CREATE VIEW stream_history_view AS SELECT (SELECT s FROM strings WHERE id = home) AS home, home_seq FROM stream_history;
