CREATE TABLE resource_relation (
  resource INTEGER NOT NULL REFERENCES resource_identity(id) ON DELETE CASCADE,
  relation TEXT NOT NULL CHECK (length(relation) > 0),
  host INTEGER NOT NULL REFERENCES strings(id),
  via INTEGER NOT NULL REFERENCES resource_identity(id) ON DELETE CASCADE,
  PRIMARY KEY (resource, relation, host, via)
) STRICT, WITHOUT ROWID;
CREATE TABLE resource_scope (
  resource INTEGER NOT NULL REFERENCES resource_identity(id) ON DELETE CASCADE,
  scope_code TEXT NOT NULL CHECK (length(scope_code) > 0),
  scope_key TEXT NOT NULL,
  PRIMARY KEY (resource, scope_code, scope_key)
) STRICT, WITHOUT ROWID;
CREATE TABLE change_scope (
  home INTEGER NOT NULL REFERENCES strings(id),
  home_seq INTEGER NOT NULL CHECK (home_seq > 0),
  scope_code TEXT NOT NULL CHECK (length(scope_code) > 0),
  scope_key TEXT NOT NULL,
  disposition TEXT NOT NULL CHECK (disposition IN ('up', 'rm')),
  PRIMARY KEY (home, home_seq, scope_code, scope_key),
  FOREIGN KEY (home, home_seq) REFERENCES log(home, home_seq) ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE TABLE stream_subscription (
  subscriber INTEGER NOT NULL REFERENCES strings(id),
  home INTEGER NOT NULL REFERENCES strings(id),
  scope_code TEXT NOT NULL CHECK (length(scope_code) > 0),
  scope_key TEXT NOT NULL,
  PRIMARY KEY (subscriber, home, scope_code, scope_key)
) STRICT, WITHOUT ROWID;
CREATE TABLE stream_watermark (
  home INTEGER NOT NULL REFERENCES strings(id),
  version INTEGER NOT NULL CHECK (version >= 0),
  applied_through INTEGER NOT NULL,
  PRIMARY KEY (home)
) STRICT, WITHOUT ROWID;
CREATE TABLE stream_history (
  home INTEGER NOT NULL REFERENCES strings(id),
  retained_after INTEGER NOT NULL CHECK (retained_after >= 0),
  PRIMARY KEY (home)
) STRICT, WITHOUT ROWID;
CREATE INDEX resource_relation_by_host ON resource_relation (host, resource);
CREATE INDEX resource_relation_by_source ON resource_relation (via, resource);
CREATE INDEX resource_scope_by_key ON resource_scope (scope_code, scope_key, resource);
CREATE INDEX change_scope_by_key ON change_scope (home, scope_code, scope_key, home_seq);
CREATE INDEX stream_subscription_by_key ON stream_subscription (home, scope_code, scope_key, subscriber);
CREATE VIEW resource_relation_view AS SELECT
  (SELECT i.kind || ':' || n.s || '@' || h.s FROM resource_identity i JOIN strings n ON n.id = i.name JOIN strings h ON h.id = i.home WHERE i.id = resource) AS resource, relation, (SELECT s FROM strings WHERE id = host) AS host, (SELECT i.kind || ':' || n.s || '@' || h.s FROM resource_identity i JOIN strings n ON n.id = i.name JOIN strings h ON h.id = i.home WHERE i.id = via) AS via
FROM resource_relation;
CREATE VIEW resource_scope_view AS SELECT
  (SELECT i.kind || ':' || n.s || '@' || h.s FROM resource_identity i JOIN strings n ON n.id = i.name JOIN strings h ON h.id = i.home WHERE i.id = resource) AS resource, scope_code, scope_key
FROM resource_scope;
CREATE VIEW change_scope_view AS SELECT
  (SELECT s FROM strings WHERE id = home) AS home, home_seq, scope_code, scope_key, disposition
FROM change_scope;
CREATE VIEW stream_subscription_view AS SELECT
  (SELECT s FROM strings WHERE id = subscriber) AS subscriber, (SELECT s FROM strings WHERE id = home) AS home, scope_code, scope_key
FROM stream_subscription;
CREATE VIEW stream_watermark_view AS SELECT
  (SELECT s FROM strings WHERE id = home) AS home, version, applied_through
FROM stream_watermark;
CREATE VIEW stream_history_view AS SELECT
  (SELECT s FROM strings WHERE id = home) AS home, retained_after
FROM stream_history;
