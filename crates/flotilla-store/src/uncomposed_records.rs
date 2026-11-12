//! The records of an engine kind on the engine's home before its port is installed: every call is
//! refused. The engine holds those kinds; its home's old store holds none of them.

use std::collections::BTreeMap;

use async_trait::async_trait;
use flotilla_resources::engine_records::{EngineObject, EngineRecords};
use futures::stream::BoxStream;

/// Refuses every call for `kind`: the engine's home has no port installed for it yet.
#[derive(Debug)]
pub(crate) struct Uncomposed {
    pub(crate) kind: String,
}

impl Uncomposed {
    fn refused<T>(&self) -> Result<T, String> {
        Err(format!("the engine's {} records are not composed on this daemon", self.kind))
    }
}

#[async_trait]
impl EngineRecords for Uncomposed {
    fn kinds(&self) -> &'static [&'static str] {
        &[]
    }

    fn home(&self) -> String {
        String::new()
    }

    async fn get(&self, _: &str, _: &str) -> Result<Option<EngineObject>, String> {
        self.refused()
    }

    async fn list(&self, _: &str) -> Result<Vec<EngineObject>, String> {
        self.refused()
    }

    async fn sources(&self, _: &str) -> Result<Vec<EngineObject>, String> {
        self.refused()
    }

    async fn copies(&self, _: &str, _: &str) -> Result<Vec<EngineObject>, String> {
        self.refused()
    }

    async fn apply(
        &self,
        _: &str,
        _: &str,
        _: &BTreeMap<String, String>,
        _: &BTreeMap<String, String>,
        _: serde_json::Value,
    ) -> Result<EngineObject, String> {
        self.refused()
    }

    async fn set_status(&self, _: &str, _: &str, _: serde_json::Value) -> Result<EngineObject, String> {
        self.refused()
    }

    async fn delete(&self, _: &str, _: &str) -> Result<(), String> {
        self.refused()
    }

    async fn changes(&self) -> Result<BoxStream<'static, ()>, String> {
        self.refused()
    }
}

#[cfg(test)]
mod tests {
    use flotilla_resources::{Checkout, CheckoutSpec, ObservedCheckoutSpec, RepositoryKey, Vessel};

    use crate::{in_memory::InMemoryBackend, InputMeta, ResourceBackend};

    fn observed_checkout() -> CheckoutSpec {
        CheckoutSpec::Observed(ObservedCheckoutSpec {
            r#ref: "topic".into(),
            path: "/observed".into(),
            repo_ref: RepositoryKey("repo".into()),
            host_ref: "host".into(),
            is_main: false,
        })
    }

    // Behaviour: the engine's home refuses a kind the engine holds while its port is not
    // installed, rather than reading or writing its old store; a backend that is not the
    // engine's home, the observed one, still serves the same kind from its store.
    #[tokio::test]
    async fn the_engine_home_refuses_an_engine_kind_with_no_port_and_the_observed_backend_serves_it() {
        let home = ResourceBackend::new(InMemoryBackend::default());
        home.compose_as_engine_home();
        let meta = InputMeta { name: "checkout".into(), ..Default::default() };
        let refused = home.using::<Checkout>("flotilla").create(&meta, &observed_checkout()).await.expect_err("refused");
        assert!(refused.to_string().contains("not composed"), "{refused}");
        let refused = home.using::<Checkout>("flotilla").get("checkout").await.expect_err("refused");
        assert!(refused.to_string().contains("not composed"), "{refused}");
        let refused = home.using::<Vessel>("flotilla").list().await.expect_err("refused");
        assert!(refused.to_string().contains("not composed"), "{refused}");

        let observed = ResourceBackend::new(InMemoryBackend::observed());
        observed.using::<Checkout>("flotilla").create(&meta, &observed_checkout()).await.expect("observed checkout created");
        let read = observed.using::<Checkout>("flotilla").get("checkout").await.expect("observed checkout read");
        assert_eq!(read.metadata.name, "checkout");
    }
}
