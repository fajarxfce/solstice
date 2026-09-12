//! A deliberately dumb, non-incremental evaluator — the oracle.
//!
//! Nothing here is on the hot path, and nothing here should ever be made
//! clever. Its entire value is being so obviously correct that a disagreement
//! with the incremental path is always the incremental path's fault.
//!
//! This is oracle (a) of the three the plan calls for (§6): a reference
//! evaluator over `Vec<Row>`, SQLite itself via `IR → SQL`, and the incremental
//! path. Oracle (b) arrives with `solstice-store`.

use crate::delta::Batch;
use crate::graph::{Graph, GraphBuilder};
use crate::operator::{OpCx, RefillKind, RefillStats, ScanRequest};
use crate::ops::{Filter, Join1N, Project, Source, TopK};
use crate::order::{cmp_entry, cmp_to_cursor, Dir};
use crate::predicate::{Params, Predicate};
use crate::relation::Relation;
use crate::schema::TableId;
use crate::value::{ColId, Row, RowKey, Value};
use std::cmp::Ordering;
use std::collections::BTreeMap;

/// A 1:N traversal, described the way the query IR would describe it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinSpec {
    pub child_table: TableId,
    pub child_pk: ColId,
    /// Column of the parent row children point at.
    pub parent_key: ColId,
    /// Column of the child row holding the foreign key.
    pub child_fk: ColId,
    /// Filters that narrow the child relation before the traversal.
    pub child_filters: Vec<Predicate>,
    pub order: Vec<(ColId, Dir)>,
    pub limit: usize,
    pub slack: usize,
}

/// One step of a reference plan, mirroring one operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stage {
    Filter {
        pred: Predicate,
    },
    Project {
        cols: Vec<ColId>,
    },
    TopK {
        order: Vec<(ColId, Dir)>,
        k: usize,
        slack: usize,
    },
    Join(JoinSpec),
}

/// The base relations a plan reads.
///
/// A plan used to be a function of one relation. A join makes it a function of
/// several, and threading a map through is less work than pretending the child
/// table is somehow part of the parent's input.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tables {
    tables: BTreeMap<TableId, Relation>,
}

impl Tables {
    pub fn new() -> Self {
        Tables::default()
    }

    pub fn with(mut self, table: TableId, rel: Relation) -> Self {
        self.tables.insert(table, rel);
        self
    }

    /// The named relation, or an empty one. A query may legitimately read a
    /// table nothing has written yet.
    pub fn get(&self, table: TableId) -> Relation {
        self.tables.get(&table).cloned().unwrap_or_default()
    }

    pub fn table(&mut self, table: TableId) -> &mut Relation {
        self.tables.entry(table).or_default()
    }

    /// The same tables with one table's batch applied — the `R ⊎ Δ` side of the
    /// delta law.
    pub fn with_applied(&self, deltas: &[(TableId, Batch)]) -> Tables {
        let mut out = self.clone();
        for (table, batch) in deltas {
            out.table(*table).apply(batch);
        }
        out
    }
}

/// A reference plan: stages applied in order, wholesale, over a whole relation.
///
/// Parameters are bound once per plan rather than per stage, because that is
/// what a query is (plan §1.2: `ViewId = hash(IR with params)`). It also makes
/// pushdown well-defined — a predicate can only be handed to a scan together
/// with the bindings it was written against.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reference {
    table: TableId,
    pk: ColId,
    stages: Vec<Stage>,
    params: Params,
}

impl Reference {
    /// # Panics
    ///
    /// If a `Join` is not the last stage. Its output is hierarchical, and
    /// nothing downstream in M0 reads a child collection: a `TopK` above it
    /// would refill by scanning the *parent* table and hand the view rows with
    /// no children attached. The planner puts `TopK` below the join precisely so
    /// that this configuration never arises (see the [`crate::ops`] docs).
    pub fn new(table: TableId, pk: ColId, stages: Vec<Stage>) -> Self {
        let body = stages.len().saturating_sub(1);
        assert!(
            stages[..body].iter().all(|s| !matches!(s, Stage::Join(_))),
            "a join must be the last stage of a plan"
        );
        Reference {
            table,
            pk,
            stages,
            params: Params::empty(),
        }
    }

    pub fn with_params(mut self, params: Params) -> Self {
        self.params = params;
        self
    }

    pub fn table(&self) -> TableId {
        self.table
    }

    pub fn stages(&self) -> &[Stage] {
        &self.stages
    }

    pub fn params(&self) -> &Params {
        &self.params
    }

    /// The incremental dataflow graph this plan corresponds to.
    ///
    /// Having both sides built from one description is what makes the delta law
    /// testable: there is no chance of the oracle and the graph being handed
    /// subtly different queries.
    ///
    /// The `Source` here deliberately scans the whole table rather than taking
    /// the first filters as hydration pushdown. Pushdown would produce the same
    /// view — and the real planner does it — but it would also mean `Filter`
    /// never has to reject a row at hydration time, quietly shrinking what the
    /// property tests cover.
    ///
    /// The one piece of real optimiser behaviour that *is* here is **filter
    /// pushdown into `TopK`**, because that one is not an optimisation. A
    /// `TopK` refills by reading the base table, so without the upstream filters
    /// it would pull in rows the graph has already decided are not part of the
    /// relation, and hand them to the view.
    pub fn build(&self) -> Graph {
        let mut b = GraphBuilder::new();
        let mut node = b.source(self.table, Box::new(Source::new(self.table, self.pk)));
        let mut seen_filters: Vec<Predicate> = Vec::new();

        for stage in &self.stages {
            node = match stage {
                Stage::Filter { pred } => {
                    seen_filters.push(pred.clone());
                    b.add(
                        Box::new(Filter::new(pred.clone(), self.params.clone())),
                        vec![node],
                    )
                }
                Stage::Project { cols } => b.add(Box::new(Project::new(cols.clone())), vec![node]),
                Stage::TopK { order, k, slack } => {
                    let mut top = TopK::new(self.table, order.clone(), *k).with_slack(*slack);
                    if !seen_filters.is_empty() {
                        top = top.with_pushdown(
                            Predicate::and(seen_filters.clone()),
                            self.params.clone(),
                        );
                    }
                    b.add(Box::new(top), vec![node])
                }
                Stage::Join(spec) => {
                    // The child source hydrates to nothing: a million comments
                    // must not be materialised so the join can keep three per
                    // issue. Children arrive per parent, by scan.
                    let mut child = b.source(
                        spec.child_table,
                        Box::new(Source::new(spec.child_table, spec.child_pk).deltas_only()),
                    );
                    for pred in &spec.child_filters {
                        child = b.add(
                            Box::new(Filter::new(pred.clone(), self.params.clone())),
                            vec![child],
                        );
                    }

                    let mut join = Join1N::new(
                        spec.child_table,
                        spec.parent_key,
                        spec.child_fk,
                        spec.order.clone(),
                        spec.limit,
                    )
                    .with_slack(spec.slack);
                    // Same reason as `TopK` above: the per-parent windows refill
                    // from the base table, so they need the child filters too.
                    if !spec.child_filters.is_empty() {
                        join = join.with_child_filter(
                            Predicate::and(spec.child_filters.clone()),
                            self.params.clone(),
                        );
                    }
                    b.add(Box::new(join), vec![node, child])
                }
            };
        }

        b.build(node)
    }

    /// Evaluate from scratch. O(relation) by construction, which is the point.
    pub fn eval(&self, tables: &Tables) -> Relation {
        let mut current = tables.get(self.table);
        for stage in &self.stages {
            current = match stage {
                Stage::Filter { pred } => current
                    .iter()
                    .filter(|(_, row)| pred.matches(row, &self.params))
                    .map(|(k, r)| (k.clone(), r.clone()))
                    .collect(),
                Stage::Project { cols } => current
                    .iter()
                    .map(|(k, r)| (k.clone(), r.project(cols)))
                    .collect(),
                Stage::TopK { order, k, .. } => {
                    // Sort the whole thing and take the first k. Quadratically
                    // dumber than the operator, which is exactly its job.
                    let mut rows: Vec<(&RowKey, &Row)> = current.iter().collect();
                    rows.sort_by(|a, b| cmp_entry(*a, *b, order));
                    rows.into_iter()
                        .take(*k)
                        .map(|(key, row)| (key.clone(), row.clone()))
                        .collect()
                }
                Stage::Join(spec) => {
                    // For every parent, re-scan the whole child table. The
                    // operator holds windows and refills; this one just looks.
                    let child_rel = tables.get(spec.child_table);
                    current
                        .iter()
                        .map(|(key, row)| {
                            let parent_key = row.get(spec.parent_key);
                            let mut kids: Vec<(&RowKey, &Row)> = child_rel
                                .iter()
                                .filter(|(_, c)| {
                                    c.get(spec.child_fk).sql_cmp(parent_key)
                                        == Some(Ordering::Equal)
                                        && spec
                                            .child_filters
                                            .iter()
                                            .all(|p| p.matches(c, &self.params))
                                })
                                .collect();
                            kids.sort_by(|a, b| cmp_entry(*a, *b, &spec.order));
                            let kids: Vec<Row> = kids
                                .into_iter()
                                .take(spec.limit)
                                .map(|(_, c)| c.clone())
                                .collect();
                            (key.clone(), row.with_appended(Value::rows(kids)))
                        })
                        .collect()
                }
            };
        }
        current
    }
}

/// An in-memory [`OpCx`] backing `Source` hydration in tests.
///
/// Stands in for `solstice-store` so that `solstice-ivm` can be exercised end to end
/// without SQLite — which is the whole reason the store is behind a trait.
pub struct MemStore {
    tables: BTreeMap<TableId, Relation>,
    pub refills: RefillStats,
}

impl MemStore {
    pub fn new() -> Self {
        MemStore {
            tables: BTreeMap::new(),
            refills: RefillStats::default(),
        }
    }

    /// A store holding exactly these relations — the same base state the oracle
    /// is handed, so the two sides of the delta law cannot drift apart.
    pub fn over(tables: &Tables) -> MemStore {
        MemStore {
            tables: tables.tables.clone(),
            refills: RefillStats::default(),
        }
    }

    pub fn table(&mut self, table: TableId) -> &mut Relation {
        self.tables.entry(table).or_default()
    }

    pub fn load(&mut self, table: TableId, rel: Relation) {
        self.tables.insert(table, rel);
    }
}

impl Default for MemStore {
    fn default() -> Self {
        MemStore::new()
    }
}

impl OpCx for MemStore {
    fn scan(&mut self, req: &ScanRequest) -> Vec<(RowKey, Row)> {
        let Some(rel) = self.tables.get(&req.table) else {
            return Vec::new();
        };

        let mut rows: Vec<(RowKey, Row)> = rel
            .iter()
            .filter(|(_, row)| match &req.filter {
                Some(p) => p.matches(row, &req.params),
                None => true,
            })
            .map(|(k, r)| (k.clone(), r.clone()))
            .collect();

        rows.sort_by(|(ak, ar), (bk, br)| cmp_entry((ak, ar), (bk, br), &req.order));

        if let Some(cursor) = &req.after {
            // Exclusive lower bound. Linear here because this is the oracle;
            // the real store seeks the index.
            rows.retain(|(k, r)| cmp_to_cursor(k, r, cursor, &req.order).is_gt());
        }

        rows.truncate(req.limit);
        rows
    }

    fn note_refill(&mut self, kind: RefillKind, rows: usize) {
        self.refills.note(kind, rows);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delta::{Batch, Change};
    use crate::operator::Operator;
    use crate::ops::Source;
    use crate::order::Cursor;
    use crate::predicate::{CmpOp, Expr};
    use crate::value::Value;

    fn rel(rows: &[(i64, i64)]) -> Relation {
        rows.iter()
            .map(|(id, v)| {
                (
                    RowKey::from(*id),
                    Row::new(vec![Value::Int(*id), Value::Int(*v)]),
                )
            })
            .collect()
    }

    #[test]
    fn hydration_produces_the_full_relation_in_key_order() {
        let mut store = MemStore::new();
        store.load(7, rel(&[(3, 30), (1, 10), (2, 20)]));

        let mut source = Source::new(7, 0);
        let batch = source.hydrate(&mut store);

        let keys: Vec<_> = batch.iter().map(|c| c.key().clone()).collect();
        assert_eq!(
            keys,
            vec![RowKey::from(1), RowKey::from(2), RowKey::from(3)]
        );
        assert_eq!(
            Relation::new().with_applied(&batch),
            rel(&[(3, 30), (1, 10), (2, 20)])
        );
    }

    #[test]
    fn hydration_applies_pushdown_and_limit() {
        let mut store = MemStore::new();
        store.load(7, rel(&[(1, 10), (2, 20), (3, 30), (4, 40)]));

        let mut source = Source::new(7, 0)
            .with_pushdown(
                Predicate::Cmp {
                    lhs: Expr::Col(1),
                    op: CmpOp::Ge,
                    rhs: Expr::Lit(Value::Int(20)),
                },
                Params::empty(),
            )
            .with_limit(2);

        let batch = source.hydrate(&mut store);
        assert_eq!(
            Relation::new().with_applied(&batch),
            rel(&[(2, 20), (3, 30)])
        );
    }

    #[test]
    fn scan_after_is_an_exclusive_lower_bound() {
        let mut store = MemStore::new();
        store.load(7, rel(&[(1, 10), (2, 20), (3, 30)]));

        let rows = store.scan(&ScanRequest {
            table: 7,
            order: vec![(0, Dir::Asc)],
            after: Some(Cursor {
                sort_key: vec![Value::Int(1)],
                pk: RowKey::from(1),
            }),
            filter: None,
            params: Params::empty(),
            limit: 10,
        });
        let keys: Vec<_> = rows.iter().map(|(k, _)| k.clone()).collect();
        assert_eq!(keys, vec![RowKey::from(2), RowKey::from(3)]);
    }

    /// Rows tied on the sort key are split by the cursor's primary key, not
    /// dropped wholesale — the bug a bare sort-key bound would have.
    #[test]
    fn a_cursor_splits_a_run_of_ties() {
        let tied: Relation = (1i64..=3)
            .map(|id| {
                (
                    RowKey::from(id),
                    Row::new(vec![Value::Int(id), Value::Int(9)]),
                )
            })
            .collect();
        let mut store = MemStore::new();
        store.load(7, tied);

        let rows = store.scan(&ScanRequest {
            table: 7,
            order: vec![(1, Dir::Asc)],
            after: Some(Cursor {
                sort_key: vec![Value::Int(9)],
                pk: RowKey::from(2),
            }),
            filter: None,
            params: Params::empty(),
            limit: 10,
        });
        let keys: Vec<_> = rows.iter().map(|(k, _)| k.clone()).collect();
        assert_eq!(keys, vec![RowKey::from(3)]);
    }

    #[test]
    fn descending_order_reverses_the_scan() {
        let mut store = MemStore::new();
        store.load(7, rel(&[(1, 10), (2, 20), (3, 30)]));

        let rows = store.scan(&ScanRequest {
            table: 7,
            order: vec![(1, Dir::Desc)],
            after: None,
            filter: None,
            params: Params::empty(),
            limit: 2,
        });
        let keys: Vec<_> = rows.iter().map(|(k, _)| k.clone()).collect();
        assert_eq!(keys, vec![RowKey::from(3), RowKey::from(2)]);
    }

    #[test]
    fn a_scan_binds_parameters_in_a_pushed_down_filter() {
        let mut store = MemStore::new();
        store.load(7, rel(&[(1, 10), (2, 20), (3, 30)]));

        let rows = store.scan(&ScanRequest {
            table: 7,
            order: vec![(0, Dir::Asc)],
            after: None,
            filter: Some(Predicate::Cmp {
                lhs: Expr::Col(1),
                op: CmpOp::Ge,
                rhs: Expr::Param(0),
            }),
            params: Params::new(vec![Value::Int(20)]),
            limit: 10,
        });
        let keys: Vec<_> = rows.iter().map(|(k, _)| k.clone()).collect();
        assert_eq!(keys, vec![RowKey::from(2), RowKey::from(3)]);
    }

    #[test]
    fn nulls_sort_first_ascending() {
        let store_rel: Relation = [
            (
                RowKey::from(1),
                Row::new(vec![Value::Int(1), Value::Int(5)]),
            ),
            (RowKey::from(2), Row::new(vec![Value::Int(2), Value::Null])),
        ]
        .into_iter()
        .collect();
        let mut store = MemStore::new();
        store.load(7, store_rel);

        let rows = store.scan(&ScanRequest {
            table: 7,
            order: vec![(1, Dir::Asc)],
            after: None,
            filter: None,
            params: Params::empty(),
            limit: 10,
        });
        assert_eq!(rows[0].0, RowKey::from(2));
    }

    #[test]
    fn the_oracle_evaluates_top_k_by_sorting_the_whole_relation() {
        let plan = Reference::new(
            7,
            0,
            vec![Stage::TopK {
                order: vec![(1, Dir::Desc)],
                k: 2,
                slack: 0,
            }],
        );
        assert_eq!(
            plan.eval(&Tables::new().with(7, rel(&[(1, 10), (2, 40), (3, 30), (4, 20)]))),
            rel(&[(2, 40), (3, 30)])
        );
    }

    #[test]
    fn the_oracle_attaches_children_as_the_last_column() {
        let spec = JoinSpec {
            child_table: 8,
            child_pk: 0,
            parent_key: 0,
            child_fk: 1,
            child_filters: Vec::new(),
            order: vec![(1, Dir::Asc)],
            limit: 1,
            slack: 0,
        };
        let plan = Reference::new(7, 0, vec![Stage::Join(spec)]);
        let tables = Tables::new()
            .with(7, rel(&[(1, 10)]))
            // Children of parent 1: `[id, fk]` = (5, 1) and (6, 1).
            .with(8, rel(&[(5, 1), (6, 1), (9, 2)]));

        let out = plan.eval(&tables);
        let row = out.get(&RowKey::from(1)).expect("parent survives the join");
        assert_eq!(
            row.get(2),
            &Value::rows(vec![Row::new(vec![Value::Int(5), Value::Int(1)])]),
            "one child, the lowest-sorting of the two that match"
        );
    }

    #[test]
    #[should_panic(expected = "last stage")]
    fn a_join_may_not_have_stages_above_it() {
        Reference::new(
            7,
            0,
            vec![
                Stage::Join(JoinSpec {
                    child_table: 8,
                    child_pk: 0,
                    parent_key: 0,
                    child_fk: 1,
                    child_filters: Vec::new(),
                    order: vec![(1, Dir::Asc)],
                    limit: 1,
                    slack: 0,
                }),
                Stage::TopK {
                    order: vec![(1, Dir::Asc)],
                    k: 1,
                    slack: 0,
                },
            ],
        );
    }

    /// Filters ahead of a `TopK` become its refill pushdown. Without this the
    /// operator would refill from rows the filter had already excluded.
    #[test]
    fn build_pushes_upstream_filters_into_top_k() {
        let pred = Predicate::Cmp {
            lhs: Expr::Col(1),
            op: CmpOp::Ge,
            rhs: Expr::Param(0),
        };
        let plan = Reference::new(
            7,
            0,
            vec![
                Stage::Filter { pred },
                Stage::TopK {
                    order: vec![(1, Dir::Asc)],
                    k: 2,
                    slack: 0,
                },
            ],
        )
        .with_params(Params::new(vec![Value::Int(20)]));

        let base = Tables::new().with(7, rel(&[(1, 10), (2, 20), (3, 30), (4, 40)]));
        let mut store = MemStore::over(&base);

        let mut graph = plan.build();
        graph.hydrate(&mut store);

        // Drop row 2, forcing a refill. If the filter had not been pushed down
        // the refill would surface row 1, which the filter excludes.
        store.table(7).remove(&RowKey::from(2));
        let delta: Batch = [Change::Delete {
            key: RowKey::from(2),
            before: Row::new(vec![Value::Int(2), Value::Int(20)]),
        }]
        .into_iter()
        .collect();
        let out = graph.pump(&[(7, delta)], &mut store);

        let mut view = plan.eval(&base);
        view.apply(&out);
        assert_eq!(view, rel(&[(3, 30), (4, 40)]));
        assert_eq!(store.refills.window, 1);
    }
}
