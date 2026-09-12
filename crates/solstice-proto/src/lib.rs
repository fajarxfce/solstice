//! Protobuf encoding for the Solstice view diff.
//!
//! # Why an encoding crate exists this early
//!
//! Plan §4.1 makes the FFI ABI **byte-based**: query IR, view diffs, mutation
//! bodies and events all cross as protobuf. The reason is not elegance. It is
//! that the ABI then **does not depend on the application's schema**, so no app
//! ever regenerates Rust FFI glue — codegen produces typed row classes in Dart
//! and Kotlin and the developer never sees a byte.
//!
//! That choice has exactly one cost, and the plan names it as the primary M0
//! kill criterion (spike S1): **the host has to decode.** Budget is an initial
//! 1000-row view in under 5ms, in Dart *and* Kotlin, on a mid-range phone, with
//! a 5-row delta under 100µs. If that fails, the answer is a columnar diff with
//! generated zero-copy accessors — a different encoding, a different `.proto`,
//! and a different shape for everything built on top.
//!
//! Which is why this comes before the bridges rather than after them. Wiring
//! `flutter_rust_bridge` and UniFFI is mechanical (plan §4.1 budgets ~500 LOC
//! each); it is also the work that would have to be redone. Finding out that the
//! encoding is too slow *after* two demo apps are written against it is the
//! expensive order.
//!
//! # Layout
//!
//! - [`wire`] — varints, tags, length-delimited framing. No schema knowledge.
//! - [`view`] — [`ViewDelta`] and [`ViewChange`], transcribed from
//!   `proto/solstice/v1/view.proto`.
//!
//! The payloads the host benchmarks decode are produced by the `s1-fixture`
//! binary in `solstice-bench`, from a real hydration against real SQLite —
//! measuring a decode of rows this crate invented would only prove that the
//! invented rows were easy to decode.

pub mod view;
pub mod wire;

pub use view::{ViewChange, ViewDelta};
pub use wire::WireError;
