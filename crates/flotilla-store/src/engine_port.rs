//! The engine ports a backend serves its kinds from (#3108): one typed [`KindRecords`] port per
//! resource kind, and what any reader may ask of it whatever its type.

use std::{any::Any, collections::BTreeSet, sync::Arc};

use flotilla_resources::engine_records::{KindCode, KindRecords};

use crate::{Resource, ResourceBackend};

/// An engine port that serves one or more resource kinds: installed on a backend, it installs a
/// [`KindRecords`] port for each through [`ResourceBackend::install_engine_kind`].
pub trait EnginePort: Send + Sync {
    fn install(self: Arc<Self>, backend: &ResourceBackend);
}

/// What any reader may ask of a kind's port, whatever the kind's type.
#[async_trait::async_trait]
pub(crate) trait KindFacts: Send + Sync {
    /// See [`KindRecords::codes`].
    fn codes(&self) -> Vec<KindCode>;
    /// See [`KindRecords::unlogged_changes`].
    fn unlogged_changes(&self) -> Option<u64>;
    /// See [`KindRecords::reads`].
    fn reads(&self) -> bool;
    /// See [`KindRecords::home`].
    fn home(&self) -> String;
    /// The homes holding any record of the kind.
    async fn homes(&self) -> Result<BTreeSet<String>, String>;
}

struct Facts<R: Resource>(Arc<dyn KindRecords<R>>);

#[async_trait::async_trait]
impl<R: Resource> KindFacts for Facts<R> {
    fn codes(&self) -> Vec<KindCode> {
        self.0.codes()
    }

    fn unlogged_changes(&self) -> Option<u64> {
        self.0.unlogged_changes()
    }

    fn reads(&self) -> bool {
        self.0.reads()
    }

    fn home(&self) -> String {
        self.0.home()
    }

    async fn homes(&self) -> Result<BTreeSet<String>, String> {
        Ok(crate::port_reads::sources(&*self.0).await?.into_iter().map(|record| record.home).collect())
    }
}

/// The facts of a kind the engine's home holds before its port is installed: its records are
/// refused.
struct Uncomposed(String);

#[async_trait::async_trait]
impl KindFacts for Uncomposed {
    fn codes(&self) -> Vec<KindCode> {
        Vec::new()
    }

    fn unlogged_changes(&self) -> Option<u64> {
        None
    }

    fn reads(&self) -> bool {
        true
    }

    fn home(&self) -> String {
        String::new()
    }

    async fn homes(&self) -> Result<BTreeSet<String>, String> {
        Err(format!("the engine's {} records are not composed on this daemon", self.0))
    }
}

pub(crate) fn uncomposed_facts(kind: &str) -> Arc<dyn KindFacts> {
    Arc::new(Uncomposed(kind.to_owned()))
}

/// One kind's installed port: typed, held as the `Arc<dyn KindRecords<R>>` of the kind it was
/// installed for, and its facts.
pub(crate) struct Installed {
    typed: Arc<dyn Any + Send + Sync>,
    facts: Arc<dyn KindFacts>,
    mark: u64,
}

impl std::fmt::Debug for Installed {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Installed").field("mark", &self.mark).finish_non_exhaustive()
    }
}

impl Installed {
    pub(crate) fn new<R: Resource>(records: Arc<dyn KindRecords<R>>, mark: u64) -> Self {
        Self { facts: Arc::new(Facts(Arc::clone(&records))), typed: Arc::new(records), mark }
    }

    /// The port as `R`'s, with its mark. A backend installs each port under its own kind's name, so
    /// `R` is the kind it was installed for.
    pub(crate) fn typed<R: Resource>(&self) -> Option<(Arc<dyn KindRecords<R>>, u64)> {
        let typed = self.typed.downcast_ref::<Arc<dyn KindRecords<R>>>();
        debug_assert!(typed.is_some(), "{} is installed as another type", R::API_PATHS.kind);
        typed.map(|records| (Arc::clone(records), self.mark))
    }

    pub(crate) fn facts(&self) -> Arc<dyn KindFacts> {
        Arc::clone(&self.facts)
    }

    pub(crate) fn mark(&self) -> u64 {
        self.mark
    }
}
