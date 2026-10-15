CREATE TABLE strings (id INTEGER PRIMARY KEY, s TEXT NOT NULL UNIQUE) STRICT;
CREATE TABLE resource_identity (
  id INTEGER PRIMARY KEY, kind INTEGER NOT NULL,
  name INTEGER NOT NULL REFERENCES strings(id), home INTEGER NOT NULL REFERENCES strings(id),
  seq INTEGER NOT NULL DEFAULT 0,
  UNIQUE (kind, name, home)) STRICT;
CREATE INDEX resource_identity_by_home ON resource_identity (home, kind);
CREATE TABLE objects (hash BLOB PRIMARY KEY, media INTEGER NOT NULL, bytes BLOB NOT NULL) STRICT;
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
  "edge" INTEGER NOT NULL CHECK ("edge" IN (1, 2, 3)),
  "edge_deliver_receiver" INTEGER REFERENCES "strings" ("id"),
  "edge_deliver_receipt" INTEGER REFERENCES "resource_identity" ("id"),
  "edge_deliver_transport" INTEGER CHECK ("edge_deliver_transport" IN (1, 2, 3)),
  "edge_settle_by" INTEGER CHECK ("edge_settle_by" IN (1, 2)),
  "edge_settle_by_answered_reply" INTEGER REFERENCES "resource_identity" ("id"),
  "edge_settle_by_outcome_observed_observation" INTEGER REFERENCES "resource_identity" ("id"),
  "edge_drop_reason" INTEGER CHECK ("edge_drop_reason" IN (1, 2, 3, 4, 5)),
  "edge_drop_reason_undeliverable_cause_category" INTEGER CHECK ("edge_drop_reason_undeliverable_cause_category" IN (1, 2, 3, 4, 5)),
  "edge_drop_reason_undeliverable_cause_detail" TEXT,
  "edge_drop_reason_withdrawn_by" INTEGER REFERENCES "strings" ("id"),
  "edge_drop_reason_withdrawn_authority" INTEGER REFERENCES "resource_identity" ("id"),
  "edge_drop_reason_consumer_unreachable_by" INTEGER REFERENCES "strings" ("id"),
  "edge_drop_reason_consumer_unreachable_authority" INTEGER REFERENCES "resource_identity" ("id"),
  "edge_drop_reason_superseded_by" INTEGER REFERENCES "resource_identity" ("id"),
  "at" INTEGER NOT NULL,
  "writer" INTEGER NOT NULL REFERENCES "strings" ("id"),
  PRIMARY KEY ("owner", "pos"),
  FOREIGN KEY ("owner") REFERENCES "message" ("id") ON DELETE CASCADE,
  CHECK (CASE WHEN "edge" = 1
      THEN "edge_deliver_receiver" IS NOT NULL
      AND "edge_deliver_receipt" IS NOT NULL
      AND "edge_deliver_transport" IS NOT NULL
      ELSE "edge_deliver_receiver" IS NULL
      AND "edge_deliver_receipt" IS NULL
      AND "edge_deliver_transport" IS NULL
      END),
  CHECK (CASE WHEN "edge" = 2
      THEN "edge_settle_by" IS NOT NULL
      ELSE "edge_settle_by" IS NULL
      END),
  CHECK (CASE WHEN "edge_settle_by" = 1
      THEN "edge_settle_by_answered_reply" IS NOT NULL
      ELSE "edge_settle_by_answered_reply" IS NULL
      END),
  CHECK (CASE WHEN "edge_settle_by" = 2
      THEN "edge_settle_by_outcome_observed_observation" IS NOT NULL
      ELSE "edge_settle_by_outcome_observed_observation" IS NULL
      END),
  CHECK (CASE WHEN "edge" = 3
      THEN "edge_drop_reason" IS NOT NULL
      ELSE "edge_drop_reason" IS NULL
      END),
  CHECK (CASE WHEN "edge_drop_reason" = 1
      THEN "edge_drop_reason_undeliverable_cause_category" IS NOT NULL
      AND "edge_drop_reason_undeliverable_cause_detail" IS NOT NULL
      ELSE "edge_drop_reason_undeliverable_cause_category" IS NULL
      AND "edge_drop_reason_undeliverable_cause_detail" IS NULL
      END),
  CHECK (CASE WHEN "edge_drop_reason" = 3
      THEN "edge_drop_reason_withdrawn_by" IS NOT NULL
      AND "edge_drop_reason_withdrawn_authority" IS NOT NULL
      ELSE "edge_drop_reason_withdrawn_by" IS NULL
      AND "edge_drop_reason_withdrawn_authority" IS NULL
      END),
  CHECK (CASE WHEN "edge_drop_reason" = 4
      THEN "edge_drop_reason_consumer_unreachable_by" IS NOT NULL
      AND "edge_drop_reason_consumer_unreachable_authority" IS NOT NULL
      ELSE "edge_drop_reason_consumer_unreachable_by" IS NULL
      AND "edge_drop_reason_consumer_unreachable_authority" IS NULL
      END),
  CHECK (CASE WHEN "edge_drop_reason" = 5
      THEN "edge_drop_reason_superseded_by" IS NOT NULL
      ELSE "edge_drop_reason_superseded_by" IS NULL
      END)
) STRICT, WITHOUT ROWID;
CREATE TABLE "message_edges_inputs" (
  "owner" INTEGER NOT NULL,
  "edges_pos" INTEGER NOT NULL,
  "pos" INTEGER NOT NULL,
  "observed" INTEGER NOT NULL REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  PRIMARY KEY ("owner", "edges_pos", "pos"),
  FOREIGN KEY ("owner", "edges_pos") REFERENCES "message_edges" ("owner", "pos") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE TABLE "message_attempts" (
  "owner" INTEGER NOT NULL,
  "pos" INTEGER NOT NULL,
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
CREATE TABLE "terminal_session" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
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
  "edge" INTEGER NOT NULL CHECK ("edge" IN (1, 2, 3, 4)),
  "edge_realise_identity_session_id" TEXT,
  "edge_realise_identity_pid" INTEGER,
  "edge_realise_identity_endpoint" TEXT,
  "edge_give_up_cause_category" INTEGER CHECK ("edge_give_up_cause_category" IN (1, 2, 3, 4, 5)),
  "edge_give_up_cause_detail" TEXT,
  "at" INTEGER NOT NULL,
  "writer" INTEGER NOT NULL REFERENCES "strings" ("id"),
  PRIMARY KEY ("owner", "pos"),
  FOREIGN KEY ("owner") REFERENCES "terminal_session" ("id") ON DELETE CASCADE,
  CHECK (CASE WHEN "edge" = 1
      THEN "edge_realise_identity_session_id" IS NOT NULL
      ELSE "edge_realise_identity_session_id" IS NULL
      AND "edge_realise_identity_pid" IS NULL
      AND "edge_realise_identity_endpoint" IS NULL
      END),
  CHECK (CASE WHEN "edge" = 4
      THEN "edge_give_up_cause_category" IS NOT NULL
      AND "edge_give_up_cause_detail" IS NOT NULL
      ELSE "edge_give_up_cause_category" IS NULL
      AND "edge_give_up_cause_detail" IS NULL
      END)
) STRICT, WITHOUT ROWID;
CREATE TABLE "terminal_session_attempts" (
  "owner" INTEGER NOT NULL,
  "pos" INTEGER NOT NULL,
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
