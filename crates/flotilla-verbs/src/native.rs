//! The handler a native verb implements, and the same handler with its parameter type erased so
//! one catalog holds every verb.
use std::any::Any;

use async_trait::async_trait;
use flotilla_address::Address;
use flotilla_protocol::{table::Row, CommandCaller};
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

use crate::{Verb, VerbDeclaration};

/// One subject a verb runs on: its address and its current row.
#[derive(Debug, Clone)]
pub struct Subject {
    pub address: Address,
    pub row: Row,
}

/// One run of a verb, as the route calls it.
#[derive(Debug, Clone, Copy)]
pub struct Call<'a> {
    /// The resolved caller, the one the `verb.invocation` span records: the principal acting
    /// (#4267 ruling 4).
    pub caller: &'a CommandCaller,
    /// Every subject of this run: one under `picks: one` and `each`, all of them under `set`.
    pub subjects: &'a [Subject],
    /// The caller's invocation id, for a verb that takes it into its effect.
    pub invocation: &'a str,
}

/// A native verb, called by the route once per subject (`one`, `each`) or once with every
/// subject (`set`). Its parameters arrive deserialized into the type that declares it.
#[async_trait]
pub trait NativeVerb: Send + Sync + 'static {
    /// The parameter struct whose `#[derive(Verb)]` is the verb's declaration.
    type Params: Verb + DeserializeOwned + Send + Sync + 'static;

    /// Runs the verb. `Ok` is the verb's answer to the caller, `Err` why it failed.
    async fn run(&self, call: Call<'_>, params: &Self::Params) -> Result<Option<Value>, String>;
}

/// A verb's deserialized parameters, their type erased so one catalog holds every verb.
pub type Params = Box<dyn Any + Send + Sync>;

/// A native verb with its parameter type erased, as a registry and the catalog hold it.
#[async_trait]
pub trait Handler: Send + Sync {
    fn declaration(&self) -> VerbDeclaration;

    /// The request's parameters as the verb's own type; `Err` is why they do not deserialize.
    fn params(&self, params: Map<String, Value>) -> Result<Params, String>;

    /// Runs the verb on parameters [`Handler::params`] made.
    async fn run(&self, call: Call<'_>, params: &Params) -> Result<Option<Value>, String>;
}

#[async_trait]
impl<V: NativeVerb> Handler for V {
    fn declaration(&self) -> VerbDeclaration {
        V::Params::declaration()
    }

    fn params(&self, params: Map<String, Value>) -> Result<Params, String> {
        serde_json::from_value::<V::Params>(Value::Object(params))
            .map(|params| Box::new(params) as Params)
            .map_err(|error| error.to_string())
    }

    async fn run(&self, call: Call<'_>, params: &Params) -> Result<Option<Value>, String> {
        let params = params.downcast_ref::<V::Params>().ok_or("the parameters are another verb's")?;
        NativeVerb::run(self, call, params).await
    }
}
