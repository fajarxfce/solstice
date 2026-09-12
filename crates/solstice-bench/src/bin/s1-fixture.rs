//! Emit the payloads the spike S1 host benchmarks decode.
//!
//! Plan §5.1's kill criterion is *"decode of an initial 1000-row view under 5ms
//! in Dart **and** Kotlin"*, and the number is only worth having if the bytes
//! are bytes the engine would really send. So this does not synthesise a
//! payload: it seeds real SQLite with the benchmark's own row generators, runs
//! a real hydration of plan §7's query with the window widened to 1000, and
//! encodes whatever came out.
//!
//! Two files land in the output directory:
//!
//! * `view-1000.bin` — the initial view: 1000 `Added`, each a joined issue row
//!   carrying its three latest comments as a nested collection.
//! * `delta-5.bin` — five of those same changes. Deliberately the *heaviest*
//!   five-change delta the pipeline can emit, since every `Added` carries a
//!   whole parent plus its children, where a realistic mix would be mostly
//!   `Changed` and `Moved`. A budget is worth more when it is set against the
//!   bad case.
//!
//! It also prints the Rust-side encode and decode cost. That is the control
//! number: without it a Dart figure is unanchored, and "slow" cannot be
//! separated into "this encoding is slow" and "this host is slow at it".
//!
//! # Scale
//!
//! Smaller than the M0 benchmark, on purpose — 20,000 issues rather than
//! 100,000. The row *generators* are the same ones `fixture::seed` uses for the
//! full run, so bytes per row are identical; the only thing the extra 80,000
//! issues would buy is a minute of seeding and an 89MB file to hold rows 1001
//! and up, which never reach the window.

use solstice_bench::fixture::{self, Query, Scale};
use solstice_bench::metrics::{bytes, dur};
use solstice_bench::probe::Probe;
use solstice_bench::rng::Rng;
use solstice_ivm::{Relation, Value};
use solstice_proto::{ViewChange, ViewDelta};
use solstice_store::SqliteStore;
use std::path::PathBuf;
use std::time::Instant;

/// Matches the bench harness, so the same seed produces the same world.
const SEED: u64 = 0x5015_71CE;
const ROWS: usize = 1000;
const DELTA_ROWS: usize = 5;

fn main() {
    let out = match args() {
        Ok(out) => out,
        Err(msg) => {
            eprintln!("{msg}");
            std::process::exit(2);
        }
    };
    if let Err(e) = run(&out) {
        eprintln!("s1-fixture: {e}");
        std::process::exit(1);
    }
}

fn args() -> Result<PathBuf, String> {
    let mut args = std::env::args().skip(1);
    let mut out = PathBuf::from("spikes/s1-decode/fixtures");
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--out" => {
                out = args
                    .next()
                    .ok_or_else(|| "--out needs a directory".to_string())?
                    .into()
            }
            "-h" | "--help" => {
                println!("usage: s1-fixture [--out DIR]");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    Ok(out)
}

fn run(out: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    // Four projects so that one of them holds comfortably more than 1000 open
    // issues: the window must be *full*, or the fixture would be short and the
    // budget would be measured against a payload nobody asked for.
    let scale = Scale {
        issues: 20_000,
        comments_per_issue: 10,
        projects: 4,
    };
    let query = Query {
        project: 1,
        k: ROWS,
        comments: 3,
    };

    let mut store = SqliteStore::in_memory(fixture::schemas())?;
    fixture::create_indexes(&mut store, fixture::IndexMode::Seek)?;
    let t = Instant::now();
    fixture::seed(&mut store, &scale, &mut Rng::new(SEED))?;
    let seeded = t.elapsed();

    let mut pipeline = fixture::build(&query);
    let mut probe = Probe::new(&mut store);
    let t = Instant::now();
    let initial = pipeline.graph.hydrate(&mut probe);
    let hydrated = t.elapsed();

    let mut view = Relation::new();
    view.apply(&initial);
    if view.len() < ROWS {
        return Err(format!(
            "hydration produced {} rows, not {ROWS} — the fixture would understate the budget",
            view.len()
        )
        .into());
    }

    // Positions are assigned in iteration order. Sorting them into sort-key
    // order is the `Output` operator's job (plan §1.3) and it does not exist
    // yet; it also cannot change a single byte of the payload, since an index
    // is a varint either way. What is being measured here is the decode.
    let changes: Vec<ViewChange> = view
        .iter()
        .take(ROWS)
        .enumerate()
        .map(|(i, (_, row))| ViewChange::Added {
            index: i as u32,
            row: row.clone(),
        })
        .collect();

    let full = ViewDelta {
        sub_id: 1,
        version: 1,
        changes,
    };
    let small = ViewDelta {
        sub_id: 1,
        version: 2,
        changes: full.changes[..DELTA_ROWS].to_vec(),
    };

    println!("# spike S1 fixtures");
    println!();
    println!(
        "seed `{SEED:#x}` · {} issues / {} comments · seeded in {} · hydrated {} rows in {}",
        scale.issues,
        scale.comments(),
        dur(seeded),
        view.len(),
        dur(hydrated),
    );
    println!();
    println!("| file | changes | scalars | bytes | bytes/row |");
    println!("|---|---|---|---|---|");

    std::fs::create_dir_all(out)?;
    for (name, delta) in [("view-1000.bin", &full), ("delta-5.bin", &small)] {
        let encoded = delta.encode();
        let path = out.join(name);
        std::fs::write(&path, &encoded)?;
        println!(
            "| `{name}` | {} | {} | {} | {} |",
            delta.changes.len(),
            scalars(delta),
            bytes(encoded.len() as u64),
            encoded.len() / delta.changes.len().max(1),
        );
    }
    println!();
    println!("Written to `{}`.", out.display());
    println!();

    control(&full, "view-1000");
    control(&small, "delta-5");

    Ok(())
}

/// The Rust-side cost of the same work the hosts are about to do.
///
/// Reported as a best-of, not a mean. A decode is deterministic, so the
/// variation between repetitions is the machine's — scheduler, frequency,
/// another process — and the fastest run is the one with least of it in.
fn control(delta: &ViewDelta, label: &str) {
    const REPS: usize = 50;

    let mut encode = std::time::Duration::MAX;
    let mut decode = std::time::Duration::MAX;
    for _ in 0..REPS {
        let t = Instant::now();
        let buf = delta.encode();
        encode = encode.min(t.elapsed());

        let t = Instant::now();
        let back = ViewDelta::decode(&buf).expect("our own bytes");
        decode = decode.min(t.elapsed());
        // Decoding into a value nobody reads is the kind of thing an optimiser
        // is entitled to delete. Touching the result keeps the work real.
        std::hint::black_box(back.changes.len());
    }
    println!(
        "{label}: rust encode {} · rust decode {}",
        dur(encode),
        dur(decode)
    );
}

/// Scalar values in the payload, children included — the unit the decode
/// actually pays per, and the one a columnar fallback would change.
fn scalars(delta: &ViewDelta) -> usize {
    fn in_row(row: &solstice_ivm::Row) -> usize {
        row.values()
            .iter()
            .map(|v| match v {
                Value::Rows(rows) => rows.iter().map(in_row).sum::<usize>(),
                _ => 1,
            })
            .sum()
    }
    delta
        .changes
        .iter()
        .map(|c| match c {
            ViewChange::Added { row, .. } | ViewChange::Changed { row, .. } => in_row(row),
            ViewChange::Removed { .. } => 1,
            ViewChange::Moved { .. } => 0,
        })
        .sum()
}
