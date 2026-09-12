//! The hardcoded M0 world: issues, comments, and the one query that matters.
//!
//! Plan §5.1 fakes everything in the spike except the risk, so there is no
//! schema DSL and no codegen here — two tables written out by hand, and the
//! query from plan §7 that names the project's most likely way to fail:
//!
//! > the top 50 issues by priority in my project, each with its 3 latest
//! > comments
//!
//! Every operator in that sentence is one of the four M0 ships, arranged in the
//! order that makes the memory bound hold: `Source → Filter → TopK → Join(1:N)`,
//! with the window *below* the join so the join only ever holds children for
//! parents that are on screen (see [`solstice_ivm::ops::join`]).
//!
//! # Hydration is bounded, and that is a planner decision
//!
//! A `Source` with no limit reads the whole table. Feeding 100k issues through
//! it to keep 50 is not just slow, it is the memory blow-up plan §7 warns about
//! arriving during the first frame. Because the source can scan in the window's
//! own order, it only has to read `k + slack + 1` rows — and the `+ 1` is load
//! bearing for the same reason it is in `Join1N::build_state`: a `TopK` learns
//! that rows exist below its window only by discarding one, so a scan of
//! exactly `k + slack` would leave it believing it holds the whole relation and
//! it would never refill.

use crate::rng::Rng;
use solstice_ivm::ops::{Filter, Join1N, Source, TopK};
use solstice_ivm::{
    Batch, Change, ColId, Column, Dir, Expr, Graph, GraphBuilder, Params, Predicate, Row, RowKey,
    Schema, TableId, Value, ValueType,
};
use solstice_store::{SqliteStore, StoreError};

pub const ISSUES: TableId = 1;
pub const COMMENTS: TableId = 2;

/// Column ids for `issues`. Named because `row.get(2)` at a call site is a bug
/// waiting for the day someone adds a column in the middle.
pub mod issue {
    use solstice_ivm::ColId;
    pub const ID: ColId = 0;
    pub const PROJECT: ColId = 1;
    pub const PRIORITY: ColId = 2;
    pub const CLOSED: ColId = 3;
    pub const TITLE: ColId = 4;
    pub const UPDATED_AT: ColId = 5;
}

/// Column ids for `comments`.
pub mod comment {
    use solstice_ivm::ColId;
    pub const ID: ColId = 0;
    pub const ISSUE: ColId = 1;
    pub const CREATED_AT: ColId = 2;
    pub const AUTHOR: ColId = 3;
    pub const BODY: ColId = 4;
}

pub fn schemas() -> Vec<Schema> {
    vec![
        Schema::new(
            ISSUES,
            "issues",
            vec![
                Column::new("id", ValueType::Int),
                Column::new("project_id", ValueType::Int),
                Column::new("priority", ValueType::Int).nullable(),
                Column::new("closed", ValueType::Int),
                Column::new("title", ValueType::Text),
                Column::new("updated_at", ValueType::Int),
            ],
            issue::ID,
        ),
        Schema::new(
            COMMENTS,
            "comments",
            vec![
                Column::new("id", ValueType::Int),
                Column::new("issue_id", ValueType::Int),
                Column::new("created_at", ValueType::Int),
                Column::new("author", ValueType::Text),
                Column::new("body", ValueType::Text),
            ],
            comment::ID,
        ),
    ]
}

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

/// `project_id = $0 AND closed = 0`.
///
/// The project is a *parameter* rather than a literal because plan §1.2 shares
/// one pipeline across every user running the same query with different
/// bindings. Writing it as a literal here would measure a query shape the real
/// engine never runs.
pub fn issue_filter() -> Predicate {
    Predicate::and([
        Predicate::Cmp {
            lhs: Expr::Col(issue::PROJECT),
            op: solstice_ivm::CmpOp::Eq,
            rhs: Expr::Param(0),
        },
        Predicate::eq(issue::CLOSED, 0i64),
    ])
}

pub fn issue_order() -> Vec<(ColId, Dir)> {
    vec![(issue::PRIORITY, Dir::Desc)]
}

pub fn comment_order() -> Vec<(ColId, Dir)> {
    vec![(comment::CREATED_AT, Dir::Desc)]
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

pub fn build(q: &Query) -> Pipeline {
    let filter = issue_filter();
    let params = Params::new(vec![Value::Int(q.project)]);
    let order = issue_order();
    let child_order = comment_order();

    let topk = TopK::new(ISSUES, order.clone(), q.k).with_pushdown(filter.clone(), params.clone());
    let hydrate_limit = q.k + topk.slack() + 1;

    let source = Source::new(ISSUES, issue::ID)
        .with_pushdown(filter.clone(), params.clone())
        .with_order(order.clone())
        .with_limit(hydrate_limit);

    let mut b = GraphBuilder::new();
    let n_issues = b.source(ISSUES, Box::new(source));
    let n_filter = b.add(
        Box::new(Filter::new(filter.clone(), params.clone())),
        vec![n_issues],
    );
    let n_topk = b.add(Box::new(topk), vec![n_filter]);
    // Deltas only: hydrating a million comments so the join can keep three per
    // issue is the mistake this operator exists to avoid.
    let n_comments = b.source(
        COMMENTS,
        Box::new(Source::new(COMMENTS, comment::ID).deltas_only()),
    );
    let n_join = b.add(
        Box::new(Join1N::new(
            COMMENTS,
            issue::ID,
            comment::ISSUE,
            child_order.clone(),
            q.comments,
        )),
        vec![n_topk, n_comments],
    );

    Pipeline {
        graph: b.build(n_join),
        filter,
        params,
        order,
        child_order,
        hydrate_limit,
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
        IndexMode::Seek => {
            // The source and the window both scan
            // `WHERE project_id = ? AND closed = 0 ORDER BY priority DESC`.
            store.index_seek(
                ISSUES,
                &[issue::PROJECT, issue::CLOSED],
                &[(issue::PRIORITY, Dir::Desc)],
            )?;
            // Every per-parent child window scans
            // `WHERE issue_id = ? ORDER BY created_at DESC`.
            store.index_seek(
                COMMENTS,
                &[comment::ISSUE],
                &[(comment::CREATED_AT, Dir::Desc)],
            )?;
        }
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
