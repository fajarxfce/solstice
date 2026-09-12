//! `Source` — the only operator that reads the store.

use crate::delta::{Batch, Change};
use crate::operator::{Inputs, OpCx, Operator, ScanRequest};
use crate::order::Dir;
use crate::predicate::{Params, Predicate};
use crate::schema::TableId;
use crate::value::ColId;

/// The entry point of a pipeline: one table's rows.
///
/// # Why `apply` is a pass-through
///
/// A source does not decide which changes are its own. The engine owns the
/// single write chokepoint (`apply_write`, plan §1.4) and therefore already
/// knows which table each delta belongs to, so it routes deltas to the sources
/// registered for that table. Re-checking here would mean tagging every
/// [`Change`] with a table id — cost paid on every row, to re-derive something
/// the caller already knew.
///
/// The source's real job is [`Operator::hydrate`]: turning the store's current
/// contents into the batch of inserts that every downstream operator then
/// processes through its ordinary incremental path.
///
/// # SQLite is not re-run on invalidation
///
/// This is the invariant the whole design rests on. After hydration the
/// pipeline sees deltas only. `hydration` reads canonical tables; the overlay
/// of pending optimistic mutations is merged in by the engine as a delta, which
/// keeps overlay-awareness out of SQL generation entirely (plan §1.4).
pub struct Source {
    table: TableId,
    /// Filter pushed down into hydration so the scan does not materialise rows
    /// the first downstream `Filter` would immediately drop.
    pushdown: Option<Predicate>,
    params: Params,
    order: Vec<(ColId, Dir)>,
    /// Upper bound on hydration. `None` means unbounded, which is only valid
    /// for a query that opted in via `allow_unbounded` (plan §1.2).
    limit: Option<usize>,
    pk: ColId,
    /// When true, [`Operator::hydrate`] yields nothing and the source only ever
    /// forwards deltas. See [`Source::deltas_only`].
    deltas_only: bool,
}

impl Source {
    pub fn new(table: TableId, pk: ColId) -> Self {
        Source {
            table,
            pushdown: None,
            params: Params::empty(),
            order: Vec::new(),
            limit: None,
            pk,
            deltas_only: false,
        }
    }

    /// Hydrate to nothing; only forward this table's deltas.
    ///
    /// This is how the child side of a `Join` is wired. A join's child table is
    /// the big one — a million comments behind a hundred thousand issues — and
    /// hydrating it would materialise the entire table just so the join could
    /// throw away all but three rows per parent. Instead the join hydrates
    /// children *per parent*, with the parent's key pushed down, which is a
    /// bounded read (plan §1.1: `limit` is mandatory on 1:N).
    ///
    /// The source still has to exist, because deltas to the child table must
    /// reach the join after hydration. It is the hydration path alone that is
    /// suppressed.
    pub fn deltas_only(mut self) -> Self {
        self.deltas_only = true;
        self
    }

    pub fn with_pushdown(mut self, pred: Predicate, params: Params) -> Self {
        self.pushdown = Some(pred);
        self.params = params;
        self
    }

    pub fn with_order(mut self, order: Vec<(ColId, Dir)>) -> Self {
        self.order = order;
        self
    }

    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }

    pub fn table(&self) -> TableId {
        self.table
    }
}

impl Operator for Source {
    fn name(&self) -> &'static str {
        "Source"
    }

    fn apply(&mut self, input: Inputs<'_>, _cx: &mut dyn OpCx) -> Batch {
        input.primary().clone()
    }

    fn hydrate(&mut self, cx: &mut dyn OpCx) -> Batch {
        if self.deltas_only {
            return Batch::new();
        }

        let order = if self.order.is_empty() {
            // Scan in primary-key order by default. An unordered scan would be
            // marginally cheaper but would make hydration output depend on
            // SQLite's physical layout, and `solstice-ivm` is required to be
            // deterministic (plan §6).
            vec![(self.pk, Dir::Asc)]
        } else {
            self.order.clone()
        };

        let req = ScanRequest {
            table: self.table,
            order,
            after: None,
            filter: self.pushdown.clone(),
            params: self.params.clone(),
            limit: self.limit.unwrap_or(usize::MAX),
        };

        cx.scan(&req)
            .into_iter()
            .map(|(key, row)| Change::Insert { key, row })
            .collect()
    }
}
