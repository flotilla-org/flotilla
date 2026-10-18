//! Schema checkpoint for #3463. These declarations render review DDL only: `Store::open`
//! neither installs nor accesses these tables before the owner's schema review.
//!
//! Shared metadata is not a resource kind: it has no resource identity, machine or event of its
//! own. The single home writer maintains relations, membership and change tags atomically with
//! its event. Subscription fan-out rebuilds a resident scope-key/bitmap index at startup; it
//! does not query subscriptions per change. See `docs/implementation-choices/change-stream.md`.

use flotilla_schema::{Code, DdlMode, Timestamp};

use crate::ResourceId;

/// A local `strings.id` in the home domain. Never appears on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HomeId(pub i64);

/// A local `strings.id` naming a host or view client. Never appears on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubscriberId(pub i64);

/// Action for one scope at a change. This is a scalar stored code, not enum-expanded columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeDisposition {
    /// The scope retains or acquires the resource (`up`).
    Upsert,
    /// The scope loses its body (`rm`), but keeps the global identity.
    Remove,
}

// One declaration produces both the stored row type and its review DDL. SQL constraints are
// explicit, as for the existing shared tables; these are not generated resource-kind tables.
macro_rules! table {
    ($row:ident, $table:literal, $doc:literal, { $($field:ident: $ty:ty => $sql:literal),+ $(,)? }, $constraints:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub struct $row { $(pub $field: $ty),+ }
        impl $row {
            const DDL: &'static str = concat!(
                "CREATE TABLE ", $table, " (\n",
                $("  ", stringify!($field), " ", $sql, ",\n",)+
                $constraints, "\n) STRICT, WITHOUT ROWID;\n"
            );
        }
    };
}

table!(RelationRow, "resource_relation",
    "A kind-derived relation to a host, with its source identity preserving independent reasons for membership.", {
    resource: ResourceId => "INTEGER NOT NULL REFERENCES resource_identity(id) ON DELETE CASCADE",
    relation: Code => "TEXT NOT NULL CHECK (length(relation) > 0)",
    host: HomeId => "INTEGER NOT NULL REFERENCES strings(id)",
    via: ResourceId => "INTEGER NOT NULL REFERENCES resource_identity(id) ON DELETE CASCADE",
}, "  PRIMARY KEY (resource, relation, host, via)");

table!(MembershipRow, "resource_scope",
    "Current resource membership. Codes are open: all (definitions), host, and later project/time keys.", {
    resource: ResourceId => "INTEGER NOT NULL REFERENCES resource_identity(id) ON DELETE CASCADE",
    scope_code: Code => "TEXT NOT NULL CHECK (length(scope_code) > 0)",
    scope_key: String => "TEXT NOT NULL",
}, "  PRIMARY KEY (resource, scope_code, scope_key)");

table!(ChangeScopeRow, "change_scope",
    "Immutable write-time tags. up means send the change (snapshot on entry); rm means remove the local body, retaining identity.", {
    home: HomeId => "INTEGER NOT NULL REFERENCES strings(id)",
    home_seq: u64 => "INTEGER NOT NULL CHECK (home_seq > 0)",
    scope_code: Code => "TEXT NOT NULL CHECK (length(scope_code) > 0)",
    scope_key: String => "TEXT NOT NULL",
    disposition: ChangeDisposition => "TEXT NOT NULL CHECK (disposition IN ('up', 'rm'))",
}, "  PRIMARY KEY (home, home_seq, scope_code, scope_key),\n  FOREIGN KEY (home, home_seq) REFERENCES log(home, home_seq) ON DELETE CASCADE");

table!(SubscriptionRow, "stream_subscription",
    "One predicate of a subscriber's union of scopes for a home; never a per-resource subscription.", {
    subscriber: SubscriberId => "INTEGER NOT NULL REFERENCES strings(id)",
    home: HomeId => "INTEGER NOT NULL REFERENCES strings(id)",
    scope_code: Code => "TEXT NOT NULL CHECK (length(scope_code) > 0)",
    scope_key: String => "TEXT NOT NULL",
}, "  PRIMARY KEY (subscriber, home, scope_code, scope_key)");

table!(WatermarkRow, "stream_watermark",
    "Latest applied prefix per home, shared by local gates. Observation only; never appended to the durable event log.", {
    home: HomeId => "INTEGER NOT NULL REFERENCES strings(id)",
    version: u64 => "INTEGER NOT NULL CHECK (version >= 0)",
    applied_through: Timestamp => "INTEGER NOT NULL",
}, "  PRIMARY KEY (home)");

table!(HistoryRow, "stream_history",
    "Home-owned resume floor: after this version the home can still serve a complete tagged tail. Older cursors require a snapshot.", {
    home: HomeId => "INTEGER NOT NULL REFERENCES strings(id)",
    retained_after: u64 => "INTEGER NOT NULL CHECK (retained_after >= 0)",
}, "  PRIMARY KEY (home)");

const INDEXES: &str = "\
CREATE INDEX resource_relation_by_host ON resource_relation (host, resource);
CREATE INDEX resource_relation_by_source ON resource_relation (via, resource);
CREATE INDEX resource_scope_by_key ON resource_scope (scope_code, scope_key, resource);
CREATE INDEX change_scope_by_key ON change_scope (home, scope_code, scope_key, home_seq);
CREATE INDEX stream_subscription_by_key ON stream_subscription (home, scope_code, scope_key, subscriber);
";

/// Proposed shared-table DDL and readable SQL views. Kept separate from `schema_ddl` until
/// owner approval. Codes stay text and have no closed-set CHECK; adding a scope needs no DDL.
pub fn stream_schema_ddl(mode: DdlMode) -> String {
    let mut ddl =
        [RelationRow::DDL, MembershipRow::DDL, ChangeScopeRow::DDL, SubscriptionRow::DDL, WatermarkRow::DDL, HistoryRow::DDL, INDEXES]
            .concat();
    if mode == DdlMode::IfNotExists {
        ddl = ddl.replace("CREATE TABLE ", "CREATE TABLE IF NOT EXISTS ").replace("CREATE INDEX ", "CREATE INDEX IF NOT EXISTS ");
    }
    for (table, columns) in views() {
        let exists = if mode == DdlMode::IfNotExists { "IF NOT EXISTS " } else { "" };
        ddl.push_str(&format!("CREATE VIEW {exists}{table}_view AS SELECT\n  {columns}\nFROM {table};\n"));
    }
    ddl
}

fn views() -> Vec<(&'static str, String)> {
    let home = |column: &str| format!("(SELECT s FROM strings WHERE id = {column}) AS {column}");
    let resource = |column: &str| {
        format!(
            "(SELECT i.kind || ':' || n.s || '@' || h.s FROM resource_identity i \
             JOIN strings n ON n.id = i.name JOIN strings h ON h.id = i.home WHERE i.id = {column}) AS {column}"
        )
    };
    vec![
        ("resource_relation", format!("{}, relation, {}, {}", resource("resource"), home("host"), resource("via"))),
        ("resource_scope", format!("{}, scope_code, scope_key", resource("resource"))),
        ("change_scope", format!("{}, home_seq, scope_code, scope_key, disposition", home("home"))),
        ("stream_subscription", format!("{}, {}, scope_code, scope_key", home("subscriber"), home("home"))),
        ("stream_watermark", format!("{}, version, applied_through", home("home"))),
        ("stream_history", format!("{}, retained_after", home("home"))),
    ]
}
