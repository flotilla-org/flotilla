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
  "body" TEXT NOT NULL,
  "body_digest" TEXT,
  "interrupting" INTEGER NOT NULL CHECK ("interrupting" IN (0, 1)),
  "accepted_seq" INTEGER NOT NULL,
  "deadline" INTEGER,
  "ordinal" INTEGER NOT NULL
) STRICT;
