//! The dataflow graph: operators wired together, pumped once per transaction.
//!
//! # Why a graph and not a chain
//!
//! A chain was enough while every operator had one input. `Join` has two, and
//! the moment one exists the shape of a query stops being a list. Two further
//! things in the plan need the same generality and get it for free here:
//! operator sharing (plan §1.3 — forty widgets subscribing to variants of
//! "issues in this project" share one `Source → Filter` prefix, so one node has
//! many consumers) and multi-table transactions (plan §1.4 — one commit feeds
//! several sources in a single coherent pump).
//!
//! # Topological order is a construction property, not a pass
//!
//! [`GraphBuilder::add`] can only reference nodes that already exist, so node
//! indices *are* a topological order and a cycle cannot be expressed. This is
//! not a limitation: DQL has no recursive CTE (plan §1.1), so a query's dataflow
//! is always acyclic. Not having a sort pass also means not having a "cycle
//! detected" error path to test, and not having to decide what a cycle would
//! even mean for incremental maintenance.

use crate::delta::Batch;
use crate::operator::{Inputs, OpCx, Operator};
use crate::schema::TableId;

/// A node's position in the graph. Only meaningful for the graph that made it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(usize);

struct Node {
    op: Box<dyn Operator>,
    /// `Some(table)` for a source — its port 0 is fed from outside the graph by
    /// whoever owns the write path. `None` for everything else.
    table: Option<TableId>,
    /// Upstream node per input port; empty for a source.
    inputs: Vec<NodeId>,
}

#[derive(Default)]
pub struct GraphBuilder {
    nodes: Vec<Node>,
}

impl GraphBuilder {
    pub fn new() -> Self {
        GraphBuilder::default()
    }

    /// Add a source: an operator whose input arrives from outside the graph,
    /// tagged with the table whose deltas it receives.
    ///
    /// The tag is what lets [`Graph::pump`] route by table instead of by node
    /// id. The engine knows which table it just wrote (plan §1.4); making it
    /// also know which node ids that maps to would be bookkeeping it should not
    /// have to keep.
    ///
    /// # Panics
    ///
    /// If `op` does not have exactly one input port.
    pub fn source(&mut self, table: TableId, op: Box<dyn Operator>) -> NodeId {
        assert_eq!(
            op.ports(),
            1,
            "a source must have exactly one input port, the external one"
        );
        self.push(Node {
            op,
            table: Some(table),
            inputs: Vec::new(),
        })
    }

    /// Add an operator fed by the given upstream nodes, one per input port.
    ///
    /// # Panics
    ///
    /// If `inputs` does not match the operator's port count, or references a
    /// node this builder did not produce.
    pub fn add(&mut self, op: Box<dyn Operator>, inputs: Vec<NodeId>) -> NodeId {
        assert_eq!(
            inputs.len(),
            op.ports(),
            "operator {} has {} input ports but was wired to {} upstream nodes",
            op.name(),
            op.ports(),
            inputs.len()
        );
        assert!(
            inputs.iter().all(|id| id.0 < self.nodes.len()),
            "an operator can only read from nodes added before it; that is what \
             makes node order a topological order"
        );
        self.push(Node {
            op,
            table: None,
            inputs,
        })
    }

    fn push(&mut self, node: Node) -> NodeId {
        self.nodes.push(node);
        NodeId(self.nodes.len() - 1)
    }

    /// Finish the graph, naming the node whose output is the query result.
    ///
    /// # Panics
    ///
    /// If `sink` is not a node of this builder.
    pub fn build(self, sink: NodeId) -> Graph {
        assert!(
            sink.0 < self.nodes.len(),
            "sink is not a node of this graph"
        );
        Graph {
            nodes: self.nodes,
            sink,
        }
    }
}

/// A wired dataflow graph, ready to be hydrated and pumped.
pub struct Graph {
    /// Topologically ordered by construction; see the module docs.
    nodes: Vec<Node>,
    sink: NodeId,
}

impl Graph {
    /// Initial contents of the view, as a batch of inserts.
    ///
    /// Sources read the store; everything downstream sees those rows through
    /// its ordinary [`Operator::apply`] path. There is deliberately no separate
    /// hydration path per operator — see the [`crate::operator`] module docs.
    pub fn hydrate(&mut self, cx: &mut dyn OpCx) -> Batch {
        self.run(cx, |node, cx| node.op.hydrate(cx))
    }

    /// One coherent pump: all of a transaction's table deltas at once.
    ///
    /// Sources not named in `deltas` receive an empty batch, and a node whose
    /// every input is empty is skipped entirely — which is what keeps a write
    /// to one table from costing anything in the pipelines that do not read it.
    /// (The real dispatcher narrows this further with a predicate index before
    /// a graph is ever pumped; plan §1.3.)
    pub fn pump(&mut self, deltas: &[(TableId, Batch)], cx: &mut dyn OpCx) -> Batch {
        self.run(cx, |node, _cx| {
            let table = node.table.expect("caller only invokes this for sources");
            deltas
                .iter()
                .find(|(t, _)| *t == table)
                .map(|(_, b)| b.clone())
                .unwrap_or_default()
        })
    }

    /// Walk every node in order, deriving source inputs with `source_input`.
    fn run(
        &mut self,
        cx: &mut dyn OpCx,
        mut source_input: impl FnMut(&mut Node, &mut dyn OpCx) -> Batch,
    ) -> Batch {
        let mut outputs: Vec<Batch> = vec![Batch::new(); self.nodes.len()];

        for i in 0..self.nodes.len() {
            if self.nodes[i].table.is_some() {
                outputs[i] = source_input(&mut self.nodes[i], cx);
                continue;
            }

            let ports: Vec<Batch> = self.nodes[i]
                .inputs
                .iter()
                .map(|id| outputs[id.0].clone())
                .collect();

            // No input change means no output change — the trait guarantees it
            // (see `Operator::apply`), so calling the operator would only burn
            // time to learn what we already know.
            if ports.iter().all(Batch::is_empty) {
                continue;
            }

            outputs[i] = self.nodes[i].op.apply(Inputs::new(&ports), cx);
        }

        std::mem::take(&mut outputs[self.sink.0])
    }

    /// Total retained state across every operator, for memory accounting.
    ///
    /// Reported so a state explosion shows up as a number before it shows up as
    /// an OOM (plan §7, mitigation 3).
    pub fn state_bytes(&self) -> usize {
        self.nodes.iter().map(|n| n.op.state_bytes()).sum()
    }

    /// Retained state per node, in topological order.
    ///
    /// The sum is [`Graph::state_bytes`]; *which* operator is holding it is the
    /// part that tells you what to do about it. Plan §7 asks for per-operator
    /// state size as a first-class metric from the first week, and a total
    /// alone cannot distinguish a join fanning out from a window that grew its
    /// slack — the first is a bug, the second is the design working.
    pub fn state_report(&self) -> Vec<(&'static str, usize)> {
        self.nodes
            .iter()
            .map(|n| (n.op.name(), n.op.state_bytes()))
            .collect()
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Operator names in topological order, for tests and `solstice inspect`.
    pub fn node_names(&self) -> Vec<&'static str> {
        self.nodes.iter().map(|n| n.op.name()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delta::Change;
    use crate::ops::{Filter, Source};
    use crate::predicate::{CmpOp, Expr, Params, Predicate};
    use crate::reference::MemStore;
    use crate::relation::Relation;
    use crate::value::{Row, RowKey, Value};

    const ISSUES: TableId = 1;
    const COMMENTS: TableId = 2;

    fn row(id: i64, n: i64) -> (RowKey, Row) {
        let row = Row::new(vec![Value::Int(id), Value::Int(n)]);
        (RowKey::from(id), row)
    }

    fn rel(rows: Vec<(RowKey, Row)>) -> Relation {
        rows.into_iter().collect()
    }

    fn inserts(rows: Vec<(RowKey, Row)>) -> Batch {
        rows.into_iter()
            .map(|(key, row)| Change::Insert { key, row })
            .collect()
    }

    /// A two-input operator, so the graph's port wiring is exercised before
    /// `Join` exists to exercise it for real.
    struct Union;

    impl Operator for Union {
        fn name(&self) -> &'static str {
            "Union"
        }

        fn ports(&self) -> usize {
            2
        }

        fn apply(&mut self, input: Inputs<'_>, _cx: &mut dyn OpCx) -> Batch {
            let mut out = input.port(0).clone();
            for change in input.port(1).iter() {
                out.push(change.clone());
            }
            out
        }
    }

    fn even_only() -> Predicate {
        Predicate::Cmp {
            lhs: Expr::Col(1),
            op: CmpOp::Ge,
            rhs: Expr::Lit(Value::Int(10)),
        }
    }

    #[test]
    fn a_chain_hydrates_from_the_store_through_every_operator() {
        let mut store = MemStore::new();
        store.load(ISSUES, rel(vec![row(1, 5), row(2, 20), row(3, 30)]));

        let mut b = GraphBuilder::new();
        let src = b.source(ISSUES, Box::new(Source::new(ISSUES, 0)));
        let filtered = b.add(
            Box::new(Filter::new(even_only(), Params::empty())),
            vec![src],
        );
        let mut graph = b.build(filtered);

        let out = graph.hydrate(&mut store);

        assert_eq!(graph.node_names(), vec!["Source", "Filter"]);
        assert_eq!(out.len(), 2);
        assert!(out.get(&RowKey::from(1i64)).is_none());
    }

    #[test]
    fn a_pump_routes_each_table_to_its_own_source() {
        let mut store = MemStore::new();
        store.load(ISSUES, Relation::new());
        store.load(COMMENTS, Relation::new());

        let mut b = GraphBuilder::new();
        let issues = b.source(ISSUES, Box::new(Source::new(ISSUES, 0)));
        let comments = b.source(COMMENTS, Box::new(Source::new(COMMENTS, 0)));
        let both = b.add(Box::new(Union), vec![issues, comments]);
        let mut graph = b.build(both);

        let (ik, ir) = row(1, 1);
        let (ck, cr) = row(7, 7);
        let out = graph.pump(
            &[
                (ISSUES, inserts(vec![(ik, ir)])),
                (COMMENTS, inserts(vec![(ck, cr)])),
            ],
            &mut store,
        );

        assert_eq!(out.len(), 2);
    }

    #[test]
    fn a_source_with_no_delta_this_pump_contributes_nothing() {
        let mut store = MemStore::new();
        store.load(ISSUES, Relation::new());
        store.load(COMMENTS, Relation::new());

        let mut b = GraphBuilder::new();
        let issues = b.source(ISSUES, Box::new(Source::new(ISSUES, 0)));
        let comments = b.source(COMMENTS, Box::new(Source::new(COMMENTS, 0)));
        let both = b.add(Box::new(Union), vec![issues, comments]);
        let mut graph = b.build(both);

        let (ik, ir) = row(1, 1);
        let out = graph.pump(&[(ISSUES, inserts(vec![(ik, ir)]))], &mut store);

        assert_eq!(out.len(), 1);
    }

    #[test]
    fn an_idle_pump_produces_nothing() {
        let mut store = MemStore::new();
        store.load(ISSUES, rel(vec![row(1, 99)]));

        let mut b = GraphBuilder::new();
        let src = b.source(ISSUES, Box::new(Source::new(ISSUES, 0)));
        let filtered = b.add(
            Box::new(Filter::new(even_only(), Params::empty())),
            vec![src],
        );
        let mut graph = b.build(filtered);

        graph.hydrate(&mut store);
        assert!(graph.pump(&[], &mut store).is_empty());
    }

    #[test]
    fn state_is_reported_per_operator_not_just_as_a_total() {
        let mut store = MemStore::new();
        store.load(ISSUES, rel(vec![row(1, 20), row(2, 30)]));

        let mut b = GraphBuilder::new();
        let src = b.source(ISSUES, Box::new(Source::new(ISSUES, 0)));
        let topk = b.add(
            Box::new(crate::ops::TopK::new(
                ISSUES,
                vec![(1, crate::order::Dir::Asc)],
                2,
            )),
            vec![src],
        );
        let mut graph = b.build(topk);
        graph.hydrate(&mut store);

        let report = graph.state_report();
        assert_eq!(report.len(), 2);
        assert_eq!(report[0], ("Source", 0), "a source retains nothing");
        assert!(report[1].1 > 0, "the window is holding two rows");
        assert_eq!(
            report.iter().map(|(_, b)| b).sum::<usize>(),
            graph.state_bytes()
        );
    }

    #[test]
    #[should_panic(expected = "input ports")]
    fn wiring_the_wrong_number_of_inputs_is_caught_at_construction() {
        let mut b = GraphBuilder::new();
        let src = b.source(ISSUES, Box::new(Source::new(ISSUES, 0)));
        b.add(Box::new(Union), vec![src]);
    }
}
