//! Index pages and the startup scan: the only listings, each keyset-paged and limited.

use flotilla_schema::{IndexQuery, Kind, KindCode, Value};
use rusqlite::{params, params_from_iter, types::Value as SqlValue};

use crate::{
    error::StoreError,
    store::{Ctx, ResourceId},
};

/// The most rows (resource ids or log entries) one listing returns; a larger limit is
/// lowered to it, so one call's work and memory stay bounded whatever the caller asks for.
pub const MAX_PAGE: u32 = 10_000;

/// A position in one index query's results: after the row with these order values and id.
///
/// Only the store makes cursors, and a cursor names its kind, index and `eq` arguments: passing
/// it to any other query is refused as `CursorMismatch`, so a cursor cannot move one query to a
/// position in another. Order values are stored values (local string and identity ids), so a
/// cursor is meaningful only in the store that made it.
#[derive(Debug, Clone, PartialEq)]
pub struct Cursor {
    kind: KindCode,
    index: &'static str,
    eq: Vec<Value>,
    order: Vec<SqlValue>,
    id: ResourceId,
}

/// One page of resource ids. `next` is present only when more rows follow: pass it back as
/// `after` for the next page. Index pages continue with a [`Cursor`], the startup scan with the
/// last id.
#[derive(Debug, Clone, PartialEq)]
pub struct Page<C = Cursor> {
    pub ids: Vec<ResourceId>,
    pub next: Option<C>,
}

impl<C> Page<C> {
    fn empty() -> Page<C> {
        Page { ids: Vec::new(), next: None }
    }
}

/// The one limit rule of every listing (`page`, `scan`, `changes`, `changes_since`, `about`): 0 is
/// refused, and a limit above [`MAX_PAGE`] is lowered to it.
pub(crate) fn page_limit(limit: u32) -> Result<i64, StoreError> {
    if limit == 0 {
        return Err(StoreError::ZeroLimit);
    }
    Ok(i64::from(limit.min(MAX_PAGE)))
}

impl Ctx<'_> {
    /// An `eq` argument as stored, without creating anything: `None` when an interned string or
    /// identity has no local id, which no row can then hold.
    fn lookup(&mut self, value: &Value) -> Result<Option<SqlValue>, StoreError> {
        Ok(match value {
            Value::Null => None,
            Value::Int(int) => Some(SqlValue::Integer(*int)),
            Value::Text(text) => Some(SqlValue::Text(text.clone())),
            Value::Blob(bytes) => Some(SqlValue::Blob(bytes.clone())),
            Value::Str(string) => self.string_id(string)?.map(SqlValue::Integer),
            Value::Ref(identity) => self.resolve(identity)?.map(|id| SqlValue::Integer(id.0)),
            Value::Object(hash) => Some(SqlValue::Blob(hash.0.to_vec())),
        })
    }

    pub(crate) fn page<K: Kind>(&mut self, query: &IndexQuery<K>, after: Option<&Cursor>, limit: u32) -> Result<Page, StoreError> {
        let limit = page_limit(limit)?;
        let eq = query.eq()?;
        // Only the order width is taken here: argument lookups need `self` mutably, and the
        // statement text is borrowed from the per-kind SQL just before it runs.
        let order = self
            .sql(K::CODE)?
            .indexes
            .get(query.index())
            .ok_or_else(|| StoreError::UnknownIndex { kind: K::CODE, index: query.index().to_owned() })?
            .order;
        if let Some(cursor) = after {
            if cursor.kind != K::CODE || cursor.index != query.index() || cursor.eq != eq {
                return Err(StoreError::CursorMismatch { index: query.index().to_owned() });
            }
        }
        let mut params = Vec::with_capacity(eq.len() + order + 2);
        for value in eq {
            match self.lookup(value)? {
                Some(param) => params.push(param),
                None => return Ok(Page::empty()),
            }
        }
        if let Some(cursor) = after {
            params.extend(cursor.order.iter().cloned());
            params.push(SqlValue::Integer(cursor.id.0));
        }
        // One row beyond the limit tells whether another page follows.
        params.push(SqlValue::Integer(limit + 1));
        let index = &self.sql(K::CODE)?.indexes[query.index()];
        let statement = if after.is_some() { &index.after } else { &index.first };
        let mut rows: Vec<(ResourceId, Vec<SqlValue>)> = self
            .conn
            .prepare_cached(statement)?
            .query_map(params_from_iter(params), |row| {
                let id = ResourceId(row.get(0)?);
                let values = (1..=order).map(|n| row.get::<_, SqlValue>(n)).collect::<Result<_, _>>()?;
                Ok((id, values))
            })?
            .collect::<Result<_, _>>()?;
        let more = rows.len() as i64 > limit;
        rows.truncate(limit as usize);
        let next = match rows.last() {
            Some((id, values)) if more => {
                Some(Cursor { kind: K::CODE, index: query.index(), eq: eq.to_vec(), order: values.clone(), id: *id })
            }
            _ => None,
        };
        Ok(Page { ids: rows.into_iter().map(|(id, _)| id).collect(), next })
    }

    pub(crate) fn scan<K: Kind>(&mut self, home: &str, after: Option<ResourceId>, limit: u32) -> Result<Page<ResourceId>, StoreError> {
        let limit = page_limit(limit)?;
        self.sql(K::CODE)?;
        let Some(home) = self.string_id(home)? else { return Ok(Page::empty()) };
        let after = after.map_or(i64::MIN, |id| id.0);
        let mut ids: Vec<ResourceId> = self
            .conn
            .prepare_cached(&self.sql(K::CODE)?.scan)?
            .query_map(params![home, K::CODE.0, after, limit + 1], |row| row.get(0).map(ResourceId))?
            .collect::<Result<_, _>>()?;
        let more = ids.len() as i64 > limit;
        ids.truncate(limit as usize);
        let next = if more { ids.last().copied() } else { None };
        Ok(Page { ids, next })
    }
}
