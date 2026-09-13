//! Data for the M0 world, and the pipeline the harness measures.
//!
//! The world itself — the two tables, the column ids, the query from plan §7 —
//! lives in [`solstice_core::m0`] and is re-exported here. It used to be
//! written out twice, and two hand-written copies of a schema agree right up
//! until the morning someone adds a column. What is left in this module is what
//! only a benchmark needs: how much data to generate, what it looks like, and
//! which indexes the store gets.
//!
//! # The pipeline is the engine's, not the harness's
//!
//! [`build`] compiles plan §7's query through [`solstice_core::compile`], the
//! same function [`Database::subscribe`](solstice_core::Database::subscribe)
//! calls. That matters more than the deduplication: a benchmark that wired its
//! own graph would be measuring a pipeline no user can subscribe to, and it
//! would keep reporting green after the compiler started building a different
//! one.
//!
//! What the harness still needs, and the engine does not expose, is the
//! *pieces* — the filter and params to run a scan under, the sort order to
//! check a window against, the hydration bound to assert against. Those come
//! out of [`solstice_core::Compiled`] and are repackaged as [`Pipeline`].

use crate::rng::Rng;
use solstice_core::{compile, m0};
use solstice_ivm::{Batch, Change, ColId, Dir, Graph, Params, Predicate, Row, RowKey, Value};
use solstice_store::{SqliteStore, StoreError};

pub use solstice_core::m0::{
    comment, comment_order, issue, issue_filter, issue_order, schemas, COMMENTS, ISSUES,
};

/// How much data to generate.
#[derive(Debug, Clone, Copy)]
pub struct Scale {
    pub issues: usize,
    pub comments_per_issue: usize,
    pub projects: i64,
}

impl Scale {
    /// Plan §5.1's numbers: 100k issues, 1M comments.
    pub const M0: Scale = Scale {
        issues: 100_000,
        comments_per_issue: 10,
        projects: 50,
    };

    /// Small enough for `cargo test`, large enough that a `TopK` still has to
    /// refill and a join still has to hold more than one parent.
    pub const TINY: Scale = Scale {
        issues: 400,
        comments_per_issue: 5,
        projects: 4,
    };

    pub fn comments(&self) -> usize {
        self.issues * self.comments_per_issue
    }

    pub fn rows(&self) -> usize {
        self.issues + self.comments()
    }
}

/// The subscription being measured.
#[derive(Debug, Clone, Copy)]
pub struct Query {
    pub project: i64,
    /// Issues on screen.
    pub k: usize,
    /// Comments shown per issue. Mandatory on a 1:N traversal (plan §1.1).
    pub comments: usize,
}

impl Default for Query {
    fn default() -> Self {
        Query {
            project: 7,
            k: 50,
            comments: 3,
        }
    }
}

/// A wired graph plus the pieces of it a harness has to ask questions about.
pub struct Pipeline {
    pub graph: Graph,
    pub filter: Predicate,
    pub params: Params,
    pub order: Vec<(ColId, Dir)>,
    pub child_order: Vec<(ColId, Dir)>,
    /// Rows the source reads at hydration. The headline number: this is what
    /// stands between the first frame and the whole table.
    pub hydrate_limit: usize,
}

/// Compile plan §7's query the way a subscription does.
///
/// Panics rather than returning an error: every `Query` this harness can build
/// is one the compiler accepts, and a benchmark that reported a compile failure
/// as a slow result would be worse than one that stopped.
pub fn build(q: &Query) -> Pipeline {
    let ir = m0::query(q.project, q.k as u32, q.comments as u32);
    let c = compile(&m0::catalog(), &ir).expect("the M0 query is what `compile` is written for");
    Pipeline {
        child_order: c.children[0].order.clone(),
        hydrate_limit: c.hydrate_limit.expect("a window query has a limit"),
        graph: c.graph,
        filter: c.filter,
        params: c.params,
        order: c.order,
    }
}

/// Which indexes the store gets — the variable this harness exists to measure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexMode {
    /// Nothing but the primary key. The control: what "we'll add indexes later"
    /// actually costs.
    None,
    /// Indexes on the sort columns only, which is all `create_order_index` can
    /// express. Correct, and bounded only when the filter is not selective.
    Order,
    /// Equality columns leading the sort columns, so a per-parent refill is a
    /// seek. See [`solstice_store::create_seek_index`].
    Seek,
}

impl IndexMode {
    pub fn parse(s: &str) -> Option<IndexMode> {
        match s {
            "none" => Some(IndexMode::None),
            "order" => Some(IndexMode::Order),
            "seek" => Some(IndexMode::Seek),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            IndexMode::None => "none",
            IndexMode::Order => "order",
            IndexMode::Seek => "seek",
        }
    }
}

pub fn create_indexes(store: &mut SqliteStore, mode: IndexMode) -> Result<(), StoreError> {
    match mode {
        IndexMode::None => {}
        IndexMode::Order => {
            store.index_order(ISSUES, &issue_order())?;
            store.index_order(COMMENTS, &comment_order())?;
        }
        // The indexes a real `Database::open` creates, so the measured plan is
        // the shipped one.
        IndexMode::Seek => m0::index(store)?,
    }
    Ok(())
}

const WORDS: [&str; 16] = [
    "crash", "layout", "timeout", "retry", "cache", "scroll", "sync", "token", "upload", "parser",
    "theme", "locale", "export", "search", "badge", "toast",
];

const AUTHORS: [&str; 8] = [
    "ana", "bruno", "chen", "dewi", "elif", "farid", "gita", "hugo",
];

fn word(rng: &mut Rng) -> &'static str {
    WORDS[rng.below(WORDS.len() as u64) as usize]
}

pub fn issue_row(id: i64, scale: &Scale, rng: &mut Rng) -> Row {
    Row::new(vec![
        Value::Int(id),
        Value::Int(rng.range(0, scale.projects - 1)),
        // A tenth of issues have no priority set, so the NULL ordering rules
        // the store goes to such trouble over are actually exercised.
        if rng.chance(10) {
            Value::Null
        } else {
            Value::Int(rng.range(0, 999))
        },
        Value::Int(i64::from(rng.chance(30))),
        Value::text(format!("{} {} in {}", word(rng), word(rng), word(rng))),
        Value::Int(rng.range(0, 2_000_000)),
    ])
}

pub fn comment_row(id: i64, issue_id: i64, rng: &mut Rng) -> Row {
    Row::new(vec![
        Value::Int(id),
        Value::Int(issue_id),
        Value::Int(rng.range(0, 2_000_000)),
        Value::text(AUTHORS[rng.below(AUTHORS.len() as u64) as usize]),
        Value::text(format!(
            "{} {} — {} {} {}",
            word(rng),
            word(rng),
            word(rng),
            word(rng),
            word(rng)
        )),
    ])
}

/// Rows are written in chunks rather than one batch, because a `Batch` of a
/// million `Change`s is a few hundred megabytes of the thing we are trying to
/// prove we do not need.
const SEED_CHUNK: usize = 8192;

/// Fill a store with `scale`, deterministically from `rng`.
pub fn seed(store: &mut SqliteStore, scale: &Scale, rng: &mut Rng) -> Result<(), StoreError> {
    let mut batch = Batch::new();
    for id in 0..scale.issues as i64 {
        batch.push(Change::Insert {
            key: RowKey::from(id),
            row: issue_row(id, scale, rng),
        });
        if batch.len() >= SEED_CHUNK {
            store.apply(ISSUES, &std::mem::take(&mut batch))?;
        }
    }
    store.apply(ISSUES, &batch)?;

    let mut batch = Batch::new();
    for id in 0..scale.comments() as i64 {
        let issue_id = id / scale.comments_per_issue as i64;
        batch.push(Change::Insert {
            key: RowKey::from(id),
            row: comment_row(id, issue_id, rng),
        });
        if batch.len() >= SEED_CHUNK {
            store.apply(COMMENTS, &std::mem::take(&mut batch))?;
        }
    }
    store.apply(COMMENTS, &batch)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::Probe;
    use solstice_ivm::OpCx;

    fn seeded(scale: Scale, mode: IndexMode) -> SqliteStore {
        let mut store = SqliteStore::in_memory(schemas()).unwrap();
        create_indexes(&mut store, mode).unwrap();
        seed(&mut store, &scale, &mut Rng::new(1)).unwrap();
        store
    }

    #[test]
    fn the_pipeline_is_the_four_operators_m0_ships() {
        let p = build(&Query::default());
        assert_eq!(
            p.graph.node_names(),
            vec!["Source", "Filter", "TopK", "Source", "Join1N"]
        );
    }

    #[test]
    fn hydration_reads_a_window_and_not_the_table() {
        // The claim the whole first frame rests on. 400 issues in the store,
        // fewer than a hundred rows read.
        let mut store = seeded(Scale::TINY, IndexMode::Seek);
        let q = Query {
            project: 1,
            k: 20,
            comments: 3,
        };
        let mut p = build(&q);

        let mut probe = Probe::new(&mut store);
        let out = p.graph.hydrate(&mut probe);
        let (_, issue_rows) = probe.table_totals(ISSUES);

        assert!(out.len() <= q.k, "the view holds at most k parents");
        assert!(
            issue_rows <= p.hydrate_limit,
            "hydration read {issue_rows} issue rows, bounded at {}",
            p.hydrate_limit
        );
        assert!(
            issue_rows < Scale::TINY.issues,
            "and that is strictly less than the table"
        );
    }

    #[test]
    fn a_child_window_is_a_seek_only_with_the_seek_index() {
        // `WHERE issue_id = ? ORDER BY created_at DESC LIMIT 4`, asked of each
        // plan. The point of `IndexMode::Seek` is this line of query plan, and
        // a timing alone would not distinguish a seek from a fast small scan.
        let req = solstice_ivm::ScanRequest {
            table: COMMENTS,
            order: comment_order(),
            after: None,
            filter: Some(Predicate::eq(comment::ISSUE, 3i64)),
            params: Params::empty(),
            limit: 4,
        };

        let seek = seeded(Scale::TINY, IndexMode::Seek).explain(&req).unwrap();
        assert!(
            seek.iter().any(|p| p.contains("idx_comments_issue_id_e_")),
            "expected a seek on the composite index, got {seek:?}"
        );

        let order = seeded(Scale::TINY, IndexMode::Order).explain(&req).unwrap();
        assert!(
            !order.iter().any(|p| p.contains("issue_id_e_")),
            "the order-only index cannot answer this without a walk: {order:?}"
        );
    }

    #[test]
    fn seeding_is_reproducible_from_the_seed() {
        let a = seeded(Scale::TINY, IndexMode::None).dump(ISSUES).unwrap();
        let b = seeded(Scale::TINY, IndexMode::None).dump(ISSUES).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn the_generated_data_exercises_null_priorities() {
        let store = seeded(Scale::TINY, IndexMode::None);
        let nulls = store
            .dump(ISSUES)
            .unwrap()
            .iter()
            .filter(|(_, r)| r.get(issue::PRIORITY).is_null())
            .count();
        assert!(
            nulls > 0,
            "a fixture with no NULLs would never test the NULL ordering rules"
        );
    }

    #[test]
    fn a_scan_of_the_seeded_store_answers_under_the_filter() {
        let mut store = seeded(Scale::TINY, IndexMode::Seek);
        let rows = store.scan(&solstice_ivm::ScanRequest {
            table: ISSUES,
            order: issue_order(),
            after: None,
            filter: Some(issue_filter()),
            params: Params::new(vec![Value::Int(1)]),
            limit: 10,
        });
        assert!(!rows.is_empty());
        for (_, r) in &rows {
            assert_eq!(r.get(issue::PROJECT), &Value::Int(1));
            assert_eq!(r.get(issue::CLOSED), &Value::Int(0));
        }
    }
}
