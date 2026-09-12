//! Concrete operators.
//!
//! M0 ships `Source → Filter → TopK`; `Join(1:N)` completes the milestone
//! because that chain is where the project's real risk lives (plan §5.1).

mod filter;
mod project;
mod source;
mod topk;

pub use filter::Filter;
pub use project::Project;
pub use source::Source;
pub use topk::TopK;
