//! The delta law, checked by construction.
//!
//! ```text
//! ∀ plan P, relation R, delta Δ:   P(R ⊎ Δ)  ==  P(R) ⊎ P.apply(Δ)
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
//!   column that changed.
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
//! before it pumps the graph (plan §1.4). Every test here therefore loads a
//! [`MemStore`] with the post-batch relation, and a `TopK` is given the
//! conjunction of the filters above it as refill pushdown.
//!
//! The parameters are tuned to make refills the common case rather than a rare
//! one: `k` of 1..=4 and slack of 0..=2 over at most 12 rows means almost every
//! delete of a visible row has to go back to the store.

use proptest::prelude::*;
use solstice_ivm::delta::Change;
use solstice_ivm::reference::{MemStore, Reference, Stage};
use solstice_ivm::{Batch, ColId, Dir, Graph, Params, Relation, Row, RowKey, Value};
use solstice_ivm::{CmpOp, Expr, Predicate};
use std::sync::Arc;

/// Width of the generated base relation.
const ARITY: usize = 4;
/// Key space. Small, so that inserts, updates and deletes actually collide.
const KEYS: i64 = 8;
/// The one table every generated plan reads.
const TABLE: solstice_ivm::TableId = 1;
/// Primary key column for the source's hydration scan. Arbitrary — generated
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

/// Plans have the shape a real optimiser emits: **filters pushed all the way
/// down, `TopK` next, projection last**.
///
/// This is not just convenience. `ORDER BY` in DQL is restricted to indexed
/// columns of the root table (plan §1.1), so a `TopK` always *can* sit directly
/// above the filters — and putting it there is what lets its refill be a single
/// bounded, filtered scan of one table. A projection below it could rename or
/// drop the very columns the refill sorts on.
fn arb_plan() -> impl Strategy<Value = Reference> {
    (
        prop::collection::vec(arb_filter(), 0..3),
        prop::option::of(arb_topk()),
        prop::collection::vec(arb_project(), 0..2),
        arb_params(),
    )
        .prop_map(|(filters, topk, projects, params)| {
            let stages = filters
                .into_iter()
                .chain(topk)
                .chain(projects)
                .collect::<Vec<_>>();
            Reference::new(TABLE, PK, stages).with_params(params)
        })
}

fn arb_relation() -> impl Strategy<Value = Relation> {
    prop::collection::vec((0..KEYS, arb_row()), 0..12).prop_map(|entries| {
        entries
            .into_iter()
            .map(|(k, row)| (RowKey::from(k), row))
            .collect()
    })
}

/// An intent to change the relation, resolved into a well-formed [`Change`]
/// only once the relation's actual contents are known.
#[derive(Debug, Clone)]
enum Edit {
    Set(i64, Row),
    Remove(i64),
}

fn arb_edits() -> impl Strategy<Value = Vec<Edit>> {
    prop::collection::vec(
        prop_oneof![
            3 => (0..KEYS, arb_row()).prop_map(|(k, r)| Edit::Set(k, r)),
            2 => (0..KEYS).prop_map(Edit::Remove),
        ],
        0..8,
    )
}

/// Turn edits into a batch that is well-formed against `rel`.
fn resolve(rel: &Relation, edits: &[Edit]) -> Batch {
    let mut shadow = rel.clone();
    let mut batch = Batch::new();
    for edit in edits {
        match edit {
            Edit::Set(k, row) => {
                let key = RowKey::from(*k);
                let before = shadow.get(&key).cloned();
                shadow.insert(key.clone(), row.clone());
                if let Some(change) = Change::from_images(key, before, Some(row.clone())) {
                    batch.push(change);
                }
            }
            Edit::Remove(k) => {
                let key = RowKey::from(*k);
                if let Some(before) = shadow.remove(&key) {
                    batch.push(Change::Delete { key, before });
                }
            }
        }
    }
    batch
}

fn store_of(rel: &Relation) -> MemStore {
    let mut store = MemStore::new();
    store.load(TABLE, rel.clone());
    store
}

/// A graph brought up to date with `base`, exactly as the engine does it:
/// hydrate against a store holding `base`, then leave it ready for deltas.
///
/// Hydrating through `apply` rather than seeding state directly is the point —
/// if the two ever diverged, a view would be right on first load and wrong
/// after an edit (or the reverse), which is the bug class this file exists to
/// rule out.
fn hydrated(plan: &Reference, base: &Relation) -> Graph {
    let mut graph = plan.build();
    graph.hydrate(&mut store_of(base));
    graph
}

/// One coherent pump of the single table these plans read.
fn pump(graph: &mut Graph, delta: Batch, store: &mut MemStore) -> Batch {
    graph.pump(&[(TABLE, delta)], store)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 2048, ..ProptestConfig::default() })]

    /// The law itself.
    #[test]
    fn incremental_agrees_with_recomputation(
        plan in arb_plan(),
        base in arb_relation(),
        edits in arb_edits(),
    ) {
        let delta = resolve(&base, &edits);
        let after = base.with_applied(&delta);

        // Left: recompute over the updated relation.
        let expected = plan.eval(&after);

        // Right: maintain the previous result incrementally. The store holds
        // the post-batch relation because the engine commits before it pumps.
        let mut graph = hydrated(&plan, &base);
        let out = pump(&mut graph, delta, &mut store_of(&after));
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
        base in arb_relation(),
    ) {
        let mut graph = plan.build();
        let out = graph.hydrate(&mut store_of(&base));
        prop_assert_eq!(plan.eval(&base), Relation::new().with_applied(&out));
    }

    /// A long run of deltas must not drift.
    ///
    /// One step can be correct while the window's bookkeeping rots over many —
    /// slack quietly filling with rows that no longer exist, `full` latching
    /// true after a truncation, a cursor built from a row that was already
    /// evicted. Only repetition finds those.
    #[test]
    fn a_run_of_deltas_never_drifts(
        plan in arb_plan(),
        base in arb_relation(),
        runs in prop::collection::vec(arb_edits(), 1..6),
    ) {
        let mut graph = hydrated(&plan, &base);
        let mut rel = base.clone();
        let mut view = plan.eval(&base);

        for edits in &runs {
            let delta = resolve(&rel, edits);
            rel = rel.with_applied(&delta);
            let out = pump(&mut graph, delta, &mut store_of(&rel));
            view.apply(&out);
            prop_assert_eq!(&view, &plan.eval(&rel));
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
        base in arb_relation(),
        first in arb_edits(),
        second in arb_edits(),
    ) {
        let d1 = resolve(&base, &first);
        let mid = base.with_applied(&d1);
        let d2 = resolve(&mid, &second);
        let end = mid.with_applied(&d2);

        let mut stepwise = hydrated(&plan, &base);
        let first_out = pump(&mut stepwise, d1.clone(), &mut store_of(&mid));
        let out_stepwise =
            first_out.compose(pump(&mut stepwise, d2.clone(), &mut store_of(&end)));

        let mut coalesced = hydrated(&plan, &base);
        let out_coalesced = pump(&mut coalesced, d1.compose(d2), &mut store_of(&end));

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
    /// are identical — invisible in a test, expensive in a long list.
    #[test]
    fn output_carries_no_noop_changes(
        plan in arb_plan(),
        base in arb_relation(),
        edits in arb_edits(),
    ) {
        let delta = resolve(&base, &edits);
        let mut graph = hydrated(&plan, &base);
        let after = base.with_applied(&delta);
        let out = pump(&mut graph, delta, &mut store_of(&after));
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
        base in arb_relation(),
        runs in prop::collection::vec(arb_edits(), 1..4),
    ) {
        let plan = Reference::new(TABLE, PK, vec![Stage::TopK { order, k, slack }]);
        let mut graph = hydrated(&plan, &base);

        let row_bytes = ARITY * std::mem::size_of::<Value>();
        let budget = (k + slack) * (row_bytes + 128);

        let mut rel = base;
        for edits in &runs {
            let delta = resolve(&rel, edits);
            rel = rel.with_applied(&delta);
            pump(&mut graph, delta, &mut store_of(&rel));
            prop_assert!(
                graph.state_bytes() <= budget,
                "TopK held {} bytes for k={} slack={} over {} rows",
                graph.state_bytes(), k, slack, rel.len()
            );
        }
    }
}
