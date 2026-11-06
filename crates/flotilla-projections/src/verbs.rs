//! The verbs whose effects this crate performs (#4267 ruling 1): `convoy.explain`, over the
//! explanation projection. The composition root registers them with [`register_verbs`].
use std::sync::Arc;

use async_trait::async_trait;
use flotilla_verbs::{Call, DuplicateVerb, NativeVerb, Picks, Presentation, Verb, VerbRegistry};
use serde_json::Value;

use crate::projections_actions::ProjectionsActionPort;

/// Registers this crate's verbs, each over `port`.
pub fn register_verbs(registry: &mut VerbRegistry, port: Arc<dyn ProjectionsActionPort>) -> Result<(), DuplicateVerb> {
    registry.register(
        ExplainConvoy { port },
        Presentation {
            label: Some("Explain convoy"),
            help: Some("Why the convoy is where it is, and what it waits on"),
            picks: Picks::Each,
            ..Presentation::default()
        },
    )
}

/// Explains a convoy: why it is where it is, and what it waits on.
#[derive(Verb, serde::Deserialize)]
#[verb(name = "convoy.explain", subject = "convoy", effect = "read", idempotent)]
pub struct Explain {}

/// `convoy.explain` over the explanation projection.
struct ExplainConvoy {
    port: Arc<dyn ProjectionsActionPort>,
}

#[async_trait]
impl NativeVerb for ExplainConvoy {
    type Params = Explain;

    async fn run(&self, call: Call<'_>, _params: &Explain) -> Result<Option<Value>, String> {
        let [subject] = call.subjects else { return Err("convoy.explain runs on one convoy at a time".into()) };
        let name = subject.row.get("metadata.name").and_then(Value::as_str).ok_or("the convoy row has no name")?;
        let namespace = subject.row.get("metadata.namespace").and_then(Value::as_str);
        let explanation = self.port.explain_convoy_internal(namespace, name).await?;
        serde_json::to_value(explanation).map(Some).map_err(|error| error.to_string())
    }
}
