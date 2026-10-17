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
  "edge" TEXT NOT NULL CHECK ("edge" IN ('at', 'de', 'en')),
  "from" TEXT NOT NULL CHECK ("from" IN ('en', 'li')),
  "to" TEXT NOT NULL CHECK ("to" IN ('en', 'li')),
  "writer" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "at" INTEGER NOT NULL,
  "payload" BLOB NOT NULL,
  PRIMARY KEY ("owner", "pos"),
  FOREIGN KEY ("owner") REFERENCES "agent_session" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE TABLE "agent_session_receipts" (
  "owner" INTEGER NOT NULL,
  "pos" INTEGER NOT NULL,
  "version" INTEGER NOT NULL,
  "message" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "transport" TEXT NOT NULL CHECK ("transport" IN ('mu', 'no', 'tu')),
  "at" INTEGER NOT NULL,
  PRIMARY KEY ("owner", "pos"),
  FOREIGN KEY ("owner") REFERENCES "agent_session" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE INDEX "agent_session_edges_by_edge" ON "agent_session_edges" ("edge", "at");
CREATE INDEX "agent_session_edges_by_time" ON "agent_session_edges" ("at");
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
  "window" INTEGER,
  "bound" INTEGER REFERENCES "strings" ("id"),
  CHECK (CASE WHEN "state" = 'un'
      THEN "window" IS NOT NULL
      AND "bound" IS NULL
      END),
  CHECK (CASE WHEN "state" = 'fu'
      THEN "window" IS NOT NULL
      AND "bound" IS NOT NULL
      END),
  CHECK (CASE WHEN "state" = 'he'
      THEN "window" IS NOT NULL
      AND "bound" IS NULL
      END),
  CHECK (CASE WHEN "state" = 'rt'
      THEN "window" IS NOT NULL
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
  "edge" TEXT NOT NULL CHECK ("edge" IN ('bi', 'cr', 'gu', 'rc', 'rs', 'rt', 'ub')),
  "from" TEXT NOT NULL CHECK ("from" IN ('fu', 'he', 'rt', 'un')),
  "to" TEXT NOT NULL CHECK ("to" IN ('fu', 'he', 'rt', 'un')),
  "writer" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "at" INTEGER NOT NULL,
  "payload" BLOB NOT NULL,
  PRIMARY KEY ("owner", "pos"),
  FOREIGN KEY ("owner") REFERENCES "ensure" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE TABLE "ensure_attempts" (
  "owner" INTEGER NOT NULL,
  "pos" INTEGER NOT NULL,
  "version" INTEGER NOT NULL,
  "at" INTEGER NOT NULL,
  "outcome" TEXT NOT NULL CHECK ("outcome" IN ('fa', 'ok')),
  "outcome_cause_category" TEXT CHECK ("outcome_cause_category" IN ('ex', 'mc', 'mx', 'rf', 'tr')),
  "outcome_cause_detail" TEXT,
  "outcome_retryable" INTEGER CHECK ("outcome_retryable" IN (0, 1)),
  PRIMARY KEY ("owner", "pos"),
  FOREIGN KEY ("owner") REFERENCES "ensure" ("id") ON DELETE CASCADE,
  CHECK (CASE WHEN "outcome" = 'ok'
      THEN "outcome_cause_category" IS NULL
      AND "outcome_cause_detail" IS NULL
      AND "outcome_retryable" IS NULL
      END),
  CHECK (CASE WHEN "outcome" = 'fa'
      THEN "outcome_cause_category" IS NOT NULL
      AND "outcome_cause_detail" IS NOT NULL
      AND "outcome_retryable" IS NOT NULL
      END)
) STRICT, WITHOUT ROWID;
CREATE INDEX "ensure_by_inbox" ON "ensure" ("inbox");
CREATE INDEX "ensure_edges_by_edge" ON "ensure_edges" ("edge", "at");
CREATE INDEX "ensure_edges_by_time" ON "ensure_edges" ("at");
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
  CHECK (CASE WHEN "state" = 'rq'
      THEN "identity" IS NULL
      AND "cause_category" IS NULL
      AND "cause_detail" IS NULL
      END),
  CHECK (CASE WHEN "state" = 'rl'
      THEN "identity" IS NOT NULL
      AND "cause_category" IS NULL
      AND "cause_detail" IS NULL
      END),
  CHECK (CASE WHEN "state" = 'go'
      THEN "identity" IS NULL
      AND "cause_category" IS NULL
      AND "cause_detail" IS NULL
      END),
  CHECK (CASE WHEN "state" = 'gu'
      THEN "cause_category" IS NOT NULL
      AND "cause_detail" IS NOT NULL
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
CREATE TABLE "terminal_session_events" (
  "owner" INTEGER NOT NULL,
  "pos" INTEGER NOT NULL,
  "version" INTEGER NOT NULL,
  "edge" TEXT NOT NULL CHECK ("edge" IN ('gu', 'rl', 'rs', 'va')),
  "from" TEXT NOT NULL CHECK ("from" IN ('go', 'gu', 'rl', 'rq')),
  "to" TEXT NOT NULL CHECK ("to" IN ('go', 'gu', 'rl', 'rq')),
  "writer" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "at" INTEGER NOT NULL,
  "payload" BLOB NOT NULL,
  PRIMARY KEY ("owner", "pos"),
  FOREIGN KEY ("owner") REFERENCES "terminal_session" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE TABLE "terminal_session_attempts" (
  "owner" INTEGER NOT NULL,
  "pos" INTEGER NOT NULL,
  "version" INTEGER NOT NULL,
  "at" INTEGER NOT NULL,
  "outcome" TEXT NOT NULL CHECK ("outcome" IN ('fa', 'ok')),
  "outcome_cause_category" TEXT CHECK ("outcome_cause_category" IN ('ex', 'mc', 'mx', 'rf', 'tr')),
  "outcome_cause_detail" TEXT,
  "outcome_retryable" INTEGER CHECK ("outcome_retryable" IN (0, 1)),
  PRIMARY KEY ("owner", "pos"),
  FOREIGN KEY ("owner") REFERENCES "terminal_session" ("id") ON DELETE CASCADE,
  CHECK (CASE WHEN "outcome" = 'ok'
      THEN "outcome_cause_category" IS NULL
      AND "outcome_cause_detail" IS NULL
      AND "outcome_retryable" IS NULL
      END),
  CHECK (CASE WHEN "outcome" = 'fa'
      THEN "outcome_cause_category" IS NOT NULL
      AND "outcome_cause_detail" IS NOT NULL
      AND "outcome_retryable" IS NOT NULL
      END)
) STRICT, WITHOUT ROWID;
CREATE INDEX "terminal_session_by_vessel" ON "terminal_session" ("vessel", "ordinal");
CREATE INDEX "terminal_session_events_by_edge" ON "terminal_session_events" ("edge", "at");
CREATE INDEX "terminal_session_events_by_time" ON "terminal_session_events" ("at");
CREATE INDEX "terminal_session_live_by_pool" ON "terminal_session" ("pool") WHERE "state" IN ('rq', 'rl');
CREATE TABLE "vessel" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "name" TEXT NOT NULL
) STRICT;
