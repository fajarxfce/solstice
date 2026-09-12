//! `Join(1:N)` — traversing a declared relationship, hierarchically.
//!
//! # The output is a tree, not a product
//!
//! A SQL join multiplies rows: one issue with three comments becomes three
//! result rows. DQL does not do that (plan §1.1) — the children land in a column
//! of the parent as a [`Value::Rows`], so one issue stays one row.
//!
//! That is an API decision with a load-bearing consequence. Because parent
//! cardinality is unchanged, `ORDER BY ... LIMIT` over parents **commutes with
//! the join**, so the planner puts `TopK` *below* it and this operator only ever
//! holds children for parents that are actually on screen. The flat alternative
//! forces `TopK` above the join, and refilling that window means asking the join
//! for children of parents it is not holding — which is exactly the unbounded
//! state that plan §7 names as the project's most likely technical failure.
//!
//! So the M0 chain is `Source → Filter → TopK → Join(1:N)`, not the order the
//! plan text lists them in.
//!
//! # Each parent's child window is a `TopK`
//!
//! "The three latest comments" is `ORDER BY created_at DESC LIMIT 3` over the
//! children of one parent, which is precisely what [`TopK`] already implements —
//! including the hard part, refilling from the store when a delete empties the
//! window. Reusing it means the per-parent window inherits an operator that the
//! property tests already hammer, instead of growing a second, shakier copy of
//! the same logic.
//!
//! The per-parent `TopK` carries `fk = <this parent> AND <child filter>` as
//! refill pushdown. That is not an optimisation: a refill reads the base table,
//! and without the fk it would pull in some other parent's comments.

use crate::delta::{Batch, Change};
use crate::operator::{Degrade, Inputs, OpCx, Operator, Port, RefillKind, ScanRequest};
use crate::ops::TopK;
use crate::order::Dir;
use crate::predicate::{CmpOp, Expr, Params, Predicate};
use crate::relation::Relation;
use crate::schema::TableId;
use crate::value::{ColId, Row, RowKey, Value};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

/// The parent stream. Rows come out of here widened, never multiplied.
pub const PARENT: Port = 0;
/// The child stream. Deltas only — see [`crate::ops::Source::deltas_only`].
pub const CHILD: Port = 1;

/// What this operator remembers about one parent in the view.
struct ParentState {
    /// The parent row as it arrived, *without* the child column.
    row: Row,
    /// The parent's join-key value at the time `children` was built.
    ///
    /// Cached rather than re-read from `row` so that a re-key is detectable by
    /// comparing against what the window was actually built for.
    key_value: Value,
    children: TopK,
}

/// Attach each parent's children as a collection in one extra column.
pub struct Join1N {
    child_table: TableId,
    /// Column of the parent row holding the value children point at.
    parent_key: ColId,
    /// Column of the child row holding the foreign key.
    child_fk: ColId,
    /// Order within each parent's child collection.
    order: Vec<(ColId, Dir)>,
    /// Children per parent. Mandatory on 1:N traversal (plan §1.1) — an
    /// unbounded fan-out is how this operator's state stops being bounded.
    limit: usize,
    /// Passed through to each per-parent window; `None` takes `TopK`'s default.
    slack: Option<usize>,
    /// Filters applied to the child stream upstream of this node, repeated here
    /// so that per-parent refills do not resurrect rows the graph excluded.
    child_filter: Option<Predicate>,
    params: Params,

    parents: BTreeMap<RowKey, ParentState>,
}

impl Join1N {
    pub fn new(
        child_table: TableId,
        parent_key: ColId,
        child_fk: ColId,
        order: Vec<(ColId, Dir)>,
        limit: usize,
    ) -> Self {
        Join1N {
            child_table,
            parent_key,
            child_fk,
            order,
            limit,
            slack: None,
            child_filter: None,
            params: Params::empty(),
            parents: BTreeMap::new(),
        }
    }

    pub fn with_slack(mut self, slack: usize) -> Self {
        self.slack = Some(slack);
        self
    }

    /// Repeat the child-side filters here, for refill pushdown.
    ///
    /// The upstream `Filter` node is what actually excludes rows from the child
    /// *stream*; this is what excludes them from the child *scans*. Both are
    /// needed, and only one of them is visible in the dataflow.
    pub fn with_child_filter(mut self, pred: Predicate, params: Params) -> Self {
        self.child_filter = Some(pred);
        self.params = params;
        self
    }

    /// How many parents this operator is currently holding children for — the
    /// number that has to stay bounded for the memory story to hold.
    pub fn parents_held(&self) -> usize {
        self.parents.len()
    }

    /// `fk = <key> AND <child filter>`, the predicate a child scan runs under.
    fn pushdown(&self, key_value: &Value) -> Predicate {
        let fk_eq = Predicate::Cmp {
            lhs: Expr::Col(self.child_fk),
            op: CmpOp::Eq,
            rhs: Expr::Lit(key_value.clone()),
        };
        match &self.child_filter {
            None => fk_eq,
            Some(filter) => Predicate::and([fk_eq, filter.clone()]),
        }
    }

    /// Admit a parent: build its child window with one bounded read.
    ///
    /// `limit + slack + 1` rows, and the `+ 1` is doing real work. `TopK` only
    /// learns that rows exist below its window by discarding one, so a scan of
    /// exactly `limit + slack` would leave the window believing it holds the
    /// whole child relation and it would never refill. Asking for one row more
    /// than fits makes the truncation happen, and a short read still means what
    /// it should: there is nothing else down there.
    fn build_state(&self, row: Row, cx: &mut dyn OpCx) -> ParentState {
        let key_value = row.get(self.parent_key).clone();
        let pushdown = self.pushdown(&key_value);

        let mut children = TopK::new(self.child_table, self.order.clone(), self.limit);
        if let Some(slack) = self.slack {
            children = children.with_slack(slack);
        }
        children = children.with_pushdown(pushdown.clone(), self.params.clone());

        let want = self
            .limit
            .saturating_add(children.slack())
            .saturating_add(1);
        let rows = cx.scan(&ScanRequest {
            table: self.child_table,
            order: self.order.clone(),
            after: None,
            filter: Some(pushdown),
            params: self.params.clone(),
            limit: want,
        });
        cx.note_refill(RefillKind::Children, rows.len());

        let batch: Batch = rows
            .into_iter()
            .map(|(key, row)| Change::Insert { key, row })
            .collect();
        if !batch.is_empty() {
            children.apply(Inputs::single(&batch), cx);
        }

        ParentState {
            row,
            key_value,
            children,
        }
    }

    /// The rows this operator would emit for `keys`, as they stand right now.
    fn images(&self, keys: &BTreeSet<RowKey>) -> Relation {
        keys.iter()
            .filter_map(|key| {
                self.parents
                    .get(key)
                    .map(|state| (key.clone(), output_row(state)))
            })
            .collect()
    }
}

/// A parent widened by its children.
fn output_row(state: &ParentState) -> Row {
    state
        .row
        .with_appended(Value::rows(state.children.visible_rows()))
}

/// Does this child belong to the parent whose join key is `key_value`?
///
/// **SQL** equality, not structural: a store scan for `fk = 1` returns a child
/// whose fk is `Real(1.0)`, so a structural index here would route the scan's
/// rows and the stream's rows to different places — a divergence that shows up
/// as a comment appearing on hydration and vanishing on the next edit.
///
/// Linear over the held parents, which `TopK` below has already bounded to the
/// size of the view.
fn belongs(child: &Row, fk: ColId, key_value: &Value) -> bool {
    child.get(fk).sql_cmp(key_value) == Some(Ordering::Equal)
}

/// Would a move from `old` to `new` change which children match?
///
/// Structural equality first as the cheap path, then SQL equality, because
/// `Int(1)` and `Real(1.0)` select the same children — rebuilding for that
/// would be a scan to arrive back where we started.
fn same_join_key(old: &Value, new: &Value) -> bool {
    old == new || old.sql_cmp(new) == Some(Ordering::Equal)
}

impl Operator for Join1N {
    fn name(&self) -> &'static str {
        "Join1N"
    }

    fn ports(&self) -> usize {
        2
    }

    /// # Children before parents
    ///
    /// The order of steps 3 and 4 below is not arbitrary. A parent that builds
    /// a window in this pump scans the store, and the store is already
    /// committed (plan §1.4), so the scan *includes* this pump's child changes.
    /// Run step 4 first and a re-keyed parent would get its fresh window, then
    /// have the child stream applied on top of it — the same comment inserted
    /// twice. Children first, and each change is accounted for exactly once.
    ///
    /// The `rebuilt` skip in step 3 is a second line of the same defence, and
    /// on its own it is redundant: step 4 replaces a rebuilt parent's state
    /// wholesale, so whatever step 3 did to the old window is discarded either
    /// way. It is kept because it is not free to skip — applying the child
    /// stream to a doomed window can trigger a refill, which is a store scan
    /// per parent, spent on a result nobody reads.
    fn apply(&mut self, input: Inputs<'_>, cx: &mut dyn OpCx) -> Batch {
        let parent_delta = input.port(PARENT);
        let child_delta = input.port(CHILD);
        let fk = self.child_fk;

        // 1. Every parent this pump could possibly move: the ones the parent
        //    stream names, plus the ones a child change points at.
        let mut touched: BTreeSet<RowKey> = parent_delta.iter().map(|c| c.key().clone()).collect();
        for change in child_delta.iter() {
            for (key, state) in &self.parents {
                let hit = change
                    .before()
                    .is_some_and(|r| belongs(r, fk, &state.key_value))
                    || change
                        .after()
                        .is_some_and(|r| belongs(r, fk, &state.key_value));
                if hit {
                    touched.insert(key.clone());
                }
            }
        }

        // 2. Parents whose window this pump throws away and rebuilds: admitted,
        //    dropped, or re-keyed. Decided up front so step 3 can skip them.
        let mut rebuilt: BTreeSet<RowKey> = BTreeSet::new();
        for change in parent_delta.iter() {
            let key = change.key();
            let stale = match (change.after(), self.parents.get(key)) {
                // Dropped, or arriving for the first time.
                (None, _) | (Some(_), None) => true,
                (Some(after), Some(state)) => {
                    !same_join_key(&state.key_value, after.get(self.parent_key))
                }
            };
            if stale {
                rebuilt.insert(key.clone());
            }
        }

        // 3. Child changes, routed into the windows that hold them.
        let before = self.images(&touched);
        for (key, state) in self.parents.iter_mut() {
            if rebuilt.contains(key) || !touched.contains(key) {
                continue;
            }
            let mut batch = Batch::new();
            for change in child_delta.iter() {
                // Exactly `Filter(fk = this parent)`'s delta rule: a child that
                // moves between parents is a delete here and an insert there.
                let was = change
                    .before()
                    .filter(|r| belongs(r, fk, &state.key_value))
                    .cloned();
                let is = change
                    .after()
                    .filter(|r| belongs(r, fk, &state.key_value))
                    .cloned();
                if let Some(c) = Change::from_images(change.key().clone(), was, is) {
                    batch.push(c);
                }
            }
            if !batch.is_empty() {
                state.children.apply(Inputs::single(&batch), cx);
            }
        }

        // 4. Parent changes.
        for change in parent_delta.iter() {
            let key = change.key().clone();
            match change.after() {
                None => {
                    self.parents.remove(&key);
                }
                Some(after) => {
                    if rebuilt.contains(&key) {
                        let state = self.build_state(after.clone(), cx);
                        self.parents.insert(key, state);
                    } else if let Some(state) = self.parents.get_mut(&key) {
                        // Same join key, so the window still holds; only the
                        // parent's own columns moved.
                        state.row = after.clone();
                    }
                }
            }
        }

        // 5. Whatever actually changed, including the parents whose only change
        //    was to their child collection.
        before.diff(&self.images(&touched))
    }

    fn state_bytes(&self) -> usize {
        self.parents
            .iter()
            .map(|(key, state)| {
                std::mem::size_of::<ParentState>()
                    + key.heap_bytes()
                    + state.row.heap_bytes()
                    + state.key_value.heap_bytes()
                    + state.children.state_bytes()
            })
            .sum()
    }

    /// Shed what the child windows can shed; keep the parents.
    ///
    /// The parent rows and their visible children *are* the view, so dropping
    /// them would leave this operator unable to say how the view changed — it
    /// would re-emit rows the client already has. What each window can give back
    /// is its slack, at the cost of refilling more often, which is the trade
    /// degradation exists to make (plan §1.5).
    fn degrade(&mut self) -> Degrade {
        let mut freed = 0;
        for state in self.parents.values_mut() {
            if let Degrade::Shed { freed_bytes } = state.children.degrade() {
                freed += freed_bytes;
            }
        }
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
    use crate::reference::MemStore;

    const ISSUES: TableId = 1;
    const COMMENTS: TableId = 2;

    /// Parent rows are `[id, priority]`; the join appends children at column 2.
    fn issue(id: i64, priority: i64) -> (RowKey, Row) {
        (
            RowKey::from(id),
            Row::new(vec![Value::Int(id), Value::Int(priority)]),
        )
    }

    /// Child rows are `[id, issue_id, created_at]`.
    fn comment(id: i64, issue_id: i64, at: i64) -> (RowKey, Row) {
        (
            RowKey::from(id),
            Row::new(vec![Value::Int(id), Value::Int(issue_id), Value::Int(at)]),
        )
    }

    fn inserts(rows: Vec<(RowKey, Row)>) -> Batch {
        rows.into_iter()
            .map(|(key, row)| Change::Insert { key, row })
            .collect()
    }

    fn store(issues: Vec<(RowKey, Row)>, comments: Vec<(RowKey, Row)>) -> MemStore {
        let mut s = MemStore::new();
        s.load(ISSUES, issues.into_iter().collect());
        s.load(COMMENTS, comments.into_iter().collect());
        s
    }

    /// Newest comment first, two per issue.
    fn join() -> Join1N {
        Join1N::new(COMMENTS, 0, 1, vec![(2, Dir::Desc)], 2).with_slack(0)
    }

    fn pump(j: &mut Join1N, parents: Batch, children: Batch, s: &mut MemStore) -> Batch {
        let ports = [parents, children];
        j.apply(Inputs::new(&ports), s)
    }

    fn ids(rows: &[Row]) -> Vec<i64> {
        rows.iter()
            .map(|r| match r.get(0) {
                Value::Int(i) => *i,
                v => panic!("unexpected child id {v:?}"),
            })
            .collect()
    }

    /// The child ids attached to a parent in an emitted batch.
    fn emitted_children(batch: &Batch, parent: i64) -> Vec<i64> {
        let change = batch
            .get(&RowKey::from(parent))
            .unwrap_or_else(|| panic!("no change emitted for issue {parent}"));
        let row = change.after().expect("issue was removed, not updated");
        match row.get(2) {
            Value::Rows(rows) => ids(rows),
            v => panic!("expected a child collection at column 2, got {v:?}"),
        }
    }

    /// The child ids the operator is currently holding for a parent.
    fn held_children(j: &Join1N, parent: i64) -> Vec<i64> {
        ids(&j.parents[&RowKey::from(parent)].children.visible_rows())
    }

    #[test]
    fn hydration_attaches_each_parents_children() {
        let mut s = store(
            vec![issue(1, 10), issue(2, 20)],
            vec![
                comment(100, 1, 5),
                comment(101, 1, 9),
                comment(102, 1, 1),
                comment(200, 2, 3),
            ],
        );
        let mut j = join();

        let out = pump(
            &mut j,
            inserts(vec![issue(1, 10), issue(2, 20)]),
            Batch::new(),
            &mut s,
        );

        assert_eq!(out.len(), 2);
        assert_eq!(
            emitted_children(&out, 1),
            vec![101, 100],
            "newest first, limited to two"
        );
        assert_eq!(emitted_children(&out, 2), vec![200]);
    }

    #[test]
    fn a_parent_with_no_children_gets_an_empty_collection() {
        let mut s = store(vec![issue(1, 10)], vec![comment(200, 2, 3)]);
        let mut j = join();

        let out = pump(&mut j, inserts(vec![issue(1, 10)]), Batch::new(), &mut s);

        assert_eq!(emitted_children(&out, 1), Vec::<i64>::new());
    }

    #[test]
    fn a_new_child_updates_its_parent() {
        let mut s = store(vec![issue(1, 10)], vec![comment(100, 1, 5)]);
        let mut j = join();
        pump(&mut j, inserts(vec![issue(1, 10)]), Batch::new(), &mut s);

        let (k, r) = comment(101, 1, 9);
        s.table(COMMENTS).insert(k.clone(), r.clone());
        let out = pump(&mut j, Batch::new(), inserts(vec![(k, r)]), &mut s);

        assert_eq!(out.len(), 1);
        assert!(matches!(
            out.get(&RowKey::from(1)),
            Some(Change::Update { .. })
        ));
        assert_eq!(emitted_children(&out, 1), vec![101, 100]);
    }

    #[test]
    fn a_child_of_a_parent_we_do_not_hold_changes_nothing() {
        let mut s = store(vec![issue(1, 10)], vec![comment(100, 1, 5)]);
        let mut j = join();
        pump(&mut j, inserts(vec![issue(1, 10)]), Batch::new(), &mut s);

        let (k, r) = comment(200, 2, 9);
        s.table(COMMENTS).insert(k.clone(), r.clone());
        let out = pump(&mut j, Batch::new(), inserts(vec![(k, r)]), &mut s);

        assert!(out.is_empty(), "issue 2 is not in the view");
    }

    #[test]
    fn a_child_below_the_window_changes_nothing() {
        let mut s = store(
            vec![issue(1, 10)],
            vec![comment(100, 1, 5), comment(101, 1, 9)],
        );
        let mut j = join();
        pump(&mut j, inserts(vec![issue(1, 10)]), Batch::new(), &mut s);

        // Older than both held comments, and the window is already full.
        let (k, r) = comment(102, 1, 1);
        s.table(COMMENTS).insert(k.clone(), r.clone());
        let out = pump(&mut j, Batch::new(), inserts(vec![(k, r)]), &mut s);

        assert!(out.is_empty());
        assert_eq!(held_children(&j, 1), vec![101, 100]);
    }

    #[test]
    fn deleting_a_visible_child_refills_from_the_store() {
        let mut s = store(
            vec![issue(1, 10)],
            vec![comment(100, 1, 5), comment(101, 1, 9), comment(102, 1, 1)],
        );
        let mut j = join();
        pump(&mut j, inserts(vec![issue(1, 10)]), Batch::new(), &mut s);
        assert_eq!(held_children(&j, 1), vec![101, 100]);

        let (k, r) = comment(101, 1, 9);
        s.table(COMMENTS).remove(&k);
        let out = pump(
            &mut j,
            Batch::new(),
            [Change::Delete { key: k, before: r }].into_iter().collect(),
            &mut s,
        );

        assert_eq!(
            emitted_children(&out, 1),
            vec![100, 102],
            "the window refilled from below its boundary"
        );
    }

    #[test]
    fn a_child_moving_between_parents_updates_both() {
        let mut s = store(vec![issue(1, 10), issue(2, 20)], vec![comment(100, 1, 5)]);
        let mut j = join();
        pump(
            &mut j,
            inserts(vec![issue(1, 10), issue(2, 20)]),
            Batch::new(),
            &mut s,
        );

        let (k, moved) = comment(100, 2, 5);
        s.table(COMMENTS).insert(k.clone(), moved.clone());
        let out = pump(
            &mut j,
            Batch::new(),
            [Change::Update {
                key: k,
                before: comment(100, 1, 5).1,
                after: moved,
            }]
            .into_iter()
            .collect(),
            &mut s,
        );

        assert_eq!(out.len(), 2, "it left one parent and joined another");
        assert_eq!(emitted_children(&out, 1), Vec::<i64>::new());
        assert_eq!(emitted_children(&out, 2), vec![100]);
    }

    #[test]
    fn re_keying_a_parent_rebuilds_its_children() {
        let mut s = store(
            vec![issue(1, 10)],
            vec![comment(100, 1, 5), comment(200, 2, 7)],
        );
        let mut j = join();
        pump(&mut j, inserts(vec![issue(1, 10)]), Batch::new(), &mut s);
        assert_eq!(held_children(&j, 1), vec![100]);

        // The row keeps its primary key but now points at a different parent
        // key, so every child it had is the wrong one.
        let rekeyed = Row::new(vec![Value::Int(2), Value::Int(10)]);
        let out = pump(
            &mut j,
            [Change::Update {
                key: RowKey::from(1),
                before: issue(1, 10).1,
                after: rekeyed,
            }]
            .into_iter()
            .collect(),
            Batch::new(),
            &mut s,
        );

        assert_eq!(emitted_children(&out, 1), vec![200]);
    }

    #[test]
    fn an_update_that_leaves_the_join_key_alone_keeps_the_window() {
        let mut s = store(vec![issue(1, 10)], vec![comment(100, 1, 5)]);
        let mut j = join();
        pump(&mut j, inserts(vec![issue(1, 10)]), Batch::new(), &mut s);
        let scans_after_hydration = s.refills;

        let out = pump(
            &mut j,
            [Change::Update {
                key: RowKey::from(1),
                before: issue(1, 10).1,
                after: issue(1, 99).1,
            }]
            .into_iter()
            .collect(),
            Batch::new(),
            &mut s,
        );

        assert_eq!(
            s.refills, scans_after_hydration,
            "changing a parent's own columns must not rescan its children"
        );
        assert_eq!(emitted_children(&out, 1), vec![100]);
    }

    #[test]
    fn a_join_key_that_only_differs_in_storage_class_is_not_a_re_key() {
        let mut s = store(vec![issue(1, 10)], vec![comment(100, 1, 5)]);
        let mut j = join();
        pump(&mut j, inserts(vec![issue(1, 10)]), Batch::new(), &mut s);
        let scans_after_hydration = s.refills;

        // `Int(1)` and `Real(1.0)` select exactly the same children.
        let coerced = Row::new(vec![Value::Real(1.0), Value::Int(10)]);
        pump(
            &mut j,
            [Change::Update {
                key: RowKey::from(1),
                before: issue(1, 10).1,
                after: coerced,
            }]
            .into_iter()
            .collect(),
            Batch::new(),
            &mut s,
        );

        assert_eq!(s.refills, scans_after_hydration);
        assert_eq!(held_children(&j, 1), vec![100]);
    }

    /// A child whose foreign key is stored as a float still belongs to the
    /// parent, because that is what the store's own `fk = 1` scan returns.
    #[test]
    fn routing_children_uses_sql_equality() {
        let coerced = (
            RowKey::from(101),
            Row::new(vec![Value::Int(101), Value::Real(1.0), Value::Int(9)]),
        );
        let mut s = store(vec![issue(1, 10)], vec![comment(100, 1, 5)]);
        let mut j = join();
        pump(&mut j, inserts(vec![issue(1, 10)]), Batch::new(), &mut s);

        s.table(COMMENTS)
            .insert(coerced.0.clone(), coerced.1.clone());
        let out = pump(&mut j, Batch::new(), inserts(vec![coerced]), &mut s);

        assert_eq!(emitted_children(&out, 1), vec![101, 100]);
    }

    #[test]
    fn deleting_a_parent_drops_it_and_its_children() {
        let mut s = store(vec![issue(1, 10)], vec![comment(100, 1, 5)]);
        let mut j = join();
        pump(&mut j, inserts(vec![issue(1, 10)]), Batch::new(), &mut s);
        assert!(j.state_bytes() > 0);

        let out = pump(
            &mut j,
            [Change::Delete {
                key: RowKey::from(1),
                before: issue(1, 10).1,
            }]
            .into_iter()
            .collect(),
            Batch::new(),
            &mut s,
        );

        assert!(matches!(
            out.get(&RowKey::from(1)),
            Some(Change::Delete { .. })
        ));
        assert_eq!(j.parents_held(), 0);
        assert_eq!(j.state_bytes(), 0);
    }

    /// A parent and one of its children, dropped in the same transaction. The
    /// parent's removal must not be preceded by an update announcing that it
    /// lost a comment — the view never held that intermediate state.
    #[test]
    fn a_parent_and_its_child_deleted_together_emit_one_delete() {
        let mut s = store(vec![issue(1, 10)], vec![comment(100, 1, 5)]);
        let mut j = join();
        pump(&mut j, inserts(vec![issue(1, 10)]), Batch::new(), &mut s);

        s.table(ISSUES).remove(&RowKey::from(1));
        s.table(COMMENTS).remove(&RowKey::from(100));
        let out = pump(
            &mut j,
            [Change::Delete {
                key: RowKey::from(1),
                before: issue(1, 10).1,
            }]
            .into_iter()
            .collect(),
            [Change::Delete {
                key: RowKey::from(100),
                before: comment(100, 1, 5).1,
            }]
            .into_iter()
            .collect(),
            &mut s,
        );

        assert_eq!(out.len(), 1);
        assert!(matches!(
            out.get(&RowKey::from(1)),
            Some(Change::Delete { .. })
        ));
    }

    /// A parent arriving in the same pump as one of its children. The parent's
    /// hydration scan already sees the child, so routing the child stream into
    /// the fresh window too would double-count it.
    #[test]
    fn a_parent_and_its_child_inserted_together_count_the_child_once() {
        let mut s = store(vec![issue(1, 10)], vec![comment(100, 1, 5)]);
        let mut j = join();

        let out = pump(
            &mut j,
            inserts(vec![issue(1, 10)]),
            inserts(vec![comment(100, 1, 5)]),
            &mut s,
        );

        assert_eq!(emitted_children(&out, 1), vec![100]);
    }

    #[test]
    fn the_child_filter_is_pushed_into_refills() {
        // Only comments with `created_at >= 5` are part of this view.
        let visible = Predicate::Cmp {
            lhs: Expr::Col(2),
            op: CmpOp::Ge,
            rhs: Expr::Lit(Value::Int(5)),
        };
        let mut s = store(
            vec![issue(1, 10)],
            vec![
                comment(100, 1, 5),
                comment(101, 1, 9),
                comment(102, 1, 7),
                comment(103, 1, 1),
            ],
        );
        let mut j = Join1N::new(COMMENTS, 0, 1, vec![(2, Dir::Desc)], 2)
            .with_slack(0)
            .with_child_filter(visible, Params::empty());

        pump(&mut j, inserts(vec![issue(1, 10)]), Batch::new(), &mut s);
        assert_eq!(held_children(&j, 1), vec![101, 102]);

        s.table(COMMENTS).remove(&RowKey::from(101));
        let out = pump(
            &mut j,
            Batch::new(),
            [Change::Delete {
                key: RowKey::from(101),
                before: comment(101, 1, 9).1,
            }]
            .into_iter()
            .collect(),
            &mut s,
        );

        assert_eq!(
            emitted_children(&out, 1),
            vec![102, 100],
            "the refill must not surface the comment the filter excludes"
        );
    }

    #[test]
    fn an_idle_pump_produces_nothing() {
        let mut s = store(vec![issue(1, 10)], vec![comment(100, 1, 5)]);
        let mut j = join();
        pump(&mut j, inserts(vec![issue(1, 10)]), Batch::new(), &mut s);

        assert!(pump(&mut j, Batch::new(), Batch::new(), &mut s).is_empty());
    }

    #[test]
    fn state_stays_proportional_to_the_parents_held() {
        // The memory story: children are held for parents in the view, not for
        // rows in the child table.
        let comments: Vec<(RowKey, Row)> =
            (0..200).map(|i| comment(1000 + i, 1 + i % 4, i)).collect();
        let mut s = store((1..=4).map(|i| issue(i, i * 10)).collect(), comments);
        let mut j = join();
        pump(
            &mut j,
            inserts((1..=4).map(|i| issue(i, i * 10)).collect()),
            Batch::new(),
            &mut s,
        );

        assert_eq!(j.parents_held(), 4);
        assert!(
            j.state_bytes() < 4 * 1024,
            "four parents with two comments each should not cost {} bytes",
            j.state_bytes()
        );
    }

    #[test]
    fn degrade_sheds_child_slack_but_keeps_the_view() {
        let mut s = store(
            vec![issue(1, 10)],
            vec![
                comment(100, 1, 5),
                comment(101, 1, 9),
                comment(102, 1, 7),
                comment(103, 1, 1),
            ],
        );
        let mut j = Join1N::new(COMMENTS, 0, 1, vec![(2, Dir::Desc)], 2).with_slack(2);
        pump(&mut j, inserts(vec![issue(1, 10)]), Batch::new(), &mut s);
        let before = j.state_bytes();

        let result = j.degrade();

        assert!(matches!(result, Degrade::Shed { .. }));
        assert!(j.state_bytes() < before);
        assert_eq!(
            held_children(&j, 1),
            vec![101, 102],
            "the view itself must survive degradation"
        );
    }
}
