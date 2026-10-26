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
CREATE TABLE "account_binding" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "requester" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "item" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "requirement" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "credential" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "account" TEXT,
  "selection" TEXT NOT NULL CHECK ("selection" IN ('dr', 'pn')),
  "selection_pinned_by" TEXT,
  "selection_index" INTEGER,
  "opened_at" INTEGER NOT NULL,
  "state" TEXT NOT NULL CHECK ("state" IN ('cl', 'op', 'rv')),
  "state_since" INTEGER NOT NULL,
  "reason" TEXT,
  "delivered_until" INTEGER,
  CHECK (CASE WHEN "selection" = 'pn'
      THEN "selection_pinned_by" IS NOT NULL
      AND "selection_index" IS NULL
      END),
  CHECK (CASE WHEN "selection" = 'dr'
      THEN "selection_index" IS NOT NULL
      AND "selection_pinned_by" IS NULL
      END),
  CHECK (CASE WHEN "state" = 'op'
      THEN "reason" IS NULL
      AND "delivered_until" IS NULL
      END),
  CHECK (CASE WHEN "state" = 'cl'
      THEN "reason" IS NOT NULL
      END),
  CHECK (CASE WHEN "state" = 'rv'
      THEN "reason" IS NOT NULL
      END)
) STRICT;
CREATE TABLE "account_binding_grants" (
  "account_binding" INTEGER NOT NULL,
  "key" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "scope" TEXT NOT NULL,
  PRIMARY KEY ("account_binding", "key"),
  FOREIGN KEY ("account_binding") REFERENCES "account_binding" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE TABLE "account_binding_candidates" (
  "account_binding" INTEGER NOT NULL,
  "value" INTEGER NOT NULL REFERENCES "strings" ("id"),
  PRIMARY KEY ("account_binding", "value"),
  FOREIGN KEY ("account_binding") REFERENCES "account_binding" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE TABLE "account_binding_exclusions" (
  "account_binding" INTEGER NOT NULL,
  "key" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "value" TEXT NOT NULL,
  PRIMARY KEY ("account_binding", "key"),
  FOREIGN KEY ("account_binding") REFERENCES "account_binding" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE TABLE "account_binding_epochs" (
  "account_binding" INTEGER NOT NULL,
  "pos" INTEGER NOT NULL,
  "version" INTEGER NOT NULL,
  "epoch" TEXT NOT NULL,
  "observed_at" INTEGER NOT NULL,
  PRIMARY KEY ("account_binding", "pos"),
  FOREIGN KEY ("account_binding") REFERENCES "account_binding" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE INDEX "account_binding_by_requester" ON "account_binding" ("requester");
CREATE INDEX "account_binding_open_by_credential" ON "account_binding" ("credential") WHERE "state" IN ('op');
INSERT INTO "codes" ("kind", "field", "code", "name") VALUES
  ('ab', 'selection', 'dr', 'Draw'),
  ('ab', 'selection', 'pn', 'Pin'),
  ('ab', 'state', 'cl', 'Closed'),
  ('ab', 'state', 'op', 'Open'),
  ('ab', 'state', 'rv', 'Revoked'),
  ('ab', 'log:edge', 'cl', 'Close'),
  ('ab', 'log:edge', 'ep', 'ObserveEpoch'),
  ('ab', 'log:edge', 'rg', 'Regrant'),
  ('ab', 'log:edge', 'rv', 'Revoke'),
  ('ab', 'log:label', 'cl', 'Closed'),
  ('ab', 'log:label', 'op', 'Open'),
  ('ab', 'log:label', 'rv', 'Revoked');
CREATE VIEW "account_binding_view" AS SELECT
  "account_binding"."id",
  "account_binding"."version",
  (SELECT "s" FROM "strings" WHERE "id" = "account_binding"."requester") AS "requester",
  (SELECT "s" FROM "strings" WHERE "id" = "account_binding"."item") AS "item",
  (SELECT "s" FROM "strings" WHERE "id" = "account_binding"."requirement") AS "requirement",
  (SELECT "s" FROM "strings" WHERE "id" = "account_binding"."credential") AS "credential",
  "account_binding"."account",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ab' AND "field" = 'selection' AND "code" = "account_binding"."selection") AS "selection",
  "account_binding"."selection_pinned_by",
  "account_binding"."selection_index",
  "account_binding"."opened_at",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ab' AND "field" = 'state' AND "code" = "account_binding"."state") AS "state",
  "account_binding"."state_since",
  "account_binding"."reason",
  "account_binding"."delivered_until"
FROM "account_binding";
CREATE VIEW "account_binding_grants_view" AS SELECT
  "account_binding_grants"."account_binding",
  (SELECT "s" FROM "strings" WHERE "id" = "account_binding_grants"."key") AS "key",
  "account_binding_grants"."scope"
FROM "account_binding_grants";
CREATE VIEW "account_binding_candidates_view" AS SELECT
  "account_binding_candidates"."account_binding",
  (SELECT "s" FROM "strings" WHERE "id" = "account_binding_candidates"."value") AS "value"
FROM "account_binding_candidates";
CREATE VIEW "account_binding_exclusions_view" AS SELECT
  "account_binding_exclusions"."account_binding",
  (SELECT "s" FROM "strings" WHERE "id" = "account_binding_exclusions"."key") AS "key",
  "account_binding_exclusions"."value"
FROM "account_binding_exclusions";
CREATE VIEW "account_binding_epochs_view" AS SELECT
  "account_binding_epochs"."account_binding",
  "account_binding_epochs"."pos",
  "account_binding_epochs"."version",
  "account_binding_epochs"."epoch",
  "account_binding_epochs"."observed_at"
FROM "account_binding_epochs";
CREATE TABLE "account_provision" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "binding" INTEGER NOT NULL REFERENCES "resource_identity" ("id"),
  "place" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "environment_kind" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "plan_implementation" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "plan_effective_access" TEXT NOT NULL,
  "plan_mechanism" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "plan_update_mode" TEXT NOT NULL CHECK ("plan_update_mode" IN ('mh', 'pu', 'rs')),
  "home" TEXT NOT NULL,
  "delivered_epoch" TEXT,
  "delivered_generation" TEXT,
  "delivered_at" INTEGER,
  CHECK (CASE WHEN "delivered_epoch" IS NOT NULL
      THEN "delivered_generation" IS NOT NULL
      AND "delivered_at" IS NOT NULL
      ELSE "delivered_generation" IS NULL
      AND "delivered_at" IS NULL
      END)
) STRICT;
CREATE INDEX "account_provision_by_binding" ON "account_provision" ("binding");
CREATE INDEX "account_provision_by_place" ON "account_provision" ("place");
INSERT INTO "codes" ("kind", "field", "code", "name") VALUES
  ('ap', 'plan_update_mode', 'mh', 'Mechanism'),
  ('ap', 'plan_update_mode', 'pu', 'Push'),
  ('ap', 'plan_update_mode', 'rs', 'Restart');
CREATE VIEW "account_provision_view" AS SELECT
  "account_provision"."id",
  "account_provision"."version",
  "account_provision"."binding",
  (SELECT "s" FROM "strings" WHERE "id" = "account_provision"."place") AS "place",
  (SELECT "s" FROM "strings" WHERE "id" = "account_provision"."environment_kind") AS "environment_kind",
  (SELECT "s" FROM "strings" WHERE "id" = "account_provision"."plan_implementation") AS "plan_implementation",
  "account_provision"."plan_effective_access",
  (SELECT "s" FROM "strings" WHERE "id" = "account_provision"."plan_mechanism") AS "plan_mechanism",
  (SELECT "name" FROM "codes" WHERE "kind" = 'ap' AND "field" = 'plan_update_mode' AND "code" = "account_provision"."plan_update_mode") AS "plan_update_mode",
  "account_provision"."home",
  "account_provision"."delivered_epoch",
  "account_provision"."delivered_generation",
  "account_provision"."delivered_at"
FROM "account_provision";
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
