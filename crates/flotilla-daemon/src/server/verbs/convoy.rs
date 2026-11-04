//! Convoy subjects and the first native verb, `convoy.explain`.
use std::sync::Arc;

use async_trait::async_trait;
use flotilla_address::{Address, Kind};
use flotilla_core::in_process::InProcessDaemon;
use flotilla_protocol::table::Row;
use flotilla_schema::{Table, Verb};
use flotilla_store::{query_resource_kind, resolve_local_convoy_name, ResourceBackend, StoreQuery};
use serde_json::Value;

use super::catalog::{NativeVerb, Subject, Subjects};
use crate::server::tables;

/// Subjects stored as resources: a convoy is its row of the `convoys` table.
pub(crate) struct ResourceSubjects {
    backend: ResourceBackend,
}

impl ResourceSubjects {
    pub(crate) fn new(backend: ResourceBackend) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl Subjects for ResourceSubjects {
    fn table(&self, kind: Kind) -> Option<Table> {
        (kind == Kind::Convoy).then(|| tables::descriptor("convoys"))
    }

    /// A convoy address names a record (`convoy:convoy-<uuid>`) or a role, within a project
    /// (`project:<p>/convoy:<role>`) or in any project; it resolves as the CLI's `convoy explain`
    /// resolves `<role>@<project>`, and the row is read by the resolved record name.
    async fn row(&self, namespace: &str, address: &Address) -> Result<Option<Row>, String> {
        let Some(table) = self.table(address.kind()) else { return Err(format!("no table serves `{}` subjects", address.kind())) };
        let reference = match address.get(Kind::Project) {
            Some(project) => format!("{}@{project}", address.name()),
            None => address.name().to_owned(),
        };
        let name = resolve_local_convoy_name(&self.backend, namespace, &reference).await?;
        let query = StoreQuery::names([name], false);
        let document = query_resource_kind(&self.backend, namespace, "convoys", &query).await.map_err(|error| error.to_string())?;
        Ok(tables::resource_rows(&document, &table)?.into_iter().next())
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
    type Params = Explain;

    async fn run(&self, subjects: &[Subject], _params: &Explain, _invocation: &str) -> Result<Option<Value>, String> {
        let daemon = self.daemon.as_ref().ok_or("the daemon is unavailable")?;
        let [subject] = subjects else { return Err("convoy.explain runs on one convoy at a time".into()) };
        let name = subject.row.get("metadata.name").and_then(Value::as_str).ok_or("the convoy row has no name")?;
        let namespace = subject.row.get("metadata.namespace").and_then(Value::as_str);
        let explanation = daemon.explain_convoy_internal(namespace, name).await?;
        serde_json::to_value(explanation).map(Some).map_err(|error| error.to_string())
    }
}
