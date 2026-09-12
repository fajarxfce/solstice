//! The delta law, checked by construction.
//!
//! ```text
//! ∀ plan P, tables R, delta Δ:   P(R ⊎ Δ)  ==  P(R) ⊎ P.apply(Δ)
//! ```
//!
//! Recomputing from scratch and maintaining incrementally must agree. Plan §6
//! calls this the highest-value test in the project, and the reason is that it
//! does not test a behaviour — it tests the definition of correctness for the
//! entire crate. Every operator added from here on gets folded into [`Stage`]
//! and is immediately covered.
//!
//! Two generator choices matter more than they look:
//!
//! * **The value domain is tiny** — integers in `-4..5`, three-letter strings,
//!   a handful of floats, and `NULL` sprinkled throughout. Large random values
//!   almost never collide, and collisions are where the bugs are: `Int(1)` vs
//!   `Real(1.0)`, a predicate flipping to unknown, a projection erasing the only
//!   column that changed. For a join it is also what makes a foreign key
//!   actually *match* a parent often enough to be worth generating.
//! * **Deltas are generated as *edits*, then resolved against the relation.**
//!   Generating `Change`s directly would produce ill-formed batches — an insert
//!   over a live key, a delete carrying the wrong before-image — and the law
//!   simply does not hold for those. Resolving edits makes every batch
//!   well-formed by construction, so a failure is always a real one.
//!
//! # `TopK` and the store
//!
//! `TopK` is the first operator with state, and the first that reads the store
//! (plan §1.3). That makes the store part of the law: a refill must see the
//! relation *after* the batch it is refilling for, because the engine commits
//! before it pumps the graph (plan §1.4). Every test here therefore builds a
//! [`MemStore`] over the post-batch tables, and a `TopK` is given the
//! conjunction of the filters above it as refill pushdown.
//!
//! The parameters are tuned to make refills the common case rather than a rare
//! one: `k` of 1..=4 and slack of 0..=2 over at most 12 rows means almost every
//! delete of a visible row has to go back to the store.
//!
//! # Two tables, one pump
//!
//! Every generated transaction edits *both* tables and is delivered as a single
//! [`Graph::pump`], because that is the case the engine actually has to get
//! right (plan §1.4) and the one where a join can most easily double-count: a
//! parent admitted in the same transaction as one of its children hydrates its
//! window from a store that already contains that child.

use proptest::prelude::*;
use solstice_ivm::delta::Change;
use solstice_ivm::reference::{JoinSpec, MemStore, Reference, Stage, Tables};
use solstice_ivm::{Batch, ColId, Dir, Graph, Params, Relation, Row, RowKey, TableId, Value};
use solstice_ivm::{CmpOp, Expr, Predicate};
use std::sync::Arc;

/// Width of the generated base relations.
const ARITY: usize = 4;
/// Key space. Small, so that inserts, updates and deletes actually collide.
const KEYS: i64 = 8;
/// Key space for the memory-bound property, where the tables have to be much
/// larger than the view for the bound to mean anything. See
/// `join_state_stays_proportional_to_the_view`.
const WIDE_KEYS: i64 = 80;
/// The root table every generated plan reads.
const PARENTS: TableId = 1;
/// The table a generated `Join` traverses into.
const CHILDREN: TableId = 2;
/// Primary key column for a source's hydration scan. Arbitrary — generated
/// keys are independent of row contents — but it has to be *some* column, and
/// scanning in a defined order is what keeps hydration deterministic (plan §6).
const PK: ColId = 0;

fn arb_value() -> impl Strategy<Value = Value> {
    prop_oneof![
        1 => Just(Value::Null),
        5 => (-4i64..5).prop_map(Value::Int),
        2 => prop_oneof![
            Just(0.0f64), Just(1.0), Just(1.5), Just(-2.0), Just(3.0)
        ].prop_map(Value::Real),
        3 => "[a-c]{0,2}".prop_map(Value::text),
    ]
}

fn arb_row() -> impl Strategy<Value = Row> {
    prop::collection::vec(arb_value(), ARITY).prop_map(Row::new)
}

fn arb_expr() -> impl Strategy<Value = Expr> {
    prop_oneof![
        4 => (0..ARITY as ColId).prop_map(Expr::Col),
        3 => arb_value().prop_map(Expr::Lit),
        1 => (0..2u16).prop_map(Expr::Param),
    ]
}

fn arb_cmp_op() -> impl Strategy<Value = CmpOp> {
    prop_oneof![
        Just(CmpOp::Eq),
        Just(CmpOp::Ne),
        Just(CmpOp::Lt),
        Just(CmpOp::Le),
        Just(CmpOp::Gt),
        Just(CmpOp::Ge),
    ]
}

fn arb_predicate() -> impl Strategy<Value = Predicate> {
    let leaf = prop_oneof![
        1 => Just(Predicate::True),
        6 => (arb_expr(), arb_cmp_op(), arb_expr())
            .prop_map(|(lhs, op, rhs)| Predicate::Cmp { lhs, op, rhs }),
        2 => (arb_expr(), prop::collection::vec(arb_value(), 0..3))
            .prop_map(|(lhs, list)| Predicate::In { lhs, list }),
        2 => (arb_expr(), arb_expr(), arb_expr())
            .prop_map(|(lhs, low, high)| Predicate::Between { lhs, low, high }),
        1 => arb_expr().prop_map(Predicate::IsNull),
        1 => arb_expr().prop_map(Predicate::IsNotNull),
        1 => (arb_expr(), "[a-c]{0,2}").prop_map(|(lhs, p)| Predicate::LikePrefix {
            lhs,
            prefix: Arc::from(p.as_str()),
        }),
    ];

    leaf.prop_recursive(3, 16, 3, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..3).prop_map(Predicate::And),
            prop::collection::vec(inner.clone(), 0..3).prop_map(Predicate::Or),
            inner.prop_map(Predicate::negate),
        ]
    })
}

fn arb_params() -> impl Strategy<Value = Params> {
    prop::collection::vec(arb_value(), 2).prop_map(Params::new)
}

fn arb_filter() -> impl Strategy<Value = Stage> {
    arb_predicate().prop_map(|pred| Stage::Filter { pred })
}

/// Projections may repeat, reorder, and drop columns — including projecting
/// away a column a later filter reads, which is legal and must simply read as
/// NULL rather than panic.
fn arb_project() -> impl Strategy<Value = Stage> {
    prop::collection::vec(0..ARITY as ColId, 1..=ARITY).prop_map(|cols| Stage::Project { cols })
}

fn arb_order() -> impl Strategy<Value = Vec<(ColId, Dir)>> {
    prop::collection::vec(
        (
            0..ARITY as ColId,
            prop_oneof![Just(Dir::Asc), Just(Dir::Desc)],
        ),
        1..=2,
    )
}

fn arb_topk() -> impl Strategy<Value = Stage> {
    (arb_order(), 1usize..=4, 0usize..=2).prop_map(|(order, k, slack)| Stage::TopK {
        order,
        k,
        slack,
    })
}

/// A 1:N traversal on an arbitrary pair of columns.
///
/// The join columns are drawn freely rather than being fixed to a plausible
/// "id / parent_id" pair, because the interesting cases are the degenerate ones:
/// a foreign key that is NULL, one that matches every parent, one that matches
/// none, and one where `Int(1)` on one side meets `Real(1.0)` on the other.
fn arb_join() -> impl Strategy<Value = Stage> {
    (
        prop::collection::vec(arb_predicate(), 0..2),
        arb_order(),
        1usize..=3,
        0usize..=2,
        0..ARITY as ColId,
        0..ARITY as ColId,
    )
        .prop_map(
            |(child_filters, order, limit, slack, parent_key, child_fk)| {
                Stage::Join(JoinSpec {
                    child_table: CHILDREN,
                    child_pk: PK,
                    parent_key,
                    child_fk,
                    child_filters,
                    order,
                    limit,
                    slack,
                })
            },
        )
}

/// Plans have the shape a real optimiser emits: **filters pushed all the way
/// down, `TopK` next, projection, then the join**.
///
/// This is not just convenience. `ORDER BY` in DQL is restricted to indexed
/// columns of the root table (plan §1.1), so a `TopK` always *can* sit directly
/// above the filters — and putting it there is what lets its refill be a single
/// bounded, filtered scan of one table. A projection below it could rename or
/// drop the very columns the refill sorts on.
///
/// The join goes last, and has to: its output is hierarchical, and a `TopK`
/// above it would refill by scanning the parent table and produce rows with no
/// children attached. Putting it last is not a limitation of the test — it is
/// the placement that makes the join's state bounded at all.
fn arb_plan() -> impl Strategy<Value = Reference> {
    (
        prop::collection::vec(arb_filter(), 0..3),
        prop::option::of(arb_topk()),
        prop::collection::vec(arb_project(), 0..2),
        prop::option::of(arb_join()),
        arb_params(),
    )
        .prop_map(|(filters, topk, projects, join, params)| {
            let stages = filters
                .into_iter()
                .chain(topk)
                .chain(projects)
                .chain(join)
                .collect::<Vec<_>>();
            Reference::new(PARENTS, PK, stages).with_params(params)
        })
}

fn arb_relation_over(keys: i64, len: usize) -> impl Strategy<Value = Relation> {
    prop::collection::vec((0..keys, arb_row()), 0..len).prop_map(|entries| {
        entries
            .into_iter()
            .map(|(k, row)| (RowKey::from(k), row))
            .collect()
    })
}

fn arb_tables_over(keys: i64, len: usize) -> impl Strategy<Value = Tables> {
    (arb_relation_over(keys, len), arb_relation_over(keys, len))
        .prop_map(|(p, c)| Tables::new().with(PARENTS, p).with(CHILDREN, c))
}

fn arb_relation() -> impl Strategy<Value = Relation> {
    arb_relation_over(KEYS, 12)
}

fn arb_tables() -> impl Strategy<Value = Tables> {
    arb_tables_over(KEYS, 12)
}

/// An intent to change a relation, resolved into a well-formed [`Change`]
/// only once the relation's actual contents are known.
#[derive(Debug, Clone)]
enum Edit {
    Set(RowKey, Row),
    Remove(RowKey),
}

fn arb_edits_over(keys: i64) -> impl Strategy<Value = Vec<Edit>> {
    prop::collection::vec(
        prop_oneof![
            3 => (0..keys, arb_row()).prop_map(|(k, r)| Edit::Set(RowKey::from(k), r)),
            2 => (0..keys).prop_map(|k| Edit::Remove(RowKey::from(k))),
        ],
        0..8,
    )
}

/// One transaction: edits to both tables, delivered in a single pump.
#[derive(Debug, Clone)]
struct Txn {
    parents: Vec<Edit>,
    children: Vec<Edit>,
}

fn arb_txn_over(keys: i64) -> impl Strategy<Value = Txn> {
    (arb_edits_over(keys), arb_edits_over(keys))
        .prop_map(|(parents, children)| Txn { parents, children })
}

fn arb_txn() -> impl Strategy<Value = Txn> {
    arb_txn_over(KEYS)
}

/// Turn edits into a batch that is well-formed against `rel`.
fn resolve_one(rel: &Relation, edits: &[Edit]) -> Batch {
    let mut shadow = rel.clone();
    let mut batch = Batch::new();
    for edit in edits {
        match edit {
            Edit::Set(key, row) => {
                let before = shadow.get(key).cloned();
                shadow.insert(key.clone(), row.clone());
                if let Some(change) = Change::from_images(key.clone(), before, Some(row.clone())) {
                    batch.push(change);
                }
            }
            Edit::Remove(key) => {
                if let Some(before) = shadow.remove(key) {
                    batch.push(Change::Delete {
                        key: key.clone(),
                        before,
                    });
                }
            }
        }
    }
    batch
}

fn resolve(tables: &Tables, txn: &Txn) -> Vec<(TableId, Batch)> {
    vec![
        (PARENTS, resolve_one(&tables.get(PARENTS), &txn.parents)),
        (CHILDREN, resolve_one(&tables.get(CHILDREN), &txn.children)),
    ]
}

/// A graph brought up to date with `base`, exactly as the engine does it:
/// hydrate against a store holding `base`, then leave it ready for deltas.
///
/// Hydrating through `apply` rather than seeding state directly is the point —
/// if the two ever diverged, a view would be right on first load and wrong
/// after an edit (or the reverse), which is the bug class this file exists to
/// rule out.
fn hydrated(plan: &Reference, base: &Tables) -> Graph {
    let mut graph = plan.build();
    graph.hydrate(&mut MemStore::over(base));
    graph
}

/// One coherent pump of everything a transaction touched.
fn pump(graph: &mut Graph, deltas: &[(TableId, Batch)], after: &Tables) -> Batch {
    graph.pump(deltas, &mut MemStore::over(after))
}

/// 2048 cases per property per commit, overridable for the nightly sweep.
///
/// `ProptestConfig::default()` already reads `PROPTEST_CASES`, so setting
/// `cases` unconditionally would silently *ignore* the environment — a run
/// asking for a million cases would quietly do two thousand and report
/// success, which is worse than not having the knob at all. Plan §6 wants
/// thousands per commit and millions overnight; this is what makes the second
/// half of that sentence reachable.
fn config() -> ProptestConfig {
    let mut cfg = ProptestConfig::default();
    if std::env::var_os("PROPTEST_CASES").is_none() {
        cfg.cases = 2048;
    }
    cfg
}

proptest! {
    #![proptest_config(config())]

    /// The law itself.
    #[test]
    fn incremental_agrees_with_recomputation(
        plan in arb_plan(),
        base in arb_tables(),
        txn in arb_txn(),
    ) {
        let deltas = resolve(&base, &txn);
        let after = base.with_applied(&deltas);

        // Left: recompute over the updated tables.
        let expected = plan.eval(&after);

        // Right: maintain the previous result incrementally. The store holds
        // the post-batch tables because the engine commits before it pumps.
        let mut graph = hydrated(&plan, &base);
        let out = pump(&mut graph, &deltas, &after);
        let actual = plan.eval(&base).with_applied(&out);

        prop_assert_eq!(expected, actual);
    }

    /// Hydration is the same code path as any other delta.
    ///
    /// If this can fail while the law above holds, hydration has drifted from
    /// the incremental path — the bug class that made `hydrate` a batch of
    /// inserts in the first place.
    #[test]
    fn hydration_agrees_with_recomputation(
        plan in arb_plan(),
        base in arb_tables(),
    ) {
        let mut graph = plan.build();
        let out = graph.hydrate(&mut MemStore::over(&base));
        prop_assert_eq!(plan.eval(&base), Relation::new().with_applied(&out));
    }

    /// A long run of deltas must not drift.
    ///
    /// One step can be correct while the window's bookkeeping rots over many —
    /// slack quietly filling with rows that no longer exist, `full` latching
    /// true after a truncation, a cursor built from a row that was already
    /// evicted, a join holding a child window for a parent it no longer has.
    /// Only repetition finds those.
    #[test]
    fn a_run_of_deltas_never_drifts(
        plan in arb_plan(),
        base in arb_tables(),
        runs in prop::collection::vec(arb_txn(), 1..6),
    ) {
        let mut graph = hydrated(&plan, &base);
        let mut tables = base.clone();
        let mut view = plan.eval(&base);

        for txn in &runs {
            let deltas = resolve(&tables, txn);
            tables = tables.with_applied(&deltas);
            let out = pump(&mut graph, &deltas, &tables);
            view.apply(&out);
            prop_assert_eq!(&view, &plan.eval(&tables));
        }
    }

    /// Coalescing under backpressure must not change the result.
    ///
    /// The FFI layer collapses a backlog of deltas into one rather than
    /// dropping events (plan §4.2). That is only sound if composing the inputs
    /// and composing the outputs agree — which is what this checks.
    #[test]
    fn composing_deltas_agrees_with_applying_them_in_sequence(
        plan in arb_plan(),
        base in arb_tables(),
        first in arb_txn(),
        second in arb_txn(),
    ) {
        let d1 = resolve(&base, &first);
        let mid = base.with_applied(&d1);
        let d2 = resolve(&mid, &second);
        let end = mid.with_applied(&d2);

        let mut stepwise = hydrated(&plan, &base);
        let first_out = pump(&mut stepwise, &d1, &mid);
        let out_stepwise = first_out.compose(pump(&mut stepwise, &d2, &end));

        let merged: Vec<(TableId, Batch)> = d1
            .iter()
            .zip(d2.iter())
            .map(|((t, a), (_, b))| (*t, a.clone().compose(b.clone())))
            .collect();
        let mut coalesced = hydrated(&plan, &base);
        let out_coalesced = pump(&mut coalesced, &merged, &end);

        let start = plan.eval(&base);
        prop_assert_eq!(
            start.with_applied(&out_stepwise),
            start.with_applied(&out_coalesced)
        );
    }

    /// `diff` is the inverse of `apply`, which is what lets a degraded
    /// `Requery` node emit exactly what the incremental node would have
    /// (plan §1.5).
    #[test]
    fn diff_reconstructs_the_target_relation(
        a in arb_relation(),
        b in arb_relation(),
    ) {
        prop_assert_eq!(a.with_applied(&a.diff(&b)), b);
    }

    /// The output never contains a change that changes nothing.
    ///
    /// A no-op update would reach the UI as a rebuild of a row whose contents
    /// are identical — invisible in a test, expensive in a long list. For a
    /// join it is also the difference between "this issue got a comment" and
    /// "this issue was touched", which is what an `AnimatedList` reacts to.
    #[test]
    fn output_carries_no_noop_changes(
        plan in arb_plan(),
        base in arb_tables(),
        txn in arb_txn(),
    ) {
        let deltas = resolve(&base, &txn);
        let after = base.with_applied(&deltas);
        let mut graph = hydrated(&plan, &base);
        let out = pump(&mut graph, &deltas, &after);
        for change in out.iter() {
            prop_assert_ne!(change.before(), change.after());
        }
    }

    /// `TopK` never holds more than `k + slack` rows, whatever the relation
    /// does to it.
    ///
    /// This is the memory bound the whole design rests on (plan §7): state
    /// proportional to the *view*, never to the table. A `TopK` that quietly
    /// accumulated rows would pass every correctness test above and still sink
    /// the project on a real device.
    #[test]
    fn state_stays_proportional_to_the_view(
        order in arb_order(),
        k in 1usize..=4,
        slack in 0usize..=2,
        base in arb_tables(),
        runs in prop::collection::vec(arb_txn(), 1..4),
    ) {
        let plan = Reference::new(PARENTS, PK, vec![Stage::TopK { order, k, slack }]);
        let mut graph = hydrated(&plan, &base);

        let row_bytes = ARITY * std::mem::size_of::<Value>();
        let budget = (k + slack) * (row_bytes + 128);

        let mut tables = base;
        for txn in &runs {
            let deltas = resolve(&tables, txn);
            tables = tables.with_applied(&deltas);
            pump(&mut graph, &deltas, &tables);
            prop_assert!(
                graph.state_bytes() <= budget,
                "TopK held {} bytes for k={} slack={} over {} rows",
                graph.state_bytes(), k, slack, tables.get(PARENTS).len()
            );
        }
    }

    /// The join's state is bounded by the *view*, not by the child table.
    ///
    /// This is plan §7's named failure mode checked head-on. `TopK` below the
    /// join caps parents at `k + slack`; each parent's window caps children at
    /// `limit + slack`. The product is the bound. If either level leaked — a
    /// window kept for a parent that left the view, a child window that grew
    /// past its limit — the number here would track the tables instead.
    ///
    /// Two things make this bite where a random workload does not.
    ///
    /// It runs over a **wide** key space, so that a table-sized state and a
    /// view-sized one are an order of magnitude apart rather than a factor of
    /// two — at eight keys, a join leaking every parent it ever saw still looks
    /// almost the same as one holding four.
    ///
    /// And the parent workload is **adversarial rather than random**: every
    /// step deletes exactly the rows currently on screen. This is the workload
    /// plan §7 names as the mitigation to spike early, and §5.1 turns into a
    /// kill criterion. It matters here because it is the only way parents churn
    /// through the window fast enough for a leak to accumulate — a random
    /// delete over eighty keys hits a visible parent perhaps one time in
    /// fifteen, which hides a leak inside the budget's rounding. Deleting the
    /// top forces `TopK` to refill on every step and the join to build and drop
    /// a child window on every step with it.
    #[test]
    fn join_state_stays_proportional_to_the_view(
        order in arb_order(),
        child_order in arb_order(),
        k in 1usize..=3,
        limit in 1usize..=3,
        parent_key in 0..ARITY as ColId,
        child_fk in 0..ARITY as ColId,
        base in arb_tables_over(WIDE_KEYS, 60),
        child_churn in prop::collection::vec(arb_edits_over(WIDE_KEYS), 8),
    ) {
        let plan = Reference::new(PARENTS, PK, vec![
            Stage::TopK { order, k, slack: 1 },
            Stage::Join(JoinSpec {
                child_table: CHILDREN,
                child_pk: PK,
                parent_key,
                child_fk,
                child_filters: Vec::new(),
                order: child_order,
                limit,
                slack: 1,
            }),
        ]);
        let mut graph = hydrated(&plan, &base);

        let row_bytes = ARITY * std::mem::size_of::<Value>();
        let held_parents = k + 1;
        let held_children = limit + 1;
        // Per parent: the `TopK` entry below the join, the parent row the join
        // keeps, and one bounded child window.
        let per_parent =
            2 * (row_bytes + 128) + held_children * (row_bytes + 128) + JOIN_PARENT_OVERHEAD;
        let budget = held_parents * per_parent;

        let mut tables = base.clone();
        let mut view = plan.eval(&base);

        for children in &child_churn {
            let txn = Txn {
                parents: view.iter().map(|(key, _)| Edit::Remove(key.clone())).collect(),
                children: children.clone(),
            };
            let deltas = resolve(&tables, &txn);
            tables = tables.with_applied(&deltas);
            let out = pump(&mut graph, &deltas, &tables);
            view.apply(&out);

            prop_assert!(
                graph.state_bytes() <= budget,
                "held {} bytes (budget {}) for k={} limit={} over {}+{} rows",
                graph.state_bytes(), budget, k, limit,
                tables.get(PARENTS).len(), tables.get(CHILDREN).len()
            );
            // Free, and a distribution nothing else generates: the view is
            // never allowed to be wrong while we are busy checking it is small.
            prop_assert_eq!(&view, &plan.eval(&tables));
        }
    }
}

/// Fixed bookkeeping the join keeps per parent: the nested `TopK` struct, the
/// cached join key, the pushdown predicate. Constant, which is the only thing
/// the budget above needs it to be.
const JOIN_PARENT_OVERHEAD: usize = 512;
