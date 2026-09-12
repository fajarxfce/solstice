//! Concrete operators.
//!
//! M0 ships `Source → Filter → TopK → Join(1:N)`, because that chain is where
//! the project's real risk lives (plan §5.1).
//!
//! The plan text writes the chain as `Source → Filter → Join(1:N) → TopK`. The
//! order here is deliberate and is the reason the memory bound holds: because a
//! DQL join produces a hierarchy rather than a product, it does not change
//! parent cardinality, so `TopK` commutes with it and belongs *below*. See the
//! [`join`] module docs.

mod filter;
mod join;
mod project;
mod source;
mod topk;

pub use filter::Filter;
pub use join::{Join1N, CHILD, PARENT};
pub use project::Project;
pub use source::Source;
pub use topk::TopK;
