//! The engine thread: one command loop, one SQLite connection, one graph.
//!
//! Plan §4.3 asks for a single OS thread that owns the store and the whole
//! dataflow graph, with no locks on the hot path. That is what this is. Every
//! public method on [`Database`](crate::Database) turns into a [`Command`] and
//! every answer comes back down a reply channel, which is why nothing in here
//! needs a mutex: the channel *is* the serialisation.
//!
//! The shape also matters beyond performance. An engine that is a function from
//! command to effects is an engine a deterministic simulation can drive (plan
//! §6) — the production driver pumps it from a real channel, the simulator
//! pumps it from a seeded scheduler, and it is the same code.
//!
//! # The invariant this module exists to hold
//!
//! Plan §1.4: **there is exactly one code path that changes base data.** It is
//! [`Engine::mutate`]. Nothing else in the workspace writes an application
//! table, and the store's connection is private to this thread so nothing else
//! can. If that leaks, incremental maintenance stops being sound and the symptom
//! is nondeterministic silent staleness — the worst bug class this product can
//! have.

use std::collections::BTreeMap;
use std::sync::mpsc::{Receiver, SyncSender};

use solstice_ivm::{
    Batch, Change, CmpOp, Expr, OpCx, Params, Predicate, Row, RowKey, ScanRequest, Schema, TableId,
};
use solstice_proto::mutation::{Mutation, Op};
use solstice_proto::query::Query as QueryIr;
use solstice_proto::ViewDelta;
use solstice_store::SqliteStore;

use crate::catalog::Catalog;
use crate::compile::{compile, Compiled};
use crate::view::View;
use crate::{EngineEventSink, EngineStats, SolsticeError, Subscribed};

pub(crate) enum Command {
    Subscribe {
        ir: Vec<u8>,
        reply: SyncSender<Result<Subscribed, SolsticeError>>,
    },
    Unsubscribe {
        sub_id: u64,
    },
    Mutate {
        body: Vec<u8>,
        reply: SyncSender<Result<u64, SolsticeError>>,
    },
    SetSink {
        sink: Option<Box<dyn EngineEventSink>>,
    },
    Stats {
        reply: SyncSender<EngineStats>,
    },
    Shutdown,
}

struct Sub {
    compiled: Compiled,
    view: View,
}

pub(crate) struct Engine {
    store: SqliteStore,
    catalog: Catalog,
    subs: BTreeMap<u64, Sub>,
    sink: Option<Box<dyn EngineEventSink>>,
    next_sub: u64,
    version: u64,
    mutations: u64,
    events: u64,
}

/// Run until told to stop or until every sender is gone.
pub(crate) fn run(store: SqliteStore, catalog: Catalog, rx: Receiver<Command>) {
    let mut engine = Engine {
        store,
        catalog,
        subs: BTreeMap::new(),
        sink: None,
        next_sub: 1,
        version: 0,
        mutations: 0,
        events: 0,
    };
    while let Ok(cmd) = rx.recv() {
        if !engine.step(cmd) {
            break;
        }
    }
}

impl Engine {
    /// Handle one command. `false` ends the loop.
    ///
    /// A failed `reply.send` is ignored throughout: it means the caller gave up
    /// waiting, which is their business and not a reason to take the engine
    /// down with them.
    fn step(&mut self, cmd: Command) -> bool {
        match cmd {
            Command::Subscribe { ir, reply } => {
                let _ = reply.send(self.subscribe(&ir));
            }
            Command::Unsubscribe { sub_id } => {
                self.subs.remove(&sub_id);
            }
            Command::Mutate { body, reply } => {
                let _ = reply.send(self.mutate(&body));
            }
            Command::SetSink { sink } => self.sink = sink,
            Command::Stats { reply } => {
                let _ = reply.send(self.stats());
            }
            Command::Shutdown => return false,
        }
        true
    }

    /// Compile, hydrate, and answer with the initial view.
    ///
    /// Synchronously, which is plan §4.2's first guaranteed behaviour: a widget
    /// that rebuilds or a user who presses back gets rows, not a loading
    /// flicker. M0 hydrates on the calling path every time — honest, and
    /// trivially satisfying the promise. What M1 adds is the *sharing* (plan
    /// §1.3 interns operators by subtree hash), which is when "if the pipeline
    /// is already hydrated" starts to mean something.
    fn subscribe(&mut self, ir: &[u8]) -> Result<Subscribed, SolsticeError> {
        let q = QueryIr::decode(ir).map_err(SolsticeError::decode)?;
        let mut compiled = compile(&self.catalog, &q).map_err(SolsticeError::compile)?;

        let batch = compiled.graph.hydrate(&mut self.store);
        let mut view = View::new(compiled.order.clone());
        view.apply(&batch);

        let sub_id = self.next_sub;
        self.next_sub += 1;
        let initial = ViewDelta {
            sub_id,
            version: self.version,
            changes: view.snapshot(),
        }
        .encode();

        self.subs.insert(sub_id, Sub { compiled, view });
        Ok(Subscribed { sub_id, initial })
    }

    /// The single write path (plan §1.4).
    fn mutate(&mut self, body: &[u8]) -> Result<u64, SolsticeError> {
        let m = Mutation::decode(body).map_err(SolsticeError::decode)?;
        // Half-applying a body is worse than refusing it: it writes a state the
        // sender never asked for into the store whose whole soundness argument
        // is that one code path owns every write.
        if m.unknown_ops > 0 {
            return Err(SolsticeError::UnsupportedOps {
                count: m.unknown_ops,
            });
        }

        let mut deltas: Vec<(TableId, Batch)> = Vec::new();
        for op in &m.ops {
            let (table, change) = self.change_for(op)?;
            let Some(change) = change else { continue };
            match deltas.iter_mut().find(|(t, _)| *t == table) {
                // `Batch::push` composes changes to one key, so two ops on one
                // row reach the graph as the one change they add up to.
                Some((_, batch)) => batch.push(change),
                None => {
                    let mut batch = Batch::new();
                    batch.push(change);
                    deltas.push((table, batch));
                }
            }
        }
        if deltas.is_empty() {
            return Ok(self.version);
        }

        // Write first, then pump — in that order, because an operator that
        // refills during the pump reads the store and must not see a state
        // older than the delta it is reacting to.
        //
        // One transaction per table, not one per mutation. Plan §1.4 requires a
        // body touching five tables to commit once, and this does not do that
        // yet: it needs a store API that holds a transaction open across tables.
        // Until then a crash mid-body can leave part of it applied, which M0
        // does not exercise because it has no crash recovery to exercise it.
        for (table, batch) in &deltas {
            self.store
                .apply(*table, batch)
                .map_err(SolsticeError::store)?;
        }

        self.version += 1;
        self.mutations += 1;
        self.pump(&deltas);
        Ok(self.version)
    }

    /// One coherent pump across every subscription, at one version.
    ///
    /// Every view touched by this mutation emits at the same `version`, which
    /// is what plan §1.4 means by no torn reads: a host that renders two lists
    /// can tell that it is looking at one consistent state.
    fn pump(&mut self, deltas: &[(TableId, Batch)]) {
        let Engine {
            store,
            subs,
            sink,
            version,
            events,
            ..
        } = self;
        let Some(sink) = sink.as_ref() else {
            // Nothing to send it to. The graphs still have to be pumped —
            // skipping that would leave operator state behind the store, and
            // the next subscribe would disagree with the next delta.
            for sub in subs.values_mut() {
                let out = sub.compiled.graph.pump(deltas, store);
                sub.view.apply(&out);
            }
            return;
        };

        for (sub_id, sub) in subs.iter_mut() {
            let out = sub.compiled.graph.pump(deltas, store);
            if out.is_empty() {
                continue;
            }
            let changes = sub.view.apply(&out);
            if changes.is_empty() {
                continue;
            }
            sink.on_event(
                ViewDelta {
                    sub_id: *sub_id,
                    version: *version,
                    changes,
                }
                .encode(),
            );
            *events += 1;
        }
    }

    /// One operation as the keyed change the graph speaks, or `None` if it is a
    /// write that changes nothing.
    fn change_for(&mut self, op: &Op) -> Result<(TableId, Option<Change>), SolsticeError> {
        match op {
            Op::Insert { table, row } => {
                let schema = self.schema(table)?;
                if row.len() != schema.arity() {
                    return Err(SolsticeError::mutation(format!(
                        "insert into {table:?} has {} values, the table has {} columns",
                        row.len(),
                        schema.arity()
                    )));
                }
                let key = RowKey::new(row.get(schema.pk).clone());
                // An insert over a key that exists is an update. Passing it down
                // as an insert would tell operators a row appeared that they are
                // already holding.
                let before = self.lookup(&schema, &key);
                Ok((
                    schema.table,
                    Change::from_images(key, before, Some(row.clone())),
                ))
            }

            Op::Update { table, key, set } => {
                let schema = self.schema(table)?;
                let key = RowKey::new(key.clone());
                let before = self.require(&schema, &key, table)?;
                let mut values = before.values().to_vec();
                for (name, value) in set {
                    let col = schema.col(name).ok_or_else(|| {
                        SolsticeError::mutation(format!("no column {name:?} in table {table:?}"))
                    })?;
                    if col == schema.pk {
                        // The key is the row's identity everywhere downstream —
                        // in the view index, in the join's parent map, in the
                        // overlay. Moving it is a delete and an insert, and the
                        // caller has to say so.
                        return Err(SolsticeError::mutation(format!(
                            "update cannot change the primary key {name:?} of {table:?}"
                        )));
                    }
                    values[col as usize] = value.clone();
                }
                Ok((
                    schema.table,
                    Change::from_images(key, Some(before), Some(Row::new(values))),
                ))
            }

            Op::Delete { table, key } => {
                let schema = self.schema(table)?;
                let key = RowKey::new(key.clone());
                let before = self.require(&schema, &key, table)?;
                Ok((schema.table, Change::from_images(key, Some(before), None)))
            }
        }
    }

    fn schema(&self, name: &str) -> Result<Schema, SolsticeError> {
        self.catalog
            .table(name)
            .cloned()
            .ok_or_else(|| SolsticeError::mutation(format!("no table named {name:?}")))
    }

    /// The row, or an error naming the one that is not there.
    ///
    /// An update or delete of a row that does not exist is refused rather than
    /// treated as a no-op. In M0 it can only mean the caller is wrong, and a
    /// write that silently does nothing is the hardest kind of bug to see. What
    /// makes this a real question is rebase (plan §2.3), where replaying a
    /// mutation against a moved base legitimately finds the row gone — and that
    /// is what `Precondition` is reserved for.
    fn require(
        &mut self,
        schema: &Schema,
        key: &RowKey,
        table: &str,
    ) -> Result<Row, SolsticeError> {
        self.lookup(schema, key).ok_or_else(|| {
            SolsticeError::mutation(format!("no row {:?} in table {table:?}", key.value()))
        })
    }

    /// One row by primary key. A seek: the pk is the table's own index.
    fn lookup(&mut self, schema: &Schema, key: &RowKey) -> Option<Row> {
        self.store
            .scan(&ScanRequest {
                table: schema.table,
                order: Vec::new(),
                after: None,
                filter: Some(Predicate::Cmp {
                    lhs: Expr::Col(schema.pk),
                    op: CmpOp::Eq,
                    rhs: Expr::Lit(key.value().clone()),
                }),
                params: Params::empty(),
                limit: 1,
            })
            .into_iter()
            .next()
            .map(|(_, row)| row)
    }

    fn stats(&self) -> EngineStats {
        let refills = self.store.refill_stats();
        EngineStats {
            subscriptions: self.subs.len() as u64,
            view_rows: self.subs.values().map(|s| s.view.len() as u64).sum(),
            view_bytes: self.subs.values().map(|s| s.view.heap_bytes() as u64).sum(),
            graph_bytes: self
                .subs
                .values()
                .map(|s| s.compiled.graph.state_bytes() as u64)
                .sum(),
            version: self.version,
            mutations: self.mutations,
            events: self.events,
            window_refills: refills.window as u64,
            window_refill_rows: refills.window_rows as u64,
            child_refills: refills.children as u64,
            child_refill_rows: refills.children_rows as u64,
        }
    }
}
