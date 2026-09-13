//! Does a host that replays every diff end up with the view the engine has?
//!
//! Every other test in this crate checks one change at a time against a shape
//! written out by hand. This one checks the property those tests are evidence
//! for, over a workload long enough that an off-by-one has somewhere to hide:
//!
//! > A list built by applying every `ViewChange` in order equals the list a
//! > fresh subscription hydrates from the store.
//!
//! The two sides are computed by completely different routes — one is the
//! incremental path through `TopK` and `View::apply`, the other is `IR → SQL`
//! against SQLite — so agreement is not a tautology. Plan §6 calls the same
//! idea a triple oracle; this is the cheap version of it, and it exists because
//! the Flutter demo found rows in the wrong order on the device and there was
//! no test in the repo that could say which side was wrong.
//!
//! The workload is plan §5.1's: 1–3 operations per transaction, biased at the
//! rows the window is showing, because writes concentrated on the window are
//! what make `TopK` refill and refills are where plan §7 says this lives or
//! dies.

use std::sync::{Arc, Mutex};

use solstice_core::{Database, EngineEventSink, OpenConfig};
use solstice_ivm::{Row, Value};
use solstice_proto::mutation::{Mutation, Op};
use solstice_proto::{ViewChange, ViewDelta};

const PROJECT: i64 = 1;

/// splitmix64, so a failure is reproducible from the seed printed with it.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        ((self.next_u64() as u128 * n as u128) >> 64) as u64
    }

    fn range(&mut self, lo: i64, hi: i64) -> i64 {
        lo + self.below((hi - lo + 1) as u64) as i64
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

#[derive(Default)]
struct Collect {
    events: Mutex<Vec<Vec<u8>>>,
}

struct Sink(Arc<Collect>);

impl EngineEventSink for Sink {
    fn on_event(&self, event: Vec<u8>) {
        self.0.events.lock().unwrap().push(event);
    }
}

/// The host side of plan §4.2, and deliberately the strict version: an index
/// out of range panics rather than clamping. The demo app clamps because a
/// crashed demo teaches nobody anything; a test that clamped would be hiding
/// the one thing it is here to catch.
fn replay(list: &mut Vec<Row>, delta: &ViewDelta) {
    for change in &delta.changes {
        match change {
            ViewChange::Added { index, row } => list.insert(*index as usize, row.clone()),
            ViewChange::Removed { index, key } => {
                let row = list.remove(*index as usize);
                assert_eq!(
                    row.get(0),
                    key,
                    "Removed named a key that is not the key at its index"
                );
            }
            ViewChange::Changed { index, row, .. } => list[*index as usize] = row.clone(),
            ViewChange::Moved { from, to } => {
                let row = list.remove(*from as usize);
                list.insert(*to as usize, row);
            }
        }
    }
}

fn rows_of(delta: &ViewDelta) -> Vec<Row> {
    let mut list = Vec::new();
    replay(&mut list, delta);
    list
}

fn int(row: &Row, col: u16) -> i64 {
    match row.get(col) {
        Value::Int(i) => *i,
        other => panic!("column {col} is {other:?}"),
    }
}

/// `(key, sort key)` for every row, which is all the comparison needs: the
/// payload is checked by `view_test.dart` on the other side of the boundary.
///
/// `Option` rather than `i64`, because a tenth of the fixture's issues have no
/// priority at all (`solstice-bench`'s `issue_row`) and NULL ordering is the
/// most likely place for two routes to the same list to disagree.
fn summary(rows: &[Row]) -> Vec<(i64, Option<i64>)> {
    rows.iter()
        .map(|r| {
            (
                int(r, 0),
                match r.get(2) {
                    Value::Null => None,
                    Value::Int(i) => Some(*i),
                    other => panic!("priority is {other:?}"),
                },
            )
        })
        .collect()
}

fn issue(id: i64, project: i64, priority: Value, closed: i64, updated: i64) -> Op {
    Op::Insert {
        table: "issues".into(),
        row: Row::new(vec![
            Value::Int(id),
            Value::Int(project),
            priority,
            Value::Int(closed),
            Value::text(format!("issue {id}")),
            Value::Int(updated),
        ]),
    }
}

fn comment(id: i64, issue_id: i64, created: i64) -> Op {
    Op::Insert {
        table: "comments".into(),
        row: Row::new(vec![
            Value::Int(id),
            Value::Int(issue_id),
            Value::Int(created),
            Value::text("ana"),
            Value::text(format!("comment {id}")),
        ]),
    }
}

fn body(id: u64, ops: Vec<Op>) -> Vec<u8> {
    Mutation {
        client_id: b"test".to_vec(),
        mutation_id: id,
        ops,
        ..Mutation::default()
    }
    .encode()
}

const COMMENTS_PER_ISSUE: i64 = 4;

/// One shape of the same question.
///
/// The parameters exist because the first version of this test ran only the
/// first row below and passed, while the Flutter demo — which runs the second —
/// showed rows in the wrong order on the device. A test whose configuration is
/// not the configuration that fails is a test that proves the wrong thing.
struct Case {
    /// Enough issues that the window has far more candidates below it than it
    /// can show — the condition plan §7's hard case needs, and the one
    /// `demo-fixture` prints the open-issue count to keep checkable.
    issues: i64,
    k: u32,
    comments: u32,
    /// Inclusive top of the priority range. Small means many ties, which is
    /// the interesting case for a sort that breaks them by key; the demo's
    /// world uses 999.
    priorities: i64,
    /// Percent of issues with no priority at all. `solstice-bench` seeds 10%,
    /// so NULL ordering — the rule the store and the comparator have to agree
    /// on without ever comparing notes — is on the path the demo takes.
    nulls: u64,
}

const CASES: [Case; 3] = [
    Case {
        issues: 400,
        k: 20,
        comments: 3,
        priorities: 99,
        nulls: 0,
    },
    // The Flutter demo's own shape, scaled down to something that can run
    // thousands of transactions in a unit test.
    Case {
        issues: 2_000,
        k: 50,
        comments: 3,
        priorities: 999,
        nulls: 10,
    },
    // Ties everywhere and a window that is nearly all of the candidates, so
    // rows cross the boundary constantly.
    Case {
        issues: 200,
        k: 50,
        comments: 1,
        priorities: 5,
        nulls: 25,
    },
];

fn priority(rng: &mut Rng, case: &Case) -> Value {
    if rng.chance(case.nulls) {
        Value::Null
    } else {
        Value::Int(rng.range(0, case.priorities))
    }
}

fn seeded(rng: &mut Rng, case: &Case) -> Arc<Database> {
    let db = Database::open(OpenConfig::in_memory()).expect("an in-memory database opens");
    let mut ops = Vec::new();
    for id in 1..=case.issues {
        // Priorities collide on purpose. Ties are resolved by key, and a tie
        // that the two sides break differently is exactly the bug this is
        // looking for.
        let priority = priority(rng, case);
        let project = if rng.chance(70) { PROJECT } else { 2 };
        let closed = i64::from(rng.chance(20));
        ops.push(issue(id, project, priority, closed, id));
        for n in 0..COMMENTS_PER_ISSUE {
            ops.push(comment(id * 10 + n, id, n));
        }
    }
    db.mutate(body(1, ops)).expect("the seed applies");
    db
}

#[test]
fn replaying_every_diff_lands_on_the_view_a_fresh_subscription_hydrates() {
    for (c, case) in CASES.iter().enumerate() {
        for seed in [0x5015_71CE_u64, 1, 2, 3, 7, 99] {
            run(seed, c, case);
        }
    }
}

fn run(seed: u64, c: usize, case: &Case) {
    let at = format!(
        "case {c} (k={}, {} issues) seed {seed:#x}",
        case.k, case.issues
    );
    let mut rng = Rng(seed);
    let db = seeded(&mut rng, case);

    let events = Arc::new(Collect::default());
    db.set_event_sink(Box::new(Sink(Arc::clone(&events))))
        .expect("the engine is running");

    let sub = db
        .subscribe(solstice_core::m0::query(PROJECT, case.k, case.comments).encode())
        .expect("the M0 query compiles");
    let initial = ViewDelta::decode(&sub.initial()).expect("the engine's own bytes");
    let mut mirror = rows_of(&initial);

    let mut next_issue = case.issues + 1;
    let mut next_comment = case.issues * 100;
    let mut now = 1_000_000;
    let mut refused = 0;

    for step in 0..2_000u64 {
        now += 1;
        let n = rng.range(1, 3);
        let mut ops = Vec::new();
        for _ in 0..n {
            // The demo driver's mix, and for the same reasons: see
            // `examples/flutter-issues/lib/src/driver.dart`.
            let roll = rng.below(100);
            let target = if !mirror.is_empty() && rng.chance(80) {
                int(&mirror[rng.below(mirror.len() as u64) as usize], 0)
            } else {
                rng.range(1, case.issues)
            };
            ops.push(if roll < 55 {
                let mut set = vec![("updated_at".to_string(), Value::Int(now))];
                if rng.chance(30) {
                    set.push(("priority".to_string(), priority(&mut rng, case)));
                }
                Op::Update {
                    table: "issues".into(),
                    key: Value::Int(target),
                    set,
                }
            } else if roll < 80 {
                next_comment += 1;
                comment(next_comment, target, now)
            } else if roll < 92 {
                Op::Delete {
                    table: "comments".into(),
                    key: Value::Int(rng.range(10, case.issues * 10 + COMMENTS_PER_ISSUE)),
                }
            } else {
                next_issue += 1;
                let p = priority(&mut rng, case);
                issue(next_issue, PROJECT, p, 0, now)
            });
        }

        // A refusal is the engine doing its job (`engine.rs`: a write that
        // silently does nothing is the hardest kind of bug to see), and it
        // leaves the view untouched, so the mirror stays valid.
        if db.mutate(body(step + 2, ops)).is_err() {
            refused += 1;
            continue;
        }

        for bytes in std::mem::take(&mut *events.events.lock().unwrap()) {
            let delta = ViewDelta::decode(&bytes).expect("the engine's own bytes");
            assert_eq!(delta.sub_id, sub.sub_id());
            replay(&mut mirror, &delta);
        }
    }

    assert!(
        refused < 1_500,
        "{at}: {refused} of 2000 transactions were refused, \
         so this ran almost no workload"
    );

    // The other route to the same list: hydrate it from SQLite again.
    //
    // The child limit differs by one, and that is the whole point. Plan §1.3
    // interns operators by the hash of their IR subtree, so a second
    // subscription to the *identical* query would be handed the very pipeline
    // under test and its `initial()` would be that pipeline's own opinion of
    // itself — agreement guaranteed, and worth nothing. One character of
    // difference forces a separate `TopK`, hydrated by `IR → SQL` against the
    // store, which is an oracle that shares no code with the incremental path.
    let fresh = db
        .subscribe(solstice_core::m0::query(PROJECT, case.k, case.comments + 1).encode())
        .expect("the M0 query compiles");
    let expected = rows_of(&ViewDelta::decode(&fresh.initial()).expect("the engine's own bytes"));

    assert_eq!(
        summary(&mirror),
        summary(&expected),
        "{at}: replaying the diffs did not land on the hydrated view"
    );

    // Sorted, and sorted the way the query asked. A window that agrees with a
    // freshly hydrated window but is in the wrong order would mean both routes
    // share a mistake, which the comparison above cannot see.
    //
    // NULL is the low end of the order — the rule SQLite uses and the one the
    // demo's host applier assumes — so `None < Some(_)` in `Option`'s own
    // ordering is exactly right here, and is not a coincidence worth hiding.
    let order = summary(&mirror);
    let mut wanted = order.clone();
    wanted.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    assert_eq!(order, wanted, "{at}: the window is not in priority order");
}
