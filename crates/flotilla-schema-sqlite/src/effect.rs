//! Effects and rows: building a write's named, field-level [`Effect`] from the rows it changes,
//! and lowering an effect to the row operations the kind's statements run (#3392 G8).
//!
//! Lowering happens here alone. A local write builds its effect, encodes it, decodes the bytes
//! and lowers what it decoded; `apply` lowers another home's effect the same way. So the rows a
//! store writes are always the rows its log entry describes, and a replica that applies the entry
//! writes the same rows. Names resolve through the descriptor's aliases (`#[store(alias)]`), so an
//! effect written under a field's earlier name still lowers.

use flotilla_schema::{Change, ColumnDef, Effect, Named, Row, Step, TableRole, Value};

use crate::{
    error::{Malformed, StoreError},
    sql::{KindSql, TableSql},
};

/// One row operation, in a kind's own table and field numbering. Keys exclude the owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Op {
    /// The root row of a created resource: its data values.
    Root {
        values: Vec<Value>,
    },
    /// An appended `records` or `history` row, its stamp first.
    Insert {
        table: usize,
        key: Vec<Value>,
        values: Vec<Value>,
    },
    /// A collection row inserted or replaced by its key.
    Put {
        table: usize,
        key: Vec<Value>,
        values: Vec<Value>,
    },
    /// One field of an existing row.
    Set {
        table: usize,
        key: Vec<Value>,
        field: usize,
        values: Vec<Value>,
    },
    /// A label-plus-data state of an existing row, every member of its group.
    State {
        table: usize,
        key: Vec<Value>,
        group: usize,
        members: Vec<Vec<Value>>,
    },
    Delete {
        table: usize,
        key: Vec<Value>,
    },
    /// The root row, and with it every child row.
    DeleteResource,
}

impl Op {
    /// Whether the op writes the root row, so the root's version moves with it.
    pub(crate) fn names_root(&self) -> bool {
        match self {
            Op::Root { .. } | Op::DeleteResource => true,
            Op::Set { table, .. } | Op::State { table, .. } => *table == 0,
            Op::Insert { .. } | Op::Put { .. } | Op::Delete { .. } => false,
        }
    }

    /// Every value the op writes, keys included.
    pub(crate) fn values(&self) -> Box<dyn Iterator<Item = &Value> + '_> {
        match self {
            Op::Root { values } => Box::new(values.iter()),
            Op::Insert { key, values, .. } | Op::Put { key, values, .. } | Op::Set { key, values, .. } => {
                Box::new(key.iter().chain(values))
            }
            Op::State { key, members, .. } => Box::new(key.iter().chain(members.iter().flatten())),
            Op::Delete { key, .. } => Box::new(key.iter()),
            Op::DeleteResource => Box::new(std::iter::empty()),
        }
    }
}

/// `values` named by `columns`, null columns omitted.
pub(crate) fn named(columns: &[ColumnDef], values: &[Value]) -> Named {
    columns
        .iter()
        .zip(values)
        .filter(|(_, value)| **value != Value::Null)
        .map(|(column, value)| (column.name.clone(), value.clone()))
        .collect()
}

impl TableSql {
    /// The data columns: every column after the key.
    pub(crate) fn data(&self) -> &[ColumnDef] {
        &self.columns[self.key_len..]
    }

    /// The table's own key columns.
    pub(crate) fn own_key_columns(&self) -> &[ColumnDef] {
        &self.columns[self.key_len - self.own_key..self.key_len]
    }

    /// The row's data values with a `records` or `history` row's stamp left out: the entry's
    /// sequence is its stamp.
    fn stamped(&self) -> bool {
        matches!(self.role, TableRole::Records | TableRole::History)
    }

    /// The current name of a column an entry names: its own name, or an earlier one.
    fn current<'n>(&'n self, name: &'n str) -> &'n str {
        self.aliases.get(name).map_or(name, String::as_str)
    }

    fn field(&self, name: &str) -> Result<usize, StoreError> {
        self.fields
            .iter()
            .position(|field| field.name == name || field.aliases.iter().any(|alias| alias == name))
            .ok_or_else(|| Malformed::UnknownField { table: self.name.clone(), field: name.to_owned() }.into())
    }

    /// Values for the data columns in `range` of the row, from `named`: every name must be one of
    /// those columns, and a column not named is null.
    fn values(&self, range: std::ops::Range<usize>, named: &Named) -> Result<Vec<Value>, StoreError> {
        let mut values = vec![Value::Null; range.len()];
        for (name, value) in named {
            let current = self.current(name);
            let at = self.data()[range.clone()]
                .iter()
                .position(|column| column.name == current)
                .ok_or_else(|| Malformed::UnknownColumn { table: self.name.clone(), column: name.clone() })?;
            values[at] = value.clone();
        }
        Ok(values)
    }

    /// The table's own key from `named`: every own key column named, and nothing else.
    fn own_key(&self, named: &Named) -> Result<Vec<Value>, StoreError> {
        let columns = self.own_key_columns();
        let mut key = vec![None; columns.len()];
        for (name, value) in named {
            let current = self.current(name);
            let at = columns
                .iter()
                .position(|column| column.name == current)
                .ok_or_else(|| Malformed::UnknownColumn { table: self.name.clone(), column: name.clone() })?;
            key[at] = Some(value.clone());
        }
        key.into_iter().collect::<Option<Vec<_>>>().ok_or_else(|| Malformed::Shape { table: self.name.clone() }.into())
    }
}

impl KindSql {
    /// The table storing `field` of `parent`'s rows, in one of `roles`, by its name or an earlier one.
    pub(crate) fn child(&self, parent: usize, field: &str, roles: &[TableRole]) -> Result<usize, StoreError> {
        self.tables
            .iter()
            .position(|table| {
                table.parent == Some(parent)
                    && roles.contains(&table.role)
                    && (table.field == field || table.field_aliases.iter().any(|alias| alias == field))
            })
            .ok_or_else(|| Malformed::UnknownTable { table: format!("{}.{field}", self.tables[parent].name) }.into())
    }

    /// The table and full key of the row `path` addresses.
    fn row(&self, path: &[Step]) -> Result<(usize, Vec<Value>), StoreError> {
        let (mut table, mut key) = (0, Vec::new());
        for step in path {
            table = self.child(table, &step.field, &[TableRole::Child, TableRole::Records, TableRole::History, TableRole::State])?;
            key.extend(self.tables[table].own_key(&step.key)?);
        }
        Ok((table, key))
    }

    /// The path to the parent of row `key` of `table`, and the row's own key, both named.
    pub(crate) fn path(&self, table: usize, key: &[Value]) -> (Vec<Step>, Named) {
        let mut chain = vec![table];
        while let Some(parent) = self.tables[*chain.last().expect("non-empty")].parent {
            chain.push(parent);
        }
        chain.pop();
        chain.reverse();
        let (mut steps, mut offset) = (Vec::new(), 0);
        for &link in &chain {
            let link = &self.tables[link];
            let own = &key[offset..offset + link.own_key];
            offset += link.own_key;
            steps.push(Step { field: link.field.clone(), key: named(link.own_key_columns(), own) });
        }
        let own = steps.pop().map(|step| step.key).unwrap_or_default();
        (steps, own)
    }

    /// The path to row `key` of `table` itself: empty for the root.
    pub(crate) fn row_path(&self, table: usize, key: &[Value]) -> Vec<Step> {
        if table == 0 {
            return Vec::new();
        }
        let (mut path, own) = self.path(table, key);
        path.push(Step { field: self.tables[table].field.clone(), key: own });
        path
    }

    /// The root row's fields, each as a `Set`: how a create names its root.
    pub(crate) fn root_sets(&self, values: &[Value]) -> Vec<Change> {
        let root = &self.tables[0];
        root.fields
            .iter()
            .map(|field| Change::Set {
                row: Vec::new(),
                field: field.name.clone(),
                values: named(&root.data()[field.columns.clone()], &values[field.columns.clone()]),
            })
            .collect()
    }

    /// A row of a nested `table`, added: appended to a `records` or `history` table, put into a
    /// collection.
    pub(crate) fn added(&self, table: usize, row: &Row) -> Result<Change, StoreError> {
        let sql = &self.tables[table];
        let (path, key) = self.path(table, &row.key);
        if sql.stamped() {
            let position = match row.key.last() {
                Some(Value::Int(position)) => u32::try_from(*position).map_err(|_| Malformed::Shape { table: sql.name.clone() })?,
                _ => return Err(Malformed::Shape { table: sql.name.clone() }.into()),
            };
            let values = named(&sql.data()[1..], &row.values[1..]);
            return Ok(Change::Append { row: path, records: sql.field.clone(), position, values });
        }
        Ok(Change::Put { row: path, collection: sql.field.clone(), key, values: named(sql.data(), &row.values) })
    }

    /// A collection row of `table`, removed by its key.
    pub(crate) fn removed(&self, table: usize, key: &[Value]) -> Change {
        let (row, key) = self.path(table, key);
        Change::Remove { row, collection: self.tables[table].field.clone(), key }
    }

    /// The row operations `effect` writes, as a write at `sequence`. A create's root fields form
    /// its root row, which comes first; a delete is the root row's removal.
    pub(crate) fn lower(&self, effect: &Effect, sequence: i64, create: bool, delete: bool) -> Result<Vec<Op>, StoreError> {
        if delete {
            if !effect.changes.is_empty() {
                return Err(Malformed::Shape { table: self.tables[0].name.clone() }.into());
            }
            return Ok(vec![Op::DeleteResource]);
        }
        let mut root = create.then(|| vec![Value::Null; self.tables[0].data().len()]);
        let mut ops = Vec::with_capacity(effect.changes.len() + 1);
        for change in &effect.changes {
            match change {
                Change::Set { row, field, values } => {
                    let (table, key) = self.row(row)?;
                    let sql = &self.tables[table];
                    let field = sql.field(field)?;
                    let columns = sql.fields[field].columns.clone();
                    let values = sql.values(columns.clone(), values)?;
                    match (&mut root, table) {
                        (Some(root), 0) => drop(root.splice(columns, values)),
                        _ => ops.push(Op::Set { table, key, field, values }),
                    }
                }
                Change::State { row, field, values } => {
                    let (table, key) = self.row(row)?;
                    let sql = &self.tables[table];
                    let label = sql.field(field)?;
                    if !sql.fields[label].label {
                        return Err(Malformed::UnknownField { table: sql.name.clone(), field: field.clone() }.into());
                    }
                    let members = match sql.fields[label].group {
                        Some(group) => sql.groups[group].fields.clone(),
                        None => vec![label],
                    };
                    let span = members.iter().map(|&member| sql.fields[member].columns.clone());
                    let (start, end) =
                        span.clone().fold((usize::MAX, 0), |(start, end), range| (start.min(range.start), end.max(range.end)));
                    // Every name must be a column of one of the state's fields; the columns of a
                    // state's fields are contiguous, label first.
                    let all = sql.values(start..end, values)?;
                    let member_values: Vec<Vec<Value>> = span.map(|range| all[range.start - start..range.end - start].to_vec()).collect();
                    match (&mut root, table, sql.fields[label].group) {
                        (Some(root), 0, _) => drop(root.splice(start..end, all)),
                        (_, _, Some(group)) => ops.push(Op::State { table, key, group, members: member_values }),
                        (_, _, None) => {
                            let values = member_values.into_iter().next().expect("one member");
                            ops.push(Op::Set { table, key, field: label, values });
                        }
                    }
                }
                Change::Put { row, collection, key, values } => {
                    let (parent, mut full) = self.row(row)?;
                    let table = self.child(parent, collection, &[TableRole::Child, TableRole::State])?;
                    let sql = &self.tables[table];
                    full.extend(sql.own_key(key)?);
                    let values = sql.values(0..sql.data().len(), values)?;
                    ops.push(Op::Put { table, key: full, values });
                }
                Change::Remove { row, collection, key } => {
                    let (parent, mut full) = self.row(row)?;
                    let table = self.child(parent, collection, &[TableRole::Child, TableRole::State])?;
                    full.extend(self.tables[table].own_key(key)?);
                    ops.push(Op::Delete { table, key: full });
                }
                Change::Append { row, records, position, values } => {
                    let (parent, mut full) = self.row(row)?;
                    let table = self.child(parent, records, &[TableRole::Records, TableRole::History])?;
                    let sql = &self.tables[table];
                    full.push(Value::Int(i64::from(*position)));
                    let mut row_values = vec![Value::Int(sequence)];
                    row_values.extend(sql.values(1..sql.data().len(), values)?);
                    ops.push(Op::Insert { table, key: full, values: row_values });
                }
            }
        }
        if let Some(values) = root {
            ops.insert(0, Op::Root { values });
        }
        Ok(ops)
    }
}
