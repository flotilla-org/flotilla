//! Subjects stored as resources (#4267 ruling 3): the table serving a kind is the resource table
//! whose description names that kind as its rows' subject (#4199), and a subject's row is that
//! table's row of the record the subject's address names.
use std::collections::BTreeMap;

use async_trait::async_trait;
use flotilla_address::{Address, Kind};
use flotilla_protocol::table::Row;
use flotilla_schema::Table;
use flotilla_store::{query_resource_kind, subject_record_name, ResourceBackend, StoreQuery, REGISTERED_RESOURCE_KINDS};

use super::catalog::Subjects;
use crate::server::tables;

/// Subject rows read from the resource store.
pub(crate) struct ResourceSubjects {
    backend: ResourceBackend,
    tables: BTreeMap<Kind, Table>,
}

impl ResourceSubjects {
    pub(crate) fn new(backend: ResourceBackend) -> Self {
        let tables = REGISTERED_RESOURCE_KINDS
            .iter()
            .map(|kind| tables::descriptor(kind.plural))
            .filter_map(|table| Some((table.subject.as_ref()?.kind, table)))
            .collect();
        Self { backend, tables }
    }
}

#[async_trait]
impl Subjects for ResourceSubjects {
    fn table(&self, kind: Kind) -> Option<Table> {
        self.tables.get(&kind).cloned()
    }

    /// The row of the record `address` names, through `flotilla_store::subject_record_name`.
    async fn row(&self, namespace: &str, address: &Address) -> Result<Option<Row>, String> {
        let Some(table) = self.tables.get(&address.kind()) else { return Err(format!("no table serves `{}` subjects", address.kind())) };
        let Some(name) = subject_record_name(&self.backend, namespace, address).await? else { return Ok(None) };
        let query = StoreQuery::names([name], false);
        let document = query_resource_kind(&self.backend, namespace, &table.name, &query).await.map_err(|error| error.to_string())?;
        Ok(tables::resource_rows(&document, table)?.into_iter().next())
    }
}
