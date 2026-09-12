//! The M0 latency and memory harness.
//!
//! Plan §7's third mitigation asks for this in week one rather than at M5, and
//! the reason is in the risk it mitigates: a `JOIN` + `ORDER BY` + `LIMIT`
//! pipeline that degenerates under delete is *beautiful in a demo and collapses
//! in a real app in month eight*, at which point it is architectural rather than
//! fixable. The only defence is to measure the thing that collapses, at the size
//! it collapses at, before there is anything else to lose.
//!
//! So this crate seeds 100k issues and a million comments into real SQLite,
//! subscribes to plan §7's own example query, and reports what the engine
//! actually does: how many rows the first frame reads, how long a transaction
//! takes to reach the view at p99, how much state each operator retains, and
//! how often a window has to go back to the store.
//!
//! # What it does not measure
//!
//! Two of the M0 kill criteria are **not** in here, and pretending otherwise
//! would be the worst outcome this file could produce:
//!
//! * The budget is `p99 delta → **committed frame**`. This measures delta →
//!   pump returned. The FFI decode and the render are the other half, and they
//!   arrive with the demo apps.
//! * A desktop is not a mid-range Android phone. Plan §9 makes the physical
//!   device the go/no-go gate. A number here that is comfortably inside budget
//!   is necessary, not sufficient; a number here that is *outside* budget is
//!   already fatal, which is why it is worth running now.
//!
//! Everything reported is labelled with which of the two it is.
//!
//! # It checks itself
//!
//! A fast wrong answer is worth less than a slow right one, and an incremental
//! engine can be wrong quietly. Each phase ends by asking SQLite the same
//! question directly — plan §6's second oracle, at scale — and comparing the
//! maintained view against it, parents and children both. A run that fails
//! [`verify`] reports no timings at all.

pub mod fixture;
pub mod metrics;
pub mod probe;
pub mod rng;
pub mod workload;

use fixture::{comment, IndexMode, Pipeline, Query, Scale, COMMENTS, ISSUES};
use metrics::{Rss, Samples, Summary};
use probe::{Counts, Probe};
use rng::Rng;
use solstice_ivm::{Params, Predicate, RefillStats, Relation, RowKey, ScanRequest, Value};
use solstice_store::{SqliteStore, StoreError};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct Config {
    pub seed: u64,
    pub scale: Scale,
    pub query: Query,
    pub index: IndexMode,
    /// Transactions per phase.
    pub txns: usize,
    /// Percent of churn aimed at the subscribed project.
    pub bias: u64,
    /// `None` runs in memory, which is faster and less honest.
    pub db: Option<PathBuf>,
    pub verify: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            seed: 0x5015_71CE,
            scale: Scale::M0,
            query: Query::default(),
            index: IndexMode::Seek,
            txns: 2_000,
            bias: 25,
            db: None,
            verify: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Hydration {
    pub elapsed: Duration,
    pub view_rows: usize,
    pub issues: Counts,
    pub comments: Counts,
    pub state: Vec<(&'static str, usize)>,
}

#[derive(Debug, Clone)]
pub struct Phase {
    pub name: &'static str,
    pub txns: usize,
    pub mutations: usize,
    /// Pumps that changed the view. The rest are the cost of not having the
    /// dispatcher of plan §1.3 yet.
    pub changed: usize,
    pub latency: Summary,
    /// Kept apart by kind, because only one of them is a kill criterion. See
    /// [`solstice_ivm::RefillKind`].
    pub refills: RefillStats,
    pub operator_scans: Counts,
    pub state_bytes: usize,
    /// Sampled with the store still attached, which is the only reading that
    /// describes what the engine costs. See [`metrics::Rss`].
    pub rss: Option<Rss>,
}

impl Phase {
    /// Refills per second at the plan's 200 rows/second write rate.
    ///
    /// The harness runs flat out, so its own refills-per-wall-second is a
    /// number about this machine's clock speed rather than about the design.
    /// Refills per mutation is the property of the design; multiplying by the
    /// rate the plan actually budgets for is what makes it comparable to the
    /// budget.
    pub fn refills_per_second_at(&self, rows_per_second: f64) -> f64 {
        if self.mutations == 0 {
            return 0.0;
        }
        self.refills.window as f64 / self.mutations as f64 * rows_per_second
    }
}

#[derive(Debug, Clone)]
pub struct Report {
    pub seed_elapsed: Duration,
    pub db_bytes: Option<u64>,
    pub subscribed_issues: usize,
    pub hydrate_limit: usize,
    pub hydration: Hydration,
    pub phases: Vec<Phase>,
}

#[derive(Debug)]
pub enum BenchError {
    Store(StoreError),
    /// The maintained view disagreed with SQLite. No timing is reported,
    /// because the timing would be for the wrong answer.
    Diverged(String),
    /// The run would have measured nothing. See [`check`].
    Misconfigured(String),
}

impl std::fmt::Display for BenchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BenchError::Store(e) => write!(f, "{e}"),
            BenchError::Diverged(m) => write!(f, "view diverged from SQLite: {m}"),
            BenchError::Misconfigured(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for BenchError {}

impl From<StoreError> for BenchError {
    fn from(e: StoreError) -> Self {
        BenchError::Store(e)
    }
}

/// Refuse to benchmark something that cannot fail.
///
/// Every budget in plan §5.1 is trivially met by a view with no rows in it: an
/// empty window never refills, retains nothing, and pumps in nanoseconds. A
/// table of passes produced that way is worse than no table, because it would be
/// published. The two ways to get one by accident are asking for a project that
/// the scale never generated, and asking for a window wider than the data.
fn check(cfg: &Config, subscribed: usize) -> Result<(), BenchError> {
    if cfg.query.project < 0 || cfg.query.project >= cfg.scale.projects {
        return Err(BenchError::Misconfigured(format!(
            "project {} does not exist: this scale generates projects 0..{}",
            cfg.query.project, cfg.scale.projects
        )));
    }
    if subscribed < cfg.query.k {
        return Err(BenchError::Misconfigured(format!(
            "project {} has {subscribed} open issues and the window holds {}, so the \
             window is never full and never refills — nothing here would be measured",
            cfg.query.project, cfg.query.k
        )));
    }
    Ok(())
}

pub fn run(cfg: &Config) -> Result<Report, BenchError> {
    // Cheap half of the sanity check, before spending a minute seeding.
    check(cfg, usize::MAX)?;

    let mut store = match &cfg.db {
        Some(path) => SqliteStore::open(path, fixture::schemas())?,
        None => SqliteStore::in_memory(fixture::schemas())?,
    };
    fixture::create_indexes(&mut store, cfg.index)?;

    let t = Instant::now();
    fixture::seed(&mut store, &cfg.scale, &mut Rng::new(cfg.seed))?;
    let seed_elapsed = t.elapsed();
    let db_bytes = cfg.db.as_deref().and_then(db_size);

    let mut pipeline = fixture::build(&cfg.query);
    let mut view = Relation::new();
    let mut probe = Probe::new(&mut store);

    let t = Instant::now();
    let initial = pipeline.graph.hydrate(&mut probe);
    let hydrate_elapsed = t.elapsed();
    view.apply(&initial);

    let hydration = Hydration {
        elapsed: hydrate_elapsed,
        view_rows: view.len(),
        issues: probe.counts(ISSUES),
        comments: probe.counts(COMMENTS),
        state: pipeline.graph.state_report(),
    };
    if cfg.verify {
        verify(probe.store_mut(), &cfg.query, &view)?;
    }

    let mut churn = workload::Churn::new(
        probe.store_mut(),
        cfg.scale,
        &cfg.query,
        cfg.bias,
        cfg.seed ^ 0x00C0_FFEE,
    );
    let subscribed_issues = churn.subscribed_issues();
    check(cfg, subscribed_issues)?;

    let mut phases = Vec::new();
    phases.push(phase(
        "churn",
        cfg,
        &mut probe,
        &mut pipeline,
        &mut view,
        |store| churn.next(store),
    )?);

    let next_issue = cfg.scale.issues as i64 + cfg.txns as i64 + 1;
    let mut adversarial =
        workload::DeleteTop::new(cfg.scale, &cfg.query, next_issue, cfg.seed ^ 0xDEAD_BEEF);
    phases.push(phase(
        "delete-the-top",
        cfg,
        &mut probe,
        &mut pipeline,
        &mut view,
        |store| adversarial.next(store).unwrap_or_default(),
    )?);

    Ok(Report {
        seed_elapsed,
        db_bytes,
        subscribed_issues,
        hydrate_limit: pipeline.hydrate_limit,
        hydration,
        phases,
    })
}

/// Run `cfg.txns` transactions, timing the engine half of each.
///
/// The timed region is exactly `apply` then `pump`, in that order — the order
/// that makes [`solstice_ivm::OpCx::scan`]'s contract hold (plan §1.4). Choosing
/// *what* to write happens outside it; see the [`workload`] module docs.
fn phase(
    name: &'static str,
    cfg: &Config,
    probe: &mut Probe<'_>,
    pipeline: &mut Pipeline,
    view: &mut Relation,
    mut next: impl FnMut(&mut SqliteStore) -> workload::Txn,
) -> Result<Phase, BenchError> {
    probe.reset();
    probe.store_mut().reset_refill_stats();

    let mut latency = Samples::new();
    let mut mutations = 0;
    let mut changed = 0;

    for _ in 0..cfg.txns {
        let txn = next(probe.store_mut());
        if txn.is_empty() {
            continue;
        }
        mutations += txn.rows;

        let t = Instant::now();
        for (table, batch) in &txn.deltas {
            probe.store_mut().apply(*table, batch)?;
        }
        let out = pipeline.graph.pump(&txn.deltas, probe);
        latency.push(t.elapsed());

        if !out.is_empty() {
            changed += 1;
            view.apply(&out);
        }
    }

    let phase = Phase {
        name,
        txns: latency.len(),
        mutations,
        changed,
        latency: latency.summary(),
        refills: probe.store_mut().refill_stats(),
        operator_scans: probe.totals(),
        state_bytes: pipeline.graph.state_bytes(),
        rss: metrics::rss(),
    };

    if cfg.verify {
        verify(probe.store_mut(), &cfg.query, view)?;
    }
    Ok(phase)
}

/// Ask SQLite the same question and insist on the same answer.
///
/// Plan §6's second oracle, run at scale against a view that has been
/// maintained incrementally for thousands of transactions. The property tests
/// prove the operators obey the delta law on small random data; this proves the
/// same pipeline has not drifted after a long run on realistic data, which is a
/// different failure and the one a benchmark is uniquely placed to catch.
pub fn verify(store: &mut SqliteStore, q: &Query, view: &Relation) -> Result<(), BenchError> {
    use solstice_ivm::OpCx;

    let expected = store.scan(&ScanRequest {
        table: ISSUES,
        order: fixture::issue_order(),
        after: None,
        filter: Some(fixture::issue_filter()),
        params: Params::new(vec![Value::Int(q.project)]),
        limit: q.k,
    });

    if expected.len() != view.len() {
        return Err(BenchError::Diverged(format!(
            "view holds {} parents, SQLite says {}",
            view.len(),
            expected.len()
        )));
    }

    for (key, _) in &expected {
        let Some(row) = view.get(key) else {
            return Err(BenchError::Diverged(format!(
                "{key:?} is in the query and not in the view"
            )));
        };

        let want: Vec<Value> = store
            .scan(&ScanRequest {
                table: COMMENTS,
                order: fixture::comment_order(),
                after: None,
                filter: Some(Predicate::eq(comment::ISSUE, key.value().clone())),
                params: Params::empty(),
                limit: q.comments,
            })
            .into_iter()
            .map(|(k, _)| k.value().clone())
            .collect();

        // The join appends the child collection, so it is the last column.
        let got: Vec<Value> = match row.get(row.len() as solstice_ivm::ColId - 1) {
            Value::Rows(rows) => rows.iter().map(|r| r.get(comment::ID).clone()).collect(),
            other => {
                return Err(BenchError::Diverged(format!(
                    "{key:?} has {other:?} where its children should be"
                )))
            }
        };

        if want != got {
            return Err(BenchError::Diverged(format!(
                "{key:?} children: view {got:?}, SQLite {want:?}"
            )));
        }
    }

    Ok(())
}

/// Bytes on disk, including the write-ahead log — which is where a WAL database
/// keeps the rows you just wrote, so ignoring it understates the answer by
/// however much was recently busy.
fn db_size(path: &Path) -> Option<u64> {
    let main = std::fs::metadata(path).ok()?.len();
    let wal = std::fs::metadata(path.with_extension("db-wal"))
        .map(|m| m.len())
        .unwrap_or(0);
    Some(main + wal)
}

/// Issue ids currently in the view, sorted, for tests that want to look at it.
pub fn view_keys(view: &Relation) -> Vec<RowKey> {
    let mut keys: Vec<RowKey> = view.iter().map(|(k, _)| k.clone()).collect();
    keys.sort();
    keys
}
