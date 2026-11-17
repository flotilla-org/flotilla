//! The rows route's `where` (#4108): conditions in the `rigging-condition` language, checked
//! against the served description. The SQLite source translates a condition to SQL; resource and
//! derived sources evaluate it over each row, after pushing what the store indexes down.
use super::{bad, Failure, Row, KEY};
use flotilla_schema::{where_operators, ColumnType, Filterable, Table};
use flotilla_store::StoreQuery;
use rigging_condition::{CheckError, Checked, Condition, Field, Literal, Lookup, Operand, Path, Relop, Type, Value as Cell};
use serde_json::{json, Value};
use std::borrow::Cow;

/// The columns a table's conditions may read. A resource kind's `metadata.labels`, inline CBOR
/// in rows, is the one map: `"k" in metadata.labels` and `metadata.labels["k"]`.
pub(crate) fn fields(table: &Table) -> Filterable {
    let mut fields = table.filterable();
    if table.columns.iter().any(|column| column.name == "metadata.labels" && column.ty == ColumnType::Cbor) {
        let labels = "metadata.labels".parse::<Path>().expect("a path");
        fields.declare("metadata.labels", labels, Type::Map(Box::new(Type::String)), false).expect("labels extend no other column");
    }
    fields
}

/// Lists on each column the operators its type allows (`whereOperators`).
pub(crate) fn describe_operators(table: &mut Table) {
    let fields = fields(table);
    for column in table.key.iter_mut().chain(&mut table.columns) {
        column.properties.insert("whereOperators".into(), json!(where_operators(fields.field_type(&column.name))));
        if let Some(values) = column.properties.get_mut("additionalProperties") {
            values["whereOperators"] = json!(where_operators(Some(&Type::String)));
        }
    }
}

/// A read's conditions: every `scope` and `where`, conjoined, and each apart.
pub(crate) struct Conditions {
    /// Every `scope` and `where`, conjoined: what the read's rows match. `None` when there is none.
    pub filter: Option<Checked<Condition>>,
    /// Every `scope`, conjoined: what a count's `total` counts.
    pub scope: Option<Checked<Condition>>,
    /// Each `where`, which a field's distinct values skip when it mentions only that field.
    pub clauses: Vec<Clause>,
}

/// One `where` condition, and the one field it mentions, if it mentions just one.
#[derive(Clone)]
pub(crate) struct Clause {
    pub condition: Checked<Condition>,
    pub only: Option<String>,
}

/// Every `scope` and `where` parameter, checked. Parsing is lenient (any CEL spelling of a
/// condition in the subset); the cursor binds the canonical text of their conjunction.
pub(crate) fn parse(params: &[(String, String)], table: &Table, fields: &Filterable) -> Result<Conditions, Failure> {
    let terms = |parameter: &str| -> Result<Vec<Condition>, Failure> {
        params
            .iter()
            .filter(|(name, _)| name == parameter)
            .map(|(_, text)| Condition::parse_lenient(text).map_err(|error| bad("invalid_filter", format!("{parameter} {text}: {error}"))))
            .collect()
    };
    let (scope, clauses) = (terms("scope")?, terms("where")?);
    let check = |terms: Vec<Condition>| -> Result<Option<Checked<Condition>>, Failure> {
        if terms.is_empty() {
            return Ok(None);
        }
        Checked::new(Condition::and(terms), fields.schema()).map(Some).map_err(|error| refusal(error, table))
    };
    Ok(Conditions {
        filter: check(scope.iter().chain(&clauses).cloned().collect())?,
        scope: check(scope)?,
        clauses: clauses
            .into_iter()
            .map(|term| {
                let only = only(&term, fields);
                Ok(Clause { condition: check(vec![term])?.expect("one term"), only })
            })
            .collect::<Result<_, Failure>>()?,
    })
}

/// The one column `condition` mentions; `None` when it mentions none, several, or a form this
/// walk does not know.
fn only(condition: &Condition, fields: &Filterable) -> Option<String> {
    fn walk<'a>(condition: &'a Condition, columns: &mut Vec<&'a Path>) -> bool {
        match condition {
            Condition::And(terms) | Condition::Or(terms) => terms.iter().all(|term| walk(term, columns)),
            Condition::Not(term) => walk(term, columns),
            Condition::Has(path) => {
                columns.push(path);
                true
            }
            Condition::Compare { field, .. }
            | Condition::In { field, .. }
            | Condition::Member { field, .. }
            | Condition::Under { field, .. }
            | Condition::Field(field) => {
                columns.push(&field.path);
                true
            }
            Condition::Bool(_) => true,
            // `Condition` is `#[non_exhaustive]`: a form rigging adds mentions what this walk
            // cannot see, so the clause always applies.
            _ => false,
        }
    }
    let mut paths = Vec::new();
    if !walk(condition, &mut paths) {
        return None;
    }
    let mut columns = paths.into_iter().map(|path| fields.column(path));
    let first = columns.next()??;
    columns.all(|column| column == Some(first)).then(|| first.to_owned())
}

/// The field a `values` parameter names, if the table can filter it.
pub(crate) fn distinct_field(name: &str, table: &Table, fields: &Filterable) -> Result<String, Failure> {
    if fields.path(name).is_some() {
        return Ok(name.to_owned());
    }
    if table.key.iter().chain(&table.columns).any(|column| column.name == name) {
        Err(bad("unsupported_filter", format!("{name} cannot be filtered")))
    } else {
        Err(bad("unknown_column", name))
    }
}

fn refusal(error: CheckError, table: &Table) -> Failure {
    match &error {
        CheckError::UnknownField(path) => {
            let name = path.segments().join(".");
            if table.key.iter().chain(&table.columns).any(|column| column.name == name) {
                bad("unsupported_filter", format!("{name} cannot be filtered"))
            } else {
                bad("unknown_column", error)
            }
        }
        CheckError::TypeMismatch { .. } | CheckError::OrderingNotAllowed { .. } | CheckError::InvalidLiteral { .. } => {
            bad("type_mismatch", error)
        }
        CheckError::UnknownVariable(_) => bad("invalid_filter", format!("{error}; a string literal is quoted")),
        // `CheckError` is `#[non_exhaustive]`: a variant rigging adds lands here until it is
        // mapped above. Limits and shape errors, and an empty `in` list, are malformed filters.
        _ => bad("invalid_filter", error),
    }
}

/// One row of a resource or derived source, read through the table's field paths.
struct Cells<'a> {
    row: &'a Row,
    fields: &'a Filterable,
}

impl rigging_condition::Row for Cells<'_> {
    fn get(&self, path: &Path) -> Lookup<'_> {
        let (Some(column), Some(field)) = (self.fields.column(path), self.fields.schema().field(path)) else { return Lookup::Absent };
        match self.row.get(column).and_then(|value| cell(&field.ty, value)) {
            Some(cell) => Lookup::Present(Cow::Owned(cell)),
            None => Lookup::Absent,
        }
    }
}

/// A JSON cell as a value of `ty`; `None` for null, which is absent.
fn cell(ty: &Type, value: &Value) -> Option<Cell> {
    Some(match (ty, value) {
        (Type::Bool, Value::Bool(value)) => Cell::Bool(*value),
        (Type::Int, Value::Number(value)) => Cell::Int(value.as_i64()?),
        (Type::Number, Value::Number(value)) => Cell::Number(value.as_f64()?),
        (Type::String | Type::Enum(_) | Type::Address, Value::String(value)) => Cell::String(value.clone()),
        (Type::Timestamp, Value::String(value)) => {
            let micros = chrono::DateTime::parse_from_rfc3339(value).ok()?.timestamp_micros();
            Cell::Timestamp(rigging_condition::Timestamp::from_unix_micros(micros)?)
        }
        (Type::Map(_), Value::Object(entries)) => Cell::Map(
            entries.iter().filter_map(|(key, value)| value.as_str().map(|value| (key.clone(), Cell::String(value.into())))).collect(),
        ),
        _ => return None,
    })
}

/// Whether `filter` keeps `row`: only a true condition does (§4.5).
pub(crate) fn keeps(filter: &Checked<Condition>, fields: &Filterable, row: &Row) -> bool {
    filter.evaluate(&Cells { row, fields }).is_true()
}

/// The equalities among `filter`'s top-level conjuncts that the resource store indexes: a string
/// on a queryable field, or a label value. The store reads a superset of the matching rows, and
/// [`keeps`] decides each one.
pub(crate) fn pushdown(filter: Option<&Checked<Condition>>, fields: &Filterable, queryable: &[&str]) -> StoreQuery {
    let mut query = StoreQuery::default();
    let conjuncts = match filter.map(|filter| &**filter) {
        Some(Condition::And(terms)) => terms.as_slice(),
        Some(term) => std::slice::from_ref(term),
        None => &[],
    };
    for term in conjuncts {
        let Condition::Compare { field: Field { path, key }, op: Relop::Eq, operand: Operand::Literal(Literal::String(literal)) } = term
        else {
            continue;
        };
        let target = match (fields.column(path), key) {
            (Some("metadata.labels"), Some(label)) => (&mut query.labels, label.as_str()),
            (Some(column), None) if queryable.contains(&column) => (&mut query.fields, column),
            _ => continue,
        };
        if target.0.get(target.1).is_some_and(|value| value != literal) {
            query.field_values.insert(KEY.into(), vec![]);
        }
        target.0.insert(target.1.into(), literal.clone());
    }
    query
}
