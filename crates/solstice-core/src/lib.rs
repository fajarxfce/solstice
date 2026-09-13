//! The Solstice engine, and the shape the FFI generators can carry.
//!
//! # Why this crate has no FFI attributes in it
//!
//! Plan §4.1 splits the engine into three crates because two generators have to
//! agree on it. What `flutter_rust_bridge` and UniFFI *both* express cleanly is
//! a small set: opaque `Arc<T>` handles with `&self` methods, plain records and
//! C-like enums, `Result<T, E>`, `Vec<u8>`, and **one** trait object for
//! callbacks. What neither shares is the callback mechanism itself — UniFFI has
//! callback interfaces, FRB has `StreamSink` — along with closures across the
//! boundary, generics, and lifetimes.
//!
//! So the public API here is written in that intersection, and the two adapter
//! crates are mechanical. The payoff is that `solstice-core` unit-tests without
//! either binding toolchain installed, which is the difference between a test
//! suite that runs on every commit and one that runs when someone remembers.
//!
//! # The ABI is bytes
//!
//! Query IR in, view diffs out, mutation bodies in, events out — all protobuf
//! (see `solstice-proto`). The reason is not that bytes are elegant. It is that
//! **the ABI then does not depend on the application's schema**, so no app ever
//! regenerates Rust FFI glue: codegen produces typed row classes in Dart and
//! Kotlin and the developer never sees a byte. One encoding is shared with the
//! wire protocol, both generators handle `Vec<u8>` perfectly, and schema
//! evolution across FFI is free.
//!
//! The cost is a decode on the host side, which is spike S1 and the primary M0
//! kill criterion. It has been measured: see `spikes/s1-decode`.
//!
//! # What M0 leaves out, and says so
//!
//! No server, no sync, no mutation log, no overlay, no rebase, no auth, no
//! codegen — plan §5.1 fakes everything except the risk. Concretely, against
//! the API plan §4.1 sketches:
//!
//! * [`Database::mutate`] returns the version the write landed at rather than a
//!   `MutationHandle`. A handle's whole purpose is its `confirmed` future, and
//!   with no server there is nothing for it to resolve against; a version the
//!   host can match against the `ViewDelta` it receives is a thing that works.
//! * There is no `SubOpts`. It has no members this build could honour, and a
//!   parameter that is always ignored is worse documentation than its absence.
//! * `set_auth_token` and `set_online` belong to the sync client (M2/M3).
//! * Unsubscribe takes effect immediately. Plan §4.4's grace period needs a
//!   clock, and plan §6 forbids ambient time in this crate specifically so the
//!   simulator can own it — so the grace period lives in the host bindings,
//!   where the platform's own lifecycle already provides one.

pub mod catalog;
pub mod compile;
pub mod engine;
pub mod m0;
pub mod view;

use std::sync::mpsc::{self, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use solstice_store::SqliteStore;

use engine::Command;

pub use catalog::{Catalog, Rel};
pub use compile::{compile, CompileError, Compiled};
pub use view::View;

/// Where the engine sends events.
///
/// The one trait object in the API (plan §4.1), because it is the one thing
/// both generators can express and neither can express twice. FRB maps it to a
/// `StreamSink`, UniFFI to a callback interface.
///
/// **One stream per database, not per subscription** (plan §4.2): fewer FFI
/// objects, one total order — so a host can observe that two views moved
/// together — and batching becomes trivial. The host demultiplexes on
/// `ViewDelta.sub_id`.
pub trait EngineEventSink: Send + Sync {
    /// One encoded [`solstice_proto::ViewDelta`].
    ///
    /// Called on the engine thread. An implementation that blocks here blocks
    /// every other subscription, so both bindings hand off immediately —
    /// Compose does `trySend` into a `Channel`, Flutter posts to the isolate's
    /// event loop.
    fn on_event(&self, event: Vec<u8>);
}

/// Everything that can go wrong, in the shape both generators can carry.
///
/// A flat enum with `String` payloads rather than the typed errors underneath.
/// That is a deliberate trade: a nested error type would survive Rust and die
/// at the boundary, and the host cannot act on the distinction between an
/// unknown column and an unknown table anyway — it shows the developer a
/// message. The variants that remain are the ones a host does something
/// *different* about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SolsticeError {
    /// A frame this build could not read. The host sent something malformed.
    Decode(String),
    /// A query this engine will not run. The developer has to change it.
    Compile(String),
    /// A mutation body the engine refuses to apply.
    Mutation(String),
    /// The body used operations this build does not implement.
    ///
    /// Separate from [`SolsticeError::Mutation`] because the answer is "upgrade
    /// the client", not "fix the call" — the same distinction plan §3.5 draws
    /// with `UpgradeRequired`.
    UnsupportedOps { count: u32 },
    /// SQLite said no.
    Store(String),
    /// The engine thread is gone.
    Shutdown,
}

impl SolsticeError {
    fn decode(e: solstice_proto::WireError) -> SolsticeError {
        SolsticeError::Decode(e.to_string())
    }

    fn compile(e: CompileError) -> SolsticeError {
        SolsticeError::Compile(e.to_string())
    }

    fn store(e: solstice_store::StoreError) -> SolsticeError {
        SolsticeError::Store(e.to_string())
    }

    fn mutation(msg: String) -> SolsticeError {
        SolsticeError::Mutation(msg)
    }
}

impl std::fmt::Display for SolsticeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SolsticeError::Decode(m) => write!(f, "decode: {m}"),
            SolsticeError::Compile(m) => write!(f, "query: {m}"),
            SolsticeError::Mutation(m) => write!(f, "mutation: {m}"),
            SolsticeError::UnsupportedOps { count } => write!(
                f,
                "mutation: {count} operation(s) this build does not implement"
            ),
            SolsticeError::Store(m) => write!(f, "store: {m}"),
            SolsticeError::Shutdown => write!(f, "the engine is shut down"),
        }
    }
}

impl std::error::Error for SolsticeError {}

/// How to open a database. A record, so both generators carry it by value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpenConfig {
    /// Path to the SQLite file. Empty opens a private in-memory store, which is
    /// what the tests and the decode benchmarks use.
    pub path: String,
}

impl OpenConfig {
    pub fn in_memory() -> OpenConfig {
        OpenConfig::default()
    }

    pub fn at(path: impl Into<String>) -> OpenConfig {
        OpenConfig { path: path.into() }
    }
}

/// Counters the host can render, and CI can fail a build on.
///
/// Plan §7's third mitigation: per-operator state is a first-class metric from
/// the first week, not a post-mortem tool. A budget nobody can read is a budget
/// nobody enforces — `window_refills` in particular is the M0 kill criterion
/// for adversarial delete-the-top workloads (plan §5.1) made observable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EngineStats {
    pub subscriptions: u64,
    pub view_rows: u64,
    /// Bytes held by materialised views.
    pub view_bytes: u64,
    /// Bytes held by operator state inside the dataflow graphs.
    pub graph_bytes: u64,
    /// Version of the last coherent pump. Matches `ViewDelta.version`.
    pub version: u64,
    pub mutations: u64,
    pub events: u64,
    pub window_refills: u64,
    pub window_refill_rows: u64,
    pub child_refills: u64,
    pub child_refill_rows: u64,
}

/// What [`Engine::subscribe`](engine) answers with, before it becomes a handle.
pub(crate) struct Subscribed {
    pub sub_id: u64,
    pub initial: Vec<u8>,
}

/// A command channel that is `Sync`, so `Arc<Database>` can be.
///
/// `mpsc::Sender` is `Send` but not `Sync`, and every method here takes `&self`
/// because that is the only shape an opaque FFI handle has. The lock is held
/// for the length of a `send` and never across the engine's work, so it is not
/// the hot path lock plan §4.3 rules out — the engine thread is still the only
/// serialisation that matters.
struct Chan(Mutex<Sender<Command>>);

impl Chan {
    fn send(&self, cmd: Command) -> Result<(), SolsticeError> {
        self.0
            .lock()
            .map_err(|_| SolsticeError::Shutdown)?
            .send(cmd)
            .map_err(|_| SolsticeError::Shutdown)
    }

    /// Send a command and wait for its reply.
    fn ask<T>(&self, cmd: impl FnOnce(SyncSender<T>) -> Command) -> Result<T, SolsticeError> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.send(cmd(tx))?;
        rx.recv().map_err(|_| SolsticeError::Shutdown)
    }
}

/// An open database. The handle everything else hangs off.
pub struct Database {
    chan: Arc<Chan>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Database {
    pub fn open(cfg: OpenConfig) -> Result<Arc<Database>, SolsticeError> {
        // M0 has no schema DSL, so the world is the hardcoded one (plan §5.1).
        let catalog = m0::catalog();
        let mut store = if cfg.path.is_empty() {
            SqliteStore::in_memory(catalog.schemas())
        } else {
            SqliteStore::open(&cfg.path, catalog.schemas())
        }
        .map_err(SolsticeError::store)?;
        m0::index(&mut store).map_err(SolsticeError::store)?;

        let (tx, rx) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("solstice-engine".to_string())
            .spawn(move || engine::run(store, catalog, rx))
            .map_err(|e| SolsticeError::Store(e.to_string()))?;

        Ok(Arc::new(Database {
            chan: Arc::new(Chan(Mutex::new(tx))),
            thread: Mutex::new(Some(thread)),
        }))
    }

    /// Subscribe to a query, given its encoded IR.
    ///
    /// Returns once the view exists, carrying it — see
    /// [`Subscription::initial`].
    pub fn subscribe(&self, query_ir: Vec<u8>) -> Result<Arc<Subscription>, SolsticeError> {
        let sub = self.chan.ask(|reply| Command::Subscribe {
            ir: query_ir,
            reply,
        })??;
        Ok(Arc::new(Subscription {
            sub_id: sub.sub_id,
            initial: sub.initial,
            chan: Arc::clone(&self.chan),
        }))
    }

    /// Apply an encoded mutation body, returning the version it landed at.
    ///
    /// The version is the join between this call and the events it caused: the
    /// `ViewDelta`s the sink receives for this write all carry it.
    pub fn mutate(&self, body: Vec<u8>) -> Result<u64, SolsticeError> {
        self.chan.ask(|reply| Command::Mutate { body, reply })?
    }

    pub fn set_event_sink(&self, sink: Box<dyn EngineEventSink>) -> Result<(), SolsticeError> {
        self.chan.send(Command::SetSink { sink: Some(sink) })
    }

    pub fn clear_event_sink(&self) -> Result<(), SolsticeError> {
        self.chan.send(Command::SetSink { sink: None })
    }

    pub fn stats(&self) -> Result<EngineStats, SolsticeError> {
        self.chan.ask(|reply| Command::Stats { reply })
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        // Explicit rather than waiting for the senders to drop: outstanding
        // `Subscription`s hold one too, and a database that stayed open because
        // a widget had not been disposed yet would be a leak nobody could see.
        let _ = self.chan.send(Command::Shutdown);
        if let Some(thread) = self.thread.lock().ok().and_then(|mut t| t.take()) {
            let _ = thread.join();
        }
    }
}

/// A live subscription. Dropping it unsubscribes.
pub struct Subscription {
    sub_id: u64,
    initial: Vec<u8>,
    chan: Arc<Chan>,
}

impl Subscription {
    /// The id every [`solstice_proto::ViewDelta`] for this query carries.
    pub fn sub_id(&self) -> u64 {
        self.sub_id
    }

    /// The view as it stood when the subscription was created, encoded.
    ///
    /// Held rather than streamed so that plan §4.2's first promise is kept
    /// literally: the rows are *already here* when `subscribe` returns, and a
    /// widget rebuilding never shows a loading state it does not need.
    ///
    /// Clones, because both generators want an owned buffer and the copy at the
    /// boundary happens either way.
    pub fn initial(&self) -> Vec<u8> {
        self.initial.clone()
    }
}

impl std::fmt::Debug for Subscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subscription")
            .field("sub_id", &self.sub_id)
            .field("initial_bytes", &self.initial.len())
            .finish()
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let _ = self.chan.send(Command::Unsubscribe {
            sub_id: self.sub_id,
        });
    }
}
