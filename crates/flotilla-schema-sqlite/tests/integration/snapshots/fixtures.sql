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
  "sender" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "inbox" INTEGER NOT NULL REFERENCES "resource_identity" ("id"),
  "relation" INTEGER NOT NULL CHECK ("relation" IN (1, 2, 3, 4, 5)),
  "body" TEXT NOT NULL,
  "body_digest" TEXT,
  "subject" INTEGER CHECK ("subject" IN (1, 2, 3, 4, 5, 6, 7)),
  "subject_change_request_service" INTEGER REFERENCES "strings" ("id"),
  "subject_change_request_scope" TEXT,
  "subject_change_request_number" INTEGER,
  "subject_change_request_revision" TEXT,
  "subject_commit_repository" INTEGER REFERENCES "resource_identity" ("id"),
  "subject_commit_revision" TEXT,
  "subject_ref_repository" INTEGER REFERENCES "resource_identity" ("id"),
  "subject_ref_name" TEXT,
  "subject_ref_revision" TEXT,
  "subject_artifact_resource" INTEGER REFERENCES "resource_identity" ("id"),
  "subject_artifact_revision" TEXT,
  "subject_issue_service" INTEGER REFERENCES "strings" ("id"),
  "subject_issue_scope" TEXT,
  "subject_issue_number" INTEGER,
  "subject_issue_revision" TEXT,
  "subject_comment_service" INTEGER REFERENCES "strings" ("id"),
  "subject_comment_scope" TEXT,
  "subject_comment_id" TEXT,
  "subject_comment_revision" TEXT,
  "subject_control_record_resource" INTEGER REFERENCES "resource_identity" ("id"),
  "subject_control_record_revision" TEXT,
  "expectation" INTEGER NOT NULL CHECK ("expectation" IN (1, 2, 3)),
  "expectation_outcome_condition_address" INTEGER CHECK ("expectation_outcome_condition_address" IN (1, 2, 3, 4, 5, 6, 7)),
  "expectation_outcome_condition_address_convoy_name" TEXT,
  "expectation_outcome_condition_address_vessel_name" TEXT,
  "expectation_outcome_condition_address_work_convoy" TEXT,
  "expectation_outcome_condition_address_work_work" TEXT,
  "expectation_outcome_condition_address_change_request_service" INTEGER REFERENCES "strings" ("id"),
  "expectation_outcome_condition_address_change_request_scope" TEXT,
  "expectation_outcome_condition_address_change_request_number" INTEGER,
  "expectation_outcome_condition_address_issue_service" INTEGER REFERENCES "strings" ("id"),
  "expectation_outcome_condition_address_issue_scope" TEXT,
  "expectation_outcome_condition_address_issue_number" INTEGER,
  "expectation_outcome_condition_address_usage_provider" TEXT,
  "expectation_outcome_condition_address_usage_account" TEXT,
  "expectation_outcome_condition_address_artifact_convoy" TEXT,
  "expectation_outcome_condition_address_artifact_producer" TEXT,
  "expectation_outcome_condition_address_artifact_kind" TEXT,
  "expectation_outcome_condition_address_artifact_subject" TEXT,
  "expectation_outcome_condition_field_path" TEXT,
  "expectation_outcome_condition_operator" INTEGER CHECK ("expectation_outcome_condition_operator" IN (1, 2, 3, 4, 5, 6)),
  "expectation_outcome_condition_literal" TEXT,
  "in_reply_to" INTEGER REFERENCES "resource_identity" ("id"),
  "deadline" INTEGER,
  "supersedes" INTEGER REFERENCES "resource_identity" ("id"),
  "interrupting" INTEGER NOT NULL CHECK ("interrupting" IN (0, 1)),
  "delivery_condition_present" INTEGER NOT NULL CHECK ("delivery_condition_present" IN (0, 1)),
  "delivery_condition_address" INTEGER CHECK ("delivery_condition_address" IN (1, 2, 3, 4, 5, 6, 7)),
  "delivery_condition_address_convoy_name" TEXT,
  "delivery_condition_address_vessel_name" TEXT,
  "delivery_condition_address_work_convoy" TEXT,
  "delivery_condition_address_work_work" TEXT,
  "delivery_condition_address_change_request_service" INTEGER REFERENCES "strings" ("id"),
  "delivery_condition_address_change_request_scope" TEXT,
  "delivery_condition_address_change_request_number" INTEGER,
  "delivery_condition_address_issue_service" INTEGER REFERENCES "strings" ("id"),
  "delivery_condition_address_issue_scope" TEXT,
  "delivery_condition_address_issue_number" INTEGER,
  "delivery_condition_address_usage_provider" TEXT,
  "delivery_condition_address_usage_account" TEXT,
  "delivery_condition_address_artifact_convoy" TEXT,
  "delivery_condition_address_artifact_producer" TEXT,
  "delivery_condition_address_artifact_kind" TEXT,
  "delivery_condition_address_artifact_subject" TEXT,
  "delivery_condition_field_path" TEXT,
  "delivery_condition_operator" INTEGER CHECK ("delivery_condition_operator" IN (1, 2, 3, 4, 5, 6)),
  "delivery_condition_literal" TEXT,
  "accepted_seq" INTEGER NOT NULL,
  "ordinal" INTEGER NOT NULL,
  "state" INTEGER NOT NULL CHECK ("state" IN (1, 2, 3, 4)),
  "state_since" INTEGER NOT NULL,
  CHECK (CASE WHEN "subject" = 1
      THEN "subject_change_request_service" IS NOT NULL
      AND "subject_change_request_scope" IS NOT NULL
      AND "subject_change_request_number" IS NOT NULL
      AND "subject_change_request_revision" IS NOT NULL
      ELSE "subject_change_request_service" IS NULL
      AND "subject_change_request_scope" IS NULL
      AND "subject_change_request_number" IS NULL
      AND "subject_change_request_revision" IS NULL
      END),
  CHECK (CASE WHEN "subject" = 2
      THEN "subject_commit_repository" IS NOT NULL
      AND "subject_commit_revision" IS NOT NULL
      ELSE "subject_commit_repository" IS NULL
      AND "subject_commit_revision" IS NULL
      END),
  CHECK (CASE WHEN "subject" = 3
      THEN "subject_ref_repository" IS NOT NULL
      AND "subject_ref_name" IS NOT NULL
      AND "subject_ref_revision" IS NOT NULL
      ELSE "subject_ref_repository" IS NULL
      AND "subject_ref_name" IS NULL
      AND "subject_ref_revision" IS NULL
      END),
  CHECK (CASE WHEN "subject" = 4
      THEN "subject_artifact_resource" IS NOT NULL
      AND "subject_artifact_revision" IS NOT NULL
      ELSE "subject_artifact_resource" IS NULL
      AND "subject_artifact_revision" IS NULL
      END),
  CHECK (CASE WHEN "subject" = 5
      THEN "subject_issue_service" IS NOT NULL
      AND "subject_issue_scope" IS NOT NULL
      AND "subject_issue_number" IS NOT NULL
      AND "subject_issue_revision" IS NOT NULL
      ELSE "subject_issue_service" IS NULL
      AND "subject_issue_scope" IS NULL
      AND "subject_issue_number" IS NULL
      AND "subject_issue_revision" IS NULL
      END),
  CHECK (CASE WHEN "subject" = 6
      THEN "subject_comment_service" IS NOT NULL
      AND "subject_comment_scope" IS NOT NULL
      AND "subject_comment_id" IS NOT NULL
      AND "subject_comment_revision" IS NOT NULL
      ELSE "subject_comment_service" IS NULL
      AND "subject_comment_scope" IS NULL
      AND "subject_comment_id" IS NULL
      AND "subject_comment_revision" IS NULL
      END),
  CHECK (CASE WHEN "subject" = 7
      THEN "subject_control_record_resource" IS NOT NULL
      AND "subject_control_record_revision" IS NOT NULL
      ELSE "subject_control_record_resource" IS NULL
      AND "subject_control_record_revision" IS NULL
      END),
  CHECK (CASE WHEN "expectation" = 3
      THEN "expectation_outcome_condition_address" IS NOT NULL
      AND "expectation_outcome_condition_field_path" IS NOT NULL
      AND "expectation_outcome_condition_operator" IS NOT NULL
      AND "expectation_outcome_condition_literal" IS NOT NULL
      ELSE "expectation_outcome_condition_address" IS NULL
      AND "expectation_outcome_condition_field_path" IS NULL
      AND "expectation_outcome_condition_operator" IS NULL
      AND "expectation_outcome_condition_literal" IS NULL
      END),
  CHECK (CASE WHEN "expectation_outcome_condition_address" = 1
      THEN "expectation_outcome_condition_address_convoy_name" IS NOT NULL
      ELSE "expectation_outcome_condition_address_convoy_name" IS NULL
      END),
  CHECK (CASE WHEN "expectation_outcome_condition_address" = 2
      THEN "expectation_outcome_condition_address_vessel_name" IS NOT NULL
      ELSE "expectation_outcome_condition_address_vessel_name" IS NULL
      END),
  CHECK (CASE WHEN "expectation_outcome_condition_address" = 3
      THEN "expectation_outcome_condition_address_work_convoy" IS NOT NULL
      AND "expectation_outcome_condition_address_work_work" IS NOT NULL
      ELSE "expectation_outcome_condition_address_work_convoy" IS NULL
      AND "expectation_outcome_condition_address_work_work" IS NULL
      END),
  CHECK (CASE WHEN "expectation_outcome_condition_address" = 4
      THEN "expectation_outcome_condition_address_change_request_service" IS NOT NULL
      AND "expectation_outcome_condition_address_change_request_scope" IS NOT NULL
      AND "expectation_outcome_condition_address_change_request_number" IS NOT NULL
      ELSE "expectation_outcome_condition_address_change_request_service" IS NULL
      AND "expectation_outcome_condition_address_change_request_scope" IS NULL
      AND "expectation_outcome_condition_address_change_request_number" IS NULL
      END),
  CHECK (CASE WHEN "expectation_outcome_condition_address" = 5
      THEN "expectation_outcome_condition_address_issue_service" IS NOT NULL
      AND "expectation_outcome_condition_address_issue_scope" IS NOT NULL
      AND "expectation_outcome_condition_address_issue_number" IS NOT NULL
      ELSE "expectation_outcome_condition_address_issue_service" IS NULL
      AND "expectation_outcome_condition_address_issue_scope" IS NULL
      AND "expectation_outcome_condition_address_issue_number" IS NULL
      END),
  CHECK (CASE WHEN "expectation_outcome_condition_address" = 6
      THEN "expectation_outcome_condition_address_usage_provider" IS NOT NULL
      AND "expectation_outcome_condition_address_usage_account" IS NOT NULL
      ELSE "expectation_outcome_condition_address_usage_provider" IS NULL
      AND "expectation_outcome_condition_address_usage_account" IS NULL
      END),
  CHECK (CASE WHEN "expectation_outcome_condition_address" = 7
      THEN "expectation_outcome_condition_address_artifact_convoy" IS NOT NULL
      AND "expectation_outcome_condition_address_artifact_producer" IS NOT NULL
      AND "expectation_outcome_condition_address_artifact_kind" IS NOT NULL
      AND "expectation_outcome_condition_address_artifact_subject" IS NOT NULL
      ELSE "expectation_outcome_condition_address_artifact_convoy" IS NULL
      AND "expectation_outcome_condition_address_artifact_producer" IS NULL
      AND "expectation_outcome_condition_address_artifact_kind" IS NULL
      AND "expectation_outcome_condition_address_artifact_subject" IS NULL
      END),
  CHECK (CASE WHEN "delivery_condition_present" = 1
      THEN "delivery_condition_address" IS NOT NULL
      AND "delivery_condition_field_path" IS NOT NULL
      AND "delivery_condition_operator" IS NOT NULL
      AND "delivery_condition_literal" IS NOT NULL
      ELSE "delivery_condition_address" IS NULL
      AND "delivery_condition_field_path" IS NULL
      AND "delivery_condition_operator" IS NULL
      AND "delivery_condition_literal" IS NULL
      END),
  CHECK (CASE WHEN "delivery_condition_address" = 1
      THEN "delivery_condition_address_convoy_name" IS NOT NULL
      ELSE "delivery_condition_address_convoy_name" IS NULL
      END),
  CHECK (CASE WHEN "delivery_condition_address" = 2
      THEN "delivery_condition_address_vessel_name" IS NOT NULL
      ELSE "delivery_condition_address_vessel_name" IS NULL
      END),
  CHECK (CASE WHEN "delivery_condition_address" = 3
      THEN "delivery_condition_address_work_convoy" IS NOT NULL
      AND "delivery_condition_address_work_work" IS NOT NULL
      ELSE "delivery_condition_address_work_convoy" IS NULL
      AND "delivery_condition_address_work_work" IS NULL
      END),
  CHECK (CASE WHEN "delivery_condition_address" = 4
      THEN "delivery_condition_address_change_request_service" IS NOT NULL
      AND "delivery_condition_address_change_request_scope" IS NOT NULL
      AND "delivery_condition_address_change_request_number" IS NOT NULL
      ELSE "delivery_condition_address_change_request_service" IS NULL
      AND "delivery_condition_address_change_request_scope" IS NULL
      AND "delivery_condition_address_change_request_number" IS NULL
      END),
  CHECK (CASE WHEN "delivery_condition_address" = 5
      THEN "delivery_condition_address_issue_service" IS NOT NULL
      AND "delivery_condition_address_issue_scope" IS NOT NULL
      AND "delivery_condition_address_issue_number" IS NOT NULL
      ELSE "delivery_condition_address_issue_service" IS NULL
      AND "delivery_condition_address_issue_scope" IS NULL
      AND "delivery_condition_address_issue_number" IS NULL
      END),
  CHECK (CASE WHEN "delivery_condition_address" = 6
      THEN "delivery_condition_address_usage_provider" IS NOT NULL
      AND "delivery_condition_address_usage_account" IS NOT NULL
      ELSE "delivery_condition_address_usage_provider" IS NULL
      AND "delivery_condition_address_usage_account" IS NULL
      END),
  CHECK (CASE WHEN "delivery_condition_address" = 7
      THEN "delivery_condition_address_artifact_convoy" IS NOT NULL
      AND "delivery_condition_address_artifact_producer" IS NOT NULL
      AND "delivery_condition_address_artifact_kind" IS NOT NULL
      AND "delivery_condition_address_artifact_subject" IS NOT NULL
      ELSE "delivery_condition_address_artifact_convoy" IS NULL
      AND "delivery_condition_address_artifact_producer" IS NULL
      AND "delivery_condition_address_artifact_kind" IS NULL
      AND "delivery_condition_address_artifact_subject" IS NULL
      END)
) STRICT;
CREATE TABLE "message_references" (
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
  FOREIGN KEY ("owner") REFERENCES "message" ("id") ON DELETE CASCADE,
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
CREATE TABLE "message_attempts" (
  "owner" INTEGER NOT NULL,
  "pos" INTEGER NOT NULL,
  "version" INTEGER NOT NULL,
  "what" TEXT NOT NULL,
  "target" TEXT NOT NULL,
  "at" INTEGER NOT NULL,
  "outcome" INTEGER NOT NULL CHECK ("outcome" IN (1, 2)),
  "outcome_failure_cause_category" INTEGER CHECK ("outcome_failure_cause_category" IN (1, 2, 3, 4, 5)),
  "outcome_failure_cause_detail" TEXT,
  "outcome_failure_retryable" INTEGER CHECK ("outcome_failure_retryable" IN (0, 1)),
  PRIMARY KEY ("owner", "pos"),
  FOREIGN KEY ("owner") REFERENCES "message" ("id") ON DELETE CASCADE,
  CHECK (CASE WHEN "outcome" = 2
      THEN "outcome_failure_cause_category" IS NOT NULL
      AND "outcome_failure_cause_detail" IS NOT NULL
      AND "outcome_failure_retryable" IS NOT NULL
      ELSE "outcome_failure_cause_category" IS NULL
      AND "outcome_failure_cause_detail" IS NULL
      AND "outcome_failure_retryable" IS NULL
      END)
) STRICT, WITHOUT ROWID;
CREATE INDEX "message_by_reply" ON "message" ("in_reply_to") WHERE "in_reply_to" IS NOT NULL;
CREATE INDEX "message_deadline" ON "message" ("deadline") WHERE "state" IN (1, 2) AND "deadline" IS NOT NULL;
CREATE INDEX "message_edges_by_edge" ON "message_edges" ("edge", "at");
CREATE INDEX "message_edges_by_time" ON "message_edges" ("at");
CREATE INDEX "message_open_by_inbox" ON "message" ("inbox", "accepted_seq") WHERE "state" IN (1, 2);
CREATE TABLE "inbox" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "role" TEXT NOT NULL
) STRICT;
CREATE TABLE "agent_session" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "harness" TEXT NOT NULL
) STRICT;
CREATE TABLE "project" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "title" TEXT NOT NULL
) STRICT;
CREATE TABLE "convoy" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "project" INTEGER REFERENCES "resource_identity" ("id"),
  "workflow_ref" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "workflow_snapshot" BLOB NOT NULL REFERENCES "objects" ("hash"),
  "placement_snapshot" BLOB REFERENCES "objects" ("hash"),
  "dispatching_principal" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "instruction" TEXT,
  "state" INTEGER NOT NULL CHECK ("state" IN (1, 2, 3)),
  "state_since" INTEGER NOT NULL
) STRICT;
CREATE TABLE "convoy_repositories" (
  "owner" INTEGER NOT NULL,
  "pos" INTEGER NOT NULL,
  "url" TEXT NOT NULL,
  "repo_key" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "source_ref" TEXT NOT NULL,
  "target_ref" TEXT NOT NULL,
  "workspace_slug" TEXT NOT NULL,
  PRIMARY KEY ("owner", "pos"),
  FOREIGN KEY ("owner") REFERENCES "convoy" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE TABLE "convoy_inputs" (
  "owner" INTEGER NOT NULL,
  "key" TEXT NOT NULL,
  "tag" INTEGER NOT NULL CHECK ("tag" IN (1, 2, 3, 4, 5)),
  "text_value" TEXT,
  "number_value" INTEGER,
  "flag_value" INTEGER CHECK ("flag_value" IN (0, 1)),
  PRIMARY KEY ("owner", "key"),
  FOREIGN KEY ("owner") REFERENCES "convoy" ("id") ON DELETE CASCADE,
  CHECK (CASE WHEN "tag" = 1
      THEN "text_value" IS NOT NULL
      ELSE "text_value" IS NULL
      END),
  CHECK (CASE WHEN "tag" = 2
      THEN "number_value" IS NOT NULL
      ELSE "number_value" IS NULL
      END),
  CHECK (CASE WHEN "tag" = 3
      THEN "flag_value" IS NOT NULL
      ELSE "flag_value" IS NULL
      END)
) STRICT, WITHOUT ROWID;
CREATE TABLE "convoy_inputs_list_items" (
  "owner" INTEGER NOT NULL,
  "inputs_key" TEXT NOT NULL,
  "pos" INTEGER NOT NULL,
  "value" TEXT NOT NULL,
  PRIMARY KEY ("owner", "inputs_key", "pos"),
  FOREIGN KEY ("owner", "inputs_key") REFERENCES "convoy_inputs" ("owner", "key") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE TABLE "convoy_deliverables" (
  "owner" INTEGER NOT NULL,
  "pos" INTEGER NOT NULL,
  "version" INTEGER NOT NULL,
  "label" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "kind" INTEGER NOT NULL CHECK ("kind" IN (1, 2, 3)),
  "role" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "source" INTEGER NOT NULL CHECK ("source" IN (1, 2, 3)),
  "source_workflow_step" INTEGER REFERENCES "strings" ("id"),
  "source_agent_session" INTEGER REFERENCES "resource_identity" ("id"),
  "state" INTEGER NOT NULL CHECK ("state" IN (1, 2)),
  "state_since" INTEGER NOT NULL,
  PRIMARY KEY ("owner", "pos"),
  FOREIGN KEY ("owner") REFERENCES "convoy" ("id") ON DELETE CASCADE,
  CHECK (CASE WHEN "source" = 1
      THEN "source_workflow_step" IS NOT NULL
      ELSE "source_workflow_step" IS NULL
      END),
  CHECK (CASE WHEN "source" = 3
      THEN "source_agent_session" IS NOT NULL
      ELSE "source_agent_session" IS NULL
      END)
) STRICT, WITHOUT ROWID;
CREATE TABLE "convoy_deliverables_evidence" (
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
  "verdict_why" TEXT,
  PRIMARY KEY ("owner", "deliverables_pos", "pos"),
  FOREIGN KEY ("owner", "deliverables_pos") REFERENCES "convoy_deliverables" ("owner", "pos") ON DELETE CASCADE,
  CHECK (CASE WHEN "verdict_present" = 1
      THEN "verdict_accepted" IS NOT NULL
      AND "verdict_by" IS NOT NULL
      AND "verdict_at" IS NOT NULL
      AND "verdict_why" IS NOT NULL
      ELSE "verdict_accepted" IS NULL
      AND "verdict_by" IS NULL
      AND "verdict_at" IS NULL
      AND "verdict_why" IS NULL
      END)
) STRICT, WITHOUT ROWID;
CREATE TABLE "convoy_deliverables_evidence_metadata" (
  "owner" INTEGER NOT NULL,
  "deliverables_pos" INTEGER NOT NULL,
  "evidence_pos" INTEGER NOT NULL,
  "key" TEXT NOT NULL,
  "value" TEXT NOT NULL,
  PRIMARY KEY ("owner", "deliverables_pos", "evidence_pos", "key"),
  FOREIGN KEY ("owner", "deliverables_pos", "evidence_pos") REFERENCES "convoy_deliverables_evidence" ("owner", "deliverables_pos", "pos") ON DELETE CASCADE
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
CREATE INDEX "convoy_open_by_project" ON "convoy" ("project") WHERE "state" IN (1) AND "project" IS NOT NULL;
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
  "realised_present" INTEGER NOT NULL CHECK ("realised_present" IN (0, 1)),
  "realised_session_id" TEXT,
  "realised_pid" INTEGER,
  "realised_endpoint" TEXT,
  "lifecycle" INTEGER NOT NULL CHECK ("lifecycle" IN (1, 2, 3, 4)),
  "lifecycle_since" INTEGER NOT NULL,
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
  CHECK (CASE WHEN "realised_present" = 1
      THEN "realised_session_id" IS NOT NULL
      ELSE "realised_session_id" IS NULL
      AND "realised_pid" IS NULL
      AND "realised_endpoint" IS NULL
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
  "what" INTEGER NOT NULL CHECK ("what" IN (1, 2, 3)),
  "target" TEXT NOT NULL,
  "at" INTEGER NOT NULL,
  "outcome" INTEGER NOT NULL CHECK ("outcome" IN (1, 2)),
  "outcome_failure_cause_category" INTEGER CHECK ("outcome_failure_cause_category" IN (1, 2, 3, 4, 5)),
  "outcome_failure_cause_detail" TEXT,
  "outcome_failure_retryable" INTEGER CHECK ("outcome_failure_retryable" IN (0, 1)),
  PRIMARY KEY ("owner", "pos"),
  FOREIGN KEY ("owner") REFERENCES "terminal_session" ("id") ON DELETE CASCADE,
  CHECK (CASE WHEN "outcome" = 2
      THEN "outcome_failure_cause_category" IS NOT NULL
      AND "outcome_failure_cause_detail" IS NOT NULL
      AND "outcome_failure_retryable" IS NOT NULL
      ELSE "outcome_failure_cause_category" IS NULL
      AND "outcome_failure_cause_detail" IS NULL
      AND "outcome_failure_retryable" IS NULL
      END)
) STRICT, WITHOUT ROWID;
CREATE INDEX "terminal_session_by_vessel" ON "terminal_session" ("vessel", "ordinal");
CREATE INDEX "terminal_session_events_by_edge" ON "terminal_session_events" ("edge", "at");
CREATE INDEX "terminal_session_events_by_time" ON "terminal_session_events" ("at");
CREATE INDEX "terminal_session_live_by_pool" ON "terminal_session" ("pool") WHERE "lifecycle" IN (1, 2);
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
