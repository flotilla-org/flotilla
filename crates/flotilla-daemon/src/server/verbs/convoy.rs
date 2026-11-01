//! Convoy subjects and the first native verb, `convoy.explain`.
use std::sync::Arc;

use async_trait::async_trait;
use flotilla_address::{Address, Kind};
use flotilla_core::in_process::InProcessDaemon;
use flotilla_protocol::table::Row;
use flotilla_schema::{Table, Verb};
use flotilla_store::{query_convoy_by_role, ResourceBackend};
use serde_json::{json, Value};

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

    /// Under an ADR 0066 address a convoy is named by its role, within its project or, at the
    /// root, among the projectless convoys; its record name (`convoy-<uuid>`, `ensure-<hash>`)
    /// is not an address. One indexed read by the role and project labels finds it.
    async fn row(&self, namespace: &str, address: &Address) -> Result<Option<Row>, String> {
        let Some(table) = self.table(address.kind()) else { return Err(format!("no table serves `{}` subjects", address.kind())) };
        let project = address.get(Kind::Project).unwrap_or_default();
        let Some(convoy) = query_convoy_by_role(&self.backend, namespace, address.name(), project).await? else { return Ok(None) };
        Ok(tables::resource_rows(&json!({ "items": [convoy] }), &table)?.into_iter().next())
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
