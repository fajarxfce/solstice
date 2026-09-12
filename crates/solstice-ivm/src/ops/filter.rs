//! `Filter` — stateless, and the delta rule is subtler than it looks.

use crate::delta::{Batch, Change};
use crate::operator::{Inputs, OpCx, Operator};
use crate::predicate::{Params, Predicate};

/// Drops rows that do not satisfy a predicate.
///
/// # The delta rule
///
/// The naive rule — "pass through changes whose row matches" — is wrong. An
/// update is a *pair* of images, and the predicate must be evaluated against
/// both:
///
/// | before matches | after matches | emits |
/// |---|---|---|
/// | no  | no  | nothing |
/// | no  | yes | **Insert** — the row enters the view |
/// | yes | no  | **Delete** — the row leaves the view |
/// | yes | yes | Update (or nothing, if the projection of the row is unchanged) |
///
/// The two middle rows are the ones a naive implementation gets wrong, and they
/// are the common case in practice: `WHERE closed = false` plus a user closing
/// an issue is precisely "before matches, after does not". Get it wrong and the
/// closed issue stays in the list forever.
///
/// [`Change::from_images`] encodes the whole table, so the rule is expressed
/// once rather than as four branches per operator.
pub struct Filter {
    pred: Predicate,
    params: Params,
}

impl Filter {
    pub fn new(pred: Predicate, params: Params) -> Self {
        Filter { pred, params }
    }

    pub fn predicate(&self) -> &Predicate {
        &self.pred
    }
}

impl Operator for Filter {
    fn name(&self) -> &'static str {
        "Filter"
    }

    fn apply(&mut self, input: Inputs<'_>, _cx: &mut dyn OpCx) -> Batch {
        input
            .primary()
            .iter()
            .filter_map(|change| {
                let keep = |row: &crate::value::Row| self.pred.matches(row, &self.params);
                let before = change.before().filter(|r| keep(r)).cloned();
                let after = change.after().filter(|r| keep(r)).cloned();
                Change::from_images(change.key().clone(), before, after)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operator::NoCx;
    use crate::value::{Row, RowKey, Value};

    /// Rows are `[id, closed]`.
    fn open_issues() -> Filter {
        Filter::new(Predicate::eq(1, false), Params::empty())
    }

    fn row(id: i64, closed: bool) -> Row {
        Row::new(vec![Value::Int(id), Value::from(closed)])
    }

    #[test]
    fn an_update_that_leaves_the_predicate_becomes_a_delete() {
        let mut f = open_issues();
        let batch: Batch = [Change::Update {
            key: RowKey::from(1),
            before: row(1, false),
            after: row(1, true),
        }]
        .into_iter()
        .collect();

        let out = f.apply(Inputs::single(&batch), &mut NoCx);
        assert_eq!(
            out.get(&RowKey::from(1)),
            Some(&Change::Delete {
                key: RowKey::from(1),
                before: row(1, false)
            })
        );
    }

    #[test]
    fn an_update_that_enters_the_predicate_becomes_an_insert() {
        let mut f = open_issues();
        let batch: Batch = [Change::Update {
            key: RowKey::from(1),
            before: row(1, true),
            after: row(1, false),
        }]
        .into_iter()
        .collect();

        let out = f.apply(Inputs::single(&batch), &mut NoCx);
        assert_eq!(
            out.get(&RowKey::from(1)),
            Some(&Change::Insert {
                key: RowKey::from(1),
                row: row(1, false)
            })
        );
    }

    #[test]
    fn changes_outside_the_predicate_emit_nothing() {
        let mut f = open_issues();
        let batch: Batch = [
            Change::Insert {
                key: RowKey::from(1),
                row: row(1, true),
            },
            Change::Update {
                key: RowKey::from(2),
                before: row(2, true),
                after: row(2, true),
            },
        ]
        .into_iter()
        .collect();

        assert!(f.apply(Inputs::single(&batch), &mut NoCx).is_empty());
    }

    #[test]
    fn a_null_column_never_matches() {
        // Unknown collapses to "not in the view", and a delete of a row that
        // was never in the view must not be emitted.
        let mut f = open_issues();
        let null_row = Row::new(vec![Value::Int(1), Value::Null]);
        let batch: Batch = [Change::Insert {
            key: RowKey::from(1),
            row: null_row,
        }]
        .into_iter()
        .collect();
        assert!(f.apply(Inputs::single(&batch), &mut NoCx).is_empty());
    }

    #[test]
    fn filter_is_stateless() {
        assert_eq!(open_issues().state_bytes(), 0);
    }
}
