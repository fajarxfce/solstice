//! A deliberately dumb, non-incremental evaluator — the oracle.
//!
//! Nothing here is on the hot path, and nothing here should ever be made
//! clever. Its entire value is being so obviously correct that a disagreement
//! with the incremental path is always the incremental path's fault.
//!
//! This is oracle (a) of the three the plan calls for (§6): a reference
//! evaluator over `Vec<Row>`, SQLite itself via `IR → SQL`, and the incremental
//! path. Oracle (b) arrives with `dq-store`.

use crate::delta::{Batch, Change};
use crate::operator::{Dir, OpCx, Operator, ScanRequest};
use crate::ops::{Filter, Project};
use crate::predicate::{Params, Predicate};
use crate::value::{ColId, Row, RowKey, Value};
use std::collections::BTreeMap;

/// A keyed set of rows.
///
/// `BTreeMap` rather than `HashMap`: iteration order must be deterministic,
/// which `dq-ivm` requires structurally (plan §6).
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
    /// incremental path rather than merely similar.
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

/// One step of a reference plan, mirroring one operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stage {
    Filter { pred: Predicate, params: Params },
    Project { cols: Vec<ColId> },
}

impl Stage {
    /// The incremental operator this stage corresponds to.
    ///
    /// Having both sides built from one description is what makes the delta law
    /// testable: there is no chance of the oracle and the pipeline being handed
    /// subtly different queries.
    pub fn build(&self) -> Box<dyn Operator> {
        match self {
            Stage::Filter { pred, params } => Box::new(Filter::new(pred.clone(), params.clone())),
            Stage::Project { cols } => Box::new(Project::new(cols.clone())),
        }
    }
}

/// A reference plan: stages applied in order, wholesale, over a whole relation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reference {
    stages: Vec<Stage>,
}

impl Reference {
    pub fn new(stages: Vec<Stage>) -> Self {
        Reference { stages }
    }

    pub fn stages(&self) -> &[Stage] {
        &self.stages
    }

    /// Evaluate from scratch. O(relation) by construction, which is the point.
    pub fn eval(&self, input: &Relation) -> Relation {
        let mut current = input.clone();
        for stage in &self.stages {
            current = match stage {
                Stage::Filter { pred, params } => current
                    .iter()
                    .filter(|(_, row)| pred.matches(row, params))
                    .map(|(k, r)| (k.clone(), r.clone()))
                    .collect(),
                Stage::Project { cols } => current
                    .iter()
                    .map(|(k, r)| (k.clone(), r.project(cols)))
                    .collect(),
            };
        }
        current
    }
}

/// An in-memory [`OpCx`] backing `Source` hydration in tests.
///
/// Stands in for `dq-store` so that `dq-ivm` can be exercised end to end
/// without SQLite — which is the whole reason the store is behind a trait.
pub struct MemStore {
    tables: BTreeMap<u16, Relation>,
    pub refills: usize,
    pub rows_refilled: usize,
}

impl MemStore {
    pub fn new() -> Self {
        MemStore {
            tables: BTreeMap::new(),
            refills: 0,
            rows_refilled: 0,
        }
    }

    pub fn table(&mut self, table: u16) -> &mut Relation {
        self.tables.entry(table).or_default()
    }

    pub fn load(&mut self, table: u16, rel: Relation) {
        self.tables.insert(table, rel);
    }
}

impl Default for MemStore {
    fn default() -> Self {
        MemStore::new()
    }
}

impl OpCx for MemStore {
    fn scan(&mut self, req: &ScanRequest) -> Vec<(RowKey, Row)> {
        let Some(rel) = self.tables.get(&req.table) else {
            return Vec::new();
        };
        let params = Params::empty();

        let mut rows: Vec<(RowKey, Row)> = rel
            .iter()
            .filter(|(_, row)| match &req.filter {
                Some(p) => p.matches(row, &params),
                None => true,
            })
            .map(|(k, r)| (k.clone(), r.clone()))
            .collect();

        rows.sort_by(|(ak, ar), (bk, br)| cmp_by_order(ar, br, &req.order).then(ak.cmp(bk)));

        if let Some(after) = &req.after {
            // Exclusive lower bound: skip everything up to and including the
            // caller's last key. Linear here because this is the oracle; the
            // real store seeks the index.
            let probe = Row::new(after.clone());
            let probe_cols: Vec<(ColId, Dir)> = req
                .order
                .iter()
                .enumerate()
                .map(|(i, (_, dir))| (i as ColId, *dir))
                .collect();
            rows.retain(|(_, r)| {
                let key = project_order_key(r, &req.order);
                cmp_by_order(&key, &probe, &probe_cols).is_gt()
            });
        }

        rows.truncate(req.limit);
        rows
    }

    fn note_refill(&mut self, rows: usize) {
        self.refills += 1;
        self.rows_refilled += rows;
    }
}

fn project_order_key(row: &Row, order: &[(ColId, Dir)]) -> Row {
    Row::new(
        order
            .iter()
            .map(|(c, _)| row.get(*c).clone())
            .collect::<Vec<Value>>(),
    )
}

/// Compare two rows under an order spec, using SQL comparison so that results
/// match the SQLite oracle. NULLs sort first, as SQLite does for `ASC`.
///
/// Ties are possible (`Int(1)` vs `Real(1.0)`), which is why every caller must
/// break them with the primary key — the same reason a cursor carries the full
/// sort key *and* the pk (plan §1.1).
pub fn cmp_by_order(a: &Row, b: &Row, order: &[(ColId, Dir)]) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    for (col, dir) in order {
        let (x, y) = (a.get(*col), b.get(*col));
        let ord = match (x.is_null(), y.is_null()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            (false, false) => x.sql_cmp(y).unwrap_or(Ordering::Equal),
        };
        let ord = match dir {
            Dir::Asc => ord,
            Dir::Desc => ord.reverse(),
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::Source;

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
        let d = a.diff(&b);
        assert_eq!(a.with_applied(&d), b);
    }

    #[test]
    fn diff_of_identical_relations_is_empty() {
        let a = rel(&[(1, 10)]);
        assert!(a.diff(&a).is_empty());
    }

    #[test]
    fn hydration_produces_the_full_relation_in_key_order() {
        let mut store = MemStore::new();
        store.load(7, rel(&[(3, 30), (1, 10), (2, 20)]));

        let mut source = Source::new(7, 0);
        let batch = source.hydrate(&mut store);

        let keys: Vec<_> = batch.iter().map(|c| c.key().clone()).collect();
        assert_eq!(
            keys,
            vec![RowKey::from(1), RowKey::from(2), RowKey::from(3)]
        );
        assert_eq!(
            Relation::new().with_applied(&batch),
            rel(&[(3, 30), (1, 10), (2, 20)])
        );
    }

    #[test]
    fn hydration_applies_pushdown_and_limit() {
        let mut store = MemStore::new();
        store.load(7, rel(&[(1, 10), (2, 20), (3, 30), (4, 40)]));

        let mut source = Source::new(7, 0)
            .with_pushdown(Predicate::Cmp {
                lhs: crate::predicate::Expr::Col(1),
                op: crate::predicate::CmpOp::Ge,
                rhs: crate::predicate::Expr::Lit(Value::Int(20)),
            })
            .with_limit(2);

        let batch = source.hydrate(&mut store);
        assert_eq!(
            Relation::new().with_applied(&batch),
            rel(&[(2, 20), (3, 30)])
        );
    }

    #[test]
    fn scan_after_is_an_exclusive_lower_bound() {
        let mut store = MemStore::new();
        store.load(7, rel(&[(1, 10), (2, 20), (3, 30)]));

        let rows = store.scan(&ScanRequest {
            table: 7,
            order: vec![(0, Dir::Asc)],
            after: Some(vec![Value::Int(1)]),
            filter: None,
            limit: 10,
        });
        let keys: Vec<_> = rows.iter().map(|(k, _)| k.clone()).collect();
        assert_eq!(keys, vec![RowKey::from(2), RowKey::from(3)]);
    }

    #[test]
    fn descending_order_reverses_the_scan() {
        let mut store = MemStore::new();
        store.load(7, rel(&[(1, 10), (2, 20), (3, 30)]));

        let rows = store.scan(&ScanRequest {
            table: 7,
            order: vec![(1, Dir::Desc)],
            after: None,
            filter: None,
            limit: 2,
        });
        let keys: Vec<_> = rows.iter().map(|(k, _)| k.clone()).collect();
        assert_eq!(keys, vec![RowKey::from(3), RowKey::from(2)]);
    }

    #[test]
    fn nulls_sort_first_ascending() {
        let store_rel: Relation = [
            (
                RowKey::from(1),
                Row::new(vec![Value::Int(1), Value::Int(5)]),
            ),
            (RowKey::from(2), Row::new(vec![Value::Int(2), Value::Null])),
        ]
        .into_iter()
        .collect();
        let mut store = MemStore::new();
        store.load(7, store_rel);

        let rows = store.scan(&ScanRequest {
            table: 7,
            order: vec![(1, Dir::Asc)],
            after: None,
            filter: None,
            limit: 10,
        });
        assert_eq!(rows[0].0, RowKey::from(2));
    }
}
