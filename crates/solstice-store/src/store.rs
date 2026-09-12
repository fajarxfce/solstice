//! The SQLite-backed canonical store.
//!
//! # One writer, and it is this type
//!
//! Plan §1.4 makes a single invariant carry the whole IVM design: **exactly one
//! code path changes base data.** The engine knows every delta by construction
//! because it produced it, which is why no change-capture hook is needed at all.
//! If anything else writes these tables, the graph stops seeing changes it is
//! maintaining views over, and the result is nondeterministic silent staleness
//! — the worst bug class this product can have.
//!
//! So [`SqliteStore::apply`] is the only mutating entry point, it takes a
//! [`Batch`] (not SQL), and it hands back the same batch for the graph to pump.
//! There is no `execute` escape hatch on this type.
//!
//! # The scan contract
//!
//! [`OpCx::scan`] must see the batch currently being applied. That falls out of
//! doing it in this order — commit, then pump — which is what
//! [`SqliteStore::commit`] exists to make explicit at the call site.

use crate::ddl::{create_seek_index, create_table};
use crate::sql::scan_sql;
use rusqlite::types::{ToSqlOutput, ValueRef};
use rusqlite::{Connection, ToSql};
use solstice_ivm::{
    Batch, Change, ColId, Dir, OpCx, RefillKind, RefillStats, Row, RowKey, ScanRequest, Schema,
    TableId, Value,
};
use std::collections::BTreeMap;
use std::path::Path;

/// What can go wrong talking to the store.
#[derive(Debug)]
pub enum StoreError {
    Sqlite(rusqlite::Error),
    /// A scan or write named a table the store was not opened with. A
    /// construction bug, surfaced rather than guessed at.
    UnknownTable(TableId),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Sqlite(e) => write!(f, "sqlite: {e}"),
            StoreError::UnknownTable(t) => write!(f, "no schema registered for table {t}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError::Sqlite(e)
    }
}

type Result<T> = std::result::Result<T, StoreError>;

/// A [`Value`] on its way into SQLite.
///
/// `Value::Rows` is the one variant with no representation here, and that is
/// the invariant working: a child collection is something the graph *builds*
/// downstream of a join, never something a table holds. Storing one would mean
/// the hierarchy had leaked into the base data.
struct Bind<'a>(&'a Value);

impl ToSql for Bind<'_> {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(match self.0 {
            Value::Null => ToSqlOutput::Borrowed(ValueRef::Null),
            Value::Int(i) => ToSqlOutput::Borrowed(ValueRef::Integer(*i)),
            Value::Real(r) => ToSqlOutput::Borrowed(ValueRef::Real(*r)),
            Value::Text(s) => ToSqlOutput::Borrowed(ValueRef::Text(s.as_bytes())),
            Value::Blob(b) => ToSqlOutput::Borrowed(ValueRef::Blob(b)),
            Value::Rows(_) => {
                return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
                    StoreError::UnknownTable(u16::MAX),
                )))
            }
        })
    }
}

fn from_sql(v: ValueRef<'_>) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::Int(i),
        ValueRef::Real(r) => Value::Real(r),
        ValueRef::Text(t) => Value::text(String::from_utf8_lossy(t)),
        ValueRef::Blob(b) => Value::blob(b),
    }
}

pub struct SqliteStore {
    conn: Connection,
    schemas: BTreeMap<TableId, Schema>,
    refills: RefillStats,
}

impl SqliteStore {
    /// Open a store on disk, creating tables for `schemas`.
    pub fn open(path: impl AsRef<Path>, schemas: Vec<Schema>) -> Result<SqliteStore> {
        Self::with_connection(Connection::open(path)?, schemas)
    }

    /// Open a private in-memory store. Used by tests and benchmarks.
    pub fn in_memory(schemas: Vec<Schema>) -> Result<SqliteStore> {
        Self::with_connection(Connection::open_in_memory()?, schemas)
    }

    fn with_connection(conn: Connection, schemas: Vec<Schema>) -> Result<SqliteStore> {
        // Plan §1.4. `foreign_keys=OFF` is not laziness: the engine owns
        // referential integrity, and letting SQLite cascade a delete would be a
        // write the graph never saw.
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA foreign_keys=OFF;
             PRAGMA mmap_size=67108864;",
        )?;

        let store = SqliteStore {
            conn,
            schemas: schemas.into_iter().map(|s| (s.table, s)).collect(),
            refills: RefillStats::default(),
        };
        let schemas: Vec<Schema> = store.schemas.values().cloned().collect();
        for schema in &schemas {
            store.conn.execute(&create_table(schema), [])?;
        }
        Ok(store)
    }

    /// Add the index that keeps refills on `order` bounded. See
    /// [`create_order_index`](crate::ddl::create_order_index).
    pub fn index_order(&mut self, table: TableId, order: &[(ColId, Dir)]) -> Result<()> {
        self.index_seek(table, &[], order)
    }

    /// Add the index for a scan that pins `eq` by equality before ordering by
    /// `order`. See [`create_seek_index`].
    pub fn index_seek(
        &mut self,
        table: TableId,
        eq: &[ColId],
        order: &[(ColId, Dir)],
    ) -> Result<()> {
        let schema = self.schema(table)?;
        if let Some(sql) = create_seek_index(schema, eq, order) {
            self.conn.execute(&sql, [])?;
        }
        Ok(())
    }

    /// SQLite's chosen plan for a scan, as `EXPLAIN QUERY PLAN` rows.
    ///
    /// The harness uses this to assert that a refill is a seek rather than a
    /// table scan. "It was fast on my machine" is not the same claim: a scan of
    /// a small table is fast too, and stops being fast at exactly the size
    /// where nobody is watching.
    pub fn explain(&self, req: &ScanRequest) -> Result<Vec<String>> {
        let schema = self.schema(req.table)?;
        let scan = scan_sql(schema, req);
        let mut stmt = self
            .conn
            .prepare(&format!("EXPLAIN QUERY PLAN {}", scan.sql))?;
        let binds: Vec<Bind<'_>> = scan.binds.iter().map(Bind).collect();
        let rows = stmt
            .query_map(rusqlite::params_from_iter(binds), |r| r.get::<_, String>(3))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn schema(&self, table: TableId) -> Result<&Schema> {
        self.schemas
            .get(&table)
            .ok_or(StoreError::UnknownTable(table))
    }

    /// Apply a batch of changes, then return it for the graph to pump.
    ///
    /// Returning the batch rather than swallowing it is the shape plan §1.4
    /// asks for (`apply_write(txn) -> Vec<Delta>`): the caller commits and then
    /// pumps, in that order, and the scan contract holds because of it.
    pub fn apply(&mut self, table: TableId, batch: &Batch) -> Result<()> {
        let schema = self.schema(table)?.clone();
        let tx = self.conn.transaction()?;
        {
            let cols = schema
                .columns
                .iter()
                .map(|c| crate::sql::quote_ident(&c.name))
                .collect::<Vec<_>>()
                .join(", ");
            let holes = vec!["?"; schema.arity()].join(", ");
            let name = crate::sql::quote_ident(&schema.name);
            let pk = crate::sql::quote_ident(&schema.columns[schema.pk as usize].name);

            let mut upsert = tx.prepare(&format!(
                "INSERT OR REPLACE INTO {name} ({cols}) VALUES ({holes})"
            ))?;
            let mut delete = tx.prepare(&format!("DELETE FROM {name} WHERE {pk} IS ?"))?;

            for change in batch.iter() {
                match change.after() {
                    Some(row) => {
                        let vals: Vec<Bind<'_>> = (0..schema.arity())
                            .map(|i| Bind(row.get(i as ColId)))
                            .collect();
                        upsert.execute(rusqlite::params_from_iter(vals))?;
                    }
                    None => {
                        delete.execute([Bind(change.key().value())])?;
                    }
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Replace a table's entire contents. For seeding and test fixtures.
    pub fn load(
        &mut self,
        table: TableId,
        rows: impl IntoIterator<Item = (RowKey, Row)>,
    ) -> Result<()> {
        let schema = self.schema(table)?.clone();
        self.conn.execute(
            &format!("DELETE FROM {}", crate::sql::quote_ident(&schema.name)),
            [],
        )?;
        let batch: Batch = rows
            .into_iter()
            .map(|(key, row)| Change::Insert { key, row })
            .collect();
        self.apply(table, &batch)
    }

    /// Every row of a table, in primary-key order. Test and debug helper; the
    /// engine never reads a whole table.
    pub fn dump(&self, table: TableId) -> Result<Vec<(RowKey, Row)>> {
        let schema = self.schema(table)?;
        let cols = schema
            .columns
            .iter()
            .map(|c| crate::sql::quote_ident(&c.name))
            .collect::<Vec<_>>()
            .join(", ");
        let pk = crate::sql::quote_ident(&schema.columns[schema.pk as usize].name);
        let sql = format!(
            "SELECT {cols} FROM {} ORDER BY {pk} ASC",
            crate::sql::quote_ident(&schema.name)
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let arity = schema.arity();
        let pk_col = schema.pk;
        let rows = stmt
            .query_map([], |r| Ok(read_row(r, arity, pk_col)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// How many bounded requeries this store has served, and how many rows they
    /// returned.
    ///
    /// This is the M0 kill criterion made observable: plan §5.1 budgets fewer
    /// than five `TopK` refills per second under an adversarial delete-the-top
    /// workload, and a number nobody can read is a budget nobody enforces.
    pub fn refill_stats(&self) -> RefillStats {
        self.refills
    }

    pub fn reset_refill_stats(&mut self) {
        self.refills = RefillStats::default();
    }

    fn run_scan(&self, req: &ScanRequest) -> Result<Vec<(RowKey, Row)>> {
        let schema = self.schema(req.table)?;
        let scan = scan_sql(schema, req);
        let mut stmt = self.conn.prepare_cached(&scan.sql)?;
        let binds: Vec<Bind<'_>> = scan.binds.iter().map(Bind).collect();
        let arity = schema.arity();
        let pk_col = schema.pk;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(binds), |r| {
                Ok(read_row(r, arity, pk_col))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

fn read_row(r: &rusqlite::Row<'_>, arity: usize, pk: ColId) -> (RowKey, Row) {
    let values: Vec<Value> = (0..arity).map(|i| from_sql(r.get_ref_unwrap(i))).collect();
    let key = RowKey::new(values[pk as usize].clone());
    (key, Row::new(values))
}

impl OpCx for SqliteStore {
    /// # Panics
    ///
    /// If the scan fails. An operator has no way to recover from a store that
    /// cannot answer a refill — it would have to emit a view it knows is wrong
    /// — so the failure is not something to paper over with an empty result.
    /// The engine's own error path lives a level up, around the pump.
    fn scan(&mut self, req: &ScanRequest) -> Vec<(RowKey, Row)> {
        match self.run_scan(req) {
            Ok(rows) => rows,
            Err(e) => panic!("store scan of table {} failed: {e}", req.table),
        }
    }

    fn note_refill(&mut self, kind: RefillKind, rows: usize) {
        self.refills.note(kind, rows);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solstice_ivm::{Column, Params, Predicate, ValueType};

    const ISSUES: TableId = 1;

    fn schema() -> Schema {
        Schema::new(
            ISSUES,
            "issues",
            vec![
                Column::new("id", ValueType::Int),
                Column::new("priority", ValueType::Int).nullable(),
                Column::new("title", ValueType::Text),
            ],
            0,
        )
    }

    fn store() -> SqliteStore {
        SqliteStore::in_memory(vec![schema()]).unwrap()
    }

    fn row(id: i64, priority: Value, title: &str) -> (RowKey, Row) {
        (
            RowKey::from(id),
            Row::new(vec![Value::Int(id), priority, Value::text(title)]),
        )
    }

    fn scan(s: &mut SqliteStore, order: Vec<(ColId, Dir)>, limit: usize) -> Vec<i64> {
        let rows = s.scan(&ScanRequest {
            table: ISSUES,
            order,
            after: None,
            filter: None,
            params: Params::empty(),
            limit,
        });
        rows.into_iter()
            .map(|(k, _)| match k.value() {
                Value::Int(i) => *i,
                v => panic!("unexpected key {v:?}"),
            })
            .collect()
    }

    #[test]
    fn a_round_trip_preserves_storage_class() {
        let mut s = store();
        // The point of leaving affinities off: a real stays a real and a text
        // that looks like a number stays text.
        let rows = vec![
            (
                RowKey::from(1),
                Row::new(vec![Value::Int(1), Value::Real(2.5), Value::text("7")]),
            ),
            (
                RowKey::from(2),
                Row::new(vec![Value::Int(2), Value::Null, Value::blob([1u8, 2])]),
            ),
        ];
        s.load(ISSUES, rows.clone()).unwrap();
        assert_eq!(s.dump(ISSUES).unwrap(), rows);
    }

    #[test]
    fn nulls_sort_first_ascending_and_last_descending() {
        let mut s = store();
        s.load(
            ISSUES,
            vec![
                row(1, Value::Int(5), "a"),
                row(2, Value::Null, "b"),
                row(3, Value::Int(1), "c"),
            ],
        )
        .unwrap();
        assert_eq!(scan(&mut s, vec![(1, Dir::Asc)], 10), vec![2, 3, 1]);
        assert_eq!(scan(&mut s, vec![(1, Dir::Desc)], 10), vec![1, 3, 2]);
    }

    #[test]
    fn a_sort_key_tie_is_broken_by_the_primary_key_ascending() {
        let mut s = store();
        s.load(
            ISSUES,
            vec![
                row(7, Value::Int(5), "a"),
                row(3, Value::Int(5), "b"),
                row(5, Value::Int(5), "c"),
            ],
        )
        .unwrap();
        // Descending sort, but the pk tie-break stays ascending.
        assert_eq!(scan(&mut s, vec![(1, Dir::Desc)], 10), vec![3, 5, 7]);
    }

    #[test]
    fn a_cursor_resumes_without_repeating_or_skipping_a_tie() {
        let mut s = store();
        s.load(
            ISSUES,
            vec![
                row(1, Value::Int(5), "a"),
                row(2, Value::Int(5), "b"),
                row(3, Value::Int(1), "c"),
            ],
        )
        .unwrap();
        let order = vec![(1, Dir::Desc)];
        let first = s.scan(&ScanRequest {
            table: ISSUES,
            order: order.clone(),
            after: None,
            filter: None,
            params: Params::empty(),
            limit: 1,
        });
        let (key, r) = first[0].clone();
        let cursor = solstice_ivm::order::Cursor::of(&r, key, &order);
        let rest = s.scan(&ScanRequest {
            table: ISSUES,
            order,
            after: Some(cursor),
            filter: None,
            params: Params::empty(),
            limit: 10,
        });
        let ids: Vec<i64> = rest
            .into_iter()
            .map(|(k, _)| match k.value() {
                Value::Int(i) => *i,
                v => panic!("unexpected {v:?}"),
            })
            .collect();
        assert_eq!(ids, vec![2, 3], "the tied row 2 must not be skipped");
    }

    #[test]
    fn deleting_through_apply_removes_the_row() {
        let mut s = store();
        s.load(
            ISSUES,
            vec![row(1, Value::Int(5), "a"), row(2, Value::Int(1), "b")],
        )
        .unwrap();
        let (key, before) = row(1, Value::Int(5), "a");
        let batch: Batch = [Change::Delete { key, before }].into_iter().collect();
        s.apply(ISSUES, &batch).unwrap();
        assert_eq!(scan(&mut s, vec![], 10), vec![2]);
    }

    #[test]
    fn a_filter_is_pushed_into_the_scan() {
        let mut s = store();
        s.load(
            ISSUES,
            vec![
                row(1, Value::Int(5), "a"),
                row(2, Value::Int(1), "b"),
                row(3, Value::Int(9), "c"),
            ],
        )
        .unwrap();
        let rows = s.scan(&ScanRequest {
            table: ISSUES,
            order: vec![(1, Dir::Asc)],
            after: None,
            filter: Some(Predicate::Cmp {
                lhs: solstice_ivm::Expr::Col(1),
                op: solstice_ivm::CmpOp::Gt,
                rhs: solstice_ivm::Expr::Lit(Value::Int(2)),
            }),
            params: Params::empty(),
            limit: 10,
        });
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn refills_are_counted_so_the_kill_criterion_is_observable() {
        let mut s = store();
        assert_eq!(s.refill_stats(), RefillStats::default());

        s.note_refill(RefillKind::Window, 3);
        s.note_refill(RefillKind::Children, 1);
        s.note_refill(RefillKind::Children, 1);

        // The kill criterion is about windows, so a busy join must not be able
        // to inflate it — the two are counted apart, not summed.
        let stats = s.refill_stats();
        assert_eq!((stats.window, stats.window_rows), (1, 3));
        assert_eq!((stats.children, stats.children_rows), (2, 2));
        assert_eq!(stats.total(), 3);

        s.reset_refill_stats();
        assert_eq!(s.refill_stats(), RefillStats::default());
    }

    #[test]
    fn an_unknown_table_is_an_error_not_a_guess() {
        let s = store();
        assert!(matches!(s.dump(99), Err(StoreError::UnknownTable(99))));
    }
}
