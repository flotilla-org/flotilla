CREATE TABLE strings (id INTEGER PRIMARY KEY, s TEXT NOT NULL UNIQUE) STRICT;
CREATE TABLE resource_identity (
  id INTEGER PRIMARY KEY, kind INTEGER NOT NULL,
  name INTEGER NOT NULL REFERENCES strings(id), home INTEGER NOT NULL REFERENCES strings(id),
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
  "last_edge" INTEGER CHECK ("last_edge" IN (1, 2, 3)),
  "last_edge_deliver_receiver" INTEGER REFERENCES "strings" ("id"),
  "last_edge_deliver_receipt" INTEGER REFERENCES "resource_identity" ("id"),
  "last_edge_deliver_transport" INTEGER CHECK ("last_edge_deliver_transport" IN (1, 2, 3)),
  "last_edge_settle_by" INTEGER CHECK ("last_edge_settle_by" IN (1, 2)),
  "last_edge_settle_by_answered_reply" INTEGER REFERENCES "resource_identity" ("id"),
  "last_edge_settle_by_outcome_observed_observation" INTEGER REFERENCES "resource_identity" ("id"),
  "last_edge_drop_reason" INTEGER CHECK ("last_edge_drop_reason" IN (1, 2, 3, 4, 5)),
  "last_edge_drop_reason_undeliverable_cause_category" INTEGER CHECK ("last_edge_drop_reason_undeliverable_cause_category" IN (1, 2, 3, 4, 5)),
  "last_edge_drop_reason_undeliverable_cause_detail" TEXT,
  "last_edge_drop_reason_withdrawn_by" INTEGER REFERENCES "strings" ("id"),
  "last_edge_drop_reason_withdrawn_authority" INTEGER REFERENCES "resource_identity" ("id"),
  "last_edge_drop_reason_consumer_unreachable_by" INTEGER REFERENCES "strings" ("id"),
  "last_edge_drop_reason_consumer_unreachable_authority" INTEGER REFERENCES "resource_identity" ("id"),
  "last_edge_drop_reason_superseded_by" INTEGER REFERENCES "resource_identity" ("id"),
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
      END),
  CHECK (CASE WHEN "last_edge" = 1
      THEN "last_edge_deliver_receiver" IS NOT NULL
      AND "last_edge_deliver_receipt" IS NOT NULL
      AND "last_edge_deliver_transport" IS NOT NULL
      ELSE "last_edge_deliver_receiver" IS NULL
      AND "last_edge_deliver_receipt" IS NULL
      AND "last_edge_deliver_transport" IS NULL
      END),
  CHECK (CASE WHEN "last_edge" = 2
      THEN "last_edge_settle_by" IS NOT NULL
      ELSE "last_edge_settle_by" IS NULL
      END),
  CHECK (CASE WHEN "last_edge_settle_by" = 1
      THEN "last_edge_settle_by_answered_reply" IS NOT NULL
      ELSE "last_edge_settle_by_answered_reply" IS NULL
      END),
  CHECK (CASE WHEN "last_edge_settle_by" = 2
      THEN "last_edge_settle_by_outcome_observed_observation" IS NOT NULL
      ELSE "last_edge_settle_by_outcome_observed_observation" IS NULL
      END),
  CHECK (CASE WHEN "last_edge" = 3
      THEN "last_edge_drop_reason" IS NOT NULL
      ELSE "last_edge_drop_reason" IS NULL
      END),
  CHECK (CASE WHEN "last_edge_drop_reason" = 1
      THEN "last_edge_drop_reason_undeliverable_cause_category" IS NOT NULL
      AND "last_edge_drop_reason_undeliverable_cause_detail" IS NOT NULL
      ELSE "last_edge_drop_reason_undeliverable_cause_category" IS NULL
      AND "last_edge_drop_reason_undeliverable_cause_detail" IS NULL
      END),
  CHECK (CASE WHEN "last_edge_drop_reason" = 3
      THEN "last_edge_drop_reason_withdrawn_by" IS NOT NULL
      AND "last_edge_drop_reason_withdrawn_authority" IS NOT NULL
      ELSE "last_edge_drop_reason_withdrawn_by" IS NULL
      AND "last_edge_drop_reason_withdrawn_authority" IS NULL
      END),
  CHECK (CASE WHEN "last_edge_drop_reason" = 4
      THEN "last_edge_drop_reason_consumer_unreachable_by" IS NOT NULL
      AND "last_edge_drop_reason_consumer_unreachable_authority" IS NOT NULL
      ELSE "last_edge_drop_reason_consumer_unreachable_by" IS NULL
      AND "last_edge_drop_reason_consumer_unreachable_authority" IS NULL
      END),
  CHECK (CASE WHEN "last_edge_drop_reason" = 5
      THEN "last_edge_drop_reason_superseded_by" IS NOT NULL
      ELSE "last_edge_drop_reason_superseded_by" IS NULL
      END)
) STRICT;
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
