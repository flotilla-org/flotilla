//! The verbs a composition root collects. Each crate that performs verbs' effects exposes a
//! `register_verbs(&mut VerbRegistry, …)` taking the ports its handlers need, and the composition
//! root calls each, as it calls `register_crew` for kinds (#4267 ruling 1).
use std::{fmt, sync::Arc};

use crate::{Handler, NativeVerb, Presentation};

/// One registered verb: its handler, which carries its declaration, and its presentation.
#[derive(Clone)]
pub struct Registered {
    pub handler: Arc<dyn Handler>,
    pub presentation: Presentation,
}

/// A second verb under a name already registered: a programming error in the declaring crates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateVerb(pub &'static str);

impl fmt::Display for DuplicateVerb {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "verb `{}` is registered twice", self.0)
    }
}

impl std::error::Error for DuplicateVerb {}

/// Native verbs by registration order, each name once.
#[derive(Clone, Default)]
pub struct VerbRegistry {
    verbs: Vec<Registered>,
}

impl VerbRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `verb` with its presentation. A name registered twice is refused.
    pub fn register(&mut self, verb: impl NativeVerb, presentation: Presentation) -> Result<(), DuplicateVerb> {
        self.register_handler(Arc::new(verb), presentation)
    }

    /// Registers a verb whose parameter type is already erased.
    pub fn register_handler(&mut self, handler: Arc<dyn Handler>, presentation: Presentation) -> Result<(), DuplicateVerb> {
        let name = handler.declaration().name;
        if self.verbs.iter().any(|registered| registered.handler.declaration().name == name) {
            return Err(DuplicateVerb(name));
        }
        self.verbs.push(Registered { handler, presentation });
        Ok(())
    }

    /// The registered verbs, in registration order.
    pub fn iter(&self) -> impl Iterator<Item = &Registered> {
        self.verbs.iter()
    }

    pub fn into_verbs(self) -> Vec<Registered> {
        self.verbs
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use serde::Deserialize;
    use serde_json::Value;

    use super::*;
    use crate::{Call, Verb};

    #[derive(Verb, Deserialize)]
    #[verb(name = "convoy.look", subject = "convoy", effect = "read")]
    struct Look {}

    struct Looked;

    #[async_trait]
    impl NativeVerb for Looked {
        type Params = Look;

        async fn run(&self, _call: Call<'_>, _params: &Look) -> Result<Option<Value>, String> {
            Ok(None)
        }
    }

    // Behaviour (#4267 ruling 1): verbs from several declaring crates meet in one registry, each
    // name once; a second verb under a registered name is refused and the first is kept.
    #[test]
    fn a_name_is_registered_once() {
        let mut registry = VerbRegistry::new();
        registry.register(Looked, Presentation { label: Some("Look"), ..Presentation::default() }).unwrap();
        assert_eq!(registry.register(Looked, Presentation::default()), Err(DuplicateVerb("convoy.look")));
        let verbs = registry.into_verbs();
        assert_eq!(verbs.len(), 1);
        assert_eq!(verbs[0].presentation.label, Some("Look"));
    }
}
