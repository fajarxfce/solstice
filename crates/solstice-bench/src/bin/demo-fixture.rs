//! Seed the world the Flutter and Compose demo apps open.
//!
//! Plan §5.1 asks for "SQLite diisi 100k issue / 1M comment" and two apps
//! rendering a scrolling list over it. The apps cannot seed that themselves:
//! the engine's only way in is `mutate` (plan §1.4's single-writer invariant),
//! so an app that built its own world would do it one `Insert` at a time, and
//! `solstice-core` has no RNG to do it with — plan §6 forbids ambient
//! nondeterminism in the crate the property tests depend on.
//!
//! So the world is built here, by the same generators the benchmark uses, and
//! pushed to the device as a file. That is also what makes the demo comparable
//! to `BENCHMARKS.md`: same seed, same row shapes, same string lengths, same
//! child fanout. A demo running on a world of its own would produce numbers
//! that could not be put next to the harness's.
//!
//! ```
//! cargo run --release -p solstice-bench --bin demo-fixture
//! ```
//!
//! # Why it prints the open-issue count
//!
//! The whole point of plan §7's query is a window with far more candidates
//! below it than it can show. If the subscribed project held 50 open issues,
//! the `TopK` would be `full` from hydration onwards, would never refill, and
//! the demo would be showing the easy case while claiming to show the hard one.
//! The number is printed so that claim stays checkable.

use solstice_bench::fixture::{self, IndexMode, Scale};
use solstice_bench::metrics::{bytes, dur};
use solstice_bench::rng::Rng;
use solstice_core::m0::{self, issue, ISSUES};
use solstice_ivm::{OpCx, Params, Predicate, ScanRequest};
use solstice_store::SqliteStore;
use std::path::PathBuf;
use std::time::Instant;

/// The same seed as `s1-fixture` and `s2-fixture`, so all three spikes and both
/// demo apps are looking at one world.
const SEED: u64 = 0x5015_71CE;

fn main() {
    let cfg = match args() {
        Ok(cfg) => cfg,
        Err(msg) => {
            eprintln!("{msg}");
            std::process::exit(2);
        }
    };
    if let Err(e) = run(&cfg) {
        eprintln!("demo-fixture: {e}");
        std::process::exit(1);
    }
}

struct Config {
    out: PathBuf,
    scale: Scale,
    project: i64,
}

fn args() -> Result<Config, String> {
    let mut cfg = Config {
        out: PathBuf::from("examples/fixtures/issues.db"),
        scale: Scale::M0,
        // `fixture::Query::default().project`, so the apps subscribe to the
        // project every number in `BENCHMARKS.md` was measured against. Any
        // other value would make the demo's latency incomparable to the
        // harness's for a reason nobody would think to check.
        project: 7,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |flag: &str| {
            args.next()
                .ok_or_else(|| format!("{flag} needs a value"))
                .map(|v| v.to_string())
        };
        match arg.as_str() {
            "--out" => cfg.out = value("--out")?.into(),
            "--scale" => {
                cfg.scale = match value("--scale")?.as_str() {
                    "m0" => Scale::M0,
                    "tiny" => Scale::TINY,
                    other => return Err(format!("--scale: {other:?} is not m0 or tiny")),
                }
            }
            "--project" => {
                cfg.project = value("--project")?
                    .parse()
                    .map_err(|_| "--project: not a number".to_string())?
            }
            "-h" | "--help" => {
                println!("usage: demo-fixture [--out FILE] [--scale m0|tiny] [--project N]");
                println!();
                println!("  seeds the database the demo apps open, and prints what it made");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    Ok(cfg)
}

fn run(cfg: &Config) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(dir) = cfg.out.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // WAL and its shared-memory sibling are state, not data. A stale `-wal` left
    // beside a freshly seeded file is how a device ends up reading a world that
    // no longer exists — and here the file is about to be pushed over USB, where
    // the sibling would be left behind on the phone rather than overwritten.
    for suffix in ["", "-wal", "-shm"] {
        let p = cfg.out.with_file_name(format!(
            "{}{suffix}",
            cfg.out.file_name().unwrap_or_default().to_string_lossy()
        ));
        if p.exists() {
            std::fs::remove_file(&p)?;
        }
    }

    let t = Instant::now();
    let open = {
        let mut store = SqliteStore::open(&cfg.out, m0::schemas())?;
        fixture::create_indexes(&mut store, IndexMode::Seek)?;
        fixture::seed(&mut store, &cfg.scale, &mut Rng::new(SEED))?;
        // Counted rather than estimated. The generator assigns projects and the
        // closed flag independently, so the size of this set is an emergent
        // property of two distributions, and the demo's claim to be exercising
        // a window rests on it.
        store
            .scan(&ScanRequest {
                table: ISSUES,
                order: Vec::new(),
                after: None,
                filter: Some(Predicate::and([
                    Predicate::eq(issue::PROJECT, cfg.project),
                    Predicate::eq(issue::CLOSED, 0i64),
                ])),
                params: Params::empty(),
                limit: cfg.scale.issues,
            })
            .len()
    };
    let seeded = t.elapsed();
    let file_bytes = std::fs::metadata(&cfg.out)?.len();

    println!(
        "seed {SEED:#x} · {} issues / {} comments / {} projects · {}",
        cfg.scale.issues,
        cfg.scale.comments(),
        cfg.scale.projects,
        dur(seeded),
    );
    println!("{} · {}", cfg.out.display(), bytes(file_bytes));
    println!(
        "project {} holds {open} open issues — {}",
        cfg.project,
        if open > 200 {
            "the window has candidates below it"
        } else {
            "WARNING: too few to make the window work for its living"
        }
    );

    // An empty project is not a warning, it is a broken fixture: the apps would
    // render an empty list and report perfect latency for it.
    if open == 0 {
        return Err(format!(
            "project {} has no open issues; the demo would render an empty list",
            cfg.project
        )
        .into());
    }
    Ok(())
}
