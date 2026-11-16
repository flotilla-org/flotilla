//! Subjects stored as records (#4267 ruling 3, #4355): the table serving a kind is the one the
//! kind's owning crate registered, and a subject's row is that table's row of the record the
//! subject's address names, in the engine's store or, for a kind not yet moved, the resource store.
use std::collections::BTreeMap;

use async_trait::async_trait;
use flotilla_address::{Address, Kind};
use flotilla_protocol::table::Row;
use flotilla_schema::Table;

use super::catalog::Subjects;
use crate::server::tables::{Catalog, TableSource};

/// Subject rows read from the stores behind `catalog`.
pub(crate) struct StoredSubjects {
    catalog: Catalog,
    tables: BTreeMap<Kind, Table>,
}

impl StoredSubjects {
    /// Describes each registered subject table once; a table no store serves serves no subject.
    pub(crate) fn new(catalog: Catalog) -> Self {
        let tables = catalog
            .subjects
            .iter()
            .filter_map(|subject| {
                let source = TableSource::open(subject.table.to_owned(), &catalog).ok()?;
                let description =
                    source.describe_stored().map_err(|error| tracing::warn!(table = subject.table, %error, "subject table")).ok()?;
                Some((subject.kind, description.table))
            })
            .collect();
        Self { catalog, tables }
    }
}

#[async_trait]
impl Subjects for StoredSubjects {
    fn table(&self, kind: Kind) -> Option<Table> {
        self.tables.get(&kind).cloned()
    }

    async fn row(&self, namespace: &str, address: &Address) -> Result<Option<Row>, String> {
        let Some(table) = self.tables.get(&address.kind()) else { return Err(format!("no table serves `{}` subjects", address.kind())) };
        self.catalog.subject_row(table, namespace, address).await
    }
}
