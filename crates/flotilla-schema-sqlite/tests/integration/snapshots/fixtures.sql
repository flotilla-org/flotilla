CREATE TABLE strings (id INTEGER PRIMARY KEY, s TEXT NOT NULL UNIQUE) STRICT;
CREATE TABLE resource_identity (
  id INTEGER PRIMARY KEY, kind TEXT NOT NULL,
  name INTEGER NOT NULL REFERENCES strings(id), home INTEGER NOT NULL REFERENCES strings(id),
  seq INTEGER NOT NULL DEFAULT 0,
  UNIQUE (kind, name, home)) STRICT;
CREATE INDEX resource_identity_by_home ON resource_identity (home, kind);
CREATE TABLE objects (hash BLOB PRIMARY KEY, bytes BLOB NOT NULL) STRICT;
CREATE TABLE home_sequence (home INTEGER PRIMARY KEY REFERENCES strings(id), last INTEGER NOT NULL) STRICT;
CREATE TABLE change_log (
  home INTEGER NOT NULL REFERENCES strings(id), home_seq INTEGER NOT NULL,
  resource_id INTEGER NOT NULL REFERENCES resource_identity(id), change BLOB NOT NULL,
  PRIMARY KEY (home, home_seq)) STRICT;
CREATE INDEX change_log_by_resource ON change_log (resource_id, home_seq);
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
CREATE TABLE "agent_session_edges" (
  "owner" INTEGER NOT NULL,
  "pos" INTEGER NOT NULL,
  "version" INTEGER NOT NULL,
  "edge" TEXT NOT NULL CHECK ("edge" IN ('at', 'de', 'en', 'rv')),
  "from" TEXT NOT NULL CHECK ("from" IN ('en', 'li')),
  "to" TEXT NOT NULL CHECK ("to" IN ('en', 'li')),
  "writer" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "at" INTEGER NOT NULL,
  "payload" BLOB NOT NULL,
  PRIMARY KEY ("owner", "pos"),
  FOREIGN KEY ("owner") REFERENCES "agent_session" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE INDEX "agent_session_edges_by_edge" ON "agent_session_edges" ("edge", "at");
CREATE INDEX "agent_session_edges_by_time" ON "agent_session_edges" ("at");
INSERT INTO "codes" ("kind", "field", "code", "name") VALUES
  ('as', 'state', 'en', 'Ended'),
  ('as', 'state', 'li', 'Live'),
  ('as', 'channel', 'at', 'Attached'),
  ('as', 'channel', 'de', 'Detached'),
  ('as', 'reason', 'co', 'Completed'),
  ('as', 'reason', 'lo', 'Lost'),
  ('as', 'edges.edge', 'at', 'Attach'),
  ('as', 'edges.edge', 'de', 'Detach'),
  ('as', 'edges.edge', 'en', 'End'),
  ('as', 'edges.edge', 'rv', 'Receive'),
  ('as', 'edges.from', 'en', 'Ended'),
  ('as', 'edges.from', 'li', 'Live'),
  ('as', 'edges.to', 'en', 'Ended'),
  ('as', 'edges.to', 'li', 'Live');
CREATE VIEW "agent_session_view" AS SELECT
  "agent_session"."id",
  "agent_session"."version",
  (SELECT "name" FROM "codes" WHERE "kind" = 'as' AND "field" = 'state' AND "code" = "agent_session"."state") AS "state",
  "agent_session"."state_since",
  (SELECT "name" FROM "codes" WHERE "kind" = 'as' AND "field" = 'channel' AND "code" = "agent_session"."channel") AS "channel",
  (SELECT "name" FROM "codes" WHERE "kind" = 'as' AND "field" = 'reason' AND "code" = "agent_session"."reason") AS "reason"
FROM "agent_session";
CREATE VIEW "agent_session_edges_view" AS SELECT
  "agent_session_edges"."owner",
  "agent_session_edges"."pos",
  "agent_session_edges"."version",
  (SELECT "name" FROM "codes" WHERE "kind" = 'as' AND "field" = 'edges.edge' AND "code" = "agent_session_edges"."edge") AS "edge",
  (SELECT "name" FROM "codes" WHERE "kind" = 'as' AND "field" = 'edges.from' AND "code" = "agent_session_edges"."from") AS "from",
  (SELECT "name" FROM "codes" WHERE "kind" = 'as' AND "field" = 'edges.to' AND "code" = "agent_session_edges"."to") AS "to",
  (SELECT "s" FROM "strings" WHERE "id" = "agent_session_edges"."writer") AS "writer",
  "agent_session_edges"."at",
  "agent_session_edges"."payload"
FROM "agent_session_edges";
CREATE TABLE "convoy" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "project" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "priority" INTEGER NOT NULL,
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
CREATE TABLE "convoy_promises" (
  "owner" INTEGER NOT NULL,
  "key" INTEGER NOT NULL,
  "kind" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "target" INTEGER NOT NULL REFERENCES "strings" ("id"),
  PRIMARY KEY ("owner", "key"),
  FOREIGN KEY ("owner") REFERENCES "convoy" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE TABLE "convoy_resolution" (
  "owner" INTEGER NOT NULL,
  "key" INTEGER NOT NULL,
  "value" TEXT NOT NULL CHECK ("value" IN ('ke', 'pe', 'rt')),
  PRIMARY KEY ("owner", "key"),
  FOREIGN KEY ("owner") REFERENCES "convoy" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE TABLE "convoy_quota" (
  "owner" INTEGER NOT NULL,
  "key" INTEGER NOT NULL,
  "value" INTEGER NOT NULL,
  PRIMARY KEY ("owner", "key"),
  FOREIGN KEY ("owner") REFERENCES "convoy" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE TABLE "convoy_edges" (
  "owner" INTEGER NOT NULL,
  "pos" INTEGER NOT NULL,
  "version" INTEGER NOT NULL,
  "edge" TEXT NOT NULL CHECK ("edge" IN ('ab', 'de', 'ke', 'ra', 'rt')),
  "from" TEXT NOT NULL CHECK ("from" IN ('ab', 'de', 'op')),
  "to" TEXT NOT NULL CHECK ("to" IN ('ab', 'de', 'op')),
  "writer" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "at" INTEGER NOT NULL,
  "payload" BLOB NOT NULL,
  PRIMARY KEY ("owner", "pos"),
  FOREIGN KEY ("owner") REFERENCES "convoy" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE INDEX "convoy_edges_by_edge" ON "convoy_edges" ("edge", "at");
CREATE INDEX "convoy_edges_by_time" ON "convoy_edges" ("at");
CREATE INDEX "convoy_open_by_project" ON "convoy" ("project") WHERE "state" IN ('op');
INSERT INTO "codes" ("kind", "field", "code", "name") VALUES
  ('cv', 'state', 'ab', 'Abandoned'),
  ('cv', 'state', 'de', 'Delivered'),
  ('cv', 'state', 'op', 'Open'),
  ('cv', 'resolution.value', 'ke', 'Kept'),
  ('cv', 'resolution.value', 'pe', 'Pending'),
  ('cv', 'resolution.value', 'rt', 'Retracted'),
  ('cv', 'edges.edge', 'ab', 'Abandon'),
  ('cv', 'edges.edge', 'de', 'Deliver'),
  ('cv', 'edges.edge', 'ke', 'Keep'),
  ('cv', 'edges.edge', 'ra', 'Readmit'),
  ('cv', 'edges.edge', 'rt', 'Retract'),
  ('cv', 'edges.from', 'ab', 'Abandoned'),
  ('cv', 'edges.from', 'de', 'Delivered'),
  ('cv', 'edges.from', 'op', 'Open'),
  ('cv', 'edges.to', 'ab', 'Abandoned'),
  ('cv', 'edges.to', 'de', 'Delivered'),
  ('cv', 'edges.to', 'op', 'Open');
CREATE VIEW "convoy_view" AS SELECT
  "convoy"."id",
  "convoy"."version",
  (SELECT "s" FROM "strings" WHERE "id" = "convoy"."project") AS "project",
  "convoy"."priority",
  (SELECT "name" FROM "codes" WHERE "kind" = 'cv' AND "field" = 'state' AND "code" = "convoy"."state") AS "state",
  "convoy"."state_since",
  "convoy"."reason"
FROM "convoy";
CREATE VIEW "convoy_promises_view" AS SELECT
  "convoy_promises"."owner",
  "convoy_promises"."key",
  (SELECT "s" FROM "strings" WHERE "id" = "convoy_promises"."kind") AS "kind",
  (SELECT "s" FROM "strings" WHERE "id" = "convoy_promises"."target") AS "target"
FROM "convoy_promises";
CREATE VIEW "convoy_resolution_view" AS SELECT
  "convoy_resolution"."owner",
  "convoy_resolution"."key",
  (SELECT "name" FROM "codes" WHERE "kind" = 'cv' AND "field" = 'resolution.value' AND "code" = "convoy_resolution"."value") AS "value"
FROM "convoy_resolution";
CREATE VIEW "convoy_quota_view" AS SELECT
  "convoy_quota"."owner",
  "convoy_quota"."key",
  "convoy_quota"."value"
FROM "convoy_quota";
CREATE VIEW "convoy_edges_view" AS SELECT
  "convoy_edges"."owner",
  "convoy_edges"."pos",
  "convoy_edges"."version",
  (SELECT "name" FROM "codes" WHERE "kind" = 'cv' AND "field" = 'edges.edge' AND "code" = "convoy_edges"."edge") AS "edge",
  (SELECT "name" FROM "codes" WHERE "kind" = 'cv' AND "field" = 'edges.from' AND "code" = "convoy_edges"."from") AS "from",
  (SELECT "name" FROM "codes" WHERE "kind" = 'cv' AND "field" = 'edges.to' AND "code" = "convoy_edges"."to") AS "to",
  (SELECT "s" FROM "strings" WHERE "id" = "convoy_edges"."writer") AS "writer",
  "convoy_edges"."at",
  "convoy_edges"."payload"
FROM "convoy_edges";
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
  "owner" INTEGER NOT NULL,
  "key" TEXT NOT NULL,
  "tag" TEXT NOT NULL CHECK ("tag" IN ('c1', 'c2', 'c3', 'c4')),
  "value" TEXT,
  "number" INTEGER,
  PRIMARY KEY ("owner", "key"),
  FOREIGN KEY ("owner") REFERENCES "dossier" ("id") ON DELETE CASCADE,
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
  "owner" INTEGER NOT NULL,
  "inputs_key" TEXT NOT NULL,
  "pos" INTEGER NOT NULL,
  "value" TEXT NOT NULL,
  PRIMARY KEY ("owner", "inputs_key", "pos"),
  FOREIGN KEY ("owner", "inputs_key") REFERENCES "dossier_inputs" ("owner", "key") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE TABLE "dossier_deliverables" (
  "owner" INTEGER NOT NULL,
  "pos" INTEGER NOT NULL,
  "version" INTEGER NOT NULL,
  "label" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "kind" TEXT NOT NULL CHECK ("kind" IN ('c1', 'c2')),
  "source" TEXT NOT NULL CHECK ("source" IN ('c1', 'c2')),
  "source_step" INTEGER REFERENCES "strings" ("id"),
  "state" TEXT NOT NULL CHECK ("state" IN ('c1', 'c2')),
  "state_since" INTEGER NOT NULL,
  PRIMARY KEY ("owner", "pos"),
  FOREIGN KEY ("owner") REFERENCES "dossier" ("id") ON DELETE CASCADE,
  CHECK (CASE WHEN "source" = 'c1'
      THEN "source_step" IS NOT NULL
      END),
  CHECK (CASE WHEN "source" = 'c2'
      THEN "source_step" IS NULL
      END)
) STRICT, WITHOUT ROWID;
CREATE TABLE "dossier_deliverables_evidence" (
  "owner" INTEGER NOT NULL,
  "deliverables_pos" INTEGER NOT NULL,
  "pos" INTEGER NOT NULL,
  "version" INTEGER NOT NULL,
  "reference" TEXT NOT NULL,
  "submitted_at" INTEGER NOT NULL,
  "verdict_accepted" INTEGER CHECK ("verdict_accepted" IN (0, 1)),
  "verdict_by" INTEGER REFERENCES "strings" ("id"),
  "verdict_at" INTEGER,
  PRIMARY KEY ("owner", "deliverables_pos", "pos"),
  FOREIGN KEY ("owner", "deliverables_pos") REFERENCES "dossier_deliverables" ("owner", "pos") ON DELETE CASCADE,
  CHECK (CASE WHEN "verdict_accepted" IS NOT NULL
      THEN "verdict_by" IS NOT NULL
      AND "verdict_at" IS NOT NULL
      ELSE "verdict_by" IS NULL
      AND "verdict_at" IS NULL
      END)
) STRICT, WITHOUT ROWID;
CREATE TABLE "dossier_deliverables_evidence_metadata" (
  "owner" INTEGER NOT NULL,
  "deliverables_pos" INTEGER NOT NULL,
  "evidence_pos" INTEGER NOT NULL,
  "key" TEXT NOT NULL,
  "value" TEXT NOT NULL,
  PRIMARY KEY ("owner", "deliverables_pos", "evidence_pos", "key"),
  FOREIGN KEY ("owner", "deliverables_pos", "evidence_pos") REFERENCES "dossier_deliverables_evidence" ("owner", "deliverables_pos", "pos") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE TABLE "dossier_edges" (
  "owner" INTEGER NOT NULL,
  "pos" INTEGER NOT NULL,
  "version" INTEGER NOT NULL,
  "edge" TEXT NOT NULL CHECK ("edge" IN ('c1', 'c2', 'c3', 'c4', 'c5')),
  "from" TEXT NOT NULL CHECK ("from" IN ('c1', 'c2', 'c3')),
  "to" TEXT NOT NULL CHECK ("to" IN ('c1', 'c2', 'c3')),
  "writer" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "at" INTEGER NOT NULL,
  "payload" BLOB NOT NULL,
  PRIMARY KEY ("owner", "pos"),
  FOREIGN KEY ("owner") REFERENCES "dossier" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE INDEX "dossier_edges_by_edge" ON "dossier_edges" ("edge", "at");
CREATE INDEX "dossier_edges_by_time" ON "dossier_edges" ("at");
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
  ('do', 'edges.edge', 'c1', 'Deliver'),
  ('do', 'edges.edge', 'c2', 'Abandon'),
  ('do', 'edges.edge', 'c3', 'Promise'),
  ('do', 'edges.edge', 'c4', 'Submit'),
  ('do', 'edges.edge', 'c5', 'Retract'),
  ('do', 'edges.from', 'c1', 'Open'),
  ('do', 'edges.from', 'c2', 'Delivered'),
  ('do', 'edges.from', 'c3', 'Abandoned'),
  ('do', 'edges.to', 'c1', 'Open'),
  ('do', 'edges.to', 'c2', 'Delivered'),
  ('do', 'edges.to', 'c3', 'Abandoned');
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
  "dossier_inputs"."owner",
  "dossier_inputs"."key",
  (SELECT "name" FROM "codes" WHERE "kind" = 'do' AND "field" = 'inputs.tag' AND "code" = "dossier_inputs"."tag") AS "tag",
  "dossier_inputs"."value",
  "dossier_inputs"."number"
FROM "dossier_inputs";
CREATE VIEW "dossier_inputs_items_view" AS SELECT
  "dossier_inputs_items"."owner",
  "dossier_inputs_items"."inputs_key",
  "dossier_inputs_items"."pos",
  "dossier_inputs_items"."value"
FROM "dossier_inputs_items";
CREATE VIEW "dossier_deliverables_view" AS SELECT
  "dossier_deliverables"."owner",
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
  "dossier_deliverables_evidence"."owner",
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
  "dossier_deliverables_evidence_metadata"."owner",
  "dossier_deliverables_evidence_metadata"."deliverables_pos",
  "dossier_deliverables_evidence_metadata"."evidence_pos",
  "dossier_deliverables_evidence_metadata"."key",
  "dossier_deliverables_evidence_metadata"."value"
FROM "dossier_deliverables_evidence_metadata";
CREATE VIEW "dossier_edges_view" AS SELECT
  "dossier_edges"."owner",
  "dossier_edges"."pos",
  "dossier_edges"."version",
  (SELECT "name" FROM "codes" WHERE "kind" = 'do' AND "field" = 'edges.edge' AND "code" = "dossier_edges"."edge") AS "edge",
  (SELECT "name" FROM "codes" WHERE "kind" = 'do' AND "field" = 'edges.from' AND "code" = "dossier_edges"."from") AS "from",
  (SELECT "name" FROM "codes" WHERE "kind" = 'do' AND "field" = 'edges.to' AND "code" = "dossier_edges"."to") AS "to",
  (SELECT "s" FROM "strings" WHERE "id" = "dossier_edges"."writer") AS "writer",
  "dossier_edges"."at",
  "dossier_edges"."payload"
FROM "dossier_edges";
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
  "owner" INTEGER NOT NULL,
  "value" INTEGER NOT NULL REFERENCES "strings" ("id"),
  PRIMARY KEY ("owner", "value"),
  FOREIGN KEY ("owner") REFERENCES "ensure" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE TABLE "ensure_created" (
  "owner" INTEGER NOT NULL,
  "value" INTEGER NOT NULL REFERENCES "strings" ("id"),
  PRIMARY KEY ("owner", "value"),
  FOREIGN KEY ("owner") REFERENCES "ensure" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE TABLE "ensure_edges" (
  "owner" INTEGER NOT NULL,
  "pos" INTEGER NOT NULL,
  "version" INTEGER NOT NULL,
  "edge" TEXT NOT NULL CHECK ("edge" IN ('at', 'bi', 'cr', 'ho', 'rc', 'rs', 'rt', 'ub')),
  "from" TEXT NOT NULL CHECK ("from" IN ('fu', 'he', 'rt', 'un')),
  "to" TEXT NOT NULL CHECK ("to" IN ('fu', 'he', 'rt', 'un')),
  "writer" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "at" INTEGER NOT NULL,
  "payload" BLOB NOT NULL,
  PRIMARY KEY ("owner", "pos"),
  FOREIGN KEY ("owner") REFERENCES "ensure" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE INDEX "ensure_by_inbox" ON "ensure" ("inbox");
CREATE INDEX "ensure_edges_by_edge" ON "ensure_edges" ("edge", "at");
CREATE INDEX "ensure_edges_by_time" ON "ensure_edges" ("at");
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
  ('en', 'edges.edge', 'at', 'Attempted'),
  ('en', 'edges.edge', 'bi', 'Bind'),
  ('en', 'edges.edge', 'cr', 'Create'),
  ('en', 'edges.edge', 'ho', 'Hold'),
  ('en', 'edges.edge', 'rc', 'Record'),
  ('en', 'edges.edge', 'rs', 'Resume'),
  ('en', 'edges.edge', 'rt', 'Retire'),
  ('en', 'edges.edge', 'ub', 'Unbind'),
  ('en', 'edges.from', 'fu', 'Fulfilled'),
  ('en', 'edges.from', 'he', 'Held'),
  ('en', 'edges.from', 'rt', 'Retired'),
  ('en', 'edges.from', 'un', 'Unfulfilled'),
  ('en', 'edges.to', 'fu', 'Fulfilled'),
  ('en', 'edges.to', 'he', 'Held'),
  ('en', 'edges.to', 'rt', 'Retired'),
  ('en', 'edges.to', 'un', 'Unfulfilled');
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
  "ensure_recorded"."owner",
  (SELECT "s" FROM "strings" WHERE "id" = "ensure_recorded"."value") AS "value"
FROM "ensure_recorded";
CREATE VIEW "ensure_created_view" AS SELECT
  "ensure_created"."owner",
  (SELECT "s" FROM "strings" WHERE "id" = "ensure_created"."value") AS "value"
FROM "ensure_created";
CREATE VIEW "ensure_edges_view" AS SELECT
  "ensure_edges"."owner",
  "ensure_edges"."pos",
  "ensure_edges"."version",
  (SELECT "name" FROM "codes" WHERE "kind" = 'en' AND "field" = 'edges.edge' AND "code" = "ensure_edges"."edge") AS "edge",
  (SELECT "name" FROM "codes" WHERE "kind" = 'en' AND "field" = 'edges.from' AND "code" = "ensure_edges"."from") AS "from",
  (SELECT "name" FROM "codes" WHERE "kind" = 'en' AND "field" = 'edges.to' AND "code" = "ensure_edges"."to") AS "to",
  (SELECT "s" FROM "strings" WHERE "id" = "ensure_edges"."writer") AS "writer",
  "ensure_edges"."at",
  "ensure_edges"."payload"
FROM "ensure_edges";
CREATE TABLE "message" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "inbox" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "expectation" TEXT NOT NULL CHECK ("expectation" IN ('no', 'ou', 're')),
  "expectation_condition" INTEGER REFERENCES "strings" ("id"),
  "deadline" INTEGER,
  "unbound_after" INTEGER NOT NULL,
  "state" TEXT NOT NULL CHECK ("state" IN ('aw', 'dr', 'pe', 'se')),
  "state_since" INTEGER NOT NULL,
  "receiver_session" INTEGER REFERENCES "strings" ("id"),
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
  CHECK (CASE WHEN "receiver_session" IS NOT NULL
      THEN "receiver_host" IS NOT NULL
      ELSE "receiver_host" IS NULL
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
      THEN "receiver_session" IS NULL
      AND "receiver_host" IS NULL
      AND "reason" IS NULL
      AND "reason_by" IS NULL
      AND "reason_authority" IS NULL
      AND "watermark" IS NULL
      END),
  CHECK (CASE WHEN "state" = 'aw'
      THEN "receiver_session" IS NOT NULL
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
CREATE TABLE "message_edges" (
  "owner" INTEGER NOT NULL,
  "pos" INTEGER NOT NULL,
  "version" INTEGER NOT NULL,
  "edge" TEXT NOT NULL CHECK ("edge" IN ('de', 'dr', 'se')),
  "from" TEXT NOT NULL CHECK ("from" IN ('aw', 'dr', 'pe', 'se')),
  "to" TEXT NOT NULL CHECK ("to" IN ('aw', 'dr', 'pe', 'se')),
  "writer" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "at" INTEGER NOT NULL,
  "payload" BLOB NOT NULL,
  PRIMARY KEY ("owner", "pos"),
  FOREIGN KEY ("owner") REFERENCES "message" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE INDEX "message_deadline" ON "message" ("deadline") WHERE "state" IN ('pe', 'aw') AND "deadline" IS NOT NULL;
CREATE INDEX "message_edges_by_edge" ON "message_edges" ("edge", "at");
CREATE INDEX "message_edges_by_time" ON "message_edges" ("at");
CREATE INDEX "message_open_by_inbox" ON "message" ("inbox") WHERE "state" IN ('pe', 'aw');
INSERT INTO "codes" ("kind", "field", "code", "name") VALUES
  ('ms', 'expectation', 'no', 'Nothing'),
  ('ms', 'expectation', 'ou', 'Outcome'),
  ('ms', 'expectation', 're', 'Reply'),
  ('ms', 'state', 'aw', 'Awaiting'),
  ('ms', 'state', 'dr', 'Dropped'),
  ('ms', 'state', 'pe', 'Pending'),
  ('ms', 'state', 'se', 'Settled'),
  ('ms', 'reason', 'cu', 'ConsumerUnreachable'),
  ('ms', 'reason', 'ex', 'Expired'),
  ('ms', 'reason', 'su', 'Superseded'),
  ('ms', 'reason', 'un', 'Undeliverable'),
  ('ms', 'reason', 'wi', 'Withdrawn'),
  ('ms', 'edges.edge', 'de', 'Deliver'),
  ('ms', 'edges.edge', 'dr', 'Drop'),
  ('ms', 'edges.edge', 'se', 'Settle'),
  ('ms', 'edges.from', 'aw', 'Awaiting'),
  ('ms', 'edges.from', 'dr', 'Dropped'),
  ('ms', 'edges.from', 'pe', 'Pending'),
  ('ms', 'edges.from', 'se', 'Settled'),
  ('ms', 'edges.to', 'aw', 'Awaiting'),
  ('ms', 'edges.to', 'dr', 'Dropped'),
  ('ms', 'edges.to', 'pe', 'Pending'),
  ('ms', 'edges.to', 'se', 'Settled');
CREATE VIEW "message_view" AS SELECT
  "message"."id",
  "message"."version",
  (SELECT "s" FROM "strings" WHERE "id" = "message"."inbox") AS "inbox",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ms' AND "field" = 'expectation' AND "code" = "message"."expectation") AS "expectation",
  (SELECT "s" FROM "strings" WHERE "id" = "message"."expectation_condition") AS "expectation_condition",
  "message"."deadline",
  "message"."unbound_after",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ms' AND "field" = 'state' AND "code" = "message"."state") AS "state",
  "message"."state_since",
  (SELECT "s" FROM "strings" WHERE "id" = "message"."receiver_session") AS "receiver_session",
  (SELECT "s" FROM "strings" WHERE "id" = "message"."receiver_host") AS "receiver_host",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ms' AND "field" = 'reason' AND "code" = "message"."reason") AS "reason",
  (SELECT "s" FROM "strings" WHERE "id" = "message"."reason_by") AS "reason_by",
  (SELECT "s" FROM "strings" WHERE "id" = "message"."reason_authority") AS "reason_authority",
  "message"."watermark"
FROM "message";
CREATE VIEW "message_edges_view" AS SELECT
  "message_edges"."owner",
  "message_edges"."pos",
  "message_edges"."version",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ms' AND "field" = 'edges.edge' AND "code" = "message_edges"."edge") AS "edge",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ms' AND "field" = 'edges.from' AND "code" = "message_edges"."from") AS "from",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ms' AND "field" = 'edges.to' AND "code" = "message_edges"."to") AS "to",
  (SELECT "s" FROM "strings" WHERE "id" = "message_edges"."writer") AS "writer",
  "message_edges"."at",
  "message_edges"."payload"
FROM "message_edges";
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
  "owner" INTEGER NOT NULL,
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
  PRIMARY KEY ("owner", "pos"),
  FOREIGN KEY ("owner") REFERENCES "referral" ("id") ON DELETE CASCADE,
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
CREATE TABLE "referral_edges" (
  "owner" INTEGER NOT NULL,
  "pos" INTEGER NOT NULL,
  "version" INTEGER NOT NULL,
  "edge" TEXT NOT NULL CHECK ("edge" IN ('c1', 'c2', 'c3')),
  "from" TEXT NOT NULL CHECK ("from" IN ('c1', 'c2', 'c3')),
  "to" TEXT NOT NULL CHECK ("to" IN ('c1', 'c2', 'c3')),
  "writer" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "at" INTEGER NOT NULL,
  "payload" BLOB NOT NULL,
  PRIMARY KEY ("owner", "pos"),
  FOREIGN KEY ("owner") REFERENCES "referral" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE INDEX "referral_by_reply" ON "referral" ("in_reply_to") WHERE "in_reply_to" IS NOT NULL;
CREATE INDEX "referral_by_session" ON "referral" ("session", "seq");
CREATE INDEX "referral_edges_by_edge" ON "referral_edges" ("edge", "at");
CREATE INDEX "referral_edges_by_time" ON "referral_edges" ("at");
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
  ('rf', 'edges.edge', 'c1', 'Send'),
  ('rf', 'edges.edge', 'c2', 'Read'),
  ('rf', 'edges.edge', 'c3', 'Amend'),
  ('rf', 'edges.from', 'c1', 'Draft'),
  ('rf', 'edges.from', 'c2', 'Sent'),
  ('rf', 'edges.from', 'c3', 'Read'),
  ('rf', 'edges.to', 'c1', 'Draft'),
  ('rf', 'edges.to', 'c2', 'Sent'),
  ('rf', 'edges.to', 'c3', 'Read');
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
  "referral_references"."owner",
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
CREATE VIEW "referral_edges_view" AS SELECT
  "referral_edges"."owner",
  "referral_edges"."pos",
  "referral_edges"."version",
  (SELECT "name" FROM "codes" WHERE "kind" = 'rf' AND "field" = 'edges.edge' AND "code" = "referral_edges"."edge") AS "edge",
  (SELECT "name" FROM "codes" WHERE "kind" = 'rf' AND "field" = 'edges.from' AND "code" = "referral_edges"."from") AS "from",
  (SELECT "name" FROM "codes" WHERE "kind" = 'rf' AND "field" = 'edges.to' AND "code" = "referral_edges"."to") AS "to",
  (SELECT "s" FROM "strings" WHERE "id" = "referral_edges"."writer") AS "writer",
  "referral_edges"."at",
  "referral_edges"."payload"
FROM "referral_edges";
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
  "owner" INTEGER NOT NULL,
  "value" TEXT NOT NULL,
  PRIMARY KEY ("owner", "value"),
  FOREIGN KEY ("owner") REFERENCES "ticket" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE TABLE "ticket_notes" (
  "owner" INTEGER NOT NULL,
  "key" TEXT NOT NULL,
  "value" TEXT NOT NULL,
  PRIMARY KEY ("owner", "key"),
  FOREIGN KEY ("owner") REFERENCES "ticket" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE TABLE "ticket_edges" (
  "owner" INTEGER NOT NULL,
  "pos" INTEGER NOT NULL,
  "version" INTEGER NOT NULL,
  "edge" TEXT NOT NULL CHECK ("edge" IN ('c1', 'c2')),
  "from" TEXT NOT NULL CHECK ("from" IN ('c1', 'c2', 'c3')),
  "to" TEXT NOT NULL CHECK ("to" IN ('c1', 'c2', 'c3')),
  "writer" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "at" INTEGER NOT NULL,
  "payload" BLOB NOT NULL,
  PRIMARY KEY ("owner", "pos"),
  FOREIGN KEY ("owner") REFERENCES "ticket" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE INDEX "ticket_edges_by_edge" ON "ticket_edges" ("edge", "at");
CREATE INDEX "ticket_edges_by_time" ON "ticket_edges" ("at");
CREATE INDEX "ticket_open_by_queue" ON "ticket" ("queue") WHERE "state" IN ('c2');
INSERT INTO "codes" ("kind", "field", "code", "name") VALUES
  ('tk', 'state', 'c1', 'Draft'),
  ('tk', 'state', 'c2', 'Open'),
  ('tk', 'state', 'c3', 'Closed'),
  ('tk', 'edges.edge', 'c1', 'Edit'),
  ('tk', 'edges.edge', 'c2', 'Move'),
  ('tk', 'edges.from', 'c1', 'Draft'),
  ('tk', 'edges.from', 'c2', 'Open'),
  ('tk', 'edges.from', 'c3', 'Closed'),
  ('tk', 'edges.to', 'c1', 'Draft'),
  ('tk', 'edges.to', 'c2', 'Open'),
  ('tk', 'edges.to', 'c3', 'Closed');
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
  "ticket_assignees"."owner",
  "ticket_assignees"."value"
FROM "ticket_assignees";
CREATE VIEW "ticket_notes_view" AS SELECT
  "ticket_notes"."owner",
  "ticket_notes"."key",
  "ticket_notes"."value"
FROM "ticket_notes";
CREATE VIEW "ticket_edges_view" AS SELECT
  "ticket_edges"."owner",
  "ticket_edges"."pos",
  "ticket_edges"."version",
  (SELECT "name" FROM "codes" WHERE "kind" = 'tk' AND "field" = 'edges.edge' AND "code" = "ticket_edges"."edge") AS "edge",
  (SELECT "name" FROM "codes" WHERE "kind" = 'tk' AND "field" = 'edges.from' AND "code" = "ticket_edges"."from") AS "from",
  (SELECT "name" FROM "codes" WHERE "kind" = 'tk' AND "field" = 'edges.to' AND "code" = "ticket_edges"."to") AS "to",
  (SELECT "s" FROM "strings" WHERE "id" = "ticket_edges"."writer") AS "writer",
  "ticket_edges"."at",
  "ticket_edges"."payload"
FROM "ticket_edges";
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
  "owner" INTEGER NOT NULL,
  "key" TEXT NOT NULL,
  "value" TEXT NOT NULL,
  PRIMARY KEY ("owner", "key"),
  FOREIGN KEY ("owner") REFERENCES "terminal_session" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE TABLE "terminal_session_labels" (
  "owner" INTEGER NOT NULL,
  "value" TEXT NOT NULL,
  PRIMARY KEY ("owner", "value"),
  FOREIGN KEY ("owner") REFERENCES "terminal_session" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE TABLE "terminal_session_edges" (
  "owner" INTEGER NOT NULL,
  "pos" INTEGER NOT NULL,
  "version" INTEGER NOT NULL,
  "edge" TEXT NOT NULL CHECK ("edge" IN ('at', 'gu', 'rl', 'rs', 'va')),
  "from" TEXT NOT NULL CHECK ("from" IN ('go', 'gu', 'rl', 'rq')),
  "to" TEXT NOT NULL CHECK ("to" IN ('go', 'gu', 'rl', 'rq')),
  "writer" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "at" INTEGER NOT NULL,
  "payload" BLOB NOT NULL,
  PRIMARY KEY ("owner", "pos"),
  FOREIGN KEY ("owner") REFERENCES "terminal_session" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE INDEX "terminal_session_by_vessel" ON "terminal_session" ("vessel", "ordinal");
CREATE INDEX "terminal_session_edges_by_edge" ON "terminal_session_edges" ("edge", "at");
CREATE INDEX "terminal_session_edges_by_time" ON "terminal_session_edges" ("at");
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
  ('ts', 'cause_category', 'ex', 'External'),
  ('ts', 'cause_category', 'mc', 'Misconfigured'),
  ('ts', 'cause_category', 'mx', 'MissingCapability'),
  ('ts', 'cause_category', 'rf', 'Refused'),
  ('ts', 'cause_category', 'tr', 'Transient'),
  ('ts', 'edges.edge', 'at', 'Attempted'),
  ('ts', 'edges.edge', 'gu', 'GiveUp'),
  ('ts', 'edges.edge', 'rl', 'Realise'),
  ('ts', 'edges.edge', 'rs', 'Release'),
  ('ts', 'edges.edge', 'va', 'Vanish'),
  ('ts', 'edges.from', 'go', 'Gone'),
  ('ts', 'edges.from', 'gu', 'GaveUp'),
  ('ts', 'edges.from', 'rl', 'Realised'),
  ('ts', 'edges.from', 'rq', 'Requested'),
  ('ts', 'edges.to', 'go', 'Gone'),
  ('ts', 'edges.to', 'gu', 'GaveUp'),
  ('ts', 'edges.to', 'rl', 'Realised'),
  ('ts', 'edges.to', 'rq', 'Requested');
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
  (SELECT "name" FROM "codes" WHERE "kind" = 'ts' AND "field" = 'cause_category' AND "code" = "terminal_session"."cause_category") AS "cause_category",
  "terminal_session"."cause_detail"
FROM "terminal_session";
CREATE VIEW "terminal_session_env_view" AS SELECT
  "terminal_session_env"."owner",
  "terminal_session_env"."key",
  "terminal_session_env"."value"
FROM "terminal_session_env";
CREATE VIEW "terminal_session_labels_view" AS SELECT
  "terminal_session_labels"."owner",
  "terminal_session_labels"."value"
FROM "terminal_session_labels";
CREATE VIEW "terminal_session_edges_view" AS SELECT
  "terminal_session_edges"."owner",
  "terminal_session_edges"."pos",
  "terminal_session_edges"."version",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ts' AND "field" = 'edges.edge' AND "code" = "terminal_session_edges"."edge") AS "edge",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ts' AND "field" = 'edges.from' AND "code" = "terminal_session_edges"."from") AS "from",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ts' AND "field" = 'edges.to' AND "code" = "terminal_session_edges"."to") AS "to",
  (SELECT "s" FROM "strings" WHERE "id" = "terminal_session_edges"."writer") AS "writer",
  "terminal_session_edges"."at",
  "terminal_session_edges"."payload"
FROM "terminal_session_edges";
CREATE TABLE "vessel" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "name" TEXT NOT NULL
) STRICT;
CREATE VIEW "vessel_view" AS SELECT
  "vessel"."id",
  "vessel"."version",
  "vessel"."name"
FROM "vessel";
