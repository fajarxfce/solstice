//! Incremental view maintenance for Solstice.
//!
//! This crate answers one question: given a query and a stream of changes to
//! the tables it reads, produce the stream of changes to its *result* — without
//! ever re-running the query.
//!
//! # The law this crate exists to satisfy
//!
//! ```text
//! ∀ operator op, relation R, delta Δ:   op(R ⊎ Δ)  ==  op(R) ⊎ op.apply(Δ)
//! ```
//!
//! Recomputing from scratch and maintaining incrementally must agree, always.
//! Everything else here — the two comparison orders in [`value`], three-valued
//! logic in [`predicate`], diff composition in [`delta`] — exists because some
//! shortcut would have broken that law in a way no unit test would have caught.
//! The property tests in `tests/delta_law.rs` check it directly against the
//! deliberately-dumb evaluator in [`reference`].
//!
//! # No IO, no time, no threads
//!
//! `solstice-ivm` has no clock, no RNG, no filesystem, no sockets, and no threads.
//! Everything the engine needs from the outside world arrives through
//! [`operator::OpCx`]. That is not tidiness: it is what makes the deterministic
//! simulation tests possible (plan §6), and a CI lint enforces it.
//!
//! # Where this runs
//!
//! Both ends of the wire. The same crate is linked into the mobile client and
//! into the reference server's incremental pipeline (plan §3.2), which is what
//! guarantees client and server agree on query semantics *by construction*
//! rather than by testing.

pub mod delta;
pub mod graph;
pub mod operator;
pub mod ops;
pub mod order;
pub mod predicate;
pub mod reference;
pub mod relation;
pub mod schema;
pub mod value;

pub use delta::{Batch, Change};
pub use graph::{Graph, GraphBuilder, NodeId};
pub use operator::{Degrade, Inputs, NoCx, OpCx, Operator, Port, ScanRequest};
pub use order::{Cursor, Dir};
pub use predicate::{CmpOp, Expr, Params, Predicate, Tri};
pub use reference::{JoinSpec, Reference, Stage, Tables};
pub use relation::Relation;
pub use schema::{Column, Schema, TableId, ValueType};
pub use value::{ColId, Row, RowKey, Value};
