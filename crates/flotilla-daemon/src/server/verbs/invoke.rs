//! One invocation (#4182 rulings 6 and 7): check the request against the verb's declaration,
//! then for each subject check that it exists, that the caller may run the verb on it, and that
//! its `when` holds of its current row, and run the verb once per subject (`one`, `each`) or once
//! with them all (`set`). The invocation id flows into the verb's effect; the route keeps no
//! record of past invocations, so a repeat runs again and the effect does nothing new.
use std::{collections::BTreeMap, sync::Arc};

use flotilla_address::Address;
use flotilla_protocol::verbs::{Invocation, Invoked, Outcome};
use flotilla_schema::{ParamDeclaration, ParamType};
use serde_json::{Map, Value};

use super::catalog::{Catalog, Entry, Picks, Subject, Subjects};
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

/// Whether a caller may run a verb on a subject (#4182 ruling 6). It allows everything for now.
pub(crate) fn policy(_caller: &str, _entry: &Entry, _subject: &Address) -> Result<(), String> {
    Ok(())
}

/// The verb route over a catalog and the subjects' rows.
pub(crate) struct Verbs {
    pub catalog: Catalog,
    pub subjects: Arc<dyn Subjects>,
}

impl Verbs {
    /// Runs one invocation inside its `verb.invocation` span, which is the intent record
    /// (#4197): the caller, verb, subjects, params and invocation id, then each subject's
    /// outcome or the request's refusal. The verb's own spans nest under it.
    pub(crate) async fn invoke(&self, caller: &str, name: &str, request: Invocation) -> Result<Invoked, Refusal> {
        let subjects = Value::from(request.subjects.clone()).to_string();
        let params = Value::Object(request.params.clone()).to_string();
        let span = tracing::info_span!(
            "verb.invocation",
            verb.caller = caller,
            verb.name = name,
            verb.invocation = %request.invocation,
            verb.subjects = subjects,
            verb.params = params,
            verb.outcomes = tracing::field::Empty,
            verb.refusal = tracing::field::Empty,
        );
        let invoked = self.invoke_inner(caller, name, request).instrument(span.clone()).await;
        span.in_scope(|| match &invoked {
            Ok(invoked) => {
                let outcomes = serde_json::to_string(&invoked.outcomes).unwrap_or_default();
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

    async fn invoke_inner(&self, caller: &str, name: &str, request: Invocation) -> Result<Invoked, Refusal> {
        let entry = self.catalog.get(name).ok_or_else(|| Refusal::UnknownVerb(name.to_owned()))?;
        if let Some(broken) = &entry.broken {
            return Err(Refusal::Conflict(format!("verb `{name}` is broken: {broken}")));
        }
        let addresses = self.addresses(entry, &request)?;
        let params = checked_params(&entry.declaration.params, &request.params)?;
        let outcomes = self.run(caller, entry, &addresses, &params, &request.invocation).await;
        Ok(Invoked { invocation: request.invocation, outcomes })
    }

    /// The subjects' addresses: at least one, exactly one for `picks: one`, each of the verb's
    /// subject kind.
    fn addresses(&self, entry: &Entry, request: &Invocation) -> Result<Vec<Address>, Refusal> {
        if request.invocation.is_empty() {
            return Err(Refusal::Invalid("an invocation id is required".into()));
        }
        if request.subjects.is_empty() {
            return Err(Refusal::Invalid("at least one subject is required".into()));
        }
        if entry.picks() == Picks::One && request.subjects.len() > 1 {
            return Err(Refusal::Invalid(format!("verb `{}` takes one subject", entry.declaration.name)));
        }
        let mut addresses: Vec<Address> = Vec::new();
        for subject in &request.subjects {
            let address: Address = subject.parse().map_err(|error| Refusal::Invalid(format!("subject `{subject}`: {error}")))?;
            if address.kind().as_str() != entry.declaration.subject {
                return Err(Refusal::Invalid(format!("subject `{subject}` is not a {}", entry.declaration.subject)));
            }
            if addresses.contains(&address) {
                return Err(Refusal::Invalid(format!("subject `{subject}` is listed twice")));
            }
            addresses.push(address);
        }
        Ok(addresses)
    }

    async fn run(&self, caller: &str, entry: &Entry, addresses: &[Address], params: &Map<String, Value>, invocation: &str) -> Vec<Outcome> {
        let mut ready = Vec::new();
        let mut outcomes = BTreeMap::new();
        for address in addresses {
            match self.check(caller, entry, address).await {
                Ok(subject) => ready.push(subject),
                Err(outcome) => {
                    outcomes.insert(address.to_string(), *outcome);
                }
            }
        }
        if entry.picks() == Picks::Set {
            if outcomes.is_empty() {
                let ran = entry.handler.run(&ready, &bound(params, entry, &ready[0]), invocation).await;
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
                let ran = entry.handler.run(std::slice::from_ref(&subject), &bound(params, entry, &subject), invocation).await;
                outcomes.insert(subject.address.to_string(), outcome(&subject, ran));
            }
        }
        addresses.iter().filter_map(|address| outcomes.remove(&address.to_string())).collect()
    }

    /// The subject's current row, once it exists, policy allows the caller and `when` holds.
    async fn check(&self, caller: &str, entry: &Entry, address: &Address) -> Result<Subject, Box<Outcome>> {
        let subject = address.to_string();
        let row = match self.subjects.row(address).await {
            Ok(Some(row)) => row,
            Ok(None) => return Err(Box::new(Outcome::failed(subject, format!("no {} at `{address}`", entry.declaration.subject)))),
            Err(error) => return Err(Box::new(Outcome::failed(subject, error))),
        };
        policy(caller, entry, address).map_err(|reason| Box::new(Outcome::refused(subject.clone(), reason)))?;
        if let Some(failing) = entry.when.iter().find(|condition| !condition.holds(&row)) {
            return Err(Box::new(Outcome::not_applicable(subject, failing.text.clone())));
        }
        Ok(Subject { address: address.clone(), row })
    }
}

fn outcome(subject: &Subject, ran: Result<Option<Value>, String>) -> Outcome {
    match ran {
        Ok(result) => Outcome::done(subject.address.to_string(), result),
        Err(error) => Outcome::failed(subject.address.to_string(), error),
    }
}

/// The parameters checked against the declaration: no undeclared name, every required one
/// present unless it has a default, and each value of its declared type. Literal defaults are
/// filled in here; `$row.<column>` defaults are bound per subject.
fn checked_params(declared: &[ParamDeclaration], given: &Map<String, Value>) -> Result<Map<String, Value>, Refusal> {
    if let Some(unknown) = given.keys().find(|name| !declared.iter().any(|param| param.name == name.as_str())) {
        return Err(Refusal::Invalid(format!("no parameter `{unknown}`")));
    }
    let mut params = Map::new();
    for param in declared {
        match given.get(param.name).filter(|value| !value.is_null()) {
            Some(value) => {
                check_type(param, value).map_err(|error| Refusal::Invalid(format!("parameter `{}`: {error}", param.name)))?;
                params.insert(param.name.into(), value.clone());
            }
            None => match param.default {
                Some(default) if default.starts_with("$row.") => {}
                Some(default) => {
                    params.insert(param.name.into(), literal(param, default));
                }
                None if param.required => return Err(Refusal::Invalid(format!("parameter `{}` is required", param.name))),
                None => {}
            },
        }
    }
    Ok(params)
}

fn check_type(param: &ParamDeclaration, value: &Value) -> Result<(), String> {
    let fits = match param.ty {
        ParamType::Str | ParamType::Text => value.is_string(),
        ParamType::Enum => value.as_str().is_some_and(|value| param.options.contains(&value)),
        ParamType::Integer => value.is_i64() || value.is_u64(),
        ParamType::Number => value.is_number(),
        ParamType::Bool => value.is_boolean(),
        ParamType::Subject(kind) => {
            let address = value.as_str().ok_or("expected an address")?.parse::<Address>().map_err(|error| error.to_string())?;
            address.kind().as_str() == kind
        }
    };
    if fits {
        Ok(())
    } else {
        Err(format!("expected {}", param.ty.as_str()))
    }
}

/// A literal default as its parameter's JSON value.
fn literal(param: &ParamDeclaration, default: &str) -> Value {
    match param.ty {
        ParamType::Integer | ParamType::Number | ParamType::Bool => serde_json::from_str(default).unwrap_or_else(|_| Value::from(default)),
        _ => Value::from(default),
    }
}

/// `params` with each unset `$row.<column>` default bound from `subject`'s row.
fn bound(params: &Map<String, Value>, entry: &Entry, subject: &Subject) -> Map<String, Value> {
    let mut bound = params.clone();
    for param in &entry.declaration.params {
        if let Some(column) = param.default.and_then(|default| default.strip_prefix("$row.")) {
            if !bound.contains_key(param.name) {
                if let Some(value) = subject.row.get(column).filter(|value| !value.is_null()) {
                    bound.insert(param.name.into(), value.clone());
                }
            }
        }
    }
    bound
}
