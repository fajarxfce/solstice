//! Query IR to dataflow graph: the one place names become ids.
//!
//! # What compiling actually decides
//!
//! Resolution is the boring half. The half that matters is the **shape**, and
//! there is exactly one shape M0 ships:
//!
//! ```text
//! Source → Filter → TopK → Join(1:N)
//! ```
//!
//! with the window *below* the join. Plan §7 names the alternative as the most
//! likely way this project fails technically: put `TopK` above a join and
//! refilling the window means asking the join for children of parents it is not
//! holding, which forces it to hold children for every row in the table. Below
//! the join, it only ever holds children for parents that are on screen. The
//! reason that reordering is legal at all is that a 1:N traversal in DQL widens
//! the parent rather than multiplying it (plan §1.1), so parent cardinality is
//! unchanged and `ORDER BY ... LIMIT` commutes with the join.
//!
//! # Why a query can be rejected
//!
//! Every rule in [`CompileError`] is a rule about *bounded work*. A limit with
//! no order is a window with no defined contents; a list query with no limit is
//! the memory blow-up arriving during the first frame. Plan §1.1 puts it as the
//! product being the restriction: rejecting these at subscribe is what lets a
//! developer reason about the memory bound at all.
//!
//! Rejections that are about this *build* rather than about the language are
//! [`CompileError::NotYet`], and they say so, because "rewrite your query" and
//! "wait for M1" are different advice.

use solstice_ivm::ops::{Filter, Join1N, Source, TopK};
use solstice_ivm::{
    ColId, Dir, Expr, Graph, GraphBuilder, NodeId, Params, Predicate, Schema, TableId,
};
use solstice_proto::query as ir;

use crate::catalog::Catalog;

/// A query rejected before it ever touched the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompileError {
    UnknownTable(String),
    UnknownColumn {
        table: String,
        col: String,
    },
    UnknownRelation {
        table: String,
        rel: String,
    },
    /// A `related` clause whose sub-query names a table the relation does not
    /// traverse — the IR contradicting itself.
    RelationTable {
        rel: String,
        expected: String,
        got: String,
    },
    /// A list query with no `limit` and no `allow_unbounded`. Plan §1.1 makes
    /// the limit mandatory for exactly this reason.
    Unbounded {
        table: String,
    },
    /// A `limit` with no `order_by`: a window with no defined contents, which
    /// would return *some* k rows and call them the top k.
    UnorderedWindow {
        table: String,
    },
    /// Plan §1.1 caps the sort at three columns, because each one is an index
    /// column the store has to carry.
    TooManyOrderColumns {
        table: String,
        n: usize,
    },
    /// A `$n` the query never bound. Left alone it would evaluate as NULL,
    /// making every comparison unknown and the view silently empty.
    UnboundParam {
        index: u32,
        bound: usize,
    },
    /// In the language, not in this build.
    NotYet(&'static str),
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompileError::UnknownTable(t) => write!(f, "no table named {t:?}"),
            CompileError::UnknownColumn { table, col } => {
                write!(f, "no column {col:?} in table {table:?}")
            }
            CompileError::UnknownRelation { table, rel } => {
                write!(f, "table {table:?} declares no relation {rel:?}")
            }
            CompileError::RelationTable { rel, expected, got } => write!(
                f,
                "relation {rel:?} traverses to {expected:?}, but the sub-query names {got:?}"
            ),
            CompileError::Unbounded { table } => write!(
                f,
                "query on {table:?} has no limit; set one or set allow_unbounded"
            ),
            CompileError::UnorderedWindow { table } => write!(
                f,
                "query on {table:?} has a limit but no order_by, so the top k is undefined"
            ),
            CompileError::TooManyOrderColumns { table, n } => {
                write!(
                    f,
                    "query on {table:?} orders by {n} columns; the limit is 3"
                )
            }
            CompileError::UnboundParam { index, bound } => {
                write!(f, "query reads ${index} but binds only {bound} parameters")
            }
            CompileError::NotYet(what) => write!(f, "not implemented in this build: {what}"),
        }
    }
}

impl std::error::Error for CompileError {}

/// Plan §1.1: at most three sort columns.
const MAX_ORDER_COLUMNS: usize = 3;

/// A wired graph, plus the parts of it callers have to ask questions about.
pub struct Compiled {
    pub graph: Graph,
    pub table: TableId,
    /// Primary key of the root table — the key positional diffs are keyed by.
    pub pk: ColId,
    pub filter: Predicate,
    /// One binding list for the whole pipeline, root and children alike.
    pub params: Params,
    pub order: Vec<(ColId, Dir)>,
    /// Rows the source reads at hydration, or `None` for an unbounded query.
    ///
    /// The headline number: this is what stands between the first frame and the
    /// whole table.
    pub hydrate_limit: Option<usize>,
    pub children: Vec<ChildPlan>,
}

/// One traversed relation, as the graph runs it.
pub struct ChildPlan {
    pub rel: String,
    pub table: TableId,
    pub order: Vec<(ColId, Dir)>,
    pub limit: usize,
}

pub fn compile(cat: &Catalog, q: &ir::Query) -> Result<Compiled, CompileError> {
    let schema = cat
        .table(&q.table)
        .ok_or_else(|| CompileError::UnknownTable(q.table.clone()))?;
    let bound = q.params.len();
    let params = Params::new(q.params.clone());

    let filter = match &q.filter {
        None => Predicate::True,
        Some(p) => resolve_pred(schema, p, bound)?,
    };
    let order = resolve_order(schema, &q.order_by)?;
    let k = window(&q.table, q.limit, q.allow_unbounded, &order)?;

    let mut b = GraphBuilder::new();

    // Hydration is bounded here or it is not bounded at all. A `Source` with no
    // limit reads the whole table, and feeding 100k issues through the graph to
    // keep 50 is the memory blow-up arriving during the first frame. Because
    // the source can scan in the window's own order, it only has to read
    // `k + slack + 1` rows — and the `+ 1` is load bearing: a `TopK` learns
    // that rows exist below its window only by discarding one, so a scan of
    // exactly `k + slack` would leave it believing it holds the whole relation
    // and it would never refill.
    let mut source =
        Source::new(schema.table, schema.pk).with_pushdown(filter.clone(), params.clone());
    let mut topk = None;
    let mut hydrate_limit = None;
    if let Some(k) = k {
        let t =
            TopK::new(schema.table, order.clone(), k).with_pushdown(filter.clone(), params.clone());
        let limit = k + t.slack() + 1;
        source = source.with_order(order.clone()).with_limit(limit);
        hydrate_limit = Some(limit);
        topk = Some(t);
    }

    let n_source = b.source(schema.table, Box::new(source));
    let mut node = b.add(
        Box::new(Filter::new(filter.clone(), params.clone())),
        vec![n_source],
    );
    if let Some(t) = topk {
        node = b.add(Box::new(t), vec![node]);
    }

    let mut children = Vec::new();
    for related in &q.related {
        node = join(
            cat,
            &mut b,
            node,
            schema,
            related,
            &params,
            bound,
            &mut children,
        )?;
    }

    Ok(Compiled {
        graph: b.build(node),
        table: schema.table,
        pk: schema.pk,
        filter,
        params,
        order,
        hydrate_limit,
        children,
    })
}

/// Add one `Join(1:N)` on top of `parent`, returning the new sink.
#[allow(clippy::too_many_arguments)]
fn join(
    cat: &Catalog,
    b: &mut GraphBuilder,
    parent: NodeId,
    parent_schema: &Schema,
    related: &ir::Related,
    params: &Params,
    bound: usize,
    children: &mut Vec<ChildPlan>,
) -> Result<NodeId, CompileError> {
    let rel = cat
        .rel(parent_schema.table, &related.rel_name)
        .ok_or_else(|| CompileError::UnknownRelation {
            table: parent_schema.name.clone(),
            rel: related.rel_name.clone(),
        })?;
    let child = cat
        .by_id(rel.child)
        .expect("a catalog names only tables it holds");
    let sub = &related.sub;

    if !sub.table.is_empty() && sub.table != child.name {
        return Err(CompileError::RelationTable {
            rel: rel.name.clone(),
            expected: child.name.clone(),
            got: sub.table.clone(),
        });
    }
    if !sub.related.is_empty() {
        return Err(CompileError::NotYet("relations nested inside relations"));
    }
    // One binding list per pipeline, held by the root. `$0` therefore means the
    // same value everywhere in the query, which is what lets a child filter
    // reference a parameter at all — and what makes plan §1.2's `ViewId` a
    // function of one params list rather than a tree of them.
    if !sub.params.is_empty() {
        return Err(CompileError::NotYet(
            "a sub-query with its own parameters; bind them on the root",
        ));
    }

    let order = resolve_order(child, &sub.order_by)?;
    // No `allow_unbounded` escape here, unlike the root. Plan §1.1 makes the
    // limit mandatory on a 1:N traversal specifically, because the fan-out is
    // per parent: one unbounded child window is as many unbounded scans as
    // there are rows in the window above it.
    if sub.limit == 0 {
        return Err(CompileError::Unbounded {
            table: child.name.clone(),
        });
    }
    if order.is_empty() {
        return Err(CompileError::UnorderedWindow {
            table: child.name.clone(),
        });
    }

    let child_filter = match &sub.filter {
        None | Some(ir::Predicate::True) => None,
        Some(p) => Some(resolve_pred(child, p, bound)?),
    };

    let n_child = b.source(
        rel.child,
        // Deltas only: hydrating a million comments so the join can keep three
        // per issue is the mistake this operator exists to avoid.
        Box::new(Source::new(rel.child, child.pk).deltas_only()),
    );

    let mut op = Join1N::new(
        rel.child,
        rel.parent_col,
        rel.child_col,
        order.clone(),
        sub.limit as usize,
    );
    // A child filter needs saying twice, and only one of the two is visible in
    // the dataflow: the `Filter` node excludes rows from the child *stream*,
    // `with_child_filter` excludes them from the per-parent refill *scans*.
    // Omit the second and a refill resurrects rows the graph had excluded.
    let child_stream = match child_filter {
        None => n_child,
        Some(p) => {
            op = op.with_child_filter(p.clone(), params.clone());
            b.add(Box::new(Filter::new(p, params.clone())), vec![n_child])
        }
    };

    children.push(ChildPlan {
        rel: rel.name.clone(),
        table: rel.child,
        order,
        limit: sub.limit as usize,
    });
    Ok(b.add(Box::new(op), vec![parent, child_stream]))
}

/// `Some(k)` for a window, `None` for a query that asked to be unbounded.
fn window(
    table: &str,
    limit: u32,
    allow_unbounded: bool,
    order: &[(ColId, Dir)],
) -> Result<Option<usize>, CompileError> {
    if limit == 0 {
        return if allow_unbounded {
            Ok(None)
        } else {
            Err(CompileError::Unbounded {
                table: table.to_string(),
            })
        };
    }
    if order.is_empty() {
        return Err(CompileError::UnorderedWindow {
            table: table.to_string(),
        });
    }
    Ok(Some(limit as usize))
}

fn resolve_order(
    schema: &Schema,
    order_by: &[ir::Order],
) -> Result<Vec<(ColId, Dir)>, CompileError> {
    if order_by.len() > MAX_ORDER_COLUMNS {
        return Err(CompileError::TooManyOrderColumns {
            table: schema.name.clone(),
            n: order_by.len(),
        });
    }
    order_by
        .iter()
        .map(|o| {
            Ok((
                col(schema, &o.col)?,
                if o.desc { Dir::Desc } else { Dir::Asc },
            ))
        })
        .collect()
}

fn resolve_pred(
    schema: &Schema,
    p: &ir::Predicate,
    bound: usize,
) -> Result<Predicate, CompileError> {
    let each = |list: &[ir::Predicate]| -> Result<Vec<Predicate>, CompileError> {
        list.iter()
            .map(|p| resolve_pred(schema, p, bound))
            .collect()
    };
    Ok(match p {
        ir::Predicate::True => Predicate::True,
        ir::Predicate::Cmp { lhs, op, rhs } => Predicate::Cmp {
            lhs: resolve_expr(schema, lhs, bound)?,
            op: cmp_op(*op),
            rhs: resolve_expr(schema, rhs, bound)?,
        },
        ir::Predicate::In { lhs, list } => Predicate::In {
            lhs: resolve_expr(schema, lhs, bound)?,
            list: list.clone(),
        },
        ir::Predicate::Between { lhs, low, high } => Predicate::Between {
            lhs: resolve_expr(schema, lhs, bound)?,
            low: resolve_expr(schema, low, bound)?,
            high: resolve_expr(schema, high, bound)?,
        },
        ir::Predicate::IsNull(e) => Predicate::IsNull(resolve_expr(schema, e, bound)?),
        ir::Predicate::IsNotNull(e) => Predicate::IsNotNull(resolve_expr(schema, e, bound)?),
        ir::Predicate::LikePrefix { lhs, prefix } => Predicate::LikePrefix {
            lhs: resolve_expr(schema, lhs, bound)?,
            prefix: prefix.as_str().into(),
        },
        ir::Predicate::And(list) => Predicate::and(each(list)?),
        ir::Predicate::Or(list) => Predicate::or(each(list)?),
        ir::Predicate::Not(inner) => Predicate::negate(resolve_pred(schema, inner, bound)?),
    })
}

fn resolve_expr(schema: &Schema, e: &ir::Expr, bound: usize) -> Result<Expr, CompileError> {
    Ok(match e {
        ir::Expr::Col(name) => Expr::Col(col(schema, name)?),
        ir::Expr::Lit(v) => Expr::Lit(v.clone()),
        ir::Expr::Param(i) => {
            let idx = u16::try_from(*i).ok().filter(|_| (*i as usize) < bound);
            Expr::Param(idx.ok_or(CompileError::UnboundParam { index: *i, bound })?)
        }
    })
}

fn col(schema: &Schema, name: &str) -> Result<ColId, CompileError> {
    schema.col(name).ok_or_else(|| CompileError::UnknownColumn {
        table: schema.name.clone(),
        col: name.to_string(),
    })
}

fn cmp_op(op: ir::CmpOp) -> solstice_ivm::CmpOp {
    use solstice_ivm::CmpOp as O;
    match op {
        ir::CmpOp::Eq => O::Eq,
        ir::CmpOp::Ne => O::Ne,
        ir::CmpOp::Lt => O::Lt,
        ir::CmpOp::Le => O::Le,
        ir::CmpOp::Gt => O::Gt,
        ir::CmpOp::Ge => O::Ge,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::m0;
    use solstice_ivm::Value;

    fn q() -> ir::Query {
        m0::query(7, 50, 3)
    }

    fn err(q: &ir::Query) -> CompileError {
        compile(&m0::catalog(), q)
            .err()
            .expect("expected a rejection")
    }

    #[test]
    fn the_m0_query_compiles_to_the_four_operators_m0_ships() {
        let c = compile(&m0::catalog(), &q()).unwrap();
        assert_eq!(
            c.graph.node_names(),
            vec!["Source", "Filter", "TopK", "Source", "Join1N"]
        );
    }

    #[test]
    fn hydration_is_bounded_by_the_window_and_not_the_table() {
        // `k + slack + 1`, with slack `max(16, k/4)` — the claim the first frame
        // rests on, asserted as a bound rather than a formula so that a change
        // to `slack` does not have to be restated here.
        let c = compile(&m0::catalog(), &q()).unwrap();
        let limit = c.hydrate_limit.expect("a window query has a limit");
        assert!(
            limit > 50,
            "a scan of exactly k can never learn it is short"
        );
        assert!(limit < 200, "hydration read {limit} rows to fill 50");
    }

    #[test]
    fn a_limit_with_no_order_is_a_window_with_no_contents() {
        let mut q = q();
        q.order_by.clear();
        assert_eq!(
            err(&q),
            CompileError::UnorderedWindow {
                table: "issues".into()
            }
        );
    }

    #[test]
    fn a_list_query_with_no_limit_has_to_say_it_means_it() {
        let mut q = q();
        q.limit = 0;
        assert_eq!(
            err(&q),
            CompileError::Unbounded {
                table: "issues".into()
            }
        );

        q.allow_unbounded = true;
        let c = compile(&m0::catalog(), &q).unwrap();
        assert_eq!(c.hydrate_limit, None);
        // No window, so no `TopK`: the operator that bounds memory is gone, and
        // the query said that is what it wanted.
        assert_eq!(
            c.graph.node_names(),
            vec!["Source", "Filter", "Source", "Join1N"]
        );
    }

    #[test]
    fn a_child_window_is_mandatory_and_allow_unbounded_does_not_rescue_it() {
        // The asymmetry with the root is the point. One unbounded root scan is
        // one scan; one unbounded child window is a scan per row on screen.
        let mut q = q();
        q.related[0].sub.limit = 0;
        q.related[0].sub.allow_unbounded = true;
        assert_eq!(
            err(&q),
            CompileError::Unbounded {
                table: "comments".into()
            }
        );
    }

    #[test]
    fn a_column_that_does_not_exist_is_named_in_the_error() {
        let mut q = q();
        q.order_by[0].col = "prioritee".into();
        assert_eq!(
            err(&q),
            CompileError::UnknownColumn {
                table: "issues".into(),
                col: "prioritee".into()
            }
        );
    }

    #[test]
    fn a_param_the_query_never_bound_is_refused_rather_than_read_as_null() {
        // Left alone this is the worst kind of failure: NULL makes every
        // comparison unknown, the filter matches nothing, and the app shows an
        // empty list with no error anywhere.
        let mut q = q();
        q.params.clear();
        assert_eq!(err(&q), CompileError::UnboundParam { index: 0, bound: 0 });
    }

    #[test]
    fn a_sort_longer_than_three_columns_is_refused() {
        let mut q = q();
        q.order_by = ["priority", "updated_at", "closed", "id"]
            .iter()
            .map(|c| ir::Order {
                col: c.to_string(),
                desc: false,
            })
            .collect();
        assert_eq!(
            err(&q),
            CompileError::TooManyOrderColumns {
                table: "issues".into(),
                n: 4
            }
        );
    }

    #[test]
    fn an_undeclared_relation_cannot_be_traversed() {
        // The join column is not something a query gets to choose (plan §1.1) —
        // it is declared in the schema, or there is no join.
        let mut q = q();
        q.related[0].rel_name = "attachments".into();
        assert_eq!(
            err(&q),
            CompileError::UnknownRelation {
                table: "issues".into(),
                rel: "attachments".into()
            }
        );
    }

    #[test]
    fn a_sub_query_that_contradicts_its_relation_is_refused() {
        let mut q = q();
        q.related[0].sub.table = "issues".into();
        assert_eq!(
            err(&q),
            CompileError::RelationTable {
                rel: "comments".into(),
                expected: "comments".into(),
                got: "issues".into()
            }
        );
    }

    #[test]
    fn depth_two_says_it_is_a_build_limit_and_not_a_language_one() {
        let mut q = q();
        let inner = q.related[0].clone();
        q.related[0].sub.related = vec![inner];
        assert!(matches!(err(&q), CompileError::NotYet(_)));
    }

    #[test]
    fn a_filter_on_a_child_is_said_twice_because_only_one_of_them_shows() {
        let mut q = q();
        q.related[0].sub.filter = Some(ir::Predicate::Cmp {
            lhs: ir::Expr::Col("author".into()),
            op: ir::CmpOp::Eq,
            rhs: ir::Expr::Lit(Value::text("ana")),
        });
        let c = compile(&m0::catalog(), &q).unwrap();
        // The visible half: a `Filter` on the child stream. The invisible half
        // is the same predicate inside `Join1N`, for refill pushdown, and the
        // only thing that could check it here is the graph's own tests.
        assert_eq!(
            c.graph.node_names(),
            vec!["Source", "Filter", "TopK", "Source", "Filter", "Join1N"]
        );
    }

    #[test]
    fn a_query_with_no_filter_and_an_explicit_true_compile_the_same() {
        let mut none = q();
        none.filter = None;
        let mut yes = q();
        yes.filter = Some(ir::Predicate::True);
        let cat = m0::catalog();
        assert_eq!(
            compile(&cat, &none).unwrap().filter,
            compile(&cat, &yes).unwrap().filter
        );
    }
}
