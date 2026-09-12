//! SQLite answering the same question as the engine — oracle (b) of plan §6.
//!
//! `solstice-ivm` already checks itself against a deliberately dumb evaluator
//! (`tests/delta_law.rs`). That catches an operator that maintains its state
//! wrongly. It cannot catch the other half: the engine asking the *store* one
//! question and the store answering a different one. Every refill in the
//! product goes through that seam, and a divergence there is silent — the view
//! is simply missing a row, or holding one twice.
//!
//! So this file checks the seam directly, in three pieces that compose:
//!
//! 1. **The store mirrors the relation.** Insert, update and delete through
//!    [`SqliteStore::apply`] leave the table equal to the `Relation` the same
//!    edits produce. Without this the rest is measuring the wrong contents.
//! 2. **A scan answers what the reference scan answers**, over arbitrary
//!    orders, filters, cursors and limits. [`MemStore`] is the reference: the
//!    same `OpCx`, implemented by sorting a `Vec` and being obviously right.
//! 3. **The delta law still holds with SQLite underneath.** The first two
//!    imply it — the graph touches the store only through [`OpCx`] — but only
//!    for the scans *this file* generates. The end-to-end property covers the
//!    scans the operators actually issue, and the commit-then-pump ordering
//!    the scan contract depends on (plan §1.4).
//!
//! # The generated data is shaped by what a store is
//!
//! Two constraints separate these generators from `delta_law.rs`, and both are
//! properties of the store rather than of the test:
//!
//! * **`row[pk]` is the key.** A `Relation` keeps the key beside the row; a
//!   table keeps it *in* the row and reads it back out. Generating the two
//!   independently would make the stores disagree for a reason that has nothing
//!   to do with the translation.
//! * **Keys are all `Int`.** The engine breaks a sort-key tie with the
//!   *structural* order on `RowKey`, SQLite breaks it by storage class, and the
//!   two disagree in exactly one place: structurally `Int` sorts before `Real`,
//!   while SQL treats them as one class and compares numerically. Mixing them
//!   in a primary key would be a real divergence, but it is a divergence about
//!   key design — a pk column holding both `1` and `1.0` is already broken —
//!   not about SQL generation, which is what this file is for. `Int` and `Text`
//!   keys would agree, incidentally; it is only the numerics that split.
//!
//! Everything else is drawn as widely as `delta_law.rs` draws it: NULLs
//! throughout, every storage class in every non-key column, predicates that
//! evaluate to unknown, projections that read past the end of a row.
//!
//! # One thing these properties cannot see
//!
//! Removing the trailing primary key from the generated `ORDER BY` — or from
//! the order index — does not fail anything here. Mutation testing says so, and
//! the reason is not a gap in the generators: the tables are `WITHOUT ROWID`,
//! so SQLite already scans and indexes them in primary-key order, and on data
//! this size a tie between sort keys comes back pk-ascending whether we asked
//! for it or not.
//!
//! That is precisely why the pk is spelled out rather than relied on. It is not
//! there to change SQLite's answer — it is there so the answer is *guaranteed*
//! rather than incidental, and so it keeps agreeing once the planner has a
//! reason to pick a different access path. A property test cannot assert the
//! difference between "right" and "right by luck", so the claim is pinned by
//! name instead: `sql::tests::the_primary_key_always_breaks_the_sort_tie_ascending`
//! and `ddl::tests::an_order_index_ends_with_the_primary_key`, both of which do
//! fail when it is dropped.

use proptest::prelude::*;
use solstice_ivm::delta::Change;
use solstice_ivm::order::cmp_entry;
use solstice_ivm::reference::{JoinSpec, MemStore, Reference, Stage, Tables};
use solstice_ivm::{
    Batch, ColId, Column, Cursor, Dir, Graph, OpCx, Params, Relation, Row, RowKey, ScanRequest,
    Schema, TableId, Value, ValueType,
};
use solstice_ivm::{CmpOp, Expr, Predicate};
use solstice_store::SqliteStore;
use std::sync::Arc;

/// Width of the generated tables. Column 0 is the primary key; the other three
/// carry arbitrary values.
const ARITY: usize = 4;
/// Key space. Small, so inserts, updates and deletes collide.
const KEYS: i64 = 8;
const PARENTS: TableId = 1;
const CHILDREN: TableId = 2;
const PK: ColId = 0;

/// A table the generators can fill with anything.
///
/// The declared [`ValueType`] is advisory in M0 — nothing reads it, and the DDL
/// deliberately emits no column types at all so SQLite applies no affinity (see
/// `solstice_store::ddl`). That is exactly what lets these columns hold every
/// storage class at once, which is the case a declared affinity would have
/// quietly converted out from under the comparison.
fn table_schema(table: TableId, name: &str) -> Schema {
    let columns = (0..ARITY)
        .map(|i| {
            let col = Column::new(format!("c{i}"), ValueType::Int);
            if i == PK as usize {
                col
            } else {
                col.nullable()
            }
        })
        .collect();
    Schema::new(table, name, columns, PK)
}

fn schemas() -> Vec<Schema> {
    vec![
        table_schema(PARENTS, "parents"),
        table_schema(CHILDREN, "children"),
    ]
}

/// Every storage class, over a domain small enough that values collide.
///
/// The blobs are the ones worth arguing for. They are the class an application
/// schema reaches for least and the store round-trips most delicately: a blob
/// and the text with the same bytes are equal to `memcmp` and *not* equal to
/// either comparison order, so reading one back as the other is a bug that only
/// a generator that produces both will find. The zero-length blob is in range
/// on purpose — that is the value SQLite's `substr` answers NULL for.
fn arb_value() -> impl Strategy<Value = Value> {
    prop_oneof![
        1 => Just(Value::Null),
        5 => (-4i64..5).prop_map(Value::Int),
        2 => prop_oneof![
            Just(0.0f64), Just(1.0), Just(1.5), Just(-2.0), Just(3.0)
        ].prop_map(Value::Real),
        3 => "[a-c]{0,2}".prop_map(Value::text),
        2 => prop::collection::vec(0x61u8..0x64, 0..3).prop_map(Value::blob),
    ]
}

/// A row whose primary-key column holds its own key, which is what a table row
/// is. See the module docs.
fn keyed_row(key: i64, rest: Vec<Value>) -> (RowKey, Row) {
    let mut values = Vec::with_capacity(ARITY);
    values.push(Value::Int(key));
    values.extend(rest);
    (RowKey::from(key), Row::new(values))
}

fn arb_keyed_row(keys: i64) -> impl Strategy<Value = (RowKey, Row)> {
    (
        0..keys,
        prop::collection::vec(arb_value(), ARITY.saturating_sub(1)),
    )
        .prop_map(|(key, rest)| keyed_row(key, rest))
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

/// Every predicate form the IR admits, including the ones whose obvious SQL
/// spelling is wrong: `LikePrefix` (SQL `LIKE` is ASCII-case-insensitive and
/// coerces numbers to text), `In` with an empty list (`IN ()` will not parse),
/// and literals on the *left* of a comparison (which makes the operand a bind,
/// and a bind that is emitted more than once has to be bound more than once).
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

fn arb_order() -> impl Strategy<Value = Vec<(ColId, Dir)>> {
    prop::collection::vec(
        (
            0..ARITY as ColId,
            prop_oneof![Just(Dir::Asc), Just(Dir::Desc)],
        ),
        1..=2,
    )
}

/// Limits worth drawing are 0, a handful, and unbounded.
///
/// `usize::MAX` is what a `Source` hydrating a whole table asks for, and it is
/// the one limit SQL cannot express as a number: it narrows to a negative
/// `LIMIT`, which SQLite reads as *no* bound. Zero is the opposite trap — a
/// scan that must come back empty rather than come back with one row.
fn arb_limit() -> impl Strategy<Value = usize> {
    prop_oneof![
        8 => 0usize..=6,
        1 => Just(usize::MAX),
    ]
}

fn arb_relation_over(keys: i64, len: usize) -> impl Strategy<Value = Relation> {
    prop::collection::vec(arb_keyed_row(keys), 0..len)
        .prop_map(|entries| entries.into_iter().collect())
}

fn arb_relation() -> impl Strategy<Value = Relation> {
    arb_relation_over(KEYS, 12)
}

fn arb_tables() -> impl Strategy<Value = Tables> {
    (arb_relation(), arb_relation())
        .prop_map(|(p, c)| Tables::new().with(PARENTS, p).with(CHILDREN, c))
}

/// Where a scan resumes from.
///
/// Both cases matter and they fail differently. A cursor taken from a real row
/// is what `TopK` builds, and it is the one that exposes an off-by-one: the row
/// it was built from must be excluded and the row tied with it must not be. A
/// cursor at a position no row occupies — a NULL sort key, a key between two
/// rows — is what exposes a comparison chain that is not NULL-aware, which
/// drops a whole column of NULLs from every page after the first.
#[derive(Debug, Clone)]
enum CursorSpec {
    None,
    AtRow(usize),
    Anywhere(Vec<Value>, i64),
}

fn arb_cursor_spec() -> impl Strategy<Value = CursorSpec> {
    prop_oneof![
        2 => Just(CursorSpec::None),
        5 => (0usize..12).prop_map(CursorSpec::AtRow),
        3 => (prop::collection::vec(arb_value(), 0..=3), 0..KEYS)
            .prop_map(|(sort_key, pk)| CursorSpec::Anywhere(sort_key, pk)),
    ]
}

fn arb_scan_request() -> impl Strategy<Value = ScanRequest> {
    (
        arb_order(),
        prop::option::of(arb_predicate()),
        arb_params(),
        arb_limit(),
    )
        .prop_map(|(order, filter, params, limit)| ScanRequest {
            table: PARENTS,
            order,
            after: None,
            filter,
            params,
            limit,
        })
}

/// Resolve a [`CursorSpec`] against the relation the scan will run over.
///
/// Done here rather than in the generator because `AtRow` needs the request's
/// own sort order to know which row is at that position.
fn with_cursor(mut req: ScanRequest, rel: &Relation, spec: CursorSpec) -> ScanRequest {
    req.after = match spec {
        CursorSpec::None => None,
        CursorSpec::AtRow(i) => {
            let mut rows: Vec<(&RowKey, &Row)> = rel.iter().collect();
            rows.sort_by(|a, b| cmp_entry(*a, *b, &req.order));
            rows.get(i % rows.len().max(1))
                .map(|(key, row)| Cursor::of(row, (*key).clone(), &req.order))
        }
        CursorSpec::Anywhere(sort_key, pk) => Some(Cursor {
            sort_key,
            pk: RowKey::from(pk),
        }),
    };
    req
}

fn entries(rel: &Relation) -> Vec<(RowKey, Row)> {
    rel.iter().map(|(k, r)| (k.clone(), r.clone())).collect()
}

fn store_over(tables: &Tables) -> SqliteStore {
    let mut store = SqliteStore::in_memory(schemas()).expect("opening an in-memory store");
    for table in [PARENTS, CHILDREN] {
        store
            .load(table, entries(&tables.get(table)))
            .expect("loading generated rows");
    }
    store
}

fn store_of(rel: &Relation) -> SqliteStore {
    store_over(&Tables::new().with(PARENTS, rel.clone()))
}

fn mem_of(rel: &Relation) -> MemStore {
    let mut mem = MemStore::new();
    mem.load(PARENTS, rel.clone());
    mem
}

// The plan generators below mirror `solstice-ivm`'s `delta_law.rs`, over the
// keyed rows this file generates. They exist for the end-to-end property, which
// needs the scans the *operators* issue rather than the ones a generator
// invents.

fn arb_filter() -> impl Strategy<Value = Stage> {
    arb_predicate().prop_map(|pred| Stage::Filter { pred })
}

fn arb_project() -> impl Strategy<Value = Stage> {
    prop::collection::vec(0..ARITY as ColId, 1..=ARITY).prop_map(|cols| Stage::Project { cols })
}

/// `k` and slack are small against a relation of up to twelve rows, so that
/// almost every delete of a visible row has to go back to the store. Refills
/// are the whole point of this file; a `TopK` that never refills exercises no
/// SQL at all.
fn arb_topk() -> impl Strategy<Value = Stage> {
    (arb_order(), 1usize..=4, 0usize..=2).prop_map(|(order, k, slack)| Stage::TopK {
        order,
        k,
        slack,
    })
}

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

/// Filters, then `TopK`, then projection, then the join — the shape a real
/// planner emits, and the only shape where both refilling operators read a base
/// table the store actually has.
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

/// An intent to change a table, resolved into a well-formed change only once
/// the current contents are known.
#[derive(Debug, Clone)]
enum Edit {
    Set(RowKey, Row),
    Remove(RowKey),
}

fn arb_edits() -> impl Strategy<Value = Vec<Edit>> {
    prop::collection::vec(
        prop_oneof![
            3 => arb_keyed_row(KEYS).prop_map(|(k, r)| Edit::Set(k, r)),
            2 => (0..KEYS).prop_map(|k| Edit::Remove(RowKey::from(k))),
        ],
        0..8,
    )
}

#[derive(Debug, Clone)]
struct Txn {
    parents: Vec<Edit>,
    children: Vec<Edit>,
}

fn arb_txn() -> impl Strategy<Value = Txn> {
    (arb_edits(), arb_edits()).prop_map(|(parents, children)| Txn { parents, children })
}

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

/// 256 cases per property per commit, overridable for the nightly sweep.
///
/// An order of magnitude below `delta_law.rs`, because every case here compiles
/// SQL and runs it. The nightly sweep is where the depth comes from; setting
/// `cases` unconditionally would make `PROPTEST_CASES` silently do nothing,
/// which is how a million-case run quietly becomes a 256-case one.
fn config() -> ProptestConfig {
    let mut cfg = ProptestConfig::default();
    if std::env::var_os("PROPTEST_CASES").is_none() {
        cfg.cases = 256;
    }
    cfg
}

proptest! {
    #![proptest_config(config())]

    /// Piece 1: the table holds what the relation holds, through every edit.
    ///
    /// `apply` is the only writer in the product (plan §1.4), so if it drops an
    /// update or mistakes a delete, every other property here is comparing two
    /// wrong answers and may well find them equal.
    #[test]
    fn the_store_mirrors_the_relation_through_every_edit(
        base in arb_relation(),
        churn in prop::collection::vec(arb_edits(), 1..5),
    ) {
        let mut rel = base.clone();
        let mut store = store_of(&base);
        prop_assert_eq!(store.dump(PARENTS).unwrap(), entries(&rel));

        for edits in &churn {
            let batch = resolve_one(&rel, edits);
            rel.apply(&batch);
            store.apply(PARENTS, &batch).unwrap();
            prop_assert_eq!(store.dump(PARENTS).unwrap(), entries(&rel));
        }
    }

    /// Piece 2: the same `ScanRequest`, the same rows, in the same order.
    ///
    /// Order is part of the answer, not a detail — a cursor resumes from the
    /// last row of the previous page, so two scans that return the same set in
    /// a different order paginate differently.
    #[test]
    fn a_scan_answers_exactly_what_the_reference_scan_answers(
        rel in arb_relation(),
        req in arb_scan_request(),
        cursor in arb_cursor_spec(),
    ) {
        let req = with_cursor(req, &rel, cursor);
        let mut sqlite = store_of(&rel);
        let mut mem = mem_of(&rel);
        prop_assert_eq!(sqlite.scan(&req), mem.scan(&req));
    }

    /// Walking a relation a page at a time must visit every row once.
    ///
    /// Subsumed by the property above for any single scan, and kept anyway
    /// because it fails more usefully: a cursor comparison that is off by one
    /// tie shows up here as a duplicated or missing row after several pages,
    /// which is the shape the bug takes in a real list.
    #[test]
    fn paging_with_a_cursor_visits_every_row_exactly_once(
        rel in arb_relation(),
        order in arb_order(),
        filter in prop::option::of(arb_predicate()),
        params in arb_params(),
        page in 1usize..=3,
    ) {
        let request = |limit, after| ScanRequest {
            table: PARENTS,
            order: order.clone(),
            after,
            filter: filter.clone(),
            params: params.clone(),
            limit,
        };

        let mut sqlite = store_of(&rel);
        let mut paged: Vec<(RowKey, Row)> = Vec::new();
        let mut after = None;
        loop {
            let got = sqlite.scan(&request(page, after.clone()));
            if got.is_empty() {
                break;
            }
            after = got
                .last()
                .map(|(key, row)| Cursor::of(row, key.clone(), &order));
            paged.extend(got);
            // A cursor that does not advance would page forever.
            prop_assert!(
                paged.len() <= rel.len(),
                "paging returned more rows than the relation has"
            );
        }

        prop_assert_eq!(paged, mem_of(&rel).scan(&request(usize::MAX, None)));
    }

    /// An index may change how SQLite answers a scan. It may not change what
    /// it answers.
    ///
    /// This is the one property that tests `create_order_index` for more than
    /// syntax. A descending index whose NULL placement disagrees with the
    /// query's would be used happily and return a different order, and the
    /// failure would only appear once a table grew past the point where SQLite
    /// stopped preferring a full sort.
    #[test]
    fn an_index_changes_the_plan_and_not_the_answer(
        rel in arb_relation(),
        req in arb_scan_request(),
        cursor in arb_cursor_spec(),
    ) {
        let req = with_cursor(req, &rel, cursor);
        let mut plain = store_of(&rel);
        let mut indexed = store_of(&rel);
        indexed.index_order(PARENTS, &req.order).unwrap();
        prop_assert_eq!(indexed.scan(&req), plain.scan(&req));
    }

    /// Piece 3: the delta law, with SQLite where `MemStore` usually is.
    ///
    /// ```text
    /// ∀ plan P, tables R, delta Δ:   P(R ⊎ Δ)  ==  P(R) ⊎ P.apply(Δ)
    /// ```
    ///
    /// The store is committed *before* the graph is pumped, which is the scan
    /// contract [`OpCx::scan`] documents and not an implementation detail: a
    /// `TopK` refilling after a delete would otherwise read the row it just
    /// deleted back out of the store and put it on screen again.
    #[test]
    fn the_delta_law_holds_with_sqlite_underneath(
        plan in arb_plan(),
        base in arb_tables(),
        txns in prop::collection::vec(arb_txn(), 1..4),
    ) {
        let mut store = store_over(&base);
        let mut graph = plan.build();

        let mut view = Relation::new();
        view.apply(&graph.hydrate(&mut store));
        prop_assert_eq!(&view, &plan.eval(&base), "hydration");

        let mut tables = base;
        for (step, txn) in txns.iter().enumerate() {
            let deltas = resolve(&tables, txn);
            tables = tables.with_applied(&deltas);

            for (table, batch) in &deltas {
                store.apply(*table, batch).unwrap();
            }
            let out = graph.pump(&deltas, &mut store);
            view.apply(&out);

            prop_assert_eq!(&view, &plan.eval(&tables), "after txn {}", step);
        }
    }
}

/// A named case for the hydration limit, which no generator reaches by accident
/// and which every `Source` uses on every open.
#[test]
fn an_unbounded_hydration_reads_the_whole_table() {
    let rel: Relation = (0..50)
        .map(|k| keyed_row(k, vec![Value::Null; 3]))
        .collect();
    let mut store = store_of(&rel);
    let got = store.scan(&ScanRequest {
        table: PARENTS,
        order: vec![(PK, Dir::Asc)],
        after: None,
        filter: None,
        params: Params::empty(),
        limit: usize::MAX,
    });
    assert_eq!(got.len(), 50, "usize::MAX must mean no bound, not no rows");
}

/// A named case for the divergence that motivated leaving column affinities
/// off, because a generator finding it would be reported as an unreadable
/// shrunk predicate rather than as the one sentence it actually is.
#[test]
fn a_text_column_does_not_coerce_a_numeric_comparison() {
    let rel: Relation = [
        keyed_row(1, vec![Value::text("5"), Value::Null, Value::Null]),
        keyed_row(2, vec![Value::Int(5), Value::Null, Value::Null]),
    ]
    .into_iter()
    .collect();

    let req = ScanRequest {
        table: PARENTS,
        // `c1 > 4`. With TEXT affinity SQLite would compare the row holding
        // Int(5) against '4' as text and still admit it; with INTEGER affinity
        // it would convert '5' and admit both. With no affinity, only the
        // integer row matches — which is what `Value::sql_cmp` says, because a
        // text value and an integer never compare equal and text sorts above
        // every number.
        order: vec![(PK, Dir::Asc)],
        after: None,
        filter: Some(Predicate::Cmp {
            lhs: Expr::Col(1),
            op: CmpOp::Gt,
            rhs: Expr::Lit(Value::Int(4)),
        }),
        params: Params::empty(),
        limit: 10,
    };

    let mut sqlite = store_of(&rel);
    let mut mem = mem_of(&rel);
    let got = sqlite.scan(&req);
    assert_eq!(got, mem.scan(&req));
    assert_eq!(
        got.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>(),
        vec![RowKey::from(1), RowKey::from(2)],
        "text sorts above every number, so both rows are greater than 4"
    );
}

/// The store's `OpCx` must be usable as a graph's only context. This is the
/// smallest thing that would break if `solstice-store` stopped satisfying the
/// trait, and it fails at compile time rather than in a property.
#[test]
fn a_graph_runs_on_the_store_alone() {
    let base = Tables::new().with(
        PARENTS,
        (0..4)
            .map(|k| keyed_row(k, vec![Value::Int(k); 3]))
            .collect(),
    );
    let plan = Reference::new(
        PARENTS,
        PK,
        vec![Stage::TopK {
            order: vec![(1, Dir::Desc)],
            k: 2,
            slack: 0,
        }],
    );

    let mut store = store_over(&base);
    let mut graph: Graph = plan.build();
    let mut view = Relation::new();
    view.apply(&graph.hydrate(&mut store));
    assert_eq!(view, plan.eval(&base));
}
