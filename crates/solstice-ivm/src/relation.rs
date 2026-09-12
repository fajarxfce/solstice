//! A keyed set of rows, and the diff between two of them.

use crate::delta::{Batch, Change};
use crate::value::{Row, RowKey};
use std::collections::BTreeMap;

/// A keyed set of rows.
///
/// `BTreeMap` rather than `HashMap`: iteration order must be deterministic,
/// which this crate requires structurally (plan §6) and CI enforces.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Relation {
    rows: BTreeMap<RowKey, Row>,
}

impl Relation {
    pub fn new() -> Self {
        Relation::default()
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn get(&self, key: &RowKey) -> Option<&Row> {
        self.rows.get(key)
    }

    pub fn insert(&mut self, key: RowKey, row: Row) {
        self.rows.insert(key, row);
    }

    pub fn remove(&mut self, key: &RowKey) -> Option<Row> {
        self.rows.remove(key)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&RowKey, &Row)> {
        self.rows.iter()
    }

    /// Apply a batch. Written to tolerate ill-formed batches — an insert over
    /// an existing key, a delete of an absent key — because the property tests
    /// deliberately generate adversarial input and a panic here would be an
    /// oracle failure rather than a finding.
    pub fn apply(&mut self, batch: &Batch) {
        for change in batch.iter() {
            match change.after() {
                Some(row) => {
                    self.rows.insert(change.key().clone(), row.clone());
                }
                None => {
                    self.rows.remove(change.key());
                }
            }
        }
    }

    pub fn with_applied(&self, batch: &Batch) -> Relation {
        let mut out = self.clone();
        out.apply(batch);
        out
    }

    /// The batch that turns `self` into `other`.
    ///
    /// This is what a `Requery` node emits after re-executing its subtree
    /// (plan §1.5), and what makes degradation semantically identical to the
    /// incremental path rather than merely similar. `TopK` uses it too, to
    /// turn "here is the window before and after" into a delta without
    /// case-analysing every way a row can enter or leave.
    pub fn diff(&self, other: &Relation) -> Batch {
        let mut batch = Batch::new();
        for (key, before) in &self.rows {
            if let Some(change) = Change::from_images(
                key.clone(),
                Some(before.clone()),
                other.rows.get(key).cloned(),
            ) {
                batch.push(change);
            }
        }
        for (key, after) in &other.rows {
            if !self.rows.contains_key(key) {
                batch.push(Change::Insert {
                    key: key.clone(),
                    row: after.clone(),
                });
            }
        }
        batch
    }
}

impl FromIterator<(RowKey, Row)> for Relation {
    fn from_iter<T: IntoIterator<Item = (RowKey, Row)>>(iter: T) -> Self {
        Relation {
            rows: iter.into_iter().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Value;

    fn rel(rows: &[(i64, i64)]) -> Relation {
        rows.iter()
            .map(|(id, v)| {
                (
                    RowKey::from(*id),
                    Row::new(vec![Value::Int(*id), Value::Int(*v)]),
                )
            })
            .collect()
    }

    #[test]
    fn diff_then_apply_round_trips() {
        let a = rel(&[(1, 10), (2, 20), (3, 30)]);
        let b = rel(&[(2, 25), (3, 30), (4, 40)]);
        assert_eq!(a.with_applied(&a.diff(&b)), b);
    }

    #[test]
    fn diff_of_identical_relations_is_empty() {
        let a = rel(&[(1, 10)]);
        assert!(a.diff(&a).is_empty());
    }
}
