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
CREATE TABLE codes (
  kind TEXT NOT NULL, field TEXT NOT NULL, code TEXT NOT NULL, name TEXT NOT NULL,
  PRIMARY KEY (kind, field, code)) STRICT, WITHOUT ROWID;
CREATE TABLE "change_request" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "service" TEXT NOT NULL,
  "scope" TEXT NOT NULL,
  "number" INTEGER NOT NULL,
  "title" TEXT,
  "author" TEXT,
  "state" TEXT CHECK ("state" IN ('cl', 'dr', 'mg', 'op')),
  "review_decision" TEXT CHECK ("review_decision" IN ('ap', 'cr', 'no', 'rq')),
  "review_requested_from_owner" INTEGER CHECK ("review_requested_from_owner" IN (0, 1)),
  "head_sha" TEXT,
  "checks" TEXT CHECK ("checks" IN ('fa', 'pa', 'pe')),
  "actionable_at_head" INTEGER CHECK ("actionable_at_head" IN (0, 1)),
  "mergeable" TEXT CHECK ("mergeable" IN ('co', 'me')),
  "failing_since" INTEGER,
  "head_branch" TEXT,
  "base_branch" TEXT,
  "closes" BLOB NOT NULL DEFAULT x'f6'
) STRICT;
CREATE INDEX "change_request_by_branch" ON "change_request" ("service", "scope", "head_branch");
CREATE INDEX "change_request_by_subject" ON "change_request" ("service", "scope", "number");
INSERT INTO "codes" ("kind", "field", "code", "name") VALUES
  ('cq', 'state', 'cl', 'Closed'),
  ('cq', 'state', 'dr', 'Draft'),
  ('cq', 'state', 'mg', 'Merged'),
  ('cq', 'state', 'op', 'Open'),
  ('cq', 'review_decision', 'ap', 'Approved'),
  ('cq', 'review_decision', 'cr', 'ChangesRequested'),
  ('cq', 'review_decision', 'no', 'None'),
  ('cq', 'review_decision', 'rq', 'Required'),
  ('cq', 'checks', 'fa', 'Fail'),
  ('cq', 'checks', 'pa', 'Pass'),
  ('cq', 'checks', 'pe', 'Pending'),
  ('cq', 'mergeable', 'co', 'Conflicting'),
  ('cq', 'mergeable', 'me', 'Mergeable');
CREATE VIEW "change_request_view" AS SELECT
  "change_request"."id",
  "change_request"."version",
  "change_request"."service",
  "change_request"."scope",
  "change_request"."number",
  "change_request"."title",
  "change_request"."author",
  (SELECT "name" FROM "codes" WHERE "kind" = 'cq' AND "field" = 'state' AND "code" = "change_request"."state") AS "state",
  (SELECT "name" FROM "codes" WHERE "kind" = 'cq' AND "field" = 'review_decision' AND "code" = "change_request"."review_decision") AS "review_decision",
  "change_request"."review_requested_from_owner",
  "change_request"."head_sha",
  (SELECT "name" FROM "codes" WHERE "kind" = 'cq' AND "field" = 'checks' AND "code" = "change_request"."checks") AS "checks",
  "change_request"."actionable_at_head",
  (SELECT "name" FROM "codes" WHERE "kind" = 'cq' AND "field" = 'mergeable' AND "code" = "change_request"."mergeable") AS "mergeable",
  "change_request"."failing_since",
  "change_request"."head_branch",
  "change_request"."base_branch",
  "change_request"."closes"
FROM "change_request";
CREATE TABLE "forge_read" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "service" TEXT NOT NULL,
  "scope" TEXT NOT NULL,
  "request" BLOB NOT NULL,
  "value" BLOB NOT NULL,
  "error" TEXT,
  "retry_at" INTEGER
) STRICT;
CREATE VIEW "forge_read_view" AS SELECT
  "forge_read"."id",
  "forge_read"."version",
  "forge_read"."service",
  "forge_read"."scope",
  "forge_read"."request",
  "forge_read"."value",
  "forge_read"."error",
  "forge_read"."retry_at"
FROM "forge_read";
CREATE TABLE "issue" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "service" TEXT NOT NULL,
  "scope" TEXT NOT NULL,
  "number" INTEGER NOT NULL,
  "title" TEXT,
  "assignees" BLOB NOT NULL,
  "state" TEXT CHECK ("state" IN ('cl', 'op')),
  "labels" BLOB NOT NULL,
  "updated_at" INTEGER,
  "failing_since" INTEGER,
  "body" TEXT,
  "issue_type" TEXT,
  "parent" BLOB NOT NULL DEFAULT x'f6',
  "blocked_by" BLOB NOT NULL DEFAULT x'f6',
  "fields" BLOB NOT NULL DEFAULT x'f6'
) STRICT;
CREATE INDEX "issue_by_subject" ON "issue" ("service", "scope", "number");
INSERT INTO "codes" ("kind", "field", "code", "name") VALUES
  ('iu', 'state', 'cl', 'Closed'),
  ('iu', 'state', 'op', 'Open');
CREATE VIEW "issue_view" AS SELECT
  "issue"."id",
  "issue"."version",
  "issue"."service",
  "issue"."scope",
  "issue"."number",
  "issue"."title",
  "issue"."assignees",
  (SELECT "name" FROM "codes" WHERE "kind" = 'iu' AND "field" = 'state' AND "code" = "issue"."state") AS "state",
  "issue"."labels",
  "issue"."updated_at",
  "issue"."failing_since",
  "issue"."body",
  "issue"."issue_type",
  "issue"."parent",
  "issue"."blocked_by",
  "issue"."fields"
FROM "issue";
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
