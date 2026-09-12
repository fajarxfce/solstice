//! Changes and batches — the currency every operator speaks.
//!
//! # Why keyed diffs rather than raw Z-sets
//!
//! The theory this engine is built on (plan §1.3) is Z-sets: multisets with
//! integer multiplicities, where an update is `-old +new`. But every relation
//! in DQL is keyed by a primary key, so the general Z-set degenerates to a
//! *keyed diff* almost everywhere, and carrying an explicit multiplicity would
//! be redundant noise on the hot path.
//!
//! So the wire format between operators is [`Change`] — insert / update /
//! delete against a key — and [`Change::to_zset`] recovers the Z-set form for
//! the one operator family that genuinely needs multiset semantics (aggregates).
//!
//! [`Batch::compose`] implements the diff algebra that makes coalescing sound.
//! That is what lets the FFI layer collapse a backlog of deltas under
//! backpressure instead of dropping them (plan §4.2).

use crate::value::{Row, RowKey};
use indexmap::IndexMap;
use smallvec::SmallVec;

/// A single keyed change to a relation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Insert {
        key: RowKey,
        row: Row,
    },
    Update {
        key: RowKey,
        before: Row,
        after: Row,
    },
    Delete {
        key: RowKey,
        before: Row,
    },
}

impl Change {
    pub fn key(&self) -> &RowKey {
        match self {
            Change::Insert { key, .. }
            | Change::Update { key, .. }
            | Change::Delete { key, .. } => key,
        }
    }

    /// The row as it existed before this change, if it existed.
    pub fn before(&self) -> Option<&Row> {
        match self {
            Change::Insert { .. } => None,
            Change::Update { before, .. } | Change::Delete { before, .. } => Some(before),
        }
    }

    /// The row as it exists after this change, if it still exists.
    pub fn after(&self) -> Option<&Row> {
        match self {
            Change::Insert { row, .. } => Some(row),
            Change::Update { after, .. } => Some(after),
            Change::Delete { .. } => None,
        }
    }

    /// Build a change from before/after images, or `None` if nothing changed.
    ///
    /// This is the constructor operators should reach for: it collapses the
    /// "predicate still matches, row is byte-identical" case to no change at
    /// all, which is what keeps idle churn from reaching the UI.
    pub fn from_images(key: RowKey, before: Option<Row>, after: Option<Row>) -> Option<Change> {
        match (before, after) {
            (None, None) => None,
            (None, Some(row)) => Some(Change::Insert { key, row }),
            (Some(before), None) => Some(Change::Delete { key, before }),
            (Some(before), Some(after)) => {
                if before == after {
                    None
                } else {
                    Some(Change::Update { key, before, after })
                }
            }
        }
    }

    /// Z-set form: `-before +after`, for operators that need true multiset
    /// semantics.
    pub fn to_zset(&self) -> SmallVec<[(Row, i64); 2]> {
        let mut out = SmallVec::new();
        if let Some(b) = self.before() {
            out.push((b.clone(), -1));
        }
        if let Some(a) = self.after() {
            out.push((a.clone(), 1));
        }
        out
    }
}

/// An ordered set of changes, at most one per key.
///
/// The "at most one per key" invariant is maintained by [`Batch::push`] and is
/// what makes composition associative. Insertion order is preserved so that
/// output remains deterministic — a requirement, not a nicety, since the whole
/// test strategy rests on reproducible runs (plan §6).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Batch {
    changes: IndexMap<RowKey, Change>,
}

impl Batch {
    pub fn new() -> Self {
        Batch::default()
    }

    pub fn len(&self) -> usize {
        self.changes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Change> {
        self.changes.values()
    }

    pub fn get(&self, key: &RowKey) -> Option<&Change> {
        self.changes.get(key)
    }

    /// Add a change, composing it with any existing change for the same key.
    ///
    /// Composing rather than overwriting is what makes `push` safe to call in a
    /// loop from an operator that may emit several times for one key.
    pub fn push(&mut self, change: Change) {
        let key = change.key().clone();
        match self.changes.shift_remove(&key) {
            None => {
                self.changes.insert(key, change);
            }
            Some(existing) => {
                // The composition of two changes is just the diff between the
                // first one's "before" and the second one's "after".
                let before = existing.before().cloned();
                let after = change.after().cloned();
                if let Some(composed) = Change::from_images(key.clone(), before, after) {
                    self.changes.insert(key, composed);
                }
                // If they cancel out (insert-then-delete, or a round trip back
                // to the original value) the key drops out entirely.
            }
        }
    }

    /// Compose this batch with a later one: the result has the same effect as
    /// applying `self` then `later`.
    pub fn compose(mut self, later: Batch) -> Batch {
        for change in later.changes.into_values() {
            self.push(change);
        }
        self
    }

    pub fn into_changes(self) -> Vec<Change> {
        self.changes.into_values().collect()
    }
}

impl FromIterator<Change> for Batch {
    fn from_iter<T: IntoIterator<Item = Change>>(iter: T) -> Self {
        let mut b = Batch::new();
        for c in iter {
            b.push(c);
        }
        b
    }
}

impl Extend<Change> for Batch {
    fn extend<T: IntoIterator<Item = Change>>(&mut self, iter: T) {
        for c in iter {
            self.push(c);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Value;

    fn row(v: i64) -> Row {
        Row::new(vec![Value::Int(v)])
    }

    fn key(k: i64) -> RowKey {
        RowKey::from(k)
    }

    #[test]
    fn insert_then_delete_cancels() {
        let mut b = Batch::new();
        b.push(Change::Insert {
            key: key(1),
            row: row(1),
        });
        b.push(Change::Delete {
            key: key(1),
            before: row(1),
        });
        assert!(b.is_empty(), "an insert undone in the same batch vanishes");
    }

    #[test]
    fn insert_then_update_is_an_insert() {
        let mut b = Batch::new();
        b.push(Change::Insert {
            key: key(1),
            row: row(1),
        });
        b.push(Change::Update {
            key: key(1),
            before: row(1),
            after: row(2),
        });
        assert_eq!(
            b.get(&key(1)),
            Some(&Change::Insert {
                key: key(1),
                row: row(2)
            })
        );
    }

    #[test]
    fn delete_then_insert_is_an_update() {
        let mut b = Batch::new();
        b.push(Change::Delete {
            key: key(1),
            before: row(1),
        });
        b.push(Change::Insert {
            key: key(1),
            row: row(2),
        });
        assert_eq!(
            b.get(&key(1)),
            Some(&Change::Update {
                key: key(1),
                before: row(1),
                after: row(2)
            })
        );
    }

    #[test]
    fn update_round_trip_cancels() {
        // 1 -> 2 -> 1 is not a change, and the UI should never see it. This is
        // the A->B->A flicker that invariant #4 in the plan guards against.
        let mut b = Batch::new();
        b.push(Change::Update {
            key: key(1),
            before: row(1),
            after: row(2),
        });
        b.push(Change::Update {
            key: key(1),
            before: row(2),
            after: row(1),
        });
        assert!(b.is_empty());
    }

    #[test]
    fn update_then_delete_reports_the_original_before_image() {
        let mut b = Batch::new();
        b.push(Change::Update {
            key: key(1),
            before: row(1),
            after: row(2),
        });
        b.push(Change::Delete {
            key: key(1),
            before: row(2),
        });
        // Downstream must see the row as it was before the whole batch, not the
        // intermediate value it never observed.
        assert_eq!(
            b.get(&key(1)),
            Some(&Change::Delete {
                key: key(1),
                before: row(1)
            })
        );
    }

    #[test]
    fn compose_is_associative() {
        let mk = |k: i64, from: i64, to: i64| Change::Update {
            key: key(k),
            before: row(from),
            after: row(to),
        };
        let a: Batch = [mk(1, 1, 2)].into_iter().collect();
        let b: Batch = [mk(1, 2, 3), mk(2, 0, 1)].into_iter().collect();
        let c: Batch = [mk(1, 3, 4)].into_iter().collect();

        let left = a.clone().compose(b.clone()).compose(c.clone());
        let right = a.compose(b.compose(c));
        assert_eq!(left, right);
    }

    #[test]
    fn from_images_collapses_noop_updates() {
        assert_eq!(
            Change::from_images(key(1), Some(row(1)), Some(row(1))),
            None
        );
    }

    #[test]
    fn zset_form_of_an_update_is_minus_old_plus_new() {
        let c = Change::Update {
            key: key(1),
            before: row(1),
            after: row(2),
        };
        assert_eq!(c.to_zset().as_slice(), &[(row(1), -1), (row(2), 1)]);
    }
}
