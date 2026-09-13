//! The materialised view: keyed diffs in, positional diffs out.
//!
//! This is plan §1.3's `Output` operator. It sits outside the dataflow graph
//! rather than inside it because it is the only node whose output is not a
//! `Batch` — everything upstream speaks keyed changes, and the host speaks
//! indices.
//!
//! # Why the host gets indices at all
//!
//! Plan §1.5 makes it non-negotiable from day one: the public API sends
//! `Added/Removed/Changed/Moved` with positions, not snapshots. An API that
//! sends "here is the new list" can never be upgraded to row-level diffs
//! without a breaking change, and — the part that actually shows — list
//! animations and scroll stability are never right without them. `AnimatedList`
//! and `LazyColumn` both want to be told what moved.
//!
//! # The contract the indices are under
//!
//! **Each change's index is relative to the list after every earlier change in
//! the same batch has been applied.** That is the contract both host list
//! widgets already implement, and it is why this applies changes one at a time
//! to its own `Vec` rather than computing a diff between two snapshots: the
//! position is read off the structure at the moment the change happens.
//!
//! An update that moves emits two changes — `Moved{from,to}` then
//! `Changed{to,..}`. Collapsing them into one would either lose the move (no
//! animation) or lose the new values, and the host applying them in order gets
//! both.

use std::collections::BTreeMap;

use solstice_ivm::order::cmp_entry;
use solstice_ivm::{Batch, ColId, Dir, Row, RowKey};
use solstice_proto::ViewChange;

/// A view's rows, in sort order, with the diff machinery to keep them there.
pub struct View {
    order: Vec<(ColId, Dir)>,
    /// Sorted by [`cmp_entry`]: sort key, then primary key.
    rows: Vec<(RowKey, Row)>,
    /// The same rows, addressable by key.
    ///
    /// Redundant in content, not in cost: a [`Row`] is one `Arc` pointer and a
    /// [`RowKey`] wraps one `Value`, so this holds refcount bumps rather than
    /// rows. It exists because a positional structure cannot answer "where is
    /// key k" without either this or a linear scan, and the view's own state —
    /// not the upstream operator's `before` image — has to be the authority on
    /// what position a row is at.
    by_key: BTreeMap<RowKey, Row>,
}

impl View {
    pub fn new(order: Vec<(ColId, Dir)>) -> View {
        View {
            order,
            rows: Vec::new(),
            by_key: BTreeMap::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn rows(&self) -> &[(RowKey, Row)] {
        &self.rows
    }

    /// The whole view as inserts, for an initial read or a resync.
    pub fn snapshot(&self) -> Vec<ViewChange> {
        self.rows
            .iter()
            .enumerate()
            .map(|(i, (_, row))| ViewChange::Added {
                index: i as u32,
                row: row.clone(),
            })
            .collect()
    }

    /// Apply a keyed batch, returning the positional diff that describes it.
    pub fn apply(&mut self, batch: &Batch) -> Vec<ViewChange> {
        let mut out = Vec::with_capacity(batch.len());
        for change in batch.iter() {
            let key = change.key();
            let before = self.by_key.remove(key);
            let from = before.as_ref().map(|row| {
                let i = self.locate(key, row);
                self.rows.remove(i);
                i
            });

            match (from, change.after()) {
                (Some(i), None) => out.push(ViewChange::Removed {
                    index: i as u32,
                    key: key.value().clone(),
                }),
                // A delete of a row the view never held. The graph does not
                // emit these, but a `Batch` is allowed to be ill-formed and a
                // panic here would be the wrong way to find out.
                (None, None) => {}
                (None, Some(row)) => {
                    let to = self.place(key, row);
                    out.push(ViewChange::Added {
                        index: to as u32,
                        row: row.clone(),
                    });
                }
                (Some(i), Some(row)) => {
                    let to = self.place(key, row);
                    if i != to {
                        out.push(ViewChange::Moved {
                            from: i as u32,
                            to: to as u32,
                        });
                    }
                    let cols = changed_cols(before.as_ref().expect("from implies before"), row);
                    if !cols.is_empty() {
                        out.push(ViewChange::Changed {
                            index: to as u32,
                            row: row.clone(),
                            cols,
                        });
                    }
                }
            }
        }
        out
    }

    /// Bytes this view is holding, for the memory accounting plan §7 asks to be
    /// a first-class metric rather than a post-mortem.
    pub fn heap_bytes(&self) -> usize {
        let rows: usize = self
            .rows
            .iter()
            .map(|(k, r)| k.heap_bytes() + r.heap_bytes() + std::mem::size_of::<(RowKey, Row)>())
            .sum();
        // The index holds clones of the same `Arc`s, so only the nodes count.
        rows + self.by_key.len() * std::mem::size_of::<(RowKey, Row)>()
    }

    /// Index of a row the view is known to hold.
    fn locate(&self, key: &RowKey, row: &Row) -> usize {
        self.rows
            .binary_search_by(|e| cmp_entry((&e.0, &e.1), (key, row), &self.order))
            .expect("a row in the index is a row in the list")
    }

    /// Insert a row not currently in the list, returning where it landed.
    fn place(&mut self, key: &RowKey, row: &Row) -> usize {
        let to = self
            .rows
            .binary_search_by(|e| cmp_entry((&e.0, &e.1), (key, row), &self.order))
            .unwrap_or_else(|i| i);
        self.rows.insert(to, (key.clone(), row.clone()));
        self.by_key.insert(key.clone(), row.clone());
        to
    }
}

/// Columns whose value differs, so the host can repaint a cell instead of a row.
///
/// Rows of different arity compare as different from the shorter one's end
/// onward; the graph does not produce that, and reporting it beats indexing
/// past the end.
fn changed_cols(before: &Row, after: &Row) -> Vec<ColId> {
    let (a, b) = (before.values(), after.values());
    (0..a.len().max(b.len()))
        .filter(|&i| a.get(i) != b.get(i))
        .map(|i| i as ColId)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use solstice_ivm::{Change, Value};

    const PRIORITY: ColId = 1;

    fn order() -> Vec<(ColId, Dir)> {
        vec![(PRIORITY, Dir::Desc)]
    }

    fn row(id: i64, priority: i64, title: &str) -> (RowKey, Row) {
        (
            RowKey::from(id),
            Row::new(vec![
                Value::Int(id),
                Value::Int(priority),
                Value::text(title),
            ]),
        )
    }

    fn inserts(rows: &[(RowKey, Row)]) -> Batch {
        rows.iter()
            .map(|(k, r)| Change::Insert {
                key: k.clone(),
                row: r.clone(),
            })
            .collect()
    }

    fn one(change: Change) -> Batch {
        let mut b = Batch::new();
        b.push(change);
        b
    }

    fn seeded() -> View {
        let mut v = View::new(order());
        v.apply(&inserts(&[
            row(1, 10, "ten"),
            row(2, 30, "thirty"),
            row(3, 20, "twenty"),
        ]));
        v
    }

    fn ids(v: &View) -> Vec<i64> {
        v.rows()
            .iter()
            .map(|(_, r)| match r.get(0) {
                Value::Int(i) => *i,
                other => panic!("{other:?}"),
            })
            .collect()
    }

    #[test]
    fn rows_land_in_sort_order_whatever_order_they_arrived_in() {
        assert_eq!(ids(&seeded()), vec![2, 3, 1]);
    }

    #[test]
    fn an_insert_reports_the_index_it_landed_at() {
        let mut v = View::new(order());
        let changes = v.apply(&inserts(&[row(1, 10, "ten"), row(2, 30, "thirty")]));
        // The second row sorts *above* the first, so its index is 0 — which is
        // the whole reason the host is told an index rather than being left to
        // append.
        assert!(matches!(changes[0], ViewChange::Added { index: 0, .. }));
        assert!(matches!(changes[1], ViewChange::Added { index: 0, .. }));
    }

    #[test]
    fn a_change_off_the_sort_key_changes_and_does_not_move() {
        let mut v = seeded();
        let (key, before) = row(3, 20, "twenty");
        let (_, after) = row(3, 20, "twenty-one");
        let changes = v.apply(&one(Change::Update { key, before, after }));

        assert_eq!(changes.len(), 1, "a move would be a spurious animation");
        match &changes[0] {
            ViewChange::Changed { index, cols, .. } => {
                assert_eq!(*index, 1);
                assert_eq!(cols, &[2], "only the title column");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(ids(&v), vec![2, 3, 1]);
    }

    #[test]
    fn a_change_to_the_sort_key_moves_and_then_changes() {
        let mut v = seeded();
        let (key, before) = row(1, 10, "ten");
        let (_, after) = row(1, 99, "ninety-nine");
        let changes = v.apply(&one(Change::Update { key, before, after }));

        // Last to first. Both changes are needed: the move carries the identity
        // the animation is built on, the change carries the new values.
        assert_eq!(
            changes,
            vec![
                ViewChange::Moved { from: 2, to: 0 },
                ViewChange::Changed {
                    index: 0,
                    row: row(1, 99, "ninety-nine").1,
                    cols: vec![1, 2],
                }
            ]
        );
        assert_eq!(ids(&v), vec![1, 2, 3]);
    }

    #[test]
    fn a_removal_reports_the_index_it_was_at_and_the_key_it_had() {
        let mut v = seeded();
        let (key, before) = row(2, 30, "thirty");
        let changes = v.apply(&one(Change::Delete { key, before }));
        assert_eq!(
            changes,
            vec![ViewChange::Removed {
                index: 0,
                key: Value::Int(2)
            }]
        );
        assert_eq!(ids(&v), vec![3, 1]);
    }

    #[test]
    fn every_index_is_relative_to_the_changes_already_applied() {
        // The contract, checked the way a host checks it: replay the diff
        // against a plain list and see whether it ends up where the view is.
        // Two removals in one batch is the case that catches an implementation
        // that computed all its indices up front — the second index shifts
        // because the first removal already happened.
        let mut v = seeded();
        let mut host: Vec<i64> = ids(&v);

        let mut batch = Batch::new();
        batch.push(Change::Delete {
            key: RowKey::from(2),
            before: row(2, 30, "thirty").1,
        });
        batch.push(Change::Delete {
            key: RowKey::from(3),
            before: row(3, 20, "twenty").1,
        });
        batch.push(Change::Insert {
            key: RowKey::from(4),
            row: row(4, 40, "forty").1,
        });
        batch.push(Change::Update {
            key: RowKey::from(1),
            before: row(1, 10, "ten").1,
            after: row(1, 50, "fifty").1,
        });

        for change in v.apply(&batch) {
            match change {
                ViewChange::Added { index, row } => {
                    host.insert(index as usize, int(&row, 0));
                }
                ViewChange::Removed { index, .. } => {
                    host.remove(index as usize);
                }
                ViewChange::Changed { index, row, .. } => {
                    host[index as usize] = int(&row, 0);
                }
                ViewChange::Moved { from, to } => {
                    let id = host.remove(from as usize);
                    host.insert(to as usize, id);
                }
            }
        }
        assert_eq!(host, ids(&v));
        assert_eq!(host, vec![1, 4]);
    }

    fn int(row: &Row, col: ColId) -> i64 {
        match row.get(col) {
            Value::Int(i) => *i,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn rows_that_tie_on_the_sort_key_are_ordered_by_key() {
        // Not a nicety: without a tiebreak the position of a tied row depends
        // on arrival order, and two clients replaying the same mutations in the
        // same order would still show different lists after a restart.
        let mut v = View::new(order());
        v.apply(&inserts(&[
            row(9, 5, "nine"),
            row(2, 5, "two"),
            row(5, 5, "five"),
        ]));
        assert_eq!(ids(&v), vec![2, 5, 9]);
    }
}
