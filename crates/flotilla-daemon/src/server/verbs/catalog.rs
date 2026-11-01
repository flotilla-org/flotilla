//! The verb catalog: each native verb's declaration joined with its presentation entry and
//! checked once, at load (#4182 rulings 2 revised and 10). A verb whose declaration fails the
//! check is listed with the reason in `broken`, never dropped, and the route refuses to run it.
//! Checks of the presentation entry itself come with the entries baked from YAML (#4198).
use std::{
    any::Any,
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use async_trait::async_trait;
use flotilla_address::{Address, Kind};
use flotilla_protocol::table::Row;
use flotilla_schema::{ColumnType, Layout, ParamDeclaration, ParamType, Table, TableRole, TableSet, Verb, VerbDeclaration, VerbEffect};
use serde::de::DeserializeOwned;
use serde_json::{json, Map, Value};

use super::when::Condition;

/// How a verb takes the rows it is offered (#4182 ruling 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Picks {
    /// The cursor row only.
    #[default]
    One,
    /// Any number of rows; the verb runs once per subject.
    Each,
    /// Any number of rows; the verb runs once with them all.
    Set,
}

impl Picks {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Picks::One => "one",
            Picks::Each => "each",
            Picks::Set => "set",
        }
    }
}

/// Presentation and applicability for one native verb (#4182 ruling 10). Built-in entries are
/// code until #4198 bakes them from `verbs/*.yaml`.
#[derive(Debug, Clone, Default)]
pub(crate) struct Presentation {
    pub verb: &'static str,
    pub label: Option<&'static str>,
    pub help: Option<&'static str>,
    pub key: Option<&'static str>,
    pub default: bool,
    pub picks: Picks,
    pub when: Vec<&'static str>,
    pub confirm: Option<&'static str>,
}

/// One subject a verb runs on: its address and its current row.
#[derive(Debug, Clone)]
pub(crate) struct Subject {
    pub address: Address,
    pub row: Row,
}

/// A native verb, called by the route once per subject (`one`, `each`) or once with every
/// subject (`set`). Its parameters arrive deserialized into the type that declares it.
#[async_trait]
pub(crate) trait NativeVerb: Send + Sync + 'static {
    /// The parameter struct whose `#[derive(Verb)]` is the verb's declaration.
    type Params: Verb + DeserializeOwned + Send + Sync + 'static;

    /// Runs the verb. `invocation` is the caller's id, for a verb that takes it into its effect.
    async fn run(&self, subjects: &[Subject], params: &Self::Params, invocation: &str) -> Result<Option<Value>, String>;
}

/// A verb's deserialized parameters, their type erased so one catalog holds every verb.
pub(crate) type Params = Box<dyn Any + Send + Sync>;

/// A native verb with its parameter type erased, as the catalog holds it.
#[async_trait]
pub(crate) trait Handler: Send + Sync {
    fn declaration(&self) -> VerbDeclaration;

    /// The request's parameters as the verb's own type; `Err` is why they do not deserialize.
    fn params(&self, params: Map<String, Value>) -> Result<Params, String>;

    /// Runs the verb on parameters [`Handler::params`] made.
    async fn run(&self, subjects: &[Subject], params: &Params, invocation: &str) -> Result<Option<Value>, String>;
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

    async fn run(&self, subjects: &[Subject], params: &Params, invocation: &str) -> Result<Option<Value>, String> {
        let params = params.downcast_ref::<V::Params>().ok_or("the parameters are another verb's")?;
        NativeVerb::run(self, subjects, params, invocation).await
    }
}

/// Where subjects' rows come from: the table holding a kind's rows, for the load check, and a
/// subject's current row, for the route's re-check.
#[async_trait]
pub(crate) trait Subjects: Send + Sync {
    /// The table holding `kind`'s rows; none when no table serves the kind.
    fn table(&self, kind: Kind) -> Option<Table>;

    /// The current row of the subject at `address`; none when there is no such subject.
    async fn row(&self, address: &Address) -> Result<Option<Row>, String>;
}

/// One listed verb.
pub(crate) struct Entry {
    pub declaration: VerbDeclaration,
    pub presentation: Presentation,
    pub when: Vec<Condition>,
    /// The literal defaults, as their parameters' JSON values.
    pub defaults: Map<String, Value>,
    pub handler: Arc<dyn Handler>,
    /// Why the verb failed its load check.
    pub broken: Option<String>,
}

impl Entry {
    pub(crate) fn label(&self) -> String {
        self.presentation.label.map(str::to_owned).unwrap_or_else(|| generated_label(self.declaration.name))
    }

    pub(crate) fn picks(&self) -> Picks {
        self.presentation.picks
    }

    /// The columns `when` reads.
    pub(crate) fn reads(&self) -> Vec<String> {
        let reads: BTreeSet<String> = self.when.iter().map(|condition| condition.column.clone()).collect();
        reads.into_iter().collect()
    }
}

/// `convoy.start_convoy` → `Start convoy`.
fn generated_label(name: &str) -> String {
    let verb = name.split_once('.').map_or(name, |(_, verb)| verb).replace('_', " ");
    let mut characters = verb.chars();
    characters.next().map(|first| first.to_uppercase().chain(characters).collect()).unwrap_or_default()
}

/// Every native verb, by name.
pub(crate) struct Catalog {
    entries: BTreeMap<&'static str, Entry>,
}

impl Catalog {
    /// Joins `natives` with `presentations` and checks each verb's meaning against `subjects`'
    /// tables. A presentation naming no native verb is a programming error in the built-in set,
    /// caught by its test; it is dropped here with a warning.
    pub(crate) fn new(natives: Vec<Arc<dyn Handler>>, presentations: Vec<Presentation>, subjects: &dyn Subjects) -> Self {
        let mut presentations: BTreeMap<_, _> = presentations.into_iter().map(|presentation| (presentation.verb, presentation)).collect();
        let mut entries = BTreeMap::new();
        for handler in natives {
            let declaration = handler.declaration();
            let presentation =
                presentations.remove(declaration.name).unwrap_or(Presentation { verb: declaration.name, ..Presentation::default() });
            let mut problems = Vec::new();
            let when = presentation
                .when
                .iter()
                .filter_map(|text| Condition::parse(text).map_err(|error| problems.push(error)).ok())
                .collect::<Vec<_>>();
            let mut defaults = Map::new();
            for param in &declaration.params {
                let Some(default) = param.default else { continue };
                match literal(param, default) {
                    Ok(value) => {
                        defaults.insert(param.name.into(), value);
                    }
                    Err(error) => problems.push(format!("parameter `{}`: {error}", param.name)),
                }
            }
            let entry = Entry { declaration, presentation, when, defaults, handler, broken: None };
            problems.extend(check(&entry, subjects));
            let broken = (!problems.is_empty()).then(|| problems.join("; "));
            entries.insert(entry.declaration.name, Entry { broken, ..entry });
        }
        for name in presentations.keys() {
            tracing::warn!(verb = %name, "verb presentation names no native verb");
        }
        Self { entries }
    }

    pub(crate) fn get(&self, name: &str) -> Option<&Entry> {
        self.entries.get(name)
    }

    #[cfg(test)]
    pub(crate) fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.entries.values()
    }

    /// The `verbs` table and its `verbs_params` child.
    pub(crate) fn description() -> TableSet {
        let array = || json!({"type": "array", "items": {"type": "string"}});
        let mut layout = Layout::description("verbs");
        layout.key(|layout| layout.column("name", ColumnType::Text, true));
        layout.column("subject", ColumnType::Text, true);
        layout.property("description", json!("ADR 0066 address kind the verb acts on"));
        layout.column("label", ColumnType::Text, true);
        layout.column("help", ColumnType::Text, false);
        layout.column("key", ColumnType::Text, false);
        layout.column("default", ColumnType::Bool, true);
        layout.categories("effect", VerbEffect::ALL.iter().map(|effect| (effect.as_str().into(), effect.as_str().into())).collect(), true);
        layout.column("idempotent", ColumnType::Bool, true);
        layout.categories(
            "picks",
            [Picks::One, Picks::Each, Picks::Set].iter().map(|picks| (picks.as_str().into(), picks.as_str().into())).collect(),
            true,
        );
        layout.column("when", ColumnType::Cbor, true);
        layout.property("type", array());
        layout.column("confirm", ColumnType::Text, false);
        layout.column("reads", ColumnType::Cbor, true);
        layout.property("type", array());
        layout.column("result_subject", ColumnType::Text, false);
        layout.column("source", ColumnType::Text, true);
        layout.column("broken", ColumnType::Text, false);
        layout.table("params", TableRole::Child, |layout| {
            layout.key(|layout| layout.column("name", ColumnType::Text, true));
            layout.column("label", ColumnType::Text, true);
            layout.column("type", ColumnType::Text, true);
            layout.property("description", json!("string, text, integer, number, bool, enum, or a subject's address kind"));
            layout.column("required", ColumnType::Bool, true);
            layout.column("default", ColumnType::Text, false);
            layout.column("options", ColumnType::Cbor, false);
            layout.property("type", array());
            layout.column("pick_view", ColumnType::Text, false);
            layout.column("pick_where", ColumnType::Text, false);
        });
        let mut set = layout.finish();
        // The child's inherited key reads as the verb it belongs to: `(verb, name)`.
        for column in &mut set.tables[1].key {
            if column.name == "verbs_name" {
                column.name = "verb".into();
            }
        }
        set.kind = "Verb".into();
        set
    }

    /// The `verbs` rows and the `verbs_params` rows.
    pub(crate) fn rows(&self) -> (Vec<Row>, Vec<Row>) {
        let mut verbs = Vec::new();
        let mut params = Vec::new();
        for entry in self.entries.values() {
            let VerbDeclaration { name, subject, effect, idempotent, result, .. } = &entry.declaration;
            let presentation = &entry.presentation;
            verbs.push(Row::from([
                ("name".into(), json!(name)),
                ("subject".into(), json!(subject.as_str())),
                ("label".into(), json!(entry.label())),
                ("help".into(), json!(presentation.help)),
                ("key".into(), json!(presentation.key)),
                ("default".into(), json!(presentation.default)),
                ("effect".into(), json!(effect.as_str())),
                ("idempotent".into(), json!(idempotent)),
                ("picks".into(), json!(presentation.picks.as_str())),
                ("when".into(), json!(presentation.when)),
                ("confirm".into(), json!(presentation.confirm)),
                ("reads".into(), json!(entry.reads())),
                ("result_subject".into(), json!(result.map(Kind::as_str))),
                ("source".into(), json!("builtin")),
                ("broken".into(), json!(entry.broken)),
            ]));
            for param in &entry.declaration.params {
                let options = (param.ty == ParamType::Enum).then_some(&param.options);
                params.push(Row::from([
                    ("verb".into(), json!(name)),
                    ("name".into(), json!(param.name)),
                    ("label".into(), json!(generated_label(param.name))),
                    ("type".into(), json!(param.ty.as_str())),
                    ("required".into(), json!(param.required)),
                    ("default".into(), json!(param.default)),
                    ("options".into(), json!(options)),
                    // A subject parameter picks from its kind's table until presentation names a
                    // view and a filter.
                    ("pick_view".into(), Value::Null),
                    ("pick_where".into(), Value::Null),
                ]));
            }
        }
        (verbs, params)
    }
}

/// The declaration check: what makes a verb unrunnable.
fn check(entry: &Entry, subjects: &dyn Subjects) -> Vec<String> {
    let declaration = &entry.declaration;
    let mut problems = Vec::new();
    let table = subjects.table(declaration.subject);
    if table.is_none() {
        problems.push(format!("no table serves `{}` subjects", declaration.subject));
    }
    for param in declaration.params.iter().filter(|param| param.ty == ParamType::Enum && param.options.is_empty()) {
        problems.push(format!("enum parameter `{}` lists no options", param.name));
    }
    if let Some(table) = &table {
        let columns: BTreeSet<&str> = table.key.iter().chain(&table.columns).map(|column| column.name.as_str()).collect();
        for read in entry.reads() {
            if !columns.contains(read.as_str()) {
                problems.push(format!("`{read}` is not a column of `{}`", table.name));
            }
        }
    }
    problems
}

/// A literal default as its parameter's JSON value, once it fits the parameter's type.
fn literal(param: &ParamDeclaration, default: &str) -> Result<Value, String> {
    let parsed = match param.ty {
        ParamType::Integer => default.parse::<i64>().ok().map(Value::from),
        ParamType::Number => default.parse::<f64>().ok().and_then(serde_json::Number::from_f64).map(Value::Number),
        ParamType::Bool => default.parse::<bool>().ok().map(Value::from),
        ParamType::Enum if !param.options.contains(&default) => {
            return Err(format!("default `{default}` is not one of {}", param.options.join(", ")))
        }
        ParamType::Subject(kind) if default.parse::<Address>().map_or(true, |address| address.kind() != kind) => {
            return Err(format!("default `{default}` is not a {kind} address"))
        }
        _ => Some(Value::from(default)),
    };
    parsed.ok_or_else(|| format!("default `{default}` does not parse as {}", param.ty.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Behaviour (#4182 ruling 10): a native verb with no presentation entry is listed with a
    // label generated from its name and no `when`.
    #[test]
    fn labels_are_generated_from_the_verb_name() {
        assert_eq!(generated_label("project.start_convoy"), "Start convoy");
        assert_eq!(generated_label("convoy.explain"), "Explain");
    }
}
