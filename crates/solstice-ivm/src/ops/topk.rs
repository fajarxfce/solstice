//! `TopK` — `ORDER BY ... LIMIT k`, maintained incrementally.
//!
//! This is where the project lives or dies (plan §1.3, §7). Everything else in
//! the crate is stateless or nearly so; this operator has to keep a bounded
//! window over an unbounded relation and stay correct as rows enter, leave, and
//! move across its boundary.

use crate::delta::Batch;
use crate::operator::{Degrade, Inputs, OpCx, Operator, ScanRequest};
use crate::order::{cmp_entry, Cursor, Dir};
use crate::predicate::{Params, Predicate};
use crate::relation::Relation;
use crate::schema::TableId;
use crate::value::{ColId, Row, RowKey};
use std::cmp::Ordering;

/// How many refills it takes before `slack` doubles.
const SLACK_GROWTH_AFTER: usize = 8;

#[derive(Debug, Clone)]
struct Entry {
    key: RowKey,
    row: Row,
}

/// Keeps the first `k` rows of a sorted relation.
///
/// # The invariant
///
/// > The window is a **contiguous prefix** of the relation in sort order —
/// > either the whole relation (`full`), or its first `window.len()` rows.
///
/// Every decision below follows from it. It is why an insert below the boundary
/// is *ignored* rather than stored when the window is not full: keeping it would
/// leave an unknown gap between the boundary and that row, and the window would
/// then be lying about what it contains. A gap is not a performance problem, it
/// is a wrong list.
///
/// # Why `full` starts `true`
///
/// A fresh operator has an empty window, and vacuously that empty window *is*
/// the whole relation it knows about. Starting `full = false` instead would make
/// hydration pathological: every insert would be measured against a boundary
/// that does not exist yet, all of them would be rejected, and the operator
/// would immediately scan the store to rediscover rows it was just handed.
///
/// `full` becomes `false` the moment the window discards a known row — which is
/// exactly when rows below the boundary start existing.
///
/// This does mean the engine must hydrate the pipeline before trusting a
/// `TopK`. That is already the contract (plan §1.4); nothing else would be
/// correct either.
///
/// # Why the window is truncated during the insert loop
///
/// Hydration arrives as one batch containing every row. Truncating only at the
/// end would mean sorting all 100k of them in memory first — precisely the
/// memory blow-up the plan warns about (§7). Truncating as we go bounds the
/// window at `k + slack` throughout, and rejecting rows against the tightened
/// boundary stays correct: a row below `k + slack` rows already seen cannot be
/// in the true top `k`.
///
/// # Refills
///
/// When deletes drop the window below `k`, the missing rows have to come from
/// somewhere, and the only honest answer is a bounded read of the store: "give
/// me the next `n` rows after this cursor". That is a requery, it is legitimate,
/// and it is the one requery in the design — so it is counted
/// ([`OpCx::note_refill`]) rather than hidden.
///
/// `slack` is what keeps refills rare: the window holds `k + slack` rows so that
/// ordinary deletes are absorbed without touching the store. Under an
/// adversarial workload — repeatedly deleting the top row — slack grows to damp
/// the thrash. Refills per second under exactly that workload is an M0 kill
/// criterion (plan §5.1).
///
/// # What the store must guarantee
///
/// A refill issued while handling a batch **must see that batch's writes.** The
/// engine commits the store transaction and then pumps the graph, so this holds
/// by construction (plan §1.4). If it ever stopped holding, a refill could
/// resurrect a row the batch just deleted, and the resulting list would be wrong
/// in a way no unit test would notice.
pub struct TopK {
    table: TableId,
    order: Vec<(ColId, Dir)>,
    k: usize,
    slack: usize,
    max_slack: usize,
    /// Filter pushed down from upstream.
    ///
    /// Not an optimisation: a refill reads the *base table*, while the window
    /// holds the *filtered* relation. Without the upstream filters a refill
    /// would hand the view rows the pipeline had already excluded.
    pushdown: Option<Predicate>,
    params: Params,

    /// Sorted by [`cmp_entry`]. Length ≤ `k + slack`.
    window: Vec<Entry>,
    full: bool,

    refills: usize,
    refills_since_growth: usize,
}

impl TopK {
    pub fn new(table: TableId, order: Vec<(ColId, Dir)>, k: usize) -> Self {
        let slack = default_slack(k);
        TopK {
            table,
            order,
            k,
            slack,
            max_slack: (k * 4).max(64),
            pushdown: None,
            params: Params::empty(),
            window: Vec::new(),
            // See the type docs: vacuously true for an empty window.
            full: true,
            refills: 0,
            refills_since_growth: 0,
        }
    }

    pub fn with_pushdown(mut self, pred: Predicate, params: Params) -> Self {
        self.pushdown = Some(pred);
        self.params = params;
        self
    }

    /// Override the starting slack. For benchmarks and tests that want to force
    /// refill pressure; production should take the default.
    pub fn with_slack(mut self, slack: usize) -> Self {
        self.slack = slack;
        self.max_slack = self.max_slack.max(slack);
        self
    }

    /// Bounded requeries issued so far.
    pub fn refills(&self) -> usize {
        self.refills
    }

    pub fn slack(&self) -> usize {
        self.slack
    }

    /// The rows currently in the view, in sort order.
    ///
    /// `Join` uses this to materialise a parent's child collection. Cloning is
    /// cheap — a `Row` is an `Arc` over its values — and the result is bounded
    /// by `k`, so this stays proportional to what is on screen.
    pub fn visible_rows(&self) -> Vec<Row> {
        self.visible().iter().map(|e| e.row.clone()).collect()
    }

    fn capacity(&self) -> usize {
        self.k + self.slack
    }

    /// The rows actually visible downstream.
    fn visible(&self) -> &[Entry] {
        &self.window[..self.k.min(self.window.len())]
    }

    fn snapshot(&self) -> Relation {
        self.visible()
            .iter()
            .map(|e| (e.key.clone(), e.row.clone()))
            .collect()
    }

    fn rank(&self, a: (&RowKey, &Row), b: (&RowKey, &Row)) -> Ordering {
        cmp_entry(a, b, &self.order)
    }

    fn boundary(&self) -> Option<&Entry> {
        self.window.last()
    }

    fn remove(&mut self, key: &RowKey, before: &Row) {
        let found = self
            .window
            .binary_search_by(|e| self.rank((&e.key, &e.row), (key, before)));
        if let Ok(pos) = found {
            self.window.remove(pos);
        }
        debug_assert!(
            !self.window.iter().any(|e| &e.key == key),
            "a row is in the window under a different image than the \
             before-image upstream sent: window state has diverged"
        );
    }

    fn try_insert(&mut self, key: RowKey, row: Row) {
        if !self.full {
            match self.boundary() {
                // No boundary to judge against, and rows below are unknown.
                // The refill at the end of this batch reads the true prefix
                // from the store, which picks this row up.
                None => return,
                Some(last) => {
                    if self.rank((&key, &row), (&last.key, &last.row)) == Ordering::Greater {
                        return; // below the window
                    }
                }
            }
        }

        let pos = self
            .window
            .binary_search_by(|e| self.rank((&e.key, &e.row), (&key, &row)))
            .unwrap_or_else(|p| p);
        self.window.insert(pos, Entry { key, row });

        if self.window.len() > self.capacity() {
            self.window.truncate(self.capacity());
            // We just discarded a row we knew about, so rows below the
            // boundary now exist.
            self.full = false;
        }
    }

    fn maybe_refill(&mut self, cx: &mut dyn OpCx) {
        if self.full || self.window.len() >= self.k {
            return;
        }
        let want = self.capacity() - self.window.len();
        if want == 0 {
            return;
        }

        let after = self
            .boundary()
            .map(|e| Cursor::of(&e.row, e.key.clone(), &self.order));

        let rows = cx.scan(&ScanRequest {
            table: self.table,
            order: self.order.clone(),
            after,
            filter: self.pushdown.clone(),
            params: self.params.clone(),
            limit: want,
        });

        let got = rows.len();
        for (key, row) in rows {
            debug_assert!(
                match self.boundary() {
                    None => true,
                    Some(last) =>
                        self.rank((&key, &row), (&last.key, &last.row)) == Ordering::Greater,
                },
                "refill returned a row at or before the cursor; the store and \
                 the window disagree about sort order"
            );
            self.window.push(Entry { key, row });
        }

        // Short read means we reached the end of the relation, which is the
        // only way to learn that the window now holds all of it.
        if got < want {
            self.full = true;
        }

        self.refills += 1;
        cx.note_refill(got);
        self.grow_slack_if_thrashing();
    }

    /// Adaptive slack (plan §1.3): a window that keeps having to refill is being
    /// churned at its boundary, and a wider window absorbs that churn. Growth is
    /// capped so a pathological query cannot turn the operator into a full
    /// materialisation of the table by degrees.
    fn grow_slack_if_thrashing(&mut self) {
        self.refills_since_growth += 1;
        if self.refills_since_growth >= SLACK_GROWTH_AFTER {
            self.refills_since_growth = 0;
            self.slack = (self.slack * 2).max(1).min(self.max_slack);
        }
    }
}

fn default_slack(k: usize) -> usize {
    16.max(k / 4)
}

impl Operator for TopK {
    fn name(&self) -> &'static str {
        "TopK"
    }

    fn apply(&mut self, input: Inputs<'_>, cx: &mut dyn OpCx) -> Batch {
        // Snapshot, mutate, diff — rather than case-analysing which of the four
        // ways each change can interact with the window boundary applies, and
        // which rows it displaces.
        //
        // The window is at most `k + slack` rows, so this is O(k) per batch with
        // a small constant, and it is correct by construction. The case analysis
        // is where every TopK implementation grows its hard-to-find bugs; if a
        // large `k` ever makes this cost show up in a profile, the answer is to
        // track displaced keys incrementally, not to unroll the cases by hand.
        let before = self.snapshot();

        for change in input.primary().iter() {
            if let Some(row) = change.before() {
                self.remove(change.key(), row);
            }
            if let Some(row) = change.after() {
                self.try_insert(change.key().clone(), row.clone());
            }
        }

        self.maybe_refill(cx);

        before.diff(&self.snapshot())
    }

    fn state_bytes(&self) -> usize {
        self.window
            .iter()
            .map(|e| std::mem::size_of::<Entry>() + e.row.heap_bytes() + e.key.heap_bytes())
            .sum()
    }

    /// Shed the slack, keep the view.
    ///
    /// A `TopK`'s visible `k` rows *are* its output; dropping them would leave
    /// the operator unable to describe how the view changed, so it would emit
    /// inserts for rows the client already has. What can be given back is the
    /// slack — at the cost of refilling more often, which is the trade
    /// degradation is supposed to make (plan §1.5).
    fn degrade(&mut self) -> Degrade {
        let before = self.state_bytes();
        self.slack = 0;
        self.refills_since_growth = 0;
        if self.window.len() > self.k {
            self.window.truncate(self.k);
            self.full = false;
        }
        let freed = before.saturating_sub(self.state_bytes());
        if freed == 0 {
            Degrade::Pinned
        } else {
            Degrade::Shed { freed_bytes: freed }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delta::Change;
    use crate::reference::MemStore;
    use crate::value::Value;

    const T: TableId = 1;

    /// Rows are `[id, priority]`, ordered by priority ascending.
    fn row(id: i64, priority: i64) -> Row {
        Row::new(vec![Value::Int(id), Value::Int(priority)])
    }

    fn order() -> Vec<(ColId, Dir)> {
        vec![(1, Dir::Asc)]
    }

    fn store(rows: &[(i64, i64)]) -> MemStore {
        let mut s = MemStore::new();
        s.load(
            T,
            rows.iter()
                .map(|(id, p)| (RowKey::from(*id), row(*id, *p)))
                .collect(),
        );
        s
    }

    fn inserts(rows: &[(i64, i64)]) -> Batch {
        rows.iter()
            .map(|(id, p)| Change::Insert {
                key: RowKey::from(*id),
                row: row(*id, *p),
            })
            .collect()
    }

    fn pump(t: &mut TopK, batch: &Batch, s: &mut MemStore) -> Batch {
        t.apply(Inputs::single(batch), s)
    }

    fn visible_ids(t: &TopK) -> Vec<i64> {
        t.visible()
            .iter()
            .map(|e| match e.key.value() {
                Value::Int(i) => *i,
                v => panic!("unexpected key {v:?}"),
            })
            .collect()
    }

    /// Hydrate a `TopK` with the full contents of a store.
    fn hydrated(rows: &[(i64, i64)], k: usize, slack: usize) -> (TopK, MemStore) {
        let mut s = store(rows);
        let mut t = TopK::new(T, order(), k).with_slack(slack);
        pump(&mut t, &inserts(rows), &mut s);
        (t, s)
    }

    #[test]
    fn hydration_keeps_only_the_top_k() {
        let (t, _) = hydrated(&[(1, 50), (2, 10), (3, 30), (4, 20), (5, 40)], 2, 1);
        assert_eq!(visible_ids(&t), vec![2, 4]);
    }

    #[test]
    fn hydration_never_holds_more_than_capacity() {
        // The memory bound that makes hydrating 100k rows survivable.
        let rows: Vec<(i64, i64)> = (0..500).map(|i| (i, 500 - i)).collect();
        let (t, _) = hydrated(&rows, 5, 2);
        assert!(
            t.window.len() <= 7,
            "window grew to {} during hydration",
            t.window.len()
        );
        assert_eq!(visible_ids(&t), vec![499, 498, 497, 496, 495]);
    }

    #[test]
    fn an_insert_below_the_window_emits_nothing() {
        let (mut t, mut s) = hydrated(&[(1, 10), (2, 20), (3, 30)], 2, 0);
        let out = pump(&mut t, &inserts(&[(4, 99)]), &mut s);
        assert!(out.is_empty());
        assert_eq!(visible_ids(&t), vec![1, 2]);
    }

    #[test]
    fn an_insert_into_the_window_pushes_the_last_row_out() {
        let (mut t, mut s) = hydrated(&[(1, 10), (2, 20), (3, 30)], 2, 0);
        let out = pump(&mut t, &inserts(&[(4, 15)]), &mut s);

        assert_eq!(visible_ids(&t), vec![1, 4]);
        assert_eq!(out.len(), 2, "one row enters the view, one leaves");
        assert!(matches!(
            out.get(&RowKey::from(4)),
            Some(Change::Insert { .. })
        ));
        assert!(matches!(
            out.get(&RowKey::from(2)),
            Some(Change::Delete { .. })
        ));
    }

    #[test]
    fn deleting_a_visible_row_refills_from_the_store() {
        let mut s = store(&[(1, 10), (2, 20), (3, 30)]);
        let mut t = TopK::new(T, order(), 2).with_slack(0);
        pump(&mut t, &inserts(&[(1, 10), (2, 20), (3, 30)]), &mut s);
        assert_eq!(visible_ids(&t), vec![1, 2]);

        // The store is the post-commit state, so row 1 is gone from it too.
        s.table(T).remove(&RowKey::from(1));
        let out = pump(
            &mut t,
            &[Change::Delete {
                key: RowKey::from(1),
                before: row(1, 10),
            }]
            .into_iter()
            .collect(),
            &mut s,
        );

        assert_eq!(visible_ids(&t), vec![2, 3]);
        assert_eq!(t.refills(), 1, "exactly one bounded requery");
        assert!(matches!(
            out.get(&RowKey::from(3)),
            Some(Change::Insert { .. })
        ));
        assert!(matches!(
            out.get(&RowKey::from(1)),
            Some(Change::Delete { .. })
        ));
    }

    #[test]
    fn slack_absorbs_deletes_without_touching_the_store() {
        let mut s = store(&[(1, 10), (2, 20), (3, 30), (4, 40)]);
        let mut t = TopK::new(T, order(), 2).with_slack(2);
        pump(
            &mut t,
            &inserts(&[(1, 10), (2, 20), (3, 30), (4, 40)]),
            &mut s,
        );

        s.table(T).remove(&RowKey::from(1));
        pump(
            &mut t,
            &[Change::Delete {
                key: RowKey::from(1),
                before: row(1, 10),
            }]
            .into_iter()
            .collect(),
            &mut s,
        );

        assert_eq!(visible_ids(&t), vec![2, 3]);
        assert_eq!(t.refills(), 0, "slack is what makes deletes cheap");
    }

    #[test]
    fn a_short_relation_is_learned_once_and_never_rescanned() {
        let mut s = store(&[(1, 10), (2, 20)]);
        let mut t = TopK::new(T, order(), 5).with_slack(0);
        pump(&mut t, &inserts(&[(1, 10), (2, 20)]), &mut s);
        // Fewer rows than k and nothing was discarded, so the operator already
        // knows it has everything.
        assert_eq!(t.refills(), 0);

        s.table(T).remove(&RowKey::from(1));
        pump(
            &mut t,
            &[Change::Delete {
                key: RowKey::from(1),
                before: row(1, 10),
            }]
            .into_iter()
            .collect(),
            &mut s,
        );
        assert_eq!(visible_ids(&t), vec![2]);
        assert_eq!(t.refills(), 0, "still nothing below the window to fetch");
    }

    #[test]
    fn an_update_can_move_a_row_out_of_the_window() {
        let (mut t, mut s) = hydrated(&[(1, 10), (2, 20), (3, 30)], 2, 1);
        s.table(T).insert(RowKey::from(1), row(1, 99));

        let out = pump(
            &mut t,
            &[Change::Update {
                key: RowKey::from(1),
                before: row(1, 10),
                after: row(1, 99),
            }]
            .into_iter()
            .collect(),
            &mut s,
        );

        assert_eq!(visible_ids(&t), vec![2, 3]);
        assert!(matches!(
            out.get(&RowKey::from(1)),
            Some(Change::Delete { .. })
        ));
    }

    #[test]
    fn an_update_can_move_a_row_into_the_window() {
        let (mut t, mut s) = hydrated(&[(1, 10), (2, 20), (3, 30)], 2, 0);
        s.table(T).insert(RowKey::from(3), row(3, 1));

        let out = pump(
            &mut t,
            &[Change::Update {
                key: RowKey::from(3),
                before: row(3, 30),
                after: row(3, 1),
            }]
            .into_iter()
            .collect(),
            &mut s,
        );

        assert_eq!(visible_ids(&t), vec![3, 1]);
        assert!(matches!(
            out.get(&RowKey::from(3)),
            Some(Change::Insert { .. })
        ));
        assert!(matches!(
            out.get(&RowKey::from(2)),
            Some(Change::Delete { .. })
        ));
    }

    #[test]
    fn an_update_within_the_window_reorders_without_churning_membership() {
        let (mut t, mut s) = hydrated(&[(1, 10), (2, 20), (3, 30)], 3, 0);
        s.table(T).insert(RowKey::from(1), row(1, 25));

        let out = pump(
            &mut t,
            &[Change::Update {
                key: RowKey::from(1),
                before: row(1, 10),
                after: row(1, 25),
            }]
            .into_iter()
            .collect(),
            &mut s,
        );

        assert_eq!(visible_ids(&t), vec![2, 1, 3]);
        assert_eq!(out.len(), 1, "only the row that actually changed");
        assert!(matches!(
            out.get(&RowKey::from(1)),
            Some(Change::Update { .. })
        ));
    }

    #[test]
    fn emptying_the_window_refills_from_the_start() {
        // No boundary left to resume from, so the refill must scan from the top
        // rather than from a stale cursor.
        let mut s = store(&[(1, 10), (2, 20), (3, 30), (4, 40)]);
        let mut t = TopK::new(T, order(), 2).with_slack(0);
        pump(
            &mut t,
            &inserts(&[(1, 10), (2, 20), (3, 30), (4, 40)]),
            &mut s,
        );

        for id in [1, 2] {
            s.table(T).remove(&RowKey::from(id));
        }
        pump(
            &mut t,
            &[
                Change::Delete {
                    key: RowKey::from(1),
                    before: row(1, 10),
                },
                Change::Delete {
                    key: RowKey::from(2),
                    before: row(2, 20),
                },
            ]
            .into_iter()
            .collect(),
            &mut s,
        );

        assert_eq!(visible_ids(&t), vec![3, 4]);
    }

    #[test]
    fn repeatedly_deleting_the_top_row_grows_slack() {
        // The adversarial workload from plan §5.1. Each delete costs one refill
        // until adaptive slack starts absorbing them.
        let rows: Vec<(i64, i64)> = (0..60).map(|i| (i, i)).collect();
        let mut s = store(&rows);
        let mut t = TopK::new(T, order(), 3).with_slack(0);
        pump(&mut t, &inserts(&rows), &mut s);

        for id in 0..40 {
            s.table(T).remove(&RowKey::from(id));
            pump(
                &mut t,
                &[Change::Delete {
                    key: RowKey::from(id),
                    before: row(id, id),
                }]
                .into_iter()
                .collect(),
                &mut s,
            );
        }

        assert_eq!(visible_ids(&t), vec![40, 41, 42]);
        assert!(t.slack() > 0, "slack should have adapted to the thrashing");
        assert!(
            t.refills() < 40,
            "slack should have absorbed some deletes; got {} refills for 40 \
             deletes",
            t.refills()
        );
    }

    #[test]
    fn degrade_sheds_slack_but_keeps_the_view() {
        let (mut t, _) = hydrated(&[(1, 10), (2, 20), (3, 30), (4, 40)], 2, 2);
        let before = t.state_bytes();

        let result = t.degrade();

        assert!(matches!(result, Degrade::Shed { .. }));
        assert!(t.state_bytes() < before);
        assert_eq!(
            visible_ids(&t),
            vec![1, 2],
            "the view itself must survive degradation"
        );
    }
}
