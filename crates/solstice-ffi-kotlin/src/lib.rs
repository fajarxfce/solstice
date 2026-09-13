//! The Solstice byte ABI, as seen by Kotlin.
//!
//! Plan §4.1's third crate. It holds no logic: every method here unwraps
//! arguments, calls [`solstice_core`], and wraps the answer. That is the whole
//! design — the engine has no FFI attributes in it so it can be tested without
//! either binding toolchain, and this file is the mechanical part.
//!
//! # Why the payloads are `Vec<u8>`
//!
//! Because the ABI does not depend on the application's schema. A Kotlin app
//! that adds a column regenerates its own typed row classes and never touches
//! Rust. UniFFI carries `Vec<u8>` as a `ByteArray` with one copy in each
//! direction, which is the cost plan §4.1 accepted and spike S2 measures.
//!
//! # What UniFFI makes different from the Dart side
//!
//! One thing, and it is the thing plan §4.1 named as outside the intersection
//! of the two generators: **the callback mechanism.** Here it is a foreign
//! trait — Kotlin implements [`EventSink`] and hands it over. `flutter_rust_bridge`
//! has no equivalent and uses a `StreamSink` instead. Everything else in this
//! file and its Dart counterpart is the same shape, which is the evidence that
//! the intersection is real and not just plausible.
//!
//! # Threading
//!
//! [`EventSink::on_event`] is called on the engine thread, so plan §4.3's rule
//! applies on the Kotlin side too: `trySend` into a `Channel` and get out. Work
//! done in the callback blocks every other subscription on the device.

use std::sync::Arc;

uniffi::setup_scaffolding!();

/// Everything that can go wrong, flattened for the boundary.
///
/// The `String` payloads are deliberate (see `solstice_core::SolsticeError`):
/// a host shows the developer a message, it does not branch on whether a column
/// or a table was the thing that did not exist. The variants that survive are
/// the ones a host does something different about.
///
/// # Why the field is `detail` and not `message`
///
/// Because UniFFI 0.32 cannot generate Kotlin for an error field called
/// `message`. It emits each variant as a class extending `kotlin.Exception`
/// with the field as a constructor property *and* an `override val message`
/// formatting it — two declarations of one name, which does not compile:
///
/// ```text
/// error: overload resolution ambiguity between candidates:
/// val message: String
/// val message: String
/// ```
///
/// It is caught by `spikes/s2-bridge/kotlin/run.sh` and by nothing before it:
/// the Rust compiles, `cargo test` passes, the bindings generate, and the
/// failure arrives in `kotlinc` — which is the argument for a spike that
/// compiles the generated Kotlin rather than trusting that it generated.
///
/// The Dart adapter renames along with it. The two files are meant to read as
/// one shape, and a field spelled differently on each side to route around one
/// generator's bug is a worse trade than a field named `detail` on both.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Error)]
pub enum SolsticeError {
    /// A frame this build could not read.
    Decode { detail: String },
    /// A query this engine will not run. The developer has to change it.
    Compile { detail: String },
    /// A mutation body the engine refuses to apply.
    Mutation { detail: String },
    /// The body used operations this build does not implement. The answer is
    /// "upgrade the client", not "fix the call".
    UnsupportedOps { count: u32 },
    /// SQLite said no.
    Store { detail: String },
    /// The engine thread is gone.
    Shutdown,
}

impl std::fmt::Display for SolsticeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SolsticeError::Decode { detail } => write!(f, "decode: {detail}"),
            SolsticeError::Compile { detail } => write!(f, "query: {detail}"),
            SolsticeError::Mutation { detail } => write!(f, "mutation: {detail}"),
            SolsticeError::UnsupportedOps { count } => write!(
                f,
                "mutation: {count} operation(s) this build does not implement"
            ),
            SolsticeError::Store { detail } => write!(f, "store: {detail}"),
            SolsticeError::Shutdown => write!(f, "the engine is shut down"),
        }
    }
}

impl std::error::Error for SolsticeError {}

impl From<solstice_core::SolsticeError> for SolsticeError {
    fn from(e: solstice_core::SolsticeError) -> SolsticeError {
        use solstice_core::SolsticeError as E;
        match e {
            E::Decode(detail) => SolsticeError::Decode { detail },
            E::Compile(detail) => SolsticeError::Compile { detail },
            E::Mutation(detail) => SolsticeError::Mutation { detail },
            E::UnsupportedOps { count } => SolsticeError::UnsupportedOps { count },
            E::Store(detail) => SolsticeError::Store { detail },
            E::Shutdown => SolsticeError::Shutdown,
        }
    }
}

/// Counters a host can render and CI can fail a build on (plan §7).
#[derive(Debug, Clone, Copy, uniffi::Record)]
pub struct EngineStats {
    pub subscriptions: u64,
    pub view_rows: u64,
    pub view_bytes: u64,
    pub graph_bytes: u64,
    pub version: u64,
    pub mutations: u64,
    pub events: u64,
    pub window_refills: u64,
    pub window_refill_rows: u64,
    pub child_refills: u64,
    pub child_refill_rows: u64,
}

impl From<solstice_core::EngineStats> for EngineStats {
    fn from(s: solstice_core::EngineStats) -> EngineStats {
        EngineStats {
            subscriptions: s.subscriptions,
            view_rows: s.view_rows,
            view_bytes: s.view_bytes,
            graph_bytes: s.graph_bytes,
            version: s.version,
            mutations: s.mutations,
            events: s.events,
            window_refills: s.window_refills,
            window_refill_rows: s.window_refill_rows,
            child_refills: s.child_refills,
            child_refill_rows: s.child_refill_rows,
        }
    }
}

/// Where view diffs arrive. Implemented in Kotlin, called from Rust.
///
/// One stream per database rather than per subscription (plan §4.2), so the
/// host demultiplexes on the `sub_id` inside each `ViewDelta`. That buys fewer
/// FFI objects and — the part that matters — one total order, so a host can
/// observe that two lists moved together.
#[uniffi::export(with_foreign)]
pub trait EventSink: Send + Sync {
    /// One encoded `solstice.v1.ViewDelta`.
    ///
    /// **Called on the engine thread.** `trySend` it into a `Channel` and
    /// return; anything done here blocks every other subscription.
    fn on_event(&self, event: Vec<u8>);
}

/// Adapts the foreign trait to the one the engine speaks.
struct SinkBridge(Arc<dyn EventSink>);

impl solstice_core::EngineEventSink for SinkBridge {
    fn on_event(&self, event: Vec<u8>) {
        self.0.on_event(event)
    }
}

/// An open database.
#[derive(uniffi::Object)]
pub struct Database(Arc<solstice_core::Database>);

#[uniffi::export]
impl Database {
    /// Open a database at `path`, or in memory when `path` is empty.
    #[uniffi::constructor]
    pub fn open(path: String) -> Result<Arc<Database>, SolsticeError> {
        let db = solstice_core::Database::open(solstice_core::OpenConfig { path })?;
        Ok(Arc::new(Database(db)))
    }

    /// Subscribe to a query, given its encoded `solstice.v1.Query`.
    ///
    /// The rows come back with the handle, not afterwards — see
    /// [`Subscription::initial`].
    pub fn subscribe(&self, query_ir: Vec<u8>) -> Result<Arc<Subscription>, SolsticeError> {
        Ok(Arc::new(Subscription(self.0.subscribe(query_ir)?)))
    }

    /// Apply an encoded `solstice.v1.Mutation`, returning the version it landed
    /// at. Every `ViewDelta` this write causes carries that version.
    pub fn mutate(&self, body: Vec<u8>) -> Result<u64, SolsticeError> {
        Ok(self.0.mutate(body)?)
    }

    pub fn set_event_sink(&self, sink: Arc<dyn EventSink>) -> Result<(), SolsticeError> {
        Ok(self.0.set_event_sink(Box::new(SinkBridge(sink)))?)
    }

    pub fn clear_event_sink(&self) -> Result<(), SolsticeError> {
        Ok(self.0.clear_event_sink()?)
    }

    pub fn stats(&self) -> Result<EngineStats, SolsticeError> {
        Ok(self.0.stats()?.into())
    }
}

/// A live subscription. Closing it — or letting Kotlin's cleaner collect it —
/// unsubscribes.
#[derive(uniffi::Object)]
pub struct Subscription(Arc<solstice_core::Subscription>);

#[uniffi::export]
impl Subscription {
    /// The id every `ViewDelta` for this query carries.
    pub fn sub_id(&self) -> u64 {
        self.0.sub_id()
    }

    /// The view as it stood when the subscription was created, encoded.
    ///
    /// Plan §4.2's first promise, kept literally: the rows are already here
    /// when `subscribe` returns, so a recomposition never shows a spinner it
    /// does not need.
    pub fn initial(&self) -> Vec<u8> {
        self.0.initial()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Stands in for the Kotlin implementation, so the adapter is exercised
    /// without a JVM in the loop. What this cannot check is the generated
    /// bindings; that is what `spikes/s2-bridge` is for.
    #[derive(Default)]
    struct Collect(Mutex<Vec<Vec<u8>>>);

    impl EventSink for Collect {
        fn on_event(&self, event: Vec<u8>) {
            self.0.lock().unwrap().push(event);
        }
    }

    fn seed(db: &Database) {
        use solstice_ivm::{Row, Value};
        use solstice_proto::mutation::{Mutation, Op};
        let ops = (1..=5)
            .map(|id: i64| Op::Insert {
                table: "issues".into(),
                row: Row::new(vec![
                    Value::Int(id),
                    Value::Int(1),
                    Value::Int(id * 10),
                    Value::Int(0),
                    Value::text("t"),
                    Value::Int(id),
                ]),
            })
            .collect();
        db.mutate(
            Mutation {
                mutation_id: 1,
                ops,
                ..Mutation::default()
            }
            .encode(),
        )
        .expect("the seed applies");
    }

    #[test]
    fn a_query_crosses_as_bytes_and_the_view_comes_back_as_bytes() {
        let db = Database::open(String::new()).expect("an in-memory database opens");
        seed(&db);

        let sub = db
            .subscribe(solstice_core::m0::query(1, 3, 2).encode())
            .expect("the M0 query compiles");
        let view = solstice_proto::ViewDelta::decode(&sub.initial()).expect("the engine's bytes");
        assert_eq!(view.sub_id, sub.sub_id());
        assert_eq!(view.changes.len(), 3);
    }

    #[test]
    fn a_foreign_sink_receives_the_diff_the_engine_produced() {
        let db = Database::open(String::new()).expect("an in-memory database opens");
        seed(&db);
        let events = Arc::new(Collect::default());
        db.set_event_sink(events.clone()).unwrap();
        let sub = db
            .subscribe(solstice_core::m0::query(1, 3, 2).encode())
            .unwrap();

        let version = db
            .mutate(
                solstice_proto::mutation::Mutation {
                    mutation_id: 2,
                    ops: vec![solstice_proto::mutation::Op::Update {
                        table: "issues".into(),
                        key: solstice_ivm::Value::Int(1),
                        set: vec![("priority".into(), solstice_ivm::Value::Int(100))],
                    }],
                    ..Default::default()
                }
                .encode(),
            )
            .unwrap();

        let sent = std::mem::take(&mut *events.0.lock().unwrap());
        assert_eq!(sent.len(), 1);
        let delta = solstice_proto::ViewDelta::decode(&sent[0]).unwrap();
        assert_eq!(delta.sub_id, sub.sub_id());
        assert_eq!(delta.version, version);
    }

    #[test]
    fn an_engine_error_arrives_as_the_variant_a_host_branches_on() {
        let db = Database::open(String::new()).expect("an in-memory database opens");
        let mut q = solstice_core::m0::query(1, 50, 3);
        q.order_by.clear();
        assert!(matches!(
            db.subscribe(q.encode()),
            Err(SolsticeError::Compile { .. })
        ));
    }
}
