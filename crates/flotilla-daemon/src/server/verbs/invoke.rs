//! One invocation (#4182 rulings 6 and 7): check the request against the verb's declaration,
//! then for each subject check that it exists, that the caller may run the verb on it, and that
//! its `when` holds of its current row, and run the verb once per subject (`one`, `each`) or once
//! with them all (`set`). The invocation id flows into the verb's effect; the route keeps no
//! record of past invocations, so a repeat runs again and the effect does nothing new.
use std::{collections::BTreeMap, sync::Arc};

use flotilla_address::Address;
use flotilla_protocol::verbs::{Invocation, Invoked, Outcome};
use flotilla_schema::ParamType;
use serde_json::{Map, Value};

use super::catalog::{Catalog, Entry, Params, Picks, Subject, Subjects};
use tracing::Instrument;

/// Why an invocation was refused as a whole, before any subject ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// No verb has this name.
    UnknownVerb(String),
    /// The request does not fit the verb's declaration.
    Invalid(String),
    /// The verb failed its load check.
    Conflict(String),
}

/// Whether a caller may run a verb on a subject (#4182 ruling 6): `Err` carries the reason a
/// `refused` outcome gives.
pub(crate) type Policy = fn(caller: &str, entry: &Entry, subject: &Address) -> Result<(), String>;

/// The policy until ruling 6 decides per caller: everything is allowed.
pub(crate) fn allow_all(_caller: &str, _entry: &Entry, _subject: &Address) -> Result<(), String> {
    Ok(())
}

/// The most text a span field carries. Longer params or outcomes are cut and end in `…`, so a
/// long text parameter cannot flood the trace and log pipeline.
pub(crate) const FIELD_LIMIT: usize = 4096;

/// The longest invocation id the route takes; a client id (a UUID is 36 bytes) needs no more.
pub(crate) const INVOCATION_LIMIT: usize = 256;

/// The most subjects one invocation names.
pub(crate) const SUBJECT_LIMIT: usize = 1024;

/// The verb route over a catalog, the subjects' rows and the policy.
pub(crate) struct Verbs {
    pub catalog: Catalog,
    pub subjects: Arc<dyn Subjects>,
    pub policy: Policy,
}

impl Verbs {
    /// Runs one invocation inside its `verb.invocation` span, which is the intent record
    /// (#4197): the caller, verb, subjects, params and invocation id, then each subject's
    /// outcome or the request's refusal. The verb's own spans nest under it. A `done` outcome's
    /// `result` is the verb's answer to the caller, not part of the record, so the span leaves it
    /// out; every field taken from the request is bounded to [`FIELD_LIMIT`], since a request
    /// refused for its size is still traced. Subjects are read in `namespace`.
    pub(crate) async fn invoke(&self, caller: &str, namespace: &str, name: &str, request: Invocation) -> Result<Invoked, Refusal> {
        let subjects = bounded(serde_json::to_string(&request.subjects).unwrap_or_default());
        let params = bounded(serde_json::to_string(&request.params).unwrap_or_default());
        let (name_field, id_field) = (bounded(name.to_owned()), bounded(request.invocation.clone()));
        let span = tracing::info_span!(
            "verb.invocation",
            verb.caller = caller,
            verb.name = name_field,
            verb.invocation = id_field,
            verb.subjects = subjects,
            verb.params = params,
            verb.outcomes = tracing::field::Empty,
            verb.refusal = tracing::field::Empty,
        );
        let invoked = self.invoke_inner(caller, namespace, name, request).instrument(span.clone()).await;
        span.in_scope(|| match &invoked {
            Ok(invoked) => {
                let outcomes: Vec<_> = invoked.outcomes.iter().map(|outcome| Outcome { result: None, ..outcome.clone() }).collect();
                let outcomes = bounded(serde_json::to_string(&outcomes).unwrap_or_default());
                span.record("verb.outcomes", outcomes.as_str());
                tracing::info!(outcomes = %outcomes, "verb invoked");
            }
            Err(refusal) => {
                span.record("verb.refusal", tracing::field::debug(refusal));
                tracing::info!(?refusal, "verb refused");
            }
        });
        invoked
    }

    async fn invoke_inner(&self, caller: &str, namespace: &str, name: &str, request: Invocation) -> Result<Invoked, Refusal> {
        let entry = self.catalog.get(name).ok_or_else(|| Refusal::UnknownVerb(name.to_owned()))?;
        if let Some(broken) = &entry.broken {
            return Err(Refusal::Conflict(format!("verb `{name}` is broken: {broken}")));
        }
        let addresses = self.addresses(entry, &request)?;
        let params = params(entry, request.params)?;
        let outcomes = self.run(caller, namespace, entry, &addresses, &params, &request.invocation).await;
        Ok(Invoked { invocation: request.invocation, outcomes })
    }

    /// The subjects' addresses: at least one and at most [`SUBJECT_LIMIT`], exactly one for
    /// `picks: one`, each of the verb's subject kind and none repeated.
    fn addresses(&self, entry: &Entry, request: &Invocation) -> Result<Vec<Address>, Refusal> {
        if request.invocation.is_empty() {
            return Err(Refusal::Invalid("an invocation id is required".into()));
        }
        if request.invocation.len() > INVOCATION_LIMIT {
            return Err(Refusal::Invalid(format!("an invocation id is at most {INVOCATION_LIMIT} bytes")));
        }
        if request.subjects.is_empty() {
            return Err(Refusal::Invalid("at least one subject is required".into()));
        }
        if request.subjects.len() > SUBJECT_LIMIT {
            return Err(Refusal::Invalid(format!("an invocation names at most {SUBJECT_LIMIT} subjects")));
        }
        if entry.picks() == Picks::One && request.subjects.len() > 1 {
            return Err(Refusal::Invalid(format!("verb `{}` takes one subject", entry.declaration.name)));
        }
        let mut addresses: Vec<Address> = Vec::new();
        for subject in &request.subjects {
            let address: Address = subject.parse().map_err(|error| Refusal::Invalid(format!("subject `{subject}`: {error}")))?;
            if address.kind() != entry.declaration.subject {
                return Err(Refusal::Invalid(format!("subject `{subject}` is not a {}", entry.declaration.subject)));
            }
            if addresses.contains(&address) {
                return Err(Refusal::Invalid(format!("subject `{subject}` is listed twice")));
            }
            addresses.push(address);
        }
        Ok(addresses)
    }

    async fn run(
        &self,
        caller: &str,
        namespace: &str,
        entry: &Entry,
        addresses: &[Address],
        params: &Params,
        invocation: &str,
    ) -> Vec<Outcome> {
        let mut ready = Vec::new();
        let mut outcomes = BTreeMap::new();
        for address in addresses {
            match self.check(caller, namespace, entry, address).await {
                Ok(subject) => ready.push(subject),
                Err(outcome) => {
                    outcomes.insert(address.to_string(), *outcome);
                }
            }
        }
        if entry.picks() == Picks::Set {
            if outcomes.is_empty() {
                // `ready` is not empty: the request names at least one subject, and none failed.
                let ran = entry.handler.run(&ready, params, invocation).await;
                for subject in &ready {
                    outcomes.insert(subject.address.to_string(), outcome(subject, ran.clone()));
                }
            } else {
                for subject in &ready {
                    let error = "not run: another subject of the set did not pass its checks".to_owned();
                    outcomes.insert(subject.address.to_string(), Outcome::failed(subject.address.to_string(), error));
                }
            }
        } else {
            for subject in ready {
                let ran = entry.handler.run(std::slice::from_ref(&subject), params, invocation).await;
                outcomes.insert(subject.address.to_string(), outcome(&subject, ran));
            }
        }
        addresses.iter().filter_map(|address| outcomes.remove(&address.to_string())).collect()
    }

    /// The subject's current row, once it exists, policy allows the caller and `when` holds.
    async fn check(&self, caller: &str, namespace: &str, entry: &Entry, address: &Address) -> Result<Subject, Box<Outcome>> {
        let subject = address.to_string();
        let row = match self.subjects.row(namespace, address).await {
            Ok(Some(row)) => row,
            Ok(None) => return Err(Box::new(Outcome::failed(subject, format!("no {} at `{address}`", entry.declaration.subject)))),
            Err(error) => return Err(Box::new(Outcome::failed(subject, error))),
        };
        (self.policy)(caller, entry, address).map_err(|reason| Box::new(Outcome::refused(subject.clone(), reason)))?;
        if let Some(failing) = entry.failing(&row) {
            return Err(Box::new(Outcome::not_applicable(subject, failing.text.to_owned())));
        }
        Ok(Subject { address: address.clone(), row })
    }
}

/// `text` cut to at most [`FIELD_LIMIT`] bytes on a character boundary, ending in `…` when cut.
fn bounded(mut text: String) -> String {
    if text.len() > FIELD_LIMIT {
        let mut end = FIELD_LIMIT - '…'.len_utf8();
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push('…');
    }
    text
}

fn outcome(subject: &Subject, ran: Result<Option<Value>, String>) -> Outcome {
    match ran {
        Ok(result) => Outcome::done(subject.address.to_string(), result),
        Err(error) => Outcome::failed(subject.address.to_string(), error),
    }
}

/// The request's parameters as the verb's own type. A null value is unset, and a literal default
/// fills an unset parameter; deserialization refuses a missing required parameter or a value of
/// the wrong type. The route checks what the type cannot: that every name is declared, that an
/// enum's value is one of its options, and that a subject parameter names its kind.
fn params(entry: &Entry, given: Map<String, Value>) -> Result<Params, Refusal> {
    let declared = &entry.declaration.params;
    let mut params = entry.defaults.clone();
    for (name, value) in given {
        let Some(param) = declared.iter().find(|param| param.name == name) else {
            return Err(Refusal::Invalid(format!("no parameter `{name}`")));
        };
        match (param.ty, &value) {
            (_, Value::Null) => continue,
            (ParamType::Enum, Value::String(text)) if !param.options.contains(&text.as_str()) => {
                return Err(Refusal::Invalid(format!("parameter `{name}`: `{text}` is not one of {}", param.options.join(", "))));
            }
            (ParamType::Subject(kind), Value::String(text)) => {
                let address: Address = text.parse().map_err(|error| Refusal::Invalid(format!("parameter `{name}`: {error}")))?;
                if address.kind() != kind {
                    return Err(Refusal::Invalid(format!("parameter `{name}`: `{text}` is not a {kind}")));
                }
            }
            _ => {}
        }
        params.insert(name, value);
    }
    entry.handler.params(params).map_err(|error| Refusal::Invalid(format!("parameters: {error}")))
}
