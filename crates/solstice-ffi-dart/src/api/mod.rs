//! The Solstice byte ABI, as seen by Dart.
//!
//! Plan §4.1's second adapter crate, and the twin of `solstice-ffi-kotlin`. It
//! holds no logic: unwrap, call [`solstice_core`], wrap. Read the two files
//! side by side — they are deliberately the same shape, because the claim the
//! whole three-crate split rests on is that one core can serve two generators
//! without either of them bending it.
//!
//! # Where they differ, and it is only here
//!
//! **The callback.** UniFFI takes a foreign trait object; `flutter_rust_bridge`
//! has [`StreamSink`], and a Dart `Stream` is what a widget wants anyway. Plan
//! §4.1 predicted this as the one thing outside the intersection of the two
//! generators, and it is the one thing that came out different.
//!
//! Everything else — opaque handles with `&self` methods, plain records,
//! `Result<T, E>`, `Vec<u8>` payloads — is expressed identically by both.
//!
//! # Threading
//!
//! Events reach Dart through the isolate's event loop, so unlike the Kotlin
//! side there is nothing for the host to remember: the platform thread is never
//! blocked and a `StreamSink` send is non-blocking on the Rust side.

use std::sync::Arc;

use flutter_rust_bridge::frb;

// Not `flutter_rust_bridge::StreamSink`. The generator emits its own alias into
// `frb_generated`, bound to this crate's codec, and the parser only recognises a
// sink parameter when it is spelled this way.
use crate::frb_generated::StreamSink;

/// Everything that can go wrong, flattened for the boundary.
///
/// The payload field is `detail` rather than `message` because UniFFI cannot
/// generate Kotlin for the latter — see the twin in `solstice-ffi-kotlin` for
/// the error it produces. `flutter_rust_bridge` would have taken `message`
/// happily; the two adapters are kept spelled the same on purpose.
#[derive(Debug, Clone, PartialEq, Eq)]
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
#[derive(Debug, Clone, Copy)]
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

/// Bridges the Dart stream to the sink the engine speaks.
///
/// A failed `add` means Dart closed the stream, which is the host's business:
/// the engine keeps maintaining the views, and dropping the `Database` is what
/// stops it. Panicking here would take down the engine thread because a widget
/// was disposed.
struct SinkBridge(StreamSink<Vec<u8>>);

impl solstice_core::EngineEventSink for SinkBridge {
    fn on_event(&self, event: Vec<u8>) {
        let _ = self.0.add(event);
    }
}

/// An open database.
///
/// Opaque: Dart holds a handle, and the Rust side owns the engine thread behind
/// it. Dropping the Dart object shuts the thread down.
#[frb(opaque)]
pub struct Database(Arc<solstice_core::Database>);

impl Database {
    /// Open a database at `path`, or in memory when `path` is empty.
    #[frb(sync)]
    pub fn open(path: String) -> Result<Database, SolsticeError> {
        Ok(Database(solstice_core::Database::open(
            solstice_core::OpenConfig { path },
        )?))
    }

    /// Subscribe to a query, given its encoded `solstice.v1.Query`.
    ///
    /// `sync`, and that is plan §4.2's first guaranteed behaviour rather than
    /// an optimisation: a widget that rebuilds or a user who presses back gets
    /// rows in the same frame, not a loading flicker. An `async` signature here
    /// would make the flicker unavoidable in every app built on it.
    #[frb(sync)]
    pub fn subscribe(&self, query_ir: Vec<u8>) -> Result<Subscription, SolsticeError> {
        Ok(Subscription(self.0.subscribe(query_ir)?))
    }

    /// Apply an encoded `solstice.v1.Mutation`, returning the version it landed
    /// at. Every `ViewDelta` this write causes carries that version.
    ///
    /// `sync`, because this is the write a user's tap makes and plan §2.4 wants
    /// `h.applied` true before the call returns. The engine is a command loop on
    /// another thread, so "sync" here means the Dart isolate waits for it.
    #[frb(sync)]
    pub fn mutate(&self, body: Vec<u8>) -> Result<u64, SolsticeError> {
        Ok(self.0.mutate(body)?)
    }

    /// The same write, dispatched off the Dart isolate.
    ///
    /// Not a convenience, and the one place the two adapters do not line up.
    /// Writes that a *user* did not make — a sync engine applying a server
    /// patch, plan §5.1's background thread at 200 rows/sec — must not stop the
    /// isolate that paints frames, and Dart has exactly one of those per app.
    /// Without a `Future`, a write that hits SQLite's WAL checkpoint would take
    /// 21ms of the UI thread (spike S3) and drop a frame and a half for reasons
    /// having nothing to do with the view.
    ///
    /// Kotlin needs no twin of this: a Compose app already has threads, so the
    /// idiomatic answer there is to call the blocking method from
    /// `Dispatchers.IO`. Dart cannot do that for an FFI call, so the hand-off
    /// has to live on this side of the boundary.
    pub fn mutate_async(&self, body: Vec<u8>) -> Result<u64, SolsticeError> {
        Ok(self.0.mutate(body)?)
    }

    /// Start delivering view diffs to `sink`.
    ///
    /// One stream per database, not per subscription (plan §4.2): the host
    /// demultiplexes on the `sub_id` inside each `ViewDelta`. That is fewer FFI
    /// objects, and — the part that matters — one total order, so an app can
    /// observe that two lists moved together.
    #[frb(sync)]
    pub fn set_event_sink(&self, sink: StreamSink<Vec<u8>>) -> Result<(), SolsticeError> {
        Ok(self.0.set_event_sink(Box::new(SinkBridge(sink)))?)
    }

    #[frb(sync)]
    pub fn clear_event_sink(&self) -> Result<(), SolsticeError> {
        Ok(self.0.clear_event_sink()?)
    }

    #[frb(sync)]
    pub fn stats(&self) -> Result<EngineStats, SolsticeError> {
        Ok(self.0.stats()?.into())
    }
}

/// A live subscription. Disposing it unsubscribes.
#[frb(opaque)]
pub struct Subscription(Arc<solstice_core::Subscription>);

impl Subscription {
    /// The id every `ViewDelta` for this query carries.
    #[frb(sync)]
    pub fn sub_id(&self) -> u64 {
        self.0.sub_id()
    }

    /// The view as it stood when the subscription was created, encoded.
    #[frb(sync)]
    pub fn initial(&self) -> Vec<u8> {
        self.0.initial()
    }
}
