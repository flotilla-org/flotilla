//! Convoy subjects and the first native verb, `convoy.explain`.
use std::sync::Arc;

use async_trait::async_trait;
use flotilla_address::{record_name, Address, Kind};
use flotilla_core::in_process::InProcessDaemon;
use flotilla_protocol::table::Row;
use flotilla_schema::{Table, Verb, VerbDeclaration};
use flotilla_store::{query_resource_kind, ResourceBackend, StoreQuery};
use serde_json::{Map, Value};

use super::catalog::{NativeVerb, Subject, Subjects};
use crate::server::tables;

/// Subjects stored as resources: a convoy is its row of the `convoys` table, in the daemon's
/// provisioning namespace.
pub(crate) struct ResourceSubjects {
    backend: ResourceBackend,
    daemon: Option<Arc<InProcessDaemon>>,
}

impl ResourceSubjects {
    pub(crate) fn new(backend: ResourceBackend, daemon: Option<Arc<InProcessDaemon>>) -> Self {
        Self { backend, daemon }
    }

    async fn namespace(&self) -> String {
        match &self.daemon {
            Some(daemon) => daemon.provisioning_namespace().await,
            None => "default".into(),
        }
    }
}

#[async_trait]
impl Subjects for ResourceSubjects {
    fn table(&self, kind: Kind) -> Option<Table> {
        (kind == Kind::Convoy).then(|| tables::descriptor("convoys"))
    }

    async fn row(&self, address: &Address) -> Result<Option<Row>, String> {
        let Some(table) = self.table(address.kind()) else { return Err(format!("no table serves `{}` subjects", address.kind())) };
        let name = record_name(address).ok_or_else(|| format!("`{address}` names no record"))?;
        let query = StoreQuery::names([name], false);
        let document =
            query_resource_kind(&self.backend, &self.namespace().await, "convoys", &query).await.map_err(|error| error.to_string())?;
        let row = tables::resource_rows(&document, &table)?.into_iter().next();
        // A convoy under a project is that project's: `project:other/convoy:c` names no convoy of `flotilla`.
        // The `convoys` table keeps `spec` as one whole-object column, so `project_ref` is read inside it.
        Ok(row.filter(|row| {
            address
                .get(Kind::Project)
                .is_none_or(|project| row.get("spec").and_then(|spec| spec.get("project_ref")).and_then(Value::as_str) == Some(project))
        }))
    }
}

/// Explains a convoy: why it is where it is, and what it waits on.
#[derive(Verb, serde::Deserialize)]
#[verb(name = "convoy.explain", subject = "convoy", effect = "read", idempotent)]
pub(crate) struct Explain {}

/// `convoy.explain` over the daemon's explanation projection.
pub(crate) struct ExplainConvoy {
    pub daemon: Option<Arc<InProcessDaemon>>,
}

#[async_trait]
impl NativeVerb for ExplainConvoy {
    fn declaration(&self) -> VerbDeclaration {
        Explain::declaration()
    }

    async fn run(&self, subjects: &[Subject], _params: &Map<String, Value>, _invocation: &str) -> Result<Option<Value>, String> {
        let daemon = self.daemon.as_ref().ok_or("the daemon is unavailable")?;
        let [subject] = subjects else { return Err("convoy.explain runs on one convoy at a time".into()) };
        let name = subject.row.get("metadata.name").and_then(Value::as_str).ok_or("the convoy row has no name")?;
        let namespace = subject.row.get("metadata.namespace").and_then(Value::as_str);
        let explanation = daemon.explain_convoy_internal(namespace, name).await?;
        serde_json::to_value(explanation).map(Some).map_err(|error| error.to_string())
    }
}
