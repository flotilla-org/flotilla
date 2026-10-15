//! Append-only record lists: the value type of `#[store(records)]` and `#[store(history)]` fields.

use std::{ops::Index, slice};

use serde::{Deserialize, Serialize};

/// An append-only list. A row's position is its index: dense, assigned by [`Records::push`] and
/// never reused, so a stored row keeps its key for the life of its owner. There is no remove,
/// insert or `get_mut`; a `records` row may be replaced in place through [`Records::replace`], which
/// the store refuses for `history` fields when the owner is written.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Records<T> {
    rows: Vec<T>,
}

impl<T> Records<T> {
    pub fn new() -> Records<T> {
        Records { rows: Vec::new() }
    }

    /// Appends a row and returns its position.
    pub fn push(&mut self, row: T) -> usize {
        self.rows.push(row);
        self.rows.len() - 1
    }

    /// Replaces the row at `pos`, returning the previous one.
    ///
    /// # Panics
    ///
    /// When `pos` is not a position of this list, like slice indexing.
    pub fn replace(&mut self, pos: usize, row: T) -> T {
        std::mem::replace(&mut self.rows[pos], row)
    }

    pub fn get(&self, pos: usize) -> Option<&T> {
        self.rows.get(pos)
    }

    pub fn last(&self) -> Option<&T> {
        self.rows.last()
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn iter(&self) -> slice::Iter<'_, T> {
        self.rows.iter()
    }

    pub fn as_slice(&self) -> &[T] {
        &self.rows
    }
}

impl<T> Default for Records<T> {
    fn default() -> Records<T> {
        Records::new()
    }
}

impl<T> From<Vec<T>> for Records<T> {
    /// Positions follow the vector's order.
    fn from(rows: Vec<T>) -> Records<T> {
        Records { rows }
    }
}

impl<T> FromIterator<T> for Records<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Records<T> {
        Records { rows: iter.into_iter().collect() }
    }
}

impl<T> Index<usize> for Records<T> {
    type Output = T;

    fn index(&self, pos: usize) -> &T {
        &self.rows[pos]
    }
}

impl<'a, T> IntoIterator for &'a Records<T> {
    type Item = &'a T;
    type IntoIter = slice::Iter<'a, T>;

    fn into_iter(self) -> slice::Iter<'a, T> {
        self.rows.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::Records;

    // Behaviour (DESIGN Records): positions are dense indexes assigned by push; replace keeps the
    // position; serde sees a plain list.
    #[test]
    fn positions_are_dense_and_serde_is_transparent() {
        let mut records = Records::new();
        assert_eq!(records.push("a"), 0);
        assert_eq!(records.push("b"), 1);
        assert_eq!(records.replace(0, "A"), "a");
        assert_eq!((records.len(), records.get(0), records.last()), (2, Some(&"A"), Some(&"b")));
        assert_eq!(serde_json::to_string(&records).expect("json"), r#"["A","b"]"#);
        let back: Records<String> = serde_json::from_str(r#"["x"]"#).expect("json");
        assert_eq!(back.as_slice(), ["x".to_owned()]);
    }
}
