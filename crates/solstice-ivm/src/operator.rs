//! The operator interface every dataflow node implements.
//!
//! # Hydration is just a big insert batch
//!
//! There is exactly *one* code path through every operator: [`Operator::apply`].
//! Sources produce their initial contents via [`Operator::hydrate`], and every
//! downstream operator receives those contents as an ordinary batch of inserts.
//!
//! That symmetry is load-bearing. If hydration had its own path, it could drift
//! from the incremental path, and the resulting bug — a view that is correct on
//! first load and subtly wrong after an edit, or vice versa — is exactly what
//! the triple-oracle property tests exist to catch (plan §6). Not having two
//! paths is cheaper than testing that two paths agree.
//!
//! # Degradation is a runtime mode, not an emergency
//!
//! [`Operator::degrade`] lets an operator drop its state and fall back to
//! bounded requery with identical semantics and worse latency (plan §1.5, §7).
//! The engine never OOMs; it slows down and says so. This is the same mechanism
//! that lets operators ship incrementally, which is why it is in the trait from
//! the first commit rather than bolted on at M5.

use crate::delta::Batch;
use crate::order::{Cursor, Dir};
use crate::predicate::{Params, Predicate};
use crate::schema::TableId;
use crate::value::{ColId, Row, RowKey};

/// An ordered, bounded read against the canonical store.
///
/// This is the *only* shape of query an operator may issue at runtime. The
/// bound is what keeps a `TopK` refill O(limit) instead of O(table), and
/// keeping it in one type means the cost of every requery in the engine is
/// visible in one place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanRequest {
    pub table: TableId,
    /// Sort order; must be an indexed prefix (plan §1.1).
    pub order: Vec<(ColId, Dir)>,
    /// Exclusive lower bound — the position of the last row the caller already
    /// holds. `None` scans from the start.
    ///
    /// A [`Cursor`] rather than a bare sort key because sort keys tie; see the
    /// [`crate::order`] module docs for what ties cost.
    pub after: Option<Cursor>,
    /// Filter pushed down from upstream, so a refill does not return rows the
    /// caller would immediately discard.
    pub filter: Option<Predicate>,
    /// Bindings for any `Param` in `filter`.
    ///
    /// Carried explicitly because a pushed-down predicate that reads an unbound
    /// parameter evaluates it as NULL, which makes the comparison unknown and
    /// the row invisible — a scan that silently returns too few rows rather
    /// than failing.
    pub params: Params,
    pub limit: usize,
}

/// Services an operator needs from the engine but cannot provide itself.
///
/// Passing this in rather than letting operators hold a store handle is what
/// keeps `solstice-ivm` free of IO and time (plan §5), and therefore what makes the
/// whole engine simulatable.
pub trait OpCx {
    /// Run a bounded scan against the canonical store.
    ///
    /// # Contract: the scan sees the batch being processed
    ///
    /// When called from [`Operator::apply`], the returned rows **must** reflect
    /// every change in the batch currently being applied. The engine commits the
    /// store transaction and only then pumps the graph (plan §1.4), so this
    /// holds by construction.
    ///
    /// It has to be stated because violating it is silent: a `TopK` refilling
    /// after a delete would read the row it just deleted back out of the store
    /// and re-insert it into the view. Nothing would panic; the list would
    /// simply be wrong.
    fn scan(&mut self, req: &ScanRequest) -> Vec<(RowKey, Row)>;

    /// Report that a bounded requery happened, so it surfaces in
    /// `stats.requeries_per_sec` instead of hiding as unexplained latency.
    fn note_refill(&mut self, _rows: usize) {}
}

/// An [`OpCx`] for stateless operators, which never scan.
///
/// It panics rather than returning empty: silently answering a refill with no
/// rows would look like correct-but-empty output, which is far harder to debug
/// than a loud failure.
pub struct NoCx;

impl OpCx for NoCx {
    fn scan(&mut self, req: &ScanRequest) -> Vec<(RowKey, Row)> {
        panic!(
            "NoCx cannot serve a scan of table {}; this operator needs a real store context",
            req.table
        );
    }
}

/// Which input an operator is reading from.
///
/// Almost every operator has exactly one. `Join` has two — parent on port 0,
/// child on port 1 — and the ports are not interchangeable, so they are named
/// by index rather than merged into one stream with a tag.
pub type Port = usize;

/// The batches arriving at an operator's input ports in one pump.
///
/// A single value rather than one `apply` call per port because a transaction
/// that touches several tables must produce **one** coherent pump (plan §1.4).
/// A two-input operator has to see both sides of that transaction together to
/// emit a single coherent delta; calling it once per port would let it emit an
/// intermediate state that never existed.
#[derive(Clone, Copy)]
pub struct Inputs<'a> {
    ports: &'a [Batch],
}

impl<'a> Inputs<'a> {
    /// The graph supplies exactly [`Operator::ports`] batches, one per port.
    pub fn new(ports: &'a [Batch]) -> Self {
        Inputs { ports }
    }

    /// The sole input of a single-input operator.
    pub fn single(batch: &'a Batch) -> Self {
        Inputs {
            ports: std::slice::from_ref(batch),
        }
    }

    /// Port 0 — the only input for most operators, the parent side for `Join`.
    pub fn primary(&self) -> &'a Batch {
        self.port(0)
    }

    /// # Panics
    ///
    /// If `port` is out of range. That means the graph wired fewer inputs than
    /// the operator declares, which is a construction bug, not a runtime
    /// condition — and one that would otherwise show up as a view that is
    /// quietly missing one side of a join.
    pub fn port(&self, port: Port) -> &'a Batch {
        &self.ports[port]
    }

    pub fn len(&self) -> usize {
        self.ports.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ports.is_empty()
    }

    /// True when no port carries a change.
    pub fn all_empty(&self) -> bool {
        self.ports.iter().all(Batch::is_empty)
    }
}

/// What happened when an operator was asked to shed state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Degrade {
    /// Nothing to shed — the operator holds no state.
    Stateless,
    /// State dropped; the operator now answers by bounded requery.
    Shed { freed_bytes: usize },
    /// Cannot shed without losing correctness.
    Pinned,
}

pub trait Operator: Send {
    /// Stable name for stats, tracing, and `solstice inspect`.
    fn name(&self) -> &'static str;

    /// How many input ports this operator reads. One unless stated otherwise.
    fn ports(&self) -> usize {
        1
    }

    /// The incremental step: given an upstream delta, produce this operator's
    /// delta.
    ///
    /// # Contract: no spontaneous output
    ///
    /// If every input port is empty, the output must be empty. Operators may
    /// not emit changes nothing upstream asked for — that is what lets the
    /// graph skip idle nodes, and more importantly it is what makes a view's
    /// contents a function of its inputs rather than of how often it was
    /// pumped.
    fn apply(&mut self, input: Inputs<'_>, cx: &mut dyn OpCx) -> Batch;

    /// Initial contents as a batch of inserts. Only sources override this;
    /// see the module docs.
    fn hydrate(&mut self, _cx: &mut dyn OpCx) -> Batch {
        Batch::new()
    }

    /// Bytes of retained state, for memory accounting. Reported per operator so
    /// that a state explosion is visible as a number before it is visible as an
    /// OOM (plan §7, mitigation 3).
    fn state_bytes(&self) -> usize {
        0
    }

    fn degrade(&mut self) -> Degrade {
        Degrade::Stateless
    }
}
