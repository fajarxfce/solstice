//! Sort orders and cursors.
//!
//! # Why a cursor carries the primary key
//!
//! Resuming a scan after "the last row I already have" needs a bound that is
//! *unique*. Sort keys are not: two issues can share a priority. A cursor made
//! only of sort-key values either re-reads every row that ties with it, or skips
//! them — duplicated or missing rows in a paginated list, depending on which way
//! the comparison rounds.
//!
//! So a [`Cursor`] is the full sort key **plus** the primary key, and the
//! primary key always breaks ties ascending regardless of the sort direction.
//! The direction does not matter, only that sorting and cursor comparison agree
//! on it — which they do because both go through [`cmp_entry`].

use crate::value::{ColId, Row, RowKey, Value};
use std::cmp::Ordering;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Dir {
    Asc,
    Desc,
}

impl Dir {
    fn apply(self, ord: Ordering) -> Ordering {
        match self {
            Dir::Asc => ord,
            Dir::Desc => ord.reverse(),
        }
    }
}

/// A position in a sorted relation: where to resume from, exclusive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    /// Values of the sort columns, positionally matching the order spec.
    pub sort_key: Vec<Value>,
    /// Primary key of the last row already seen. Breaks ties on `sort_key`.
    pub pk: RowKey,
}

impl Cursor {
    pub fn of(row: &Row, pk: RowKey, order: &[(ColId, Dir)]) -> Cursor {
        Cursor {
            sort_key: order.iter().map(|(c, _)| row.get(*c).clone()).collect(),
            pk,
        }
    }
}

/// Compare two values for sorting: NULLs first, then SQL comparison.
///
/// SQL comparison rather than the structural order so that results match the
/// SQLite oracle. It is not a strict total order — `Int(1)` and `Real(1.0)` tie
/// — which is exactly why every caller appends the primary key.
fn cmp_for_sort(a: &Value, b: &Value) -> Ordering {
    match (a.is_null(), b.is_null()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        (false, false) => a.sql_cmp(b).unwrap_or(Ordering::Equal),
    }
}

/// Compare two rows under an order spec, ignoring primary keys.
pub fn cmp_by_order(a: &Row, b: &Row, order: &[(ColId, Dir)]) -> Ordering {
    for (col, dir) in order {
        let ord = dir.apply(cmp_for_sort(a.get(*col), b.get(*col)));
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

/// The total order every sorted structure in the engine uses: sort key, then
/// primary key ascending.
pub fn cmp_entry(a: (&RowKey, &Row), b: (&RowKey, &Row), order: &[(ColId, Dir)]) -> Ordering {
    cmp_by_order(a.1, b.1, order).then_with(|| a.0.cmp(b.0))
}

/// Where a row sits relative to a cursor, under the same total order as
/// [`cmp_entry`].
pub fn cmp_to_cursor(key: &RowKey, row: &Row, cursor: &Cursor, order: &[(ColId, Dir)]) -> Ordering {
    for (i, (col, dir)) in order.iter().enumerate() {
        let rhs = cursor.sort_key.get(i).unwrap_or(&Value::Null);
        let ord = dir.apply(cmp_for_sort(row.get(*col), rhs));
        if ord != Ordering::Equal {
            return ord;
        }
    }
    key.cmp(&cursor.pk)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(a: i64, b: i64) -> Row {
        Row::new(vec![Value::Int(a), Value::Int(b)])
    }

    #[test]
    fn descending_reverses_only_the_marked_column() {
        let order = [(0, Dir::Desc), (1, Dir::Asc)];
        assert_eq!(cmp_by_order(&row(2, 1), &row(1, 9), &order), Ordering::Less);
        assert_eq!(cmp_by_order(&row(1, 1), &row(1, 9), &order), Ordering::Less);
    }

    #[test]
    fn nulls_sort_first_ascending_and_last_descending() {
        let null_row = Row::new(vec![Value::Null, Value::Int(0)]);
        assert_eq!(
            cmp_by_order(&null_row, &row(1, 0), &[(0, Dir::Asc)]),
            Ordering::Less
        );
        assert_eq!(
            cmp_by_order(&null_row, &row(1, 0), &[(0, Dir::Desc)]),
            Ordering::Greater
        );
    }

    #[test]
    fn ties_on_the_sort_key_are_broken_by_primary_key() {
        let order = [(0, Dir::Desc)];
        // The sort column ties, so the pk decides — and it decides *ascending*
        // even though the sort is descending.
        assert_eq!(
            cmp_entry(
                (&RowKey::from(1), &row(5, 0)),
                (&RowKey::from(2), &row(5, 0)),
                &order
            ),
            Ordering::Less
        );
    }

    #[test]
    fn a_cursor_excludes_exactly_the_row_it_was_built_from() {
        let order = [(0, Dir::Asc)];
        let cursor = Cursor::of(&row(5, 0), RowKey::from(7), &order);

        // The row itself: equal, so an exclusive `after` skips it.
        assert_eq!(
            cmp_to_cursor(&RowKey::from(7), &row(5, 0), &cursor, &order),
            Ordering::Equal
        );
        // Same sort key, higher pk: still ahead, so it is not skipped. Without
        // the pk in the cursor this row would tie and be dropped.
        assert_eq!(
            cmp_to_cursor(&RowKey::from(8), &row(5, 0), &cursor, &order),
            Ordering::Greater
        );
        assert_eq!(
            cmp_to_cursor(&RowKey::from(6), &row(5, 0), &cursor, &order),
            Ordering::Less
        );
    }

    #[test]
    fn cursor_comparison_agrees_with_entry_comparison() {
        let order = [(0, Dir::Desc), (1, Dir::Asc)];
        let rows = [
            (RowKey::from(1), row(3, 1)),
            (RowKey::from(2), row(3, 2)),
            (RowKey::from(3), row(1, 9)),
        ];
        for (ak, ar) in &rows {
            let cursor = Cursor::of(ar, ak.clone(), &order);
            for (bk, br) in &rows {
                assert_eq!(
                    cmp_to_cursor(bk, br, &cursor, &order),
                    cmp_entry((bk, br), (ak, ar), &order),
                    "cursor and entry comparison must define the same order"
                );
            }
        }
    }
}
