//! Concrete operators.
//!
//! M0 ships the stateless pair plus the source; `Join` and `TopK` follow in the
//! same milestone because they carry the project's real risk (plan §5.1).

mod filter;
mod project;
mod source;

pub use filter::Filter;
pub use project::Project;
pub use source::Source;
