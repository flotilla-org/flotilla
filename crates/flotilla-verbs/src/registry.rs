//! The verbs a composition root collects. Each crate that performs verbs' effects exposes a
//! `register_verbs(&mut VerbRegistry, …)` taking the ports its handlers need, and the composition
//! root calls each, as it calls `register_crew` for kinds (#4267 ruling 1). The crate owning a
//! subject kind registers its [`SubjectTable`] here too (#4355).
use std::{fmt, sync::Arc};

use flotilla_address::Kind;

use crate::{Handler, NativeVerb, Presentation, SubjectTable};

/// One registered verb: its handler, which carries its declaration, and its presentation.
#[derive(Clone)]
pub struct Registered {
    pub handler: Arc<dyn Handler>,
    pub presentation: Presentation,
}

/// A second registration of a verb name or a subject kind: a programming error in the declaring
/// crates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Duplicate {
    Verb(&'static str),
    Subject(Kind),
}

impl fmt::Display for Duplicate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Verb(name) => write!(formatter, "verb `{name}` is registered twice"),
            Self::Subject(kind) => write!(formatter, "the `{kind}` subject table is registered twice"),
        }
    }
}

impl std::error::Error for Duplicate {}

/// Native verbs by registration order, each name once, and subject tables, each kind once.
#[derive(Clone, Default)]
pub struct VerbRegistry {
    verbs: Vec<Registered>,
    subjects: Vec<SubjectTable>,
}

impl VerbRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `verb` with its presentation. A name registered twice is refused.
    pub fn register(&mut self, verb: impl NativeVerb, presentation: Presentation) -> Result<(), Duplicate> {
        let handler: Arc<dyn Handler> = Arc::new(verb);
        let name = handler.declaration().name;
        if self.verbs.iter().any(|registered| registered.handler.declaration().name == name) {
            return Err(Duplicate::Verb(name));
        }
        self.verbs.push(Registered { handler, presentation });
        Ok(())
    }

    /// Registers the table serving `subject.kind`'s rows. A kind registered twice is refused.
    pub fn register_subject(&mut self, subject: SubjectTable) -> Result<(), Duplicate> {
        if self.subjects.iter().any(|registered| registered.kind == subject.kind) {
            return Err(Duplicate::Subject(subject.kind));
        }
        self.subjects.push(subject);
        Ok(())
    }

    /// The registered subject tables, in registration order.
    pub fn subjects(&self) -> &[SubjectTable] {
        &self.subjects
    }

    /// The registered verbs, in registration order.
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
        assert_eq!(registry.register(Looked, Presentation::default()), Err(Duplicate::Verb("convoy.look")));
        let verbs = registry.into_verbs();
        assert_eq!(verbs.len(), 1);
        assert_eq!(verbs[0].presentation.label, Some("Look"));
    }

    // Behaviour (#4355): one table serves a kind's subjects; a second registration of the kind is
    // refused and the first is kept.
    #[test]
    fn a_subject_kind_is_registered_once() {
        let table = |table| SubjectTable { kind: Kind::Convoy, table, address: |_| None };
        let mut registry = VerbRegistry::new();
        registry.register_subject(table("convoys")).unwrap();
        assert_eq!(registry.register_subject(table("other")), Err(Duplicate::Subject(Kind::Convoy)));
        assert_eq!(registry.subjects().iter().map(|subject| subject.table).collect::<Vec<_>>(), ["convoys"]);
    }
}
