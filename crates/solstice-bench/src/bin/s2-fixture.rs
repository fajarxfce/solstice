//! Seed the SQLite file the spike S2 hosts open across FFI, and print the
//! control numbers they are measured against.
//!
//! S1 handed the hosts a `.bin` and timed the decode. S2 hands them a `.db` and
//! makes them go through [`solstice_core::Database`] — `open`, `subscribe`,
//! `mutate` — so what gets timed is the round trip: a query built in Dart or
//! Kotlin, encoded, crossed into Rust, hydrated, and a view crossed back.
//!
//! # Why a file and not a seeding API
//!
//! The engine's only way in is `mutate`, so a host could seed its own world one
//! `Insert` at a time. It would also then be measuring 20,000 mutations, and the
//! number S2 is after is what the *boundary* costs, not what seeding costs. A
//! file seeded here by the same generators `s1-fixture` used keeps the row shape
//! — string lengths, null density, child fanout — identical to the payload S1
//! measured, which is the only way the two spikes' numbers can be subtracted.
//!
//! It also answers a question M0 asks later: the Flutter and Compose demo apps
//! need a 100k-row database and `solstice-core` has no RNG (plan §6 forbids
//! ambient nondeterminism in it). This is where that database comes from.
//!
//! # The control numbers
//!
//! Everything the hosts time is timed here first, in Rust, against the same
//! file. Without that a Dart figure is unanchored — "6ms" cannot be split into
//! "the engine took 6ms" and "the boundary took 6ms" — and the whole point of
//! S2 is the difference between the two columns.

use solstice_bench::fixture::{self, Scale};
use solstice_bench::metrics::{bytes, dur};
use solstice_bench::rng::Rng;
use solstice_core::{m0, Database, OpenConfig};
use solstice_proto::mutation::{Mutation, Op};
use solstice_proto::ViewDelta;
use solstice_store::SqliteStore;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Matches `s1-fixture`, so the same seed produces the same world.
const SEED: u64 = 0x5015_71CE;
/// The window S1 measured, so the two spikes' numbers line up.
const ROWS: u32 = 1000;
/// The window plan §7 actually names.
const K: u32 = 50;
const COMMENTS: u32 = 3;
const PROJECT: i64 = 1;
const REPS: usize = 50;

fn main() {
    let out = match args() {
        Ok(out) => out,
        Err(msg) => {
            eprintln!("{msg}");
            std::process::exit(2);
        }
    };
    if let Err(e) = run(&out) {
        eprintln!("s2-fixture: {e}");
        std::process::exit(1);
    }
}

fn args() -> Result<PathBuf, String> {
    let mut args = std::env::args().skip(1);
    let mut out = PathBuf::from("spikes/s2-bridge/fixtures");
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--out" => {
                out = args
                    .next()
                    .ok_or_else(|| "--out needs a directory".to_string())?
                    .into()
            }
            "-h" | "--help" => {
                println!("usage: s2-fixture [--out DIR]");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    Ok(out)
}

fn run(out: &Path) -> Result<(), Box<dyn std::error::Error>> {
    // Four projects, so project 1 holds comfortably more than 1000 open issues:
    // the 1000-row window has to be *full* or the hosts would be timing a
    // payload nobody asked for.
    let scale = Scale {
        issues: 20_000,
        comments_per_issue: 10,
        projects: 4,
    };

    std::fs::create_dir_all(out)?;
    let db_path = out.join("s2.db");
    // WAL and its shared-memory sibling are state, not data. Leaving a stale
    // `-wal` beside a freshly seeded file is how a host ends up reading a world
    // that no longer exists.
    for suffix in ["", "-wal", "-shm"] {
        let p = out.join(format!("s2.db{suffix}"));
        if p.exists() {
            std::fs::remove_file(&p)?;
        }
    }

    let t = Instant::now();
    {
        let mut store = SqliteStore::open(&db_path, m0::schemas())?;
        fixture::create_indexes(&mut store, fixture::IndexMode::Seek)?;
        fixture::seed(&mut store, &scale, &mut Rng::new(SEED))?;
    }
    let seeded = t.elapsed();
    let file_bytes = std::fs::metadata(&db_path)?.len();

    println!("# spike S2 fixture");
    println!();
    println!(
        "seed `{SEED:#x}` · {} issues / {} comments · seeded in {} · `{}` is {}",
        scale.issues,
        scale.comments(),
        dur(seeded),
        db_path.display(),
        bytes(file_bytes),
    );
    println!();

    // The query IR the hosts build for themselves. Written out so a host can
    // check its own encoder against it byte for byte — a Dart benchmark that
    // built a subtly different query would produce a different view and its
    // number would not be comparable with anything here.
    println!("| file | bytes |");
    println!("|---|---|");
    for (name, k) in [("query-1000.bin", ROWS), ("query-50.bin", K)] {
        let ir = m0::query(PROJECT, k, COMMENTS).encode();
        std::fs::write(out.join(name), &ir)?;
        println!("| `{name}` | {} |", ir.len());
    }
    // `mutation.bin` is written by the control below, because the row it
    // targets is not knowable until a view exists — see [`touch`].
    control(&db_path, out)?;
    Ok(())
}

/// A one-column update to one issue: the smallest write that moves a view.
///
/// Two choices in here, both of which a first draft got wrong.
///
/// **The key comes from the view, not from a constant.** An arbitrary issue id
/// is almost certainly *outside* the window — 20,000 issues across 4 projects,
/// and the window holds the top 1000 open ones of project 1 by priority — so
/// the write would be correctly ignored and the benchmark would time a mutation
/// that produces no event at all. Timing the path that emits nothing is the
/// easiest way to make a boundary look cheap.
///
/// **The column is `updated_at`, not `priority`.** Priority is the sort key, so
/// writing it would emit a `Moved` as well, and a different one on each
/// repetition as the row walked through the window. This emits exactly one
/// `Changed` per view, every time.
fn touch(n: u64, key: i64) -> Mutation {
    Mutation {
        client_id: vec![2],
        mutation_id: n,
        timestamp_ms: 0,
        ops: vec![Op::Update {
            table: "issues".to_string(),
            key: solstice_ivm::Value::Int(key),
            set: vec![("updated_at".to_string(), solstice_ivm::Value::Int(n as i64))],
        }],
        unknown_ops: 0,
    }
}

/// The id of the first row of an encoded view — a row the window certainly
/// holds, since it just came out of it.
fn first_id(initial: &[u8]) -> Result<i64, Box<dyn std::error::Error>> {
    let delta = ViewDelta::decode(initial)?;
    match delta.changes.first() {
        Some(solstice_proto::ViewChange::Added { row, .. }) => match row.values().first() {
            Some(solstice_ivm::Value::Int(id)) => Ok(*id),
            other => Err(format!("first column is {other:?}, not an id").into()),
        },
        other => Err(format!("initial view opens with {other:?}, not an Added").into()),
    }
}

/// The same calls the hosts make, made from Rust against the same file.
///
/// Best-of rather than mean, for the reason `s1-fixture` gives: the work is
/// deterministic, so the spread between repetitions is the machine's and the
/// fastest run has least of it in.
fn control(path: &Path, out: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let db = Database::open(OpenConfig::at(path.to_string_lossy().to_string()))?;

    println!("| what | rust |");
    println!("|---|---|");

    // Held, not dropped at the end of each iteration. The `mutate` timing below
    // has to include maintaining a live 1000-row view, because that is the only
    // version of it a running app ever performs.
    let mut live = Vec::new();
    for k in [ROWS, K] {
        let ir = m0::query(PROJECT, k, COMMENTS).encode();

        // `subscribe` includes hydration, so it is timed once and reported as
        // what it is. Repeating it would measure a warm page cache, and the
        // first subscribe after a cold start is the frame users actually see.
        let t = Instant::now();
        let sub = db.subscribe(ir.clone())?;
        let subscribed = t.elapsed();
        let initial = sub.initial();

        // This one is the control that matters. `initial()` is a clone of a
        // buffer the engine already holds — no engine work at all — so it is
        // the pure cost of producing the bytes the boundary then has to carry.
        // Whatever a host measures for the same call, minus this, is the copy
        // `flutter_rust_bridge` and UniFFI make.
        let mut copy = Duration::MAX;
        for _ in 0..REPS {
            let t = Instant::now();
            let buf = sub.initial();
            copy = copy.min(t.elapsed());
            std::hint::black_box(buf.len());
        }

        let mut decode = Duration::MAX;
        for _ in 0..REPS {
            let t = Instant::now();
            let delta = ViewDelta::decode(&initial).expect("our own bytes");
            decode = decode.min(t.elapsed());
            std::hint::black_box(delta.changes.len());
        }

        println!(
            "| subscribe k={k} ({}) | {} |",
            bytes(initial.len() as u64),
            dur(subscribed)
        );
        println!("| initial() k={k} — buffer copy | {} |", dur(copy));
        println!("| decode k={k} | {} |", dur(decode));
        live.push(sub);
    }

    // Inbound. A `mutate` is a channel round trip plus a SQLite write, and the
    // host pays a boundary copy on top of it — small, but it is the direction
    // S1 never measured at all.
    let key = first_id(&live[0].initial())?;
    let body = touch(1, key).encode();
    std::fs::write(out.join("mutation.bin"), &body)?;
    println!("| `mutation.bin` — issue {key} | {} bytes |", body.len());

    // Twice, without and then with a sink, because that pair is what isolates
    // the callback. `Engine::pump` maintains every graph and every view either
    // way; a sink only adds encoding the `ViewDelta` and handing it over. Here
    // the hand-over is a virtual call into a counter, so the difference is
    // essentially just the encode — and a host's version of that same
    // difference is what its binding charges to deliver an event.
    let mut n = 0u64;
    let mutate = |db: &Database, n: &mut u64| -> Result<Duration, Box<dyn std::error::Error>> {
        let mut best = Duration::MAX;
        for _ in 0..REPS {
            *n += 1;
            let body = touch(*n, key).encode();
            let t = Instant::now();
            db.mutate(body)?;
            best = best.min(t.elapsed());
        }
        Ok(best)
    };

    let deaf = mutate(&db, &mut n)?;
    println!("| mutate — no sink installed | {} |", dur(deaf));

    let events = Arc::new(Count::default());
    db.set_event_sink(Box::new(Sink(Arc::clone(&events))))?;
    let heard = mutate(&db, &mut n)?;
    println!(
        "| mutate — 1 update, {} views live | {} |",
        live.len(),
        dur(heard)
    );

    let (count, event_bytes) = events.read();
    println!("| event emitted per mutate | {} |", bytes(event_bytes));

    let stats = db.stats()?;
    println!();
    println!(
        "cross-check: {} subscriptions · {} view rows · {} mutations ({} before the sink) · \
         {count} events · v{}",
        stats.subscriptions, stats.view_rows, stats.mutations, REPS, stats.version
    );
    Ok(())
}

/// Counts the events the sink receives, so the host has a size to compare
/// against and the cross-check has something to assert on.
#[derive(Default)]
struct Count {
    events: Mutex<(u64, u64)>,
}

impl Count {
    fn read(&self) -> (u64, u64) {
        *self.events.lock().expect("not poisoned")
    }
}

/// A newtype, because `impl EngineEventSink for Arc<Count>` is not allowed:
/// `Arc` is not `#[fundamental]`, so neither the trait nor the type is local.
struct Sink(Arc<Count>);

impl solstice_core::EngineEventSink for Sink {
    fn on_event(&self, event: Vec<u8>) {
        let mut g = self.0.events.lock().expect("not poisoned");
        g.0 += 1;
        g.1 = event.len() as u64;
    }
}
