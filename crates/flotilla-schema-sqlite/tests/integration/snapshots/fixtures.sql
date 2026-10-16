CREATE TABLE strings (id INTEGER PRIMARY KEY, s TEXT NOT NULL UNIQUE) STRICT;
CREATE TABLE resource_identity (
  id INTEGER PRIMARY KEY, kind INTEGER NOT NULL,
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
CREATE TABLE "message" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "inbox" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "expectation" INTEGER NOT NULL CHECK ("expectation" IN (1, 2, 3)),
  "expectation_outcome_condition" INTEGER REFERENCES "strings" ("id"),
  "deadline" INTEGER,
  "unbound_after" INTEGER NOT NULL,
  "state" INTEGER NOT NULL CHECK ("state" IN (1, 2, 3, 4)),
  "state_since" INTEGER NOT NULL,
  "receiver_session" INTEGER REFERENCES "strings" ("id"),
  "receiver_host" INTEGER REFERENCES "strings" ("id"),
  "reason" INTEGER CHECK ("reason" IN (1, 2, 3, 4, 5)),
  "reason_superseded_by" INTEGER REFERENCES "strings" ("id"),
  "reason_withdrawn_by" INTEGER REFERENCES "strings" ("id"),
  "reason_withdrawn_authority" INTEGER REFERENCES "strings" ("id"),
  "reason_consumer_unreachable_by" INTEGER REFERENCES "strings" ("id"),
  "reason_consumer_unreachable_authority" INTEGER REFERENCES "strings" ("id"),
  "dropped_receiver_present" INTEGER CHECK ("dropped_receiver_present" IN (0, 1)),
  "dropped_receiver_session" INTEGER REFERENCES "strings" ("id"),
  "dropped_receiver_host" INTEGER REFERENCES "strings" ("id"),
  "at" INTEGER,
  "watermark" INTEGER,
  CHECK (CASE WHEN "expectation" = 3
      THEN "expectation_outcome_condition" IS NOT NULL
      ELSE "expectation_outcome_condition" IS NULL
      END),
  CHECK (CASE WHEN "reason" = 3
      THEN "reason_superseded_by" IS NOT NULL
      ELSE "reason_superseded_by" IS NULL
      END),
  CHECK (CASE WHEN "reason" = 4
      THEN "reason_withdrawn_by" IS NOT NULL
      AND "reason_withdrawn_authority" IS NOT NULL
      ELSE "reason_withdrawn_by" IS NULL
      AND "reason_withdrawn_authority" IS NULL
      END),
  CHECK (CASE WHEN "reason" = 5
      THEN "reason_consumer_unreachable_by" IS NOT NULL
      AND "reason_consumer_unreachable_authority" IS NOT NULL
      ELSE "reason_consumer_unreachable_by" IS NULL
      AND "reason_consumer_unreachable_authority" IS NULL
      END),
  CHECK (CASE WHEN "dropped_receiver_present" = 1
      THEN "dropped_receiver_session" IS NOT NULL
      AND "dropped_receiver_host" IS NOT NULL
      ELSE "dropped_receiver_session" IS NULL
      AND "dropped_receiver_host" IS NULL
      END),
  CHECK (CASE WHEN "state" = 1
      THEN "receiver_session" IS NULL
      AND "receiver_host" IS NULL
      AND "reason" IS NULL
      AND "dropped_receiver_present" IS NULL
      AND "at" IS NULL
      AND "watermark" IS NULL
      END),
  CHECK (CASE WHEN "state" = 2
      THEN "receiver_session" IS NOT NULL
      AND "receiver_host" IS NOT NULL
      AND "reason" IS NULL
      AND "dropped_receiver_present" IS NULL
      AND "at" IS NULL
      AND "watermark" IS NULL
      END),
  CHECK (CASE WHEN "state" = 3
      THEN "receiver_session" IS NULL
      AND "receiver_host" IS NULL
      AND "reason" IS NULL
      AND "dropped_receiver_present" IS NULL
      AND "at" IS NULL
      AND "watermark" IS NULL
      END),
  CHECK (CASE WHEN "state" = 4
      THEN "reason" IS NOT NULL
      AND "dropped_receiver_present" IS NOT NULL
      AND "at" IS NOT NULL
      AND "receiver_session" IS NULL
      AND "receiver_host" IS NULL
      END)
) STRICT;
CREATE TABLE "message_edges" (
  "owner" INTEGER NOT NULL,
  "pos" INTEGER NOT NULL,
  "version" INTEGER NOT NULL,
  "edge" INTEGER NOT NULL CHECK ("edge" IN (1, 2, 3)),
  "from" INTEGER NOT NULL CHECK ("from" IN (1, 2, 3, 4)),
  "to" INTEGER NOT NULL CHECK ("to" IN (1, 2, 3, 4)),
  "writer" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "at" INTEGER NOT NULL,
  "payload" BLOB NOT NULL,
  PRIMARY KEY ("owner", "pos"),
  FOREIGN KEY ("owner") REFERENCES "message" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE INDEX "message_deadline" ON "message" ("deadline") WHERE "state" IN (1, 2) AND "deadline" IS NOT NULL;
CREATE INDEX "message_edges_by_edge" ON "message_edges" ("edge", "at");
CREATE INDEX "message_edges_by_time" ON "message_edges" ("at");
CREATE INDEX "message_open_by_inbox" ON "message" ("inbox") WHERE "state" IN (1, 2);
CREATE TABLE "agent_session" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "state" INTEGER NOT NULL CHECK ("state" IN (1, 2)),
  "state_since" INTEGER NOT NULL,
  "channel" INTEGER CHECK ("channel" IN (1, 2)),
  "reason" INTEGER CHECK ("reason" IN (1, 2)),
  CHECK (CASE WHEN "state" = 1
      THEN "channel" IS NOT NULL
      AND "reason" IS NULL
      END),
  CHECK (CASE WHEN "state" = 2
      THEN "reason" IS NOT NULL
      AND "channel" IS NULL
      END)
) STRICT;
CREATE TABLE "agent_session_edges" (
  "owner" INTEGER NOT NULL,
  "pos" INTEGER NOT NULL,
  "version" INTEGER NOT NULL,
  "edge" INTEGER NOT NULL CHECK ("edge" IN (1, 2, 3)),
  "from" INTEGER NOT NULL CHECK ("from" IN (1, 2)),
  "to" INTEGER NOT NULL CHECK ("to" IN (1, 2)),
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
  "transport" INTEGER NOT NULL CHECK ("transport" IN (1, 2, 3)),
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
  "state" INTEGER NOT NULL CHECK ("state" IN (1, 2, 3)),
  "state_since" INTEGER NOT NULL,
  "reason" TEXT,
  CHECK (CASE WHEN "state" = 1
      THEN "reason" IS NULL
      END),
  CHECK (CASE WHEN "state" = 2
      THEN "reason" IS NULL
      END),
  CHECK (CASE WHEN "state" = 3
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
  "value" INTEGER NOT NULL CHECK ("value" IN (1, 2, 3)),
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
  "edge" INTEGER NOT NULL CHECK ("edge" IN (1, 2, 3, 4, 5)),
  "from" INTEGER NOT NULL CHECK ("from" IN (1, 2, 3)),
  "to" INTEGER NOT NULL CHECK ("to" IN (1, 2, 3)),
  "writer" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "at" INTEGER NOT NULL,
  "payload" BLOB NOT NULL,
  PRIMARY KEY ("owner", "pos"),
  FOREIGN KEY ("owner") REFERENCES "convoy" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE INDEX "convoy_edges_by_edge" ON "convoy_edges" ("edge", "at");
CREATE INDEX "convoy_edges_by_time" ON "convoy_edges" ("at");
CREATE INDEX "convoy_open_by_project" ON "convoy" ("project") WHERE "state" IN (1);
CREATE TABLE "terminal_session" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "vessel" INTEGER NOT NULL REFERENCES "resource_identity" ("id"),
  "role" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "ordinal" INTEGER NOT NULL,
  "pool" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "cwd" TEXT NOT NULL,
  "source" INTEGER NOT NULL CHECK ("source" IN (1, 2)),
  "source_tool_command" TEXT,
  "source_agent_selector" INTEGER REFERENCES "strings" ("id"),
  "source_agent_brief" BLOB REFERENCES "objects" ("hash"),
  "state" INTEGER NOT NULL CHECK ("state" IN (1, 2, 3, 4)),
  "state_since" INTEGER NOT NULL,
  "identity" TEXT,
  "cause_category" INTEGER CHECK ("cause_category" IN (1, 2, 3, 4, 5)),
  "cause_detail" TEXT,
  CHECK (CASE WHEN "source" = 1
      THEN "source_tool_command" IS NOT NULL
      ELSE "source_tool_command" IS NULL
      END),
  CHECK (CASE WHEN "source" = 2
      THEN "source_agent_selector" IS NOT NULL
      AND "source_agent_brief" IS NOT NULL
      ELSE "source_agent_selector" IS NULL
      AND "source_agent_brief" IS NULL
      END),
  CHECK (CASE WHEN "state" = 1
      THEN "identity" IS NULL
      AND "cause_category" IS NULL
      AND "cause_detail" IS NULL
      END),
  CHECK (CASE WHEN "state" = 2
      THEN "identity" IS NOT NULL
      AND "cause_category" IS NULL
      AND "cause_detail" IS NULL
      END),
  CHECK (CASE WHEN "state" = 3
      THEN "identity" IS NULL
      AND "cause_category" IS NULL
      AND "cause_detail" IS NULL
      END),
  CHECK (CASE WHEN "state" = 4
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
  "edge" INTEGER NOT NULL CHECK ("edge" IN (1, 2, 3, 4)),
  "from" INTEGER NOT NULL CHECK ("from" IN (1, 2, 3, 4)),
  "to" INTEGER NOT NULL CHECK ("to" IN (1, 2, 3, 4)),
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
  "outcome" INTEGER NOT NULL CHECK ("outcome" IN (1, 2)),
  "outcome_failed_cause_category" INTEGER CHECK ("outcome_failed_cause_category" IN (1, 2, 3, 4, 5)),
  "outcome_failed_cause_detail" TEXT,
  "outcome_failed_retryable" INTEGER CHECK ("outcome_failed_retryable" IN (0, 1)),
  PRIMARY KEY ("owner", "pos"),
  FOREIGN KEY ("owner") REFERENCES "terminal_session" ("id") ON DELETE CASCADE,
  CHECK (CASE WHEN "outcome" = 2
      THEN "outcome_failed_cause_category" IS NOT NULL
      AND "outcome_failed_cause_detail" IS NOT NULL
      AND "outcome_failed_retryable" IS NOT NULL
      ELSE "outcome_failed_cause_category" IS NULL
      AND "outcome_failed_cause_detail" IS NULL
      AND "outcome_failed_retryable" IS NULL
      END)
) STRICT, WITHOUT ROWID;
CREATE INDEX "terminal_session_by_vessel" ON "terminal_session" ("vessel", "ordinal");
CREATE INDEX "terminal_session_events_by_edge" ON "terminal_session_events" ("edge", "at");
CREATE INDEX "terminal_session_events_by_time" ON "terminal_session_events" ("at");
CREATE INDEX "terminal_session_live_by_pool" ON "terminal_session" ("pool") WHERE "state" IN (1, 2);
CREATE TABLE "vessel" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "name" TEXT NOT NULL
) STRICT;
CREATE TABLE "ticket" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "queue" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "title" TEXT NOT NULL,
  "state" INTEGER NOT NULL CHECK ("state" IN (1, 2, 3)),
  "state_since" INTEGER NOT NULL,
  "priority" INTEGER,
  "owner" INTEGER REFERENCES "strings" ("id"),
  "hold_present" INTEGER CHECK ("hold_present" IN (0, 1)),
  "hold_by" TEXT,
  "hold_until" INTEGER,
  "reason" TEXT,
  "closed_by" INTEGER REFERENCES "strings" ("id"),
  "signature_who" INTEGER REFERENCES "strings" ("id"),
  "signature_at" INTEGER,
  CHECK (CASE WHEN "hold_present" = 1
      THEN "hold_by" IS NOT NULL
      AND "hold_until" IS NOT NULL
      ELSE "hold_by" IS NULL
      AND "hold_until" IS NULL
      END),
  CHECK (CASE WHEN "state" = 1
      THEN "priority" IS NULL
      AND "owner" IS NULL
      AND "hold_present" IS NULL
      AND "reason" IS NULL
      AND "closed_by" IS NULL
      AND "signature_who" IS NULL
      AND "signature_at" IS NULL
      END),
  CHECK (CASE WHEN "state" = 2
      THEN "priority" IS NOT NULL
      AND "hold_present" IS NOT NULL
      AND "reason" IS NULL
      AND "closed_by" IS NULL
      AND "signature_who" IS NULL
      AND "signature_at" IS NULL
      END),
  CHECK (CASE WHEN "state" = 3
      THEN "priority" IS NOT NULL
      AND "reason" IS NOT NULL
      AND "closed_by" IS NOT NULL
      AND "signature_who" IS NOT NULL
      AND "signature_at" IS NOT NULL
      AND "owner" IS NULL
      AND "hold_present" IS NULL
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
  "edge" INTEGER NOT NULL CHECK ("edge" IN (1, 2)),
  "from" INTEGER NOT NULL CHECK ("from" IN (1, 2, 3)),
  "to" INTEGER NOT NULL CHECK ("to" IN (1, 2, 3)),
  "writer" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "at" INTEGER NOT NULL,
  "payload" BLOB NOT NULL,
  PRIMARY KEY ("owner", "pos"),
  FOREIGN KEY ("owner") REFERENCES "ticket" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE INDEX "ticket_edges_by_edge" ON "ticket_edges" ("edge", "at");
CREATE INDEX "ticket_edges_by_time" ON "ticket_edges" ("at");
CREATE INDEX "ticket_open_by_queue" ON "ticket" ("queue") WHERE "state" IN (2);
CREATE TABLE "ensure" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "inbox" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "policy_max_failures" INTEGER NOT NULL,
  "policy_base" INTEGER NOT NULL,
  "policy_factor" INTEGER NOT NULL,
  "policy_cap" INTEGER NOT NULL,
  "state" INTEGER NOT NULL CHECK ("state" IN (1, 2, 3, 4)),
  "state_since" INTEGER NOT NULL,
  "window" INTEGER,
  "bound" INTEGER REFERENCES "strings" ("id"),
  CHECK (CASE WHEN "state" = 1
      THEN "window" IS NOT NULL
      AND "bound" IS NULL
      END),
  CHECK (CASE WHEN "state" = 2
      THEN "window" IS NOT NULL
      AND "bound" IS NOT NULL
      END),
  CHECK (CASE WHEN "state" = 3
      THEN "window" IS NOT NULL
      AND "bound" IS NULL
      END),
  CHECK (CASE WHEN "state" = 4
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
  "edge" INTEGER NOT NULL CHECK ("edge" IN (1, 2, 3, 4, 5, 6, 7)),
  "from" INTEGER NOT NULL CHECK ("from" IN (1, 2, 3, 4)),
  "to" INTEGER NOT NULL CHECK ("to" IN (1, 2, 3, 4)),
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
  "outcome" INTEGER NOT NULL CHECK ("outcome" IN (1, 2)),
  "outcome_failed_cause_category" INTEGER CHECK ("outcome_failed_cause_category" IN (1, 2, 3, 4, 5)),
  "outcome_failed_cause_detail" TEXT,
  "outcome_failed_retryable" INTEGER CHECK ("outcome_failed_retryable" IN (0, 1)),
  PRIMARY KEY ("owner", "pos"),
  FOREIGN KEY ("owner") REFERENCES "ensure" ("id") ON DELETE CASCADE,
  CHECK (CASE WHEN "outcome" = 2
      THEN "outcome_failed_cause_category" IS NOT NULL
      AND "outcome_failed_cause_detail" IS NOT NULL
      AND "outcome_failed_retryable" IS NOT NULL
      ELSE "outcome_failed_cause_category" IS NULL
      AND "outcome_failed_cause_detail" IS NULL
      AND "outcome_failed_retryable" IS NULL
      END)
) STRICT, WITHOUT ROWID;
CREATE INDEX "ensure_by_inbox" ON "ensure" ("inbox");
CREATE INDEX "ensure_edges_by_edge" ON "ensure_edges" ("edge", "at");
CREATE INDEX "ensure_edges_by_time" ON "ensure_edges" ("at");
CREATE TABLE "referral" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "sender" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "session" INTEGER NOT NULL REFERENCES "resource_identity" ("id"),
  "in_reply_to" INTEGER REFERENCES "resource_identity" ("id"),
  "subject" INTEGER CHECK ("subject" IN (1, 2, 3)),
  "subject_convoy_name" TEXT,
  "subject_change_request_service" INTEGER REFERENCES "strings" ("id"),
  "subject_change_request_number" INTEGER,
  "subject_usage_provider" TEXT,
  "subject_usage_account" TEXT,
  "condition_present" INTEGER NOT NULL CHECK ("condition_present" IN (0, 1)),
  "condition_address" INTEGER CHECK ("condition_address" IN (1, 2, 3)),
  "condition_address_convoy_name" TEXT,
  "condition_address_change_request_service" INTEGER REFERENCES "strings" ("id"),
  "condition_address_change_request_number" INTEGER,
  "condition_address_usage_provider" TEXT,
  "condition_address_usage_account" TEXT,
  "condition_operator" INTEGER CHECK ("condition_operator" IN (1, 2, 3)),
  "condition_literal" TEXT,
  "sent_at" INTEGER,
  "seq" INTEGER NOT NULL,
  "ordinal" INTEGER NOT NULL,
  "phase" INTEGER NOT NULL CHECK ("phase" IN (1, 2, 3)),
  "phase_since" INTEGER NOT NULL,
  CHECK (CASE WHEN "subject" = 1
      THEN "subject_convoy_name" IS NOT NULL
      ELSE "subject_convoy_name" IS NULL
      END),
  CHECK (CASE WHEN "subject" = 2
      THEN "subject_change_request_service" IS NOT NULL
      AND "subject_change_request_number" IS NOT NULL
      ELSE "subject_change_request_service" IS NULL
      AND "subject_change_request_number" IS NULL
      END),
  CHECK (CASE WHEN "subject" = 3
      THEN "subject_usage_provider" IS NOT NULL
      AND "subject_usage_account" IS NOT NULL
      ELSE "subject_usage_provider" IS NULL
      AND "subject_usage_account" IS NULL
      END),
  CHECK (CASE WHEN "condition_present" = 1
      THEN "condition_address" IS NOT NULL
      AND "condition_operator" IS NOT NULL
      AND "condition_literal" IS NOT NULL
      ELSE "condition_address" IS NULL
      AND "condition_operator" IS NULL
      AND "condition_literal" IS NULL
      END),
  CHECK (CASE WHEN "condition_address" = 1
      THEN "condition_address_convoy_name" IS NOT NULL
      ELSE "condition_address_convoy_name" IS NULL
      END),
  CHECK (CASE WHEN "condition_address" = 2
      THEN "condition_address_change_request_service" IS NOT NULL
      AND "condition_address_change_request_number" IS NOT NULL
      ELSE "condition_address_change_request_service" IS NULL
      AND "condition_address_change_request_number" IS NULL
      END),
  CHECK (CASE WHEN "condition_address" = 3
      THEN "condition_address_usage_provider" IS NOT NULL
      AND "condition_address_usage_account" IS NOT NULL
      ELSE "condition_address_usage_provider" IS NULL
      AND "condition_address_usage_account" IS NULL
      END)
) STRICT;
CREATE TABLE "referral_references" (
  "owner" INTEGER NOT NULL,
  "pos" INTEGER NOT NULL,
  "tag" INTEGER NOT NULL CHECK ("tag" IN (1, 2, 3, 4, 5, 6, 7)),
  "change_request_service" INTEGER REFERENCES "strings" ("id"),
  "change_request_scope" TEXT,
  "change_request_number" INTEGER,
  "change_request_revision" TEXT,
  "commit_repository" INTEGER REFERENCES "resource_identity" ("id"),
  "commit_revision" TEXT,
  "ref_repository" INTEGER REFERENCES "resource_identity" ("id"),
  "ref_name" TEXT,
  "ref_revision" TEXT,
  "artifact_resource" INTEGER REFERENCES "resource_identity" ("id"),
  "artifact_revision" TEXT,
  "issue_service" INTEGER REFERENCES "strings" ("id"),
  "issue_scope" TEXT,
  "issue_number" INTEGER,
  "issue_revision" TEXT,
  "comment_service" INTEGER REFERENCES "strings" ("id"),
  "comment_scope" TEXT,
  "comment_id" TEXT,
  "comment_revision" TEXT,
  "control_record_resource" INTEGER REFERENCES "resource_identity" ("id"),
  "control_record_revision" TEXT,
  PRIMARY KEY ("owner", "pos"),
  FOREIGN KEY ("owner") REFERENCES "referral" ("id") ON DELETE CASCADE,
  CHECK (CASE WHEN "tag" = 1
      THEN "change_request_service" IS NOT NULL
      AND "change_request_scope" IS NOT NULL
      AND "change_request_number" IS NOT NULL
      AND "change_request_revision" IS NOT NULL
      ELSE "change_request_service" IS NULL
      AND "change_request_scope" IS NULL
      AND "change_request_number" IS NULL
      AND "change_request_revision" IS NULL
      END),
  CHECK (CASE WHEN "tag" = 2
      THEN "commit_repository" IS NOT NULL
      AND "commit_revision" IS NOT NULL
      ELSE "commit_repository" IS NULL
      AND "commit_revision" IS NULL
      END),
  CHECK (CASE WHEN "tag" = 3
      THEN "ref_repository" IS NOT NULL
      AND "ref_name" IS NOT NULL
      AND "ref_revision" IS NOT NULL
      ELSE "ref_repository" IS NULL
      AND "ref_name" IS NULL
      AND "ref_revision" IS NULL
      END),
  CHECK (CASE WHEN "tag" = 4
      THEN "artifact_resource" IS NOT NULL
      AND "artifact_revision" IS NOT NULL
      ELSE "artifact_resource" IS NULL
      AND "artifact_revision" IS NULL
      END),
  CHECK (CASE WHEN "tag" = 5
      THEN "issue_service" IS NOT NULL
      AND "issue_scope" IS NOT NULL
      AND "issue_number" IS NOT NULL
      AND "issue_revision" IS NOT NULL
      ELSE "issue_service" IS NULL
      AND "issue_scope" IS NULL
      AND "issue_number" IS NULL
      AND "issue_revision" IS NULL
      END),
  CHECK (CASE WHEN "tag" = 6
      THEN "comment_service" IS NOT NULL
      AND "comment_scope" IS NOT NULL
      AND "comment_id" IS NOT NULL
      AND "comment_revision" IS NOT NULL
      ELSE "comment_service" IS NULL
      AND "comment_scope" IS NULL
      AND "comment_id" IS NULL
      AND "comment_revision" IS NULL
      END),
  CHECK (CASE WHEN "tag" = 7
      THEN "control_record_resource" IS NOT NULL
      AND "control_record_revision" IS NOT NULL
      ELSE "control_record_resource" IS NULL
      AND "control_record_revision" IS NULL
      END)
) STRICT, WITHOUT ROWID;
CREATE TABLE "referral_edges" (
  "owner" INTEGER NOT NULL,
  "pos" INTEGER NOT NULL,
  "version" INTEGER NOT NULL,
  "edge" INTEGER NOT NULL CHECK ("edge" IN (1, 2, 3)),
  "from" INTEGER NOT NULL CHECK ("from" IN (1, 2, 3)),
  "to" INTEGER NOT NULL CHECK ("to" IN (1, 2, 3)),
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
CREATE TABLE "dossier" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "workflow" BLOB NOT NULL REFERENCES "objects" ("hash"),
  "placement" BLOB REFERENCES "objects" ("hash"),
  "instruction" TEXT,
  "state" INTEGER NOT NULL CHECK ("state" IN (1, 2, 3)),
  "state_since" INTEGER NOT NULL
) STRICT;
CREATE TABLE "dossier_inputs" (
  "owner" INTEGER NOT NULL,
  "key" TEXT NOT NULL,
  "tag" INTEGER NOT NULL CHECK ("tag" IN (1, 2, 3, 4)),
  "text_value" TEXT,
  "number_value" INTEGER,
  PRIMARY KEY ("owner", "key"),
  FOREIGN KEY ("owner") REFERENCES "dossier" ("id") ON DELETE CASCADE,
  CHECK (CASE WHEN "tag" = 1
      THEN "text_value" IS NOT NULL
      ELSE "text_value" IS NULL
      END),
  CHECK (CASE WHEN "tag" = 2
      THEN "number_value" IS NOT NULL
      ELSE "number_value" IS NULL
      END)
) STRICT, WITHOUT ROWID;
CREATE TABLE "dossier_inputs_list_items" (
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
  "kind" INTEGER NOT NULL CHECK ("kind" IN (1, 2)),
  "source" INTEGER NOT NULL CHECK ("source" IN (1, 2)),
  "source_workflow_step" INTEGER REFERENCES "strings" ("id"),
  "state" INTEGER NOT NULL CHECK ("state" IN (1, 2)),
  "state_since" INTEGER NOT NULL,
  PRIMARY KEY ("owner", "pos"),
  FOREIGN KEY ("owner") REFERENCES "dossier" ("id") ON DELETE CASCADE,
  CHECK (CASE WHEN "source" = 1
      THEN "source_workflow_step" IS NOT NULL
      ELSE "source_workflow_step" IS NULL
      END)
) STRICT, WITHOUT ROWID;
CREATE TABLE "dossier_deliverables_evidence" (
  "owner" INTEGER NOT NULL,
  "deliverables_pos" INTEGER NOT NULL,
  "pos" INTEGER NOT NULL,
  "version" INTEGER NOT NULL,
  "reference" TEXT NOT NULL,
  "submitted_at" INTEGER NOT NULL,
  "verdict_present" INTEGER NOT NULL CHECK ("verdict_present" IN (0, 1)),
  "verdict_accepted" INTEGER CHECK ("verdict_accepted" IN (0, 1)),
  "verdict_by" INTEGER REFERENCES "strings" ("id"),
  "verdict_at" INTEGER,
  PRIMARY KEY ("owner", "deliverables_pos", "pos"),
  FOREIGN KEY ("owner", "deliverables_pos") REFERENCES "dossier_deliverables" ("owner", "pos") ON DELETE CASCADE,
  CHECK (CASE WHEN "verdict_present" = 1
      THEN "verdict_accepted" IS NOT NULL
      AND "verdict_by" IS NOT NULL
      AND "verdict_at" IS NOT NULL
      ELSE "verdict_accepted" IS NULL
      AND "verdict_by" IS NULL
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
  "edge" INTEGER NOT NULL CHECK ("edge" IN (1, 2, 3, 4, 5)),
  "from" INTEGER NOT NULL CHECK ("from" IN (1, 2, 3)),
  "to" INTEGER NOT NULL CHECK ("to" IN (1, 2, 3)),
  "writer" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "at" INTEGER NOT NULL,
  "payload" BLOB NOT NULL,
  PRIMARY KEY ("owner", "pos"),
  FOREIGN KEY ("owner") REFERENCES "dossier" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE INDEX "dossier_edges_by_edge" ON "dossier_edges" ("edge", "at");
CREATE INDEX "dossier_edges_by_time" ON "dossier_edges" ("at");
