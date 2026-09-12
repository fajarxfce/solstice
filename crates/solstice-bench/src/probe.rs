//! An [`OpCx`] that counts what passes through it.
//!
//! The M0 question is not "was it fast on this laptop" but "is the work
//! bounded". Those come apart precisely where it matters: a scan of a small
//! table is fast *and* unbounded, and it stays fast right up to the size at
//! which nobody is watching any more. Counting scans and rows says which one is
//! happening; a stopwatch cannot.
//!
//! This wraps the store rather than replacing it, so the numbers describe the
//! real SQLite path, not a model of it.

use solstice_ivm::{OpCx, RefillKind, Row, RowKey, ScanRequest, TableId};
use solstice_store::SqliteStore;
use std::collections::BTreeMap;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    pub scans: usize,
    pub rows: usize,
}

pub struct Probe<'a> {
    store: &'a mut SqliteStore,
    per_table: BTreeMap<TableId, Counts>,
}

impl<'a> Probe<'a> {
    pub fn new(store: &'a mut SqliteStore) -> Probe<'a> {
        Probe {
            store,
            per_table: BTreeMap::new(),
        }
    }

    /// The store underneath, for the write path — which is the caller's job,
    /// not an operator's (plan §1.4).
    pub fn store_mut(&mut self) -> &mut SqliteStore {
        self.store
    }

    pub fn counts(&self, table: TableId) -> Counts {
        self.per_table.get(&table).copied().unwrap_or_default()
    }

    /// `(scans, rows)` for one table.
    pub fn table_totals(&self, table: TableId) -> (usize, usize) {
        let c = self.counts(table);
        (c.scans, c.rows)
    }

    pub fn totals(&self) -> Counts {
        self.per_table
            .values()
            .fold(Counts::default(), |acc, c| Counts {
                scans: acc.scans + c.scans,
                rows: acc.rows + c.rows,
            })
    }

    pub fn reset(&mut self) {
        self.per_table.clear();
    }
}

impl OpCx for Probe<'_> {
    fn scan(&mut self, req: &ScanRequest) -> Vec<(RowKey, Row)> {
        let rows = self.store.scan(req);
        let entry = self.per_table.entry(req.table).or_default();
        entry.scans += 1;
        entry.rows += rows.len();
        rows
    }

    fn note_refill(&mut self, kind: RefillKind, rows: usize) {
        self.store.note_refill(kind, rows);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{self, ISSUES};
    use crate::rng::Rng;
    use solstice_ivm::{Params, Predicate};

    #[test]
    fn a_probe_counts_scans_and_rows_per_table() {
        let mut store = SqliteStore::in_memory(fixture::schemas()).unwrap();
        fixture::seed(&mut store, &fixture::Scale::TINY, &mut Rng::new(3)).unwrap();

        let mut probe = Probe::new(&mut store);
        let req = ScanRequest {
            table: ISSUES,
            order: fixture::issue_order(),
            after: None,
            filter: Some(Predicate::eq(fixture::issue::CLOSED, 0i64)),
            params: Params::empty(),
            limit: 5,
        };
        probe.scan(&req);
        probe.scan(&req);

        assert_eq!(probe.table_totals(ISSUES), (2, 10));
        assert_eq!(probe.counts(fixture::COMMENTS), Counts::default());
        assert_eq!(probe.totals(), Counts { scans: 2, rows: 10 });

        probe.reset();
        assert_eq!(probe.totals(), Counts::default());
    }

    #[test]
    fn a_refill_noted_through_the_probe_reaches_the_store() {
        // The probe must not swallow the one statistic the kill criteria are
        // written in terms of.
        let mut store = SqliteStore::in_memory(fixture::schemas()).unwrap();
        {
            let mut probe = Probe::new(&mut store);
            probe.note_refill(RefillKind::Window, 4);
        }
        let stats = store.refill_stats();
        assert_eq!((stats.window, stats.window_rows), (1, 4));
    }
}
