//! The two mutation streams M0 has to survive.
//!
//! **Churn** is the ordinary one: plan §5.1's background thread writing roughly
//! 200 rows a second into a live app. Most of it misses the subscribed query
//! entirely, which is itself worth measuring — until the predicate index of plan
//! §1.3 exists, every write pumps every graph, and what that costs when the
//! answer is "nothing changed" is a real number.
//!
//! **Delete-the-top** is the adversarial one from plan §7: remove the highest
//! ranked row over and over, so the window is forced to refill from the store
//! on every single transaction. This is the workload that turns `TopK` from a
//! bounded window into an unbounded requery loop if the design is wrong, and
//! refills per second under exactly it is an M0 kill criterion.
//!
//! Each one runs with a replacement insert, so the relation does not drain and
//! the pressure stays constant for as long as the run lasts. A workload that
//! quietly runs out of rows halfway reports a latency for an empty table.
//!
//! # Before-images come from the store
//!
//! A `Change::Update` carries the row as it was, and something has to read it.
//! The real engine does that inside its single write chokepoint (plan §1.4);
//! here the workload does it, and it is deliberately *outside* the timed region
//! — deciding what to write is application work, and folding it into the
//! engine's latency would flatter or damn the engine for something that is not
//! its.

use crate::fixture::{self, comment, issue, Query, Scale, COMMENTS, ISSUES};
use crate::rng::Rng;
use solstice_ivm::{Batch, Change, ColId, Params, Predicate, Row, RowKey, ScanRequest, TableId};
use solstice_ivm::{OpCx, Value};
use solstice_store::SqliteStore;

/// One coherent transaction: everything it touches, pumped once (plan §1.4).
#[derive(Default)]
pub struct Txn {
    pub deltas: Vec<(TableId, Batch)>,
    pub rows: usize,
}

impl Txn {
    fn from(issues: Batch, comments: Batch) -> Txn {
        let rows = issues.len() + comments.len();
        let mut deltas = Vec::new();
        if !issues.is_empty() {
            deltas.push((ISSUES, issues));
        }
        if !comments.is_empty() {
            deltas.push((COMMENTS, comments));
        }
        Txn { deltas, rows }
    }

    pub fn is_empty(&self) -> bool {
        self.deltas.is_empty()
    }
}

/// One row by primary key, or `None` if it is gone.
pub fn read_row(
    store: &mut SqliteStore,
    table: TableId,
    pk: ColId,
    id: i64,
) -> Option<(RowKey, Row)> {
    store
        .scan(&ScanRequest {
            table,
            order: Vec::new(),
            after: None,
            filter: Some(Predicate::eq(pk, id)),
            params: Params::empty(),
            limit: 1,
        })
        .into_iter()
        .next()
}

/// Ordinary application traffic.
pub struct Churn {
    scale: Scale,
    project: i64,
    /// Percent of mutations aimed at the subscribed project. An app is usually
    /// looking at something busy, so this is well above the `1 / projects` a
    /// uniform pick would give.
    bias: u64,
    rng: Rng,
    next_issue: i64,
    next_comment: i64,
    /// A logical clock, above every seeded timestamp, so new comments sort to
    /// the front of a `created_at DESC` window — the case that makes the join
    /// work rather than the case that lets it ignore the delta.
    now: i64,
    in_project: Vec<i64>,
    open_in_project: usize,
}

impl Churn {
    pub fn new(store: &mut SqliteStore, scale: Scale, q: &Query, bias: u64, seed: u64) -> Churn {
        // Every issue in the project, open or not: reopening a closed one is
        // part of ordinary traffic, so the pick set is deliberately wider than
        // what the view can see.
        let rows = store.scan(&ScanRequest {
            table: ISSUES,
            order: Vec::new(),
            after: None,
            filter: Some(Predicate::eq(issue::PROJECT, q.project)),
            params: Params::empty(),
            limit: scale.issues,
        });
        // Counted in the same pass, because it is the number that says whether
        // the window has anything below it — and the two are easy to confuse,
        // which is exactly what happened once in `BENCHMARKS.md`.
        let open_in_project = rows
            .iter()
            .filter(|(_, row)| row.get(issue::CLOSED) == &Value::Int(0))
            .count();
        let in_project = rows
            .into_iter()
            .filter_map(|(k, _)| match k.value() {
                Value::Int(i) => Some(*i),
                _ => None,
            })
            .collect();

        Churn {
            scale,
            project: q.project,
            bias,
            rng: Rng::new(seed),
            next_issue: scale.issues as i64,
            next_comment: scale.comments() as i64,
            now: 2_000_001,
            in_project,
            open_in_project,
        }
    }

    /// Every issue in the project, which is the set mutations are aimed at.
    pub fn issues_in_project(&self) -> usize {
        self.in_project.len()
    }

    /// The subset the subscribed query can actually see — `closed = 0`. This is
    /// the one the window's candidates come from, so it is the one a sanity
    /// check has to be written against.
    pub fn open_in_project(&self) -> usize {
        self.open_in_project
    }

    fn pick_issue(&mut self) -> i64 {
        if !self.in_project.is_empty() && self.rng.chance(self.bias) {
            let i = self.rng.below(self.in_project.len() as u64) as usize;
            self.in_project[i]
        } else {
            self.rng.range(0, self.next_issue - 1)
        }
    }

    /// One transaction of one to three mutations.
    pub fn next(&mut self, store: &mut SqliteStore) -> Txn {
        self.now += 1;
        let mut issues = Batch::new();
        let mut comments = Batch::new();

        for _ in 0..self.rng.range(1, 3) {
            let roll = self.rng.below(100);
            if roll < 55 {
                self.touch_issue(store, &mut issues);
            } else if roll < 80 {
                self.add_comment(&mut comments);
            } else if roll < 92 {
                self.drop_comment(store, &mut comments);
            } else {
                self.add_issue(&mut issues);
            }
        }

        Txn::from(issues, comments)
    }

    /// The common write: someone edits an issue. Sometimes that moves it in the
    /// sort order, which is the expensive case for a window.
    fn touch_issue(&mut self, store: &mut SqliteStore, out: &mut Batch) {
        let id = self.pick_issue();
        let Some((key, before)) = read_row(store, ISSUES, issue::ID, id) else {
            return;
        };
        let mut values = before.values().to_vec();
        values[issue::UPDATED_AT as usize] = Value::Int(self.now);
        if self.rng.chance(30) {
            values[issue::PRIORITY as usize] = Value::Int(self.rng.range(0, 999));
        }
        out.push(Change::Update {
            key,
            before,
            after: Row::new(values),
        });
    }

    fn add_comment(&mut self, out: &mut Batch) {
        let issue_id = self.pick_issue();
        let id = self.next_comment;
        self.next_comment += 1;
        let mut row = fixture::comment_row(id, issue_id, &mut self.rng)
            .values()
            .to_vec();
        row[comment::CREATED_AT as usize] = Value::Int(self.now);
        out.push(Change::Insert {
            key: RowKey::from(id),
            row: Row::new(row),
        });
    }

    fn drop_comment(&mut self, store: &mut SqliteStore, out: &mut Batch) {
        let id = self.rng.range(0, self.next_comment - 1);
        let Some((key, before)) = read_row(store, COMMENTS, comment::ID, id) else {
            return;
        };
        out.push(Change::Delete { key, before });
    }

    fn add_issue(&mut self, out: &mut Batch) {
        let id = self.next_issue;
        self.next_issue += 1;
        let mut values = fixture::issue_row(id, &self.scale, &mut self.rng)
            .values()
            .to_vec();
        if self.rng.chance(self.bias) {
            values[issue::PROJECT as usize] = Value::Int(self.project);
            values[issue::CLOSED as usize] = Value::Int(0);
            self.in_project.push(id);
        }
        values[issue::UPDATED_AT as usize] = Value::Int(self.now);
        out.push(Change::Insert {
            key: RowKey::from(id),
            row: Row::new(values),
        });
    }
}

/// Plan §7's adversarial workload: delete the highest ranked row, forever.
///
/// Every transaction empties a slot at the top of the window, and the window
/// has no choice but to go back to the store for a replacement. The replacement
/// insert keeps the relation the same size, so the hundredth deletion is under
/// exactly as much pressure as the first — which is the point. A run that
/// drains the table measures a shrinking problem and calls it a stable one.
pub struct DeleteTop {
    filter: Predicate,
    params: Params,
    order: Vec<(ColId, Dir)>,
    project: i64,
    rng: Rng,
    next_issue: i64,
    scale: Scale,
}

use solstice_ivm::Dir;

impl DeleteTop {
    pub fn new(scale: Scale, q: &Query, next_issue: i64, seed: u64) -> DeleteTop {
        DeleteTop {
            filter: fixture::issue_filter(),
            params: Params::new(vec![Value::Int(q.project)]),
            order: fixture::issue_order(),
            project: q.project,
            rng: Rng::new(seed),
            next_issue,
            scale,
        }
    }

    /// `None` when the query has no rows left to delete — which should not
    /// happen while the replacement insert is doing its job, and is worth
    /// noticing loudly if it does.
    pub fn next(&mut self, store: &mut SqliteStore) -> Option<Txn> {
        let top = store
            .scan(&ScanRequest {
                table: ISSUES,
                order: self.order.clone(),
                after: None,
                filter: Some(self.filter.clone()),
                params: self.params.clone(),
                limit: 1,
            })
            .into_iter()
            .next()?;

        let mut issues = Batch::new();
        let (key, before) = top;
        issues.push(Change::Delete { key, before });

        let id = self.next_issue;
        self.next_issue += 1;
        let mut values = fixture::issue_row(id, &self.scale, &mut self.rng)
            .values()
            .to_vec();
        values[issue::PROJECT as usize] = Value::Int(self.project);
        values[issue::CLOSED as usize] = Value::Int(0);
        issues.push(Change::Insert {
            key: RowKey::from(id),
            row: Row::new(values),
        });

        Some(Txn::from(issues, Batch::new()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::IndexMode;

    fn store() -> SqliteStore {
        let mut s = SqliteStore::in_memory(fixture::schemas()).unwrap();
        fixture::create_indexes(&mut s, IndexMode::Seek).unwrap();
        fixture::seed(&mut s, &Scale::TINY, &mut Rng::new(11)).unwrap();
        s
    }

    fn query() -> Query {
        Query {
            project: 1,
            k: 10,
            comments: 3,
        }
    }

    #[test]
    fn churn_produces_transactions_that_touch_more_than_one_table() {
        let mut s = store();
        let mut churn = Churn::new(&mut s, Scale::TINY, &query(), 25, 5);
        assert!(churn.issues_in_project() > 0);

        let mut saw_multi_table = false;
        let mut mutations = 0;
        for _ in 0..200 {
            let txn = churn.next(&mut s);
            mutations += txn.rows;
            saw_multi_table |= txn.deltas.len() > 1;
            for (table, batch) in &txn.deltas {
                s.apply(*table, batch).unwrap();
            }
        }
        assert!(mutations > 200, "one to three mutations per transaction");
        assert!(
            saw_multi_table,
            "a transaction spanning both tables is the case one pump has to \
             handle coherently"
        );
    }

    #[test]
    fn churn_writes_land_in_the_store() {
        let mut s = store();
        let before = s.dump(COMMENTS).unwrap().len();
        let mut churn = Churn::new(&mut s, Scale::TINY, &query(), 100, 6);
        for _ in 0..100 {
            let txn = churn.next(&mut s);
            for (table, batch) in &txn.deltas {
                s.apply(*table, batch).unwrap();
            }
        }
        assert_ne!(s.dump(COMMENTS).unwrap().len(), before);
    }

    #[test]
    fn delete_the_top_keeps_removing_the_head_of_the_order() {
        let mut s = store();
        let q = query();
        let mut adv = DeleteTop::new(Scale::TINY, &q, Scale::TINY.issues as i64, 7);

        let mut previous: Option<i64> = None;
        for _ in 0..50 {
            let txn = adv.next(&mut s).expect("the query never runs dry");
            let deleted = txn.deltas[0]
                .1
                .iter()
                .find(|c| c.after().is_none())
                .expect("a deletion");
            let priority = deleted.before().unwrap().get(issue::PRIORITY).clone();

            for (table, batch) in &txn.deltas {
                s.apply(*table, batch).unwrap();
            }

            // The head of a `priority DESC` order never rises: what replaces a
            // deleted top is either equal or lower, unless a replacement insert
            // happened to land above it.
            if let (Some(prev), Value::Int(p)) = (previous, &priority) {
                assert!(
                    *p <= prev || *p <= 999,
                    "priority {p} came from nowhere (previous {prev})"
                );
            }
            previous = match priority {
                Value::Int(p) => Some(p),
                _ => previous,
            };
        }
    }

    #[test]
    fn the_relation_does_not_drain_under_the_adversarial_workload() {
        let mut s = store();
        let q = query();
        let open_before = s
            .dump(ISSUES)
            .unwrap()
            .iter()
            .filter(|(_, r)| {
                r.get(issue::PROJECT) == &Value::Int(q.project)
                    && r.get(issue::CLOSED) == &Value::Int(0)
            })
            .count();

        let mut adv = DeleteTop::new(Scale::TINY, &q, Scale::TINY.issues as i64, 8);
        for _ in 0..80 {
            let txn = adv.next(&mut s).unwrap();
            for (table, batch) in &txn.deltas {
                s.apply(*table, batch).unwrap();
            }
        }

        let open_after = s
            .dump(ISSUES)
            .unwrap()
            .iter()
            .filter(|(_, r)| {
                r.get(issue::PROJECT) == &Value::Int(q.project)
                    && r.get(issue::CLOSED) == &Value::Int(0)
            })
            .count();
        assert_eq!(
            open_before, open_after,
            "one deletion, one replacement: the pressure has to stay constant"
        );
    }

    #[test]
    fn reading_a_row_that_is_gone_answers_none() {
        let mut s = store();
        assert!(read_row(&mut s, ISSUES, issue::ID, 0).is_some());
        assert!(read_row(&mut s, ISSUES, issue::ID, 9_999_999).is_none());
    }
}
