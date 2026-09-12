//! `Project` — stateless, and it does more work than reshaping rows.

use crate::delta::{Batch, Change};
use crate::operator::{Inputs, OpCx, Operator};
use crate::value::ColId;

/// Selects a subset of columns, in the given order.
///
/// # Projection suppresses updates, and that is the point
///
/// Because [`Change::from_images`] drops a change whose before and after images
/// are equal, projecting away a column means edits to that column stop
/// propagating entirely. A list showing `(id, title)` does not re-render when
/// someone edits an issue's `body`.
///
/// That is not an optimisation bolted on afterwards — it falls out of pushing
/// `Project` as close to the source as the plan allows, and it is a large part
/// of why a reactive list can stay quiet under a busy database. It also means
/// `Project` must sit *after* `Filter`, since the filter may read columns the
/// projection discards.
pub struct Project {
    cols: Vec<ColId>,
}

impl Project {
    pub fn new(cols: Vec<ColId>) -> Self {
        Project { cols }
    }

    pub fn columns(&self) -> &[ColId] {
        &self.cols
    }
}

impl Operator for Project {
    fn name(&self) -> &'static str {
        "Project"
    }

    fn apply(&mut self, input: Inputs<'_>, _cx: &mut dyn OpCx) -> Batch {
        input
            .primary()
            .iter()
            .filter_map(|change| {
                Change::from_images(
                    change.key().clone(),
                    change.before().map(|r| r.project(&self.cols)),
                    change.after().map(|r| r.project(&self.cols)),
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operator::NoCx;
    use crate::value::{Row, RowKey, Value};

    /// Rows are `[id, title, body]`.
    fn row(id: i64, title: &str, body: &str) -> Row {
        Row::new(vec![Value::Int(id), Value::text(title), Value::text(body)])
    }

    #[test]
    fn edits_to_projected_away_columns_do_not_propagate() {
        let mut p = Project::new(vec![0, 1]);
        let batch: Batch = [Change::Update {
            key: RowKey::from(1),
            before: row(1, "t", "old body"),
            after: row(1, "t", "new body"),
        }]
        .into_iter()
        .collect();

        assert!(
            p.apply(Inputs::single(&batch), &mut NoCx).is_empty(),
            "the view does not show `body`, so nothing downstream should wake up"
        );
    }

    #[test]
    fn edits_to_kept_columns_propagate_projected() {
        let mut p = Project::new(vec![0, 1]);
        let batch: Batch = [Change::Update {
            key: RowKey::from(1),
            before: row(1, "old", "b"),
            after: row(1, "new", "b"),
        }]
        .into_iter()
        .collect();

        assert_eq!(
            p.apply(Inputs::single(&batch), &mut NoCx)
                .get(&RowKey::from(1)),
            Some(&Change::Update {
                key: RowKey::from(1),
                before: Row::new(vec![Value::Int(1), Value::text("old")]),
                after: Row::new(vec![Value::Int(1), Value::text("new")]),
            })
        );
    }

    #[test]
    fn columns_are_emitted_in_the_requested_order() {
        let mut p = Project::new(vec![1, 0]);
        let batch: Batch = [Change::Insert {
            key: RowKey::from(1),
            row: row(1, "t", "b"),
        }]
        .into_iter()
        .collect();

        assert_eq!(
            p.apply(Inputs::single(&batch), &mut NoCx)
                .get(&RowKey::from(1)),
            Some(&Change::Insert {
                key: RowKey::from(1),
                row: Row::new(vec![Value::text("t"), Value::Int(1)]),
            })
        );
    }
}
