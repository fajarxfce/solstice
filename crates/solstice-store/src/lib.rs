//! The canonical store for Solstice: SQLite, and the one path that writes it.
//!
//! `solstice-ivm` is deliberately blind — no clock, no IO, no store handle. It
//! asks for what it needs through [`OpCx`], and this crate is the answer for a
//! real device. Two jobs:
//!
//! 1. **Serve bounded scans.** A `TopK` refill is `WHERE … ORDER BY … LIMIT k`,
//!    and whether that is O(limit) or O(table) is decided by the index behind
//!    it (see [`ddl::create_order_index`]). This is where plan §7's "refill
//!    degenerates into unbounded requery" is actually won or lost.
//! 2. **Be the only writer.** Plan §1.4's single-writer invariant is what lets
//!    the engine know every delta by construction, which is why Solstice needs
//!    no change-capture hook at all. [`SqliteStore`] exposes no way to run
//!    arbitrary SQL.
//!
//! # Why this is also a test oracle
//!
//! Plan §6 wants the incremental path checked against *both* a dumb evaluator
//! and SQLite itself. The second one only means something if SQLite is being
//! asked the same question — so [`sql`] goes to some trouble to avoid the
//! spellings where SQL's defaults and the engine's semantics differ (type
//! affinity, `LIKE`, NULLs in a keyset cursor). `tests/oracle.rs` then checks
//! the translation against the reference evaluator over random data.
//!
//! [`OpCx`]: solstice_ivm::OpCx

pub mod ddl;
pub mod sql;
pub mod store;

pub use sql::{scan_sql, SqlScan};
pub use store::{SqliteStore, StoreError};
