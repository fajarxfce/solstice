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

use proptest::prelude::*;
use solstice_ivm::delta::Change;
use solstice_ivm::operator::NoCx;
use solstice_ivm::predicate::{CmpOp, Expr, Params, Predicate};
use solstice_ivm::reference::{Reference, Relation, Stage};
use solstice_ivm::{Batch, ColId, Pipeline, Row, RowKey, Value};
use std::sync::Arc;

/// Width of the generated base relation.
const ARITY: usize = 4;
/// Key space. Small, so that inserts, updates and deletes actually collide.
const KEYS: i64 = 8;

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

fn arb_stage() -> impl Strategy<Value = Stage> {
    prop_oneof![
        3 => (arb_predicate(), arb_params())
            .prop_map(|(pred, params)| Stage::Filter { pred, params }),
        // Projections may repeat, reorder, and drop columns — including
        // projecting away a column a later filter reads, which is legal and
        // must simply read as NULL rather than panic.
        1 => prop::collection::vec(0..ARITY as ColId, 1..=ARITY)
            .prop_map(|cols| Stage::Project { cols }),
    ]
}

fn arb_plan() -> impl Strategy<Value = Reference> {
    prop::collection::vec(arb_stage(), 0..4).prop_map(Reference::new)
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

/// The whole relation expressed as a batch of inserts — what a `Source` emits
/// on hydration.
fn as_inserts(rel: &Relation) -> Batch {
    rel.iter()
        .map(|(key, row)| Change::Insert {
            key: key.clone(),
            row: row.clone(),
        })
        .collect()
}

fn pipeline_of(plan: &Reference) -> Pipeline {
    Pipeline::new(plan.stages().iter().map(|s| s.build()).collect())
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

        // Left: recompute over the updated relation.
        let expected = plan.eval(&base.with_applied(&delta));

        // Right: maintain the previous result incrementally.
        let mut pipeline = pipeline_of(&plan);
        let out = pipeline.apply(&delta, &mut NoCx);
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
        let mut pipeline = pipeline_of(&plan);
        let out = pipeline.apply(&as_inserts(&base), &mut NoCx);
        prop_assert_eq!(plan.eval(&base), Relation::new().with_applied(&out));
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

        let mut stepwise = pipeline_of(&plan);
        let out_stepwise = stepwise
            .apply(&d1, &mut NoCx)
            .compose(stepwise.apply(&d2, &mut NoCx));

        let mut coalesced = pipeline_of(&plan);
        let out_coalesced = coalesced.apply(&d1.clone().compose(d2), &mut NoCx);

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
        let mut pipeline = pipeline_of(&plan);
        for change in pipeline.apply(&delta, &mut NoCx).iter() {
            prop_assert_ne!(change.before(), change.after());
        }
    }
}
