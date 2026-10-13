//! Retirement of the field indexes that generations before #3057 created.
//!
//! #3034 created one expression index per declared scalar field directly on
//! `resource_objects` and `replica_objects`; #3056 added C1 `_members` tables,
//! each with an index and three persistent triggers on the object table. Both
//! named the index `{table}_{Kind}_{path}` without a version suffix and recorded
//! that name in `resource_store_migrations`. #3057 moved every declared field to
//! `_values` tables named with a `_scalar_v3` or `_collection_v2` suffix, so
//! nothing selects the unsuffixed objects; every write of a matching kind still
//! maintains them.
//!
//! Rollback safety: those generations recreate and backfill a field index on
//! open whenever its marker is absent, and their `CREATE` statements have no
//! `IF NOT EXISTS`. Each object set and its marker therefore leave in one
//! transaction: a rolled-back daemon then rebuilds them on startup rather than
//! failing an `INDEXED BY` query or the `CREATE`.
//!
//! Remove this module one roll after every host has run it, once no rollback
//! target predates #3057 (tracked by #3173).

use super::*;

/// Recorded after the sweep; it marks that this generation has run it. The
/// sweep itself runs on every open, because a rollback to a pre-#3057
/// generation recreates these objects and a later roll forward must drop them
/// again.
pub(super) const LEGACY_FIELD_INDEX_CLEANUP: &str = "legacy-field-indexes-dropped-v1";

const ACTIONS: [&str; 3] = ["insert", "update", "delete"];
/// Triggers write the member tables; indexes go before the tables they sit on,
/// so each DROP names an object that still exists.
const DROP_ORDER: [&str; 3] = ["trigger", "index", "table"];

/// One declared field's index name as generations before #3057 spelled it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LegacyFieldIndex {
    pub(super) table: &'static str,
    pub(super) name: String,
}

impl LegacyFieldIndex {
    /// Every schema object either legacy shape may have left, as
    /// `(type, name, tbl_name)`. Matching the owning table as well as the name
    /// keeps the sweep away from any same-named object of another shape.
    pub(super) fn objects(&self) -> Vec<(&'static str, String, String)> {
        let members = format!("{}_members", self.name);
        let mut objects: Vec<_> =
            ACTIONS.iter().map(|action| ("trigger", format!("{}_{action}", self.name), self.table.to_string())).collect();
        objects.push(("index", self.name.clone(), self.table.to_string()));
        objects.push(("index", self.name.clone(), members.clone()));
        objects.push(("table", members.clone(), members));
        objects
    }
}

impl SqliteBackend {
    /// Frozen copy of the pre-#3057 `field_index`. It names what those
    /// generations created, so it must not follow later naming changes.
    fn pre_3057_field_index(table: &str, kind: ResourceKind, path: &str) -> String {
        format!(
            "{}_{}_{}",
            table,
            kind.paths.kind,
            path.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect::<String>()
        )
    }

    /// Unsuffixed names for every declared field whose current name differs.
    /// Array paths (`.*`) keep the unsuffixed name for their current `_values`
    /// index; no earlier generation declared them, so they are never legacy.
    ///
    /// Every field declared before #3057 is still declared, so this covers all
    /// objects those generations could have created.
    pub(super) fn legacy_field_indexes() -> Vec<LegacyFieldIndex> {
        let mut legacy = Vec::new();
        for kind in crate::declared_resource_kinds() {
            for path in kind.queryable_fields {
                for table in ["resource_objects", "replica_objects"] {
                    let name = Self::pre_3057_field_index(table, kind, path);
                    if name != Self::field_index(table, kind, path) {
                        legacy.push(LegacyFieldIndex { table, name });
                    }
                }
            }
        }
        legacy
    }

    /// Drop the pre-#3057 field indexes, C1 member tables and their persistent
    /// triggers, and delete their markers in the caller's transaction. Present
    /// objects are found in `sqlite_schema`, so a database that never had some
    /// of them, or has already been swept, is left unchanged.
    pub(super) fn drop_legacy_field_indexes(tx: &rusqlite::Transaction<'_>) -> Result<(), ResourceError> {
        let legacy = Self::legacy_field_indexes();
        let wanted = legacy.iter().flat_map(LegacyFieldIndex::objects).collect::<HashSet<_>>();
        let names = legacy.iter().map(|index| index.name.as_str()).collect::<HashSet<_>>();
        let mut present = {
            let mut statement = tx
                .prepare("SELECT type,name,tbl_name FROM main.sqlite_schema WHERE type IN ('index','table','trigger')")
                .map_err(|e| Self::map_sqlite(e, "inspect legacy field indexes"))?;
            let rows = statement
                .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?)))
                .map_err(|e| Self::map_sqlite(e, "list legacy field indexes"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| Self::map_sqlite(e, "read legacy field indexes"))?;
            rows.into_iter()
                .filter_map(|(kind, name, table)| {
                    let kind = DROP_ORDER.into_iter().find(|candidate| *candidate == kind)?;
                    wanted.contains(&(kind, name.clone(), table)).then_some((kind, name))
                })
                .collect::<Vec<_>>()
        };
        let markers = {
            let mut statement = tx
                .prepare("SELECT name FROM resource_store_migrations")
                .map_err(|e| Self::map_sqlite(e, "inspect legacy field index markers"))?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(|e| Self::map_sqlite(e, "list legacy field index markers"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| Self::map_sqlite(e, "read legacy field index markers"))?;
            rows.into_iter().filter(|name| names.contains(name.as_str())).collect::<Vec<_>>()
        };
        present.sort_by_key(|(kind, name)| (DROP_ORDER.iter().position(|order| order == kind), name.clone()));
        for (kind, name) in &present {
            let quoted = name.replace('"', "\"\"");
            tx.execute_batch(&format!("DROP {kind} main.\"{quoted}\"")).map_err(|e| Self::map_sqlite(e, "drop legacy field index"))?;
        }
        for marker in &markers {
            tx.execute("DELETE FROM resource_store_migrations WHERE name=?1", [marker])
                .map_err(|e| Self::map_sqlite(e, "remove legacy field index marker"))?;
        }
        tx.execute("INSERT OR IGNORE INTO resource_store_migrations(name) VALUES (?1)", [LEGACY_FIELD_INDEX_CLEANUP])
            .map_err(|e| Self::map_sqlite(e, "record legacy field index cleanup"))?;
        if !present.is_empty() || !markers.is_empty() {
            tracing::info!(objects = present.len(), markers = markers.len(), "dropped pre-#3057 field indexes");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::StoreQuery;

    /// Fields declared beyond `metadata.name` at `1a5d2d2f^`, the last
    /// generation that created unsuffixed field indexes. Every registered kind
    /// also declared `metadata.name`. To re-verify, list the
    /// `QUERYABLE_FIELDS` declarations at that commit:
    /// `git grep -n -A12 'QUERYABLE_FIELDS' 1a5d2d2f^ -- crates/flotilla-resources/src`.
    const PRE_3057_FIELDS: &[(&str, &[&str])] = &[
        ("Artifact", &["spec.convoy"]),
        ("ChangeRequest", &["status.state.value"]),
        ("Checkout", &["status.phase"]),
        ("Convoy", &["status.phase", "spec.role", "spec.project_ref"]),
        ("ConvoyEnsure", &["spec.project_ref", "spec.role"]),
        ("Demand", &["spec.originating_work_ref.name"]),
        ("Message", &["spec.receiver", "spec.in_reply_to", "status.phase", "status.submission.batch_id"]),
        ("Project", &["spec.repositories[].repo"]),
        (
            "TerminalSession",
            &[
                "status.phase",
                "status.crew.id",
                "status.completion_pending.authority",
                "spec.role",
                "spec.env_ref",
                "spec.source.context.convoy",
                "spec.source.context.vessel_ref",
                "metadata.annotations.\"flotilla.work/role-address\"",
            ],
        ),
        ("Vessel", &["status.phase"]),
    ];

    fn pre_3057_fields() -> Vec<(ResourceKind, &'static str)> {
        let kinds = crate::declared_resource_kinds();
        let mut fields = kinds.iter().map(|kind| (*kind, "metadata.name")).collect::<Vec<_>>();
        for (name, paths) in PRE_3057_FIELDS {
            let kind = *kinds.iter().find(|kind| kind.paths.kind == *name).expect("pre-#3057 kind is still registered");
            fields.extend(paths.iter().map(|path| (kind, *path)));
        }
        fields
    }

    /// The field loop of `initialize_query_indexes` at `1a5d2d2f^`
    /// (crates/flotilla-store-sqlite/src/sqlite.rs:549-579), restricted to its
    /// declared fields. Returns how many field indexes it had to create. It is
    /// a copy, so the rollback test also runs the old `INDEXED BY` and
    /// `_members` queries against what it builds.
    fn open_as_pre_3057(connection: &mut rusqlite::Connection) -> usize {
        let tx = connection.transaction().unwrap();
        let mut created = 0;
        for (kind, path) in pre_3057_fields() {
            for table in ["resource_objects", "replica_objects"] {
                let index = SqliteBackend::pre_3057_field_index(table, kind, path);
                let done: bool =
                    tx.query_row("SELECT EXISTS(SELECT 1 FROM resource_store_migrations WHERE name=?1)", [&index], |r| r.get(0)).unwrap();
                if done {
                    continue;
                }
                if let Some((prefix, suffix)) = path.split_once("[].") {
                    let members = format!("{index}_members");
                    let origin = if table == "replica_objects" { "origin_root" } else { "''" };
                    let new_origin: String = if origin == "''" { "''".into() } else { "NEW.origin_root".into() };
                    let old_origin: String = if origin == "''" { "''".into() } else { "OLD.origin_root".into() };
                    let deletion = format!("group_name=OLD.group_name AND version=OLD.version AND kind=OLD.kind AND namespace=OLD.namespace AND name=OLD.name AND origin={old_origin}");
                    let insertion = format!("INSERT OR IGNORE INTO {members} SELECT NEW.group_name,NEW.version,NEW.kind,NEW.namespace,NEW.name,{new_origin},json_extract(j.value,'$.{suffix}') FROM json_each(CASE WHEN json_valid(NEW.body_json) THEN NEW.body_json ELSE '{{}}' END,'$.{prefix}') j WHERE NEW.kind='{}' AND json_type(j.value,'$.{suffix}')='text';", kind.paths.kind);
                    tx.execute_batch(&format!("
                        CREATE TABLE {members}(group_name TEXT,version TEXT,kind TEXT,namespace TEXT,name TEXT,origin TEXT,value TEXT,PRIMARY KEY(group_name,version,kind,namespace,name,origin,value));
                        CREATE INDEX {index} ON {members}(group_name,version,kind,namespace,value,name,origin);
                        CREATE TRIGGER {index}_insert AFTER INSERT ON {table} BEGIN {insertion} END;
                        CREATE TRIGGER {index}_update AFTER UPDATE ON {table} BEGIN DELETE FROM {members} WHERE {deletion}; {insertion} END;
                        CREATE TRIGGER {index}_delete AFTER DELETE ON {table} BEGIN DELETE FROM {members} WHERE {deletion}; END;
                        INSERT OR IGNORE INTO {members} SELECT o.group_name,o.version,o.kind,o.namespace,o.name,{origin},json_extract(j.value,'$.{suffix}') FROM {table} o,json_each(CASE WHEN json_valid(o.body_json) THEN o.body_json ELSE '{{}}' END,'$.{prefix}') j WHERE o.kind='{}' AND json_type(j.value,'$.{suffix}')='text';",kind.paths.kind)).unwrap();
                } else {
                    tx.execute_batch(&format!("CREATE INDEX {index} ON {table}(group_name,version,kind,namespace,json_extract(body_json,'$.{path}'),name) WHERE kind='{}' AND json_valid(body_json)", kind.paths.kind)).unwrap();
                }
                tx.execute("INSERT INTO resource_store_migrations(name) VALUES (?1)", [&index]).unwrap();
                created += 1;
            }
        }
        tx.commit().unwrap();
        created
    }

    fn rows(connection: &rusqlite::Connection, sql: &str) -> Vec<String> {
        let mut statement = connection.prepare(sql).unwrap();
        let rows = statement
            .query_map([], |row| (0..row.as_ref().column_count()).map(|i| row.get::<_, String>(i)).collect::<Result<Vec<_>, _>>())
            .unwrap();
        rows.map(|row| row.unwrap().join("|")).collect()
    }

    /// Everything the cleanup must leave as it was: schema objects, markers and
    /// stored bodies of local and replica objects.
    type Shape = (Vec<String>, Vec<String>, Vec<String>);

    fn stored_shape(path: &Path) -> Shape {
        let connection = rusqlite::Connection::open(path).unwrap();
        (
            rows(&connection, "SELECT type,name,tbl_name,COALESCE(sql,'') FROM main.sqlite_schema ORDER BY type,name"),
            rows(&connection, "SELECT name FROM resource_store_migrations ORDER BY name"),
            rows(&connection, "SELECT 'local',kind,name,'',body_json FROM resource_objects UNION ALL SELECT 'replica',kind,name,origin_root,body_json FROM replica_objects ORDER BY 1,2,3,4"),
        )
    }

    /// Compare two shapes, reporting only the rows that differ.
    fn assert_shape(actual: &Shape, expected: &Shape, context: &str) {
        for (part, actual, expected) in
            [("schema", &actual.0, &expected.0), ("markers", &actual.1, &expected.1), ("bodies", &actual.2, &expected.2)]
        {
            let extra = actual.iter().filter(|row| !expected.contains(row)).collect::<Vec<_>>();
            let missing = expected.iter().filter(|row| !actual.contains(row)).collect::<Vec<_>>();
            assert!(actual == expected, "{context}: {part} differs; extra {extra:#?}; missing {missing:#?}");
        }
    }

    /// Declared query results over local and replica rows, rendered in full.
    async fn query_results(path: &Path) -> Vec<String> {
        let backend = crate::ResourceBackend::new(SqliteBackend::open(path).unwrap());
        let convoy = ResourceKind::of::<crate::Convoy>();
        let project = ResourceKind::of::<flotilla_resources::Project>();
        let queries = [
            (convoy, StoreQuery { fields: BTreeMap::from([("metadata.name".into(), "stored".into())]), ..Default::default() }),
            (convoy, StoreQuery { present: vec!["spec.repositories.*.repo_ref".into()], ..Default::default() }),
            (convoy, StoreQuery::labels(&BTreeMap::from([("team".into(), "target".into())]), true, true)),
            (project, StoreQuery { fields: BTreeMap::from([("spec.repositories[].repo".into(), "target".into())]), ..Default::default() }),
        ];
        let mut results = Vec::new();
        for (kind, query) in queries {
            for (include_replicas, replicas_only) in [(false, false), (true, true)] {
                let query = StoreQuery { include_replicas, replicas_only, ..query.clone() };
                let result = backend.store().query_raw(kind, "flotilla", &query).await.unwrap();
                assert_eq!(result.items.len(), 1, "{} {query:?} selects one stored object", kind.paths.kind);
                results.push(format!("{:?} {:?}", result.items, result.position));
            }
        }
        results
    }

    async fn seed(path: &Path) {
        let backend = crate::ResourceBackend::new(SqliteBackend::open(path).unwrap());
        let meta = InputMeta::builder().name("stored".into()).labels(BTreeMap::from([("team".into(), "target".into())])).build();
        let convoy = backend
            .using::<crate::Convoy>("flotilla")
            .create(
                &meta,
                &crate::ConvoySpec::builder()
                    .workflow_ref("workflow".into())
                    .repositories(vec![crate::ConvoyRepositorySpec::builder()
                        .url("https://example.com/repo".into())
                        .repo_ref(crate::RepositoryKey("repository".into()))
                        .source_ref("main".into())
                        .target_ref("main".into())
                        .workspace_slug("repo".into())
                        .subpaths(Vec::new())
                        .build()])
                    .build(),
            )
            .await
            .unwrap();
        let project = backend
            .definitions::<flotilla_resources::Project>("flotilla")
            .create(
                &InputMeta::builder().name("existing".into()).build(),
                &flotilla_resources::ProjectSpec::builder()
                    .display_name("Existing".into())
                    .repositories(vec![flotilla_resources::ProjectRepositorySpec::builder()
                        .repo(flotilla_resources::RepositoryKey("target".into()))
                        .build()])
                    .build(),
            )
            .await
            .unwrap();
        let foreign = NodeId::new("foreign");
        let convoys = ResourceList { items: vec![convoy], resource_version: "1".into(), generation: None };
        backend.replace_replicas_typed::<crate::Convoy>(&foreign, "flotilla", &convoys, Utc::now(), None).await.unwrap();
        let projects = ResourceList { items: vec![project], resource_version: "1".into(), generation: None };
        backend.replace_replicas_typed::<flotilla_resources::Project>(&foreign, "flotilla", &projects, Utc::now(), None).await.unwrap();
    }

    // A database carrying every pre-#3057 field index, C1 member table and
    // trigger, with their markers, reopens with exactly the schema, markers,
    // bodies and query results of a database that never had them. Reopening
    // is a no-op, and a rollback to `1a5d2d2f^` finds every marker absent and
    // rebuilds its indexes, which the next roll forward drops again.
    #[tokio::test]
    async fn pre_3057_field_indexes_leave_with_their_markers_and_rebuild_on_rollback() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("upgrade.sqlite");
        seed(&path).await;
        let current = stored_shape(&path);
        let results = query_results(&path).await;
        assert_shape(&stored_shape(&path), &current, "reading leaves the stored shape unchanged");

        // The N-1 generation never recorded the cleanup marker.
        let mut connection = rusqlite::Connection::open(&path).unwrap();
        connection.execute("DELETE FROM resource_store_migrations WHERE name=?1", [LEGACY_FIELD_INDEX_CLEANUP]).unwrap();
        let fields = pre_3057_fields();
        assert_eq!(open_as_pre_3057(&mut connection), fields.len() * 2, "the current schema holds no pre-#3057 field index");
        drop(connection);
        let legacy = stored_shape(&path);
        let collections = fields.iter().filter(|(_, path)| path.contains("[].")).count();
        assert_eq!(
            legacy.0.len() - current.0.len(),
            2 * ((fields.len() - collections) + 6 * collections),
            "per table: one expression index per scalar field; per collection a members table, its key autoindex, an index and three triggers"
        );
        assert_eq!(legacy.1.len() + 1 - current.1.len(), fields.len() * 2);
        assert_eq!(legacy.2, current.2);

        for round in ["cleanup", "reopen"] {
            assert_eq!(query_results(&path).await, results, "{round}: query results are unchanged");
            assert_shape(&stored_shape(&path), &current, &format!("{round}: the shape matches a database without legacy objects"));
        }
        let marker = rows(
            &rusqlite::Connection::open(&path).unwrap(),
            &format!("SELECT CAST(count(*) AS TEXT) FROM resource_store_migrations WHERE name='{LEGACY_FIELD_INDEX_CLEANUP}'"),
        );
        assert_eq!(marker, ["1"]);

        // Roll back to the generation before #3057: with each marker gone, its
        // unconditional CREATE runs and backfills instead of failing.
        let mut connection = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(open_as_pre_3057(&mut connection), fields.len() * 2, "rollback rebuilds every dropped field index");
        let convoy = SqliteBackend::pre_3057_field_index("resource_objects", ResourceKind::of::<crate::Convoy>(), "metadata.name");
        let selected = rows(&connection, &format!("SELECT name FROM resource_objects INDEXED BY {convoy} WHERE group_name='flotilla.work' AND version='v1' AND kind='Convoy' AND namespace='flotilla' AND json_valid(body_json) AND json_extract(body_json,'$.metadata.name')='stored'"));
        assert_eq!(selected, ["stored"]);
        let members = SqliteBackend::pre_3057_field_index(
            "replica_objects",
            ResourceKind::of::<flotilla_resources::Project>(),
            "spec.repositories[].repo",
        );
        let selected = rows(&connection, &format!("SELECT name,origin FROM {members}_members INDEXED BY {members} WHERE group_name='flotilla.work' AND version='v1' AND kind='Project' AND namespace='flotilla' AND value='target'"));
        assert_eq!(selected, ["existing|foreign"]);
        drop(connection);
        // The older generation ignores the cleanup marker; the sweep never relies on it.
        let mut rolled_back = legacy.clone();
        rolled_back.1.push(LEGACY_FIELD_INDEX_CLEANUP.into());
        rolled_back.1.sort();
        assert_shape(&stored_shape(&path), &rolled_back, "rollback restores the pre-#3057 schema and markers");

        assert_eq!(query_results(&path).await, results);
        assert_shape(&stored_shape(&path), &current, "rolling forward again drops the rebuilt objects");
    }

    // The sweep matches only unsuffixed pre-#3057 names, which no current
    // object or marker uses in either schema, and it covers every field a
    // pre-#3057 generation declared.
    #[tokio::test]
    async fn legacy_field_index_names_never_name_current_objects() {
        let legacy = SqliteBackend::legacy_field_indexes();
        let swept = legacy.iter().flat_map(LegacyFieldIndex::objects).map(|(_, name, _)| name.to_lowercase()).collect::<BTreeSet<_>>();
        let names = legacy.iter().map(|index| index.name.clone()).collect::<BTreeSet<_>>();
        for (kind, path) in pre_3057_fields() {
            for table in ["resource_objects", "replica_objects"] {
                let name = SqliteBackend::pre_3057_field_index(table, kind, path);
                assert!(names.contains(&name), "{name} must stay declared so the sweep can retire it");
            }
        }
        let backend = SqliteBackend::open_in_memory().unwrap();
        let current = backend
            .call("list current schema", |connection| {
                let mut names = rows(connection, "SELECT name FROM main.sqlite_schema UNION ALL SELECT name FROM temp.sqlite_schema");
                names.extend(rows(connection, "SELECT name FROM resource_store_migrations"));
                Ok(names)
            })
            .await
            .unwrap();
        assert!(current.iter().any(|name| name.ends_with("_scalar_v3")) && current.iter().any(|name| name.ends_with("_collection_v2")));
        for name in current {
            assert!(!swept.contains(&name.to_lowercase()), "the legacy sweep would match current object {name}");
        }
    }
}
