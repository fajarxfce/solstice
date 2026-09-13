//! `solstice-bench` — run the M0 harness and print the kill-criteria table.
//!
//! ```text
//! cargo run --release -p solstice-bench
//! cargo run --release -p solstice-bench -- --index order --txns 5000
//! ```
//!
//! The defaults are plan §5.1's: 100k issues, a million comments, and the top
//! 50 open issues of one project with their three latest comments each. The
//! output is markdown, because its destination is `BENCHMARKS.md` and a number
//! that has to be reformatted by hand before it can be published is a number
//! that will not be published every release.

use solstice_bench::fixture::{IndexMode, Query, Scale};
use solstice_bench::metrics::{bytes, dur};
use solstice_bench::{run, Config, Phase, Report};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

/// Plan §5.1's write rate for the demo app's background thread.
const BUDGET_ROWS_PER_SECOND: f64 = 200.0;

/// Plan §5.1's kill criteria, in the units this harness can actually produce.
const BUDGET_P99: Duration = Duration::from_millis(16);
const BUDGET_RSS: u64 = 60 << 20;
const BUDGET_REFILLS_PER_SECOND: f64 = 5.0;

fn main() -> ExitCode {
    let cfg = match parse(std::env::args().skip(1)) {
        Ok(Some(cfg)) => cfg,
        Ok(None) => return ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("solstice-bench: {msg}");
            eprintln!("{USAGE}");
            return ExitCode::FAILURE;
        }
    };

    let keep = cfg.keep;
    let db = cfg.config.db.clone();
    let result = run(&cfg.config);

    if let (Some(path), false) = (&db, keep) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(path.with_extension("db-wal"));
        let _ = std::fs::remove_file(path.with_extension("db-shm"));
    }

    match result {
        Ok(report) => {
            print(&cfg, &report);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("solstice-bench: {e}");
            ExitCode::FAILURE
        }
    }
}

struct Args {
    config: Config,
    keep: bool,
}

const USAGE: &str = "\
usage: solstice-bench [options]

  --seed N            generator seed (every number below is reproducible from it)
  --scale m0|tiny     100k/1M, or something that finishes while you watch
  --issues N          override the issue count
  --comments-per N    comments generated per issue
  --projects N        how many projects the issues are spread across
  --project N         which project the subscription is for
  --k N               issues on screen
  --comments N        comments shown per issue
  --index seek|order|none
                      which indexes the store gets; `order` is what
                      create_order_index alone can express
  --txns N            transactions per phase
  --bias PCT          percent of churn aimed at the subscribed project
  --memory            run in memory instead of on disk (faster, less honest)
  --db PATH           where to put the database
  --keep              do not delete the database afterwards
  --no-verify         skip the SQLite cross-check (do not)
  --help";

fn parse(args: impl Iterator<Item = String>) -> Result<Option<Args>, String> {
    let mut config = Config::default();
    let mut keep = false;
    let mut on_disk = true;
    let mut args = args.peekable();

    fn value(
        args: &mut std::iter::Peekable<impl Iterator<Item = String>>,
        flag: &str,
    ) -> Result<String, String> {
        args.next().ok_or_else(|| format!("{flag} needs a value"))
    }

    fn number<T: std::str::FromStr>(s: &str, flag: &str) -> Result<T, String> {
        s.parse()
            .map_err(|_| format!("{flag}: {s:?} is not a number"))
    }

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                println!("{USAGE}");
                return Ok(None);
            }
            "--seed" => config.seed = number(&value(&mut args, "--seed")?, "--seed")?,
            "--scale" => {
                let v = value(&mut args, "--scale")?;
                config.scale = match v.as_str() {
                    "m0" => Scale::M0,
                    "tiny" => Scale::TINY,
                    _ => return Err(format!("--scale: {v:?} is not m0 or tiny")),
                };
            }
            "--issues" => config.scale.issues = number(&value(&mut args, "--issues")?, "--issues")?,
            "--comments-per" => {
                config.scale.comments_per_issue =
                    number(&value(&mut args, "--comments-per")?, "--comments-per")?
            }
            "--projects" => {
                config.scale.projects = number(&value(&mut args, "--projects")?, "--projects")?
            }
            "--project" => {
                config.query.project = number(&value(&mut args, "--project")?, "--project")?
            }
            "--k" => config.query.k = number(&value(&mut args, "--k")?, "--k")?,
            "--comments" => {
                config.query.comments = number(&value(&mut args, "--comments")?, "--comments")?
            }
            "--index" => {
                let v = value(&mut args, "--index")?;
                config.index = IndexMode::parse(&v)
                    .ok_or_else(|| format!("--index: {v:?} is not seek, order or none"))?;
            }
            "--txns" => config.txns = number(&value(&mut args, "--txns")?, "--txns")?,
            "--bias" => config.bias = number(&value(&mut args, "--bias")?, "--bias")?,
            "--memory" => on_disk = false,
            "--db" => {
                config.db = Some(PathBuf::from(value(&mut args, "--db")?));
                on_disk = true;
            }
            "--keep" => keep = true,
            "--no-verify" => config.verify = false,
            other => return Err(format!("unknown option {other:?}")),
        }
    }

    if on_disk && config.db.is_none() {
        config.db = Some(std::env::temp_dir().join(format!(
            "solstice-bench-{}-{:x}.db",
            std::process::id(),
            config.seed
        )));
    }
    if !on_disk {
        config.db = None;
    }

    Ok(Some(Args { config, keep }))
}

fn print(args: &Args, r: &Report) {
    let cfg = &args.config;
    let q: &Query = &cfg.query;

    println!("# Solstice M0 bench");
    println!();
    println!(
        "seed `{:#x}` · scale {} issues / {} comments across {} projects · index `{}`",
        cfg.seed,
        cfg.scale.issues,
        cfg.scale.comments(),
        cfg.scale.projects,
        cfg.index.name()
    );
    println!(
        "query: top {} open issues in project {} by priority desc, {} latest comments each",
        q.k, q.project, q.comments
    );
    println!(
        "store: {} · {} issues in the project, {} of them open",
        match (&cfg.db, r.db_bytes) {
            (Some(p), Some(n)) => format!("{} ({})", p.display(), bytes(n)),
            (Some(p), None) => p.display().to_string(),
            (None, _) => "in memory".to_string(),
        },
        r.issues_in_project,
        r.open_in_project
    );
    println!();

    println!("## Hydration — the first frame");
    println!();
    println!(
        "- {} in {}",
        plural(r.hydration.view_rows, "row"),
        dur(r.hydration.elapsed)
    );
    println!(
        "- issues: {} scan(s) reading {} rows, bounded at {}",
        r.hydration.issues.scans, r.hydration.issues.rows, r.hydrate_limit
    );
    println!(
        "- comments: {} scan(s) reading {} rows — one bounded read per parent on screen",
        r.hydration.comments.scans, r.hydration.comments.rows
    );
    println!(
        "- read {:.4}% of the {} rows in the store",
        (r.hydration.issues.rows + r.hydration.comments.rows) as f64 / cfg.scale.rows() as f64
            * 100.0,
        cfg.scale.rows()
    );
    println!("- state: {}", state_line(&r.hydration.state));
    println!();

    for phase in &r.phases {
        print_phase(phase);
    }

    println!("## Kill criteria (plan §5.1)");
    println!();
    println!(
        "Engine half only, on this machine. The budgets are written for a \
         committed frame on a mid-range Android phone; see the crate docs for \
         what that means for these numbers."
    );
    println!();
    println!("| Metric | Budget | Measured | |");
    println!("|---|---|---|---|");

    let worst_p99 = r
        .phases
        .iter()
        .map(|p| p.latency.p99)
        .max()
        .unwrap_or_default();
    row(
        "p99 delta → pump returned",
        &format!("< {}", dur(BUDGET_P99)),
        &dur(worst_p99),
        worst_p99 < BUDGET_P99,
    );

    let state = r.phases.last().map(|p| p.state_bytes).unwrap_or(0) as u64;
    row(
        "engine operator state",
        &format!("< {}", bytes(BUDGET_RSS)),
        &bytes(state),
        state < BUDGET_RSS,
    );

    match r
        .phases
        .iter()
        .filter_map(|p| p.rss)
        .max_by_key(|r| r.anon())
    {
        Some(peak) => {
            row(
                "peak anonymous RSS (engine + harness)",
                &format!("< {}", bytes(BUDGET_RSS)),
                &bytes(peak.anon()),
                peak.anon() < BUDGET_RSS,
            );
            println!(
                "| peak total RSS | — | {} ({} of it SQLite's reclaimable mmap window) | — |",
                bytes(peak.resident),
                bytes(peak.file_backed)
            );
        }
        None => println!(
            "| peak anonymous RSS | < {} | not measurable on this platform | — |",
            bytes(BUDGET_RSS)
        ),
    }

    if let Some(adv) = r.phases.iter().find(|p| p.name == "delete-the-top") {
        let per_sec = adv.refills_per_second_at(BUDGET_ROWS_PER_SECOND);
        row(
            "TopK refills/sec, adversarial @200 rows/s",
            &format!("< {BUDGET_REFILLS_PER_SECOND:.0}"),
            &format!("{per_sec:.1}"),
            per_sec < BUDGET_REFILLS_PER_SECOND,
        );
    }

    println!();
    println!("Seeding took {}.", dur(r.seed_elapsed));
}

fn print_phase(p: &Phase) {
    println!("## {} — {} transactions", p.name, p.txns);
    println!();
    println!(
        "- {} row mutations, {} of {} pumps changed the view",
        p.mutations, p.changed, p.txns
    );
    println!(
        "- apply + pump: p50 {} · p99 {} · max {}",
        dur(p.latency.p50),
        dur(p.latency.p99),
        dur(p.latency.max)
    );
    println!(
        "- window refills: {} returning {} rows ({:.2} per 100 mutations → {:.1}/s at {} rows/s)",
        p.refills.window,
        p.refills.window_rows,
        if p.mutations == 0 {
            0.0
        } else {
            p.refills.window as f64 / p.mutations as f64 * 100.0
        },
        p.refills_per_second_at(BUDGET_ROWS_PER_SECOND),
        BUDGET_ROWS_PER_SECOND as u64,
    );
    println!(
        "- child-window reads: {} returning {} rows — one per parent entering the view, \
         not a kill criterion",
        p.refills.children, p.refills.children_rows
    );
    println!(
        "- {} operator scan(s) reading {} rows in total",
        p.operator_scans.scans, p.operator_scans.rows
    );
    println!(
        "- state {}{}",
        bytes(p.state_bytes as u64),
        match p.rss {
            Some(rss) => format!(
                ", rss {} anonymous + {} file-backed",
                bytes(rss.anon()),
                bytes(rss.file_backed)
            ),
            None => String::new(),
        }
    );
    println!();
}

fn state_line(state: &[(&'static str, usize)]) -> String {
    let total: usize = state.iter().map(|(_, b)| b).sum();
    let parts: Vec<String> = state
        .iter()
        .map(|(name, b)| format!("{name} {}", bytes(*b as u64)))
        .collect();
    format!("{} (total {})", parts.join(" · "), bytes(total as u64))
}

fn row(metric: &str, budget: &str, measured: &str, pass: bool) {
    println!(
        "| {metric} | {budget} | {measured} | {} |",
        if pass { "PASS" } else { "**FAIL**" }
    );
}

fn plural(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("{n} {noun}")
    } else {
        format!("{n} {noun}s")
    }
}
