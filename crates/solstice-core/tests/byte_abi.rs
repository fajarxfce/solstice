//! The engine driven the way a host drives it: bytes in, bytes out.
//!
//! Every call here goes through the public API with an encoded payload, because
//! that is the thing plan §4.1 is betting on and the thing the two FFI adapters
//! will forward without adding anything. A test that reached for
//! `Engine::subscribe` directly would be testing a shape nobody ships.

use std::sync::{Arc, Mutex};

use solstice_core::{Database, EngineEventSink, OpenConfig, SolsticeError};
use solstice_ivm::{Row, Value};
use solstice_proto::mutation::{Mutation, Op};
use solstice_proto::{ViewChange, ViewDelta};

/// Collects events so a test can read them back in the order the engine sent
/// them. `mutate` returns only after the pump, so there is nothing to wait for.
#[derive(Default)]
struct Collect {
    events: Mutex<Vec<Vec<u8>>>,
}

impl Collect {
    fn take(&self) -> Vec<ViewDelta> {
        std::mem::take(&mut *self.events.lock().unwrap())
            .iter()
            .map(|b| ViewDelta::decode(b).expect("the engine's own bytes"))
            .collect()
    }
}

/// The sink handed to the engine, holding the same `Collect` the test reads.
struct Sink(Arc<Collect>);

impl EngineEventSink for Sink {
    fn on_event(&self, event: Vec<u8>) {
        self.0.events.lock().unwrap().push(event);
    }
}

fn collector(db: &Database) -> Arc<Collect> {
    let events = Arc::new(Collect::default());
    db.set_event_sink(Box::new(Sink(Arc::clone(&events))))
        .expect("the engine is running");
    events
}

fn issue(id: i64, project: i64, priority: Value, closed: i64, title: &str) -> Op {
    Op::Insert {
        table: "issues".into(),
        row: Row::new(vec![
            Value::Int(id),
            Value::Int(project),
            priority,
            Value::Int(closed),
            Value::text(title),
            Value::Int(id * 7),
        ]),
    }
}

fn comment(id: i64, issue_id: i64, created: i64, body: &str) -> Op {
    Op::Insert {
        table: "comments".into(),
        row: Row::new(vec![
            Value::Int(id),
            Value::Int(issue_id),
            Value::Int(created),
            Value::text("ana"),
            Value::text(body),
        ]),
    }
}

fn body(ops: Vec<Op>) -> Vec<u8> {
    Mutation {
        client_id: b"test".to_vec(),
        mutation_id: 1,
        ops,
        ..Mutation::default()
    }
    .encode()
}

/// Five issues in project 1, priorities 10..50, two comments each.
fn seeded() -> Arc<Database> {
    let db = Database::open(OpenConfig::in_memory()).expect("an in-memory database opens");
    let mut ops = Vec::new();
    for id in 1..=5 {
        ops.push(issue(id, 1, Value::Int(id * 10), 0, &format!("issue {id}")));
        for n in 0..2 {
            ops.push(comment(id * 10 + n, id, n, &format!("comment {id}.{n}")));
        }
    }
    // One issue in another project and one closed issue, so that a view which
    // ignored the filter would be visibly wrong rather than coincidentally
    // right.
    ops.push(issue(6, 2, Value::Int(999), 0, "other project"));
    ops.push(issue(7, 1, Value::Int(999), 1, "closed"));
    db.mutate(body(ops)).expect("the seed applies");
    db
}

fn subscribe(
    db: &Database,
    k: u32,
    comments: u32,
) -> (Arc<solstice_core::Subscription>, ViewDelta) {
    let sub = db
        .subscribe(solstice_core::m0::query(1, k, comments).encode())
        .expect("the M0 query compiles");
    let initial = ViewDelta::decode(&sub.initial()).expect("the engine's own bytes");
    (sub, initial)
}

fn id_of(row: &Row) -> i64 {
    match row.get(0) {
        Value::Int(i) => *i,
        other => panic!("issue id is {other:?}"),
    }
}

fn ids(rows: &[Row]) -> Vec<i64> {
    rows.iter().map(id_of).collect()
}

/// The host side of plan §4.2: apply a positional diff to a list.
fn replay(list: &mut Vec<Row>, delta: &ViewDelta) {
    for change in &delta.changes {
        match change {
            ViewChange::Added { index, row } => list.insert(*index as usize, row.clone()),
            ViewChange::Removed { index, .. } => {
                list.remove(*index as usize);
            }
            ViewChange::Changed { index, row, .. } => list[*index as usize] = row.clone(),
            ViewChange::Moved { from, to } => {
                let row = list.remove(*from as usize);
                list.insert(*to as usize, row);
            }
        }
    }
}

fn rows(delta: &ViewDelta) -> Vec<Row> {
    let mut list = Vec::new();
    replay(&mut list, delta);
    list
}

#[test]
fn the_initial_view_is_the_window_the_query_asked_for() {
    let db = seeded();
    let (_sub, initial) = subscribe(&db, 3, 2);

    // Top 3 by priority descending, the other project and the closed issue
    // excluded by the filter.
    assert_eq!(ids(&rows(&initial)), vec![5, 4, 3]);
    assert!(initial
        .changes
        .iter()
        .all(|c| matches!(c, ViewChange::Added { .. })));
}

#[test]
fn each_row_carries_its_children_as_a_nested_collection() {
    // Plan §1.1: a 1:N traversal widens the parent, it does not multiply it.
    // Three issues with two comments each is three rows, not six.
    let db = seeded();
    let (_sub, initial) = subscribe(&db, 3, 2);
    let view = rows(&initial);
    assert_eq!(view.len(), 3);

    for row in &view {
        let children = match row.values().last() {
            Some(Value::Rows(rows)) => rows.clone(),
            other => panic!("the join column is {other:?}"),
        };
        assert_eq!(children.len(), 2);
        // Latest first, and belonging to this parent.
        assert_eq!(children[0].get(2), &Value::Int(1));
        assert_eq!(children[0].get(1), &Value::Int(id_of(row)));
    }
}

#[test]
fn a_write_reaches_the_sink_as_a_positional_diff_at_the_version_it_returned() {
    let db = seeded();
    let events = collector(&db);
    let (sub, _initial) = subscribe(&db, 3, 2);

    let version = db
        .mutate(body(vec![Op::Update {
            table: "issues".into(),
            key: Value::Int(1),
            set: vec![("priority".into(), Value::Int(100))],
        }]))
        .expect("the update applies");

    let deltas = events.take();
    assert_eq!(deltas.len(), 1, "one subscription, one event");
    assert_eq!(deltas[0].sub_id, sub.sub_id());
    assert_eq!(
        deltas[0].version, version,
        "the version mutate returned is the join between the call and its events"
    );
}

#[test]
fn replaying_the_diffs_lands_where_a_fresh_hydration_does() {
    // The invariant the whole design rests on, at M0 scale: maintaining a view
    // incrementally and computing it from scratch must agree. Everything else
    // in this crate is an optimisation of the second into the first.
    let db = seeded();
    let events = collector(&db);

    let (_sub, initial) = subscribe(&db, 3, 2);
    let mut host = rows(&initial);

    let writes: Vec<Vec<Op>> = vec![
        // Enters the window from below, pushing the bottom row out.
        vec![Op::Update {
            table: "issues".into(),
            key: Value::Int(1),
            set: vec![("priority".into(), Value::Int(100))],
        }],
        // Leaves the window from the top: the case that forces `TopK` to refill
        // from the store, which is plan §7's named failure mode.
        vec![Op::Delete {
            table: "issues".into(),
            key: Value::Int(5),
        }],
        // A child appears under a parent that is on screen.
        vec![comment(999, 4, 50, "newest")],
        // Off the sort key entirely: a change, not a move.
        vec![Op::Update {
            table: "issues".into(),
            key: Value::Int(4),
            set: vec![("title".into(), Value::text("retitled"))],
        }],
        // A brand new row straight into the window.
        vec![issue(8, 1, Value::Int(500), 0, "newcomer")],
        // And one that the filter excludes, which must change nothing at all.
        vec![issue(9, 2, Value::Int(500), 0, "wrong project")],
    ];

    for ops in writes {
        db.mutate(body(ops)).expect("the write applies");
        for delta in events.take() {
            replay(&mut host, &delta);
        }
    }

    let (_fresh, hydrated) = subscribe(&db, 3, 2);
    assert_eq!(ids(&host), ids(&rows(&hydrated)));
    assert_eq!(host, rows(&hydrated), "rows, children included");
}

#[test]
fn a_write_to_a_table_no_view_reads_produces_no_event() {
    // Not an optimisation being asserted, a correctness property: a host that
    // received an empty delta would still rebuild a list for nothing.
    let db = seeded();
    let events = collector(&db);
    let (_sub, _initial) = subscribe(&db, 3, 2);

    db.mutate(body(vec![issue(10, 2, Value::Int(999), 0, "elsewhere")]))
        .expect("the write applies");
    assert!(events.take().is_empty());
}

#[test]
fn an_unsubscribed_query_stops_producing_events() {
    let db = seeded();
    let events = collector(&db);
    let (sub, _initial) = subscribe(&db, 3, 2);
    drop(sub);

    db.mutate(body(vec![issue(8, 1, Value::Int(500), 0, "newcomer")]))
        .expect("the write applies");
    assert!(events.take().is_empty());
    assert_eq!(db.stats().unwrap().subscriptions, 0);
}

#[test]
fn a_body_this_build_half_understands_is_refused_whole() {
    use solstice_proto::wire::Encoder;

    let db = seeded();
    let (_sub, before) = subscribe(&db, 3, 2);

    // A `Patch` holding one operation in a case this build does not implement.
    let mut e = Encoder::new();
    e.uint64_field(3, 7);
    e.message(5, |e| {
        e.message(1, |e| {
            // `increment`, reserved in mutation.proto and not built.
            e.message(5, |e| e.bytes_field(1, b"issues"));
        });
    });

    assert_eq!(
        db.mutate(e.finish()),
        Err(SolsticeError::UnsupportedOps { count: 1 })
    );
    let (_sub, after) = subscribe(&db, 3, 2);
    assert_eq!(rows(&before), rows(&after), "and nothing was written");
}

#[test]
fn a_query_the_engine_will_not_run_comes_back_as_an_error() {
    let db = seeded();
    let mut q = solstice_core::m0::query(1, 50, 3);
    q.order_by.clear();
    match db.subscribe(q.encode()) {
        Err(SolsticeError::Compile(msg)) => assert!(msg.contains("order_by"), "{msg}"),
        other => panic!("{other:?} should have been a compile error"),
    }
}

#[test]
fn a_frame_that_is_not_a_query_is_an_error_and_not_a_panic() {
    let db = seeded();
    assert!(matches!(
        db.subscribe(vec![0xff, 0xff, 0xff, 0xff]),
        Err(SolsticeError::Decode(_))
    ));
}

#[test]
fn an_update_that_would_move_a_row_to_a_new_identity_is_refused() {
    let db = seeded();
    let err = db
        .mutate(body(vec![Op::Update {
            table: "issues".into(),
            key: Value::Int(1),
            set: vec![("id".into(), Value::Int(42))],
        }]))
        .expect_err("the primary key is the row's identity downstream");
    assert!(matches!(err, SolsticeError::Mutation(_)), "{err:?}");
}

#[test]
fn the_database_shuts_down_with_a_subscription_still_alive() {
    // The `Subscription` holds a sender too, so a database that waited for
    // every sender to drop would hang here — and a Flutter widget that had not
    // been disposed yet is exactly how that would happen in the wild.
    let db = seeded();
    let (sub, _initial) = subscribe(&db, 3, 2);
    drop(db);
    // Unsubscribing into a dead engine is a no-op, not a panic.
    drop(sub);
}

#[test]
fn deleting_the_top_row_over_and_over_costs_one_refill_and_not_one_each() {
    // Plan §5.1's adversarial workload, and plan §7's named failure mode: a
    // `TopK` that refilled on every delete is a `TopK` that has quietly become
    // a requery loop. `slack` is what buys the amortisation, and `stats` is the
    // only way anyone would ever notice it stopped working.
    let db = seeded();
    let mut ops = Vec::new();
    for n in 0..40 {
        ops.push(issue(100 + n, 1, Value::Int(1000 + n), 0, "adversary"));
    }
    db.mutate(body(ops)).expect("the extra issues apply");

    // Subscribing now, with far more matching rows than the window hydrates,
    // is what leaves the window knowing rows exist below it.
    let (_sub, initial) = subscribe(&db, 3, 2);
    assert_eq!(ids(&rows(&initial)), vec![139, 138, 137]);

    // Priority ascends with id, so descending ids *are* the top of the window.
    // Eighteen of them: one past the slack, which is where the first refill has
    // to be and the only place it is allowed to be.
    for id in (122..=139).rev() {
        db.mutate(body(vec![Op::Delete {
            table: "issues".into(),
            key: Value::Int(id),
        }]))
        .expect("the delete applies");
    }

    let stats = db.stats().unwrap();
    assert_eq!(stats.view_rows, 3, "the window refilled itself");
    assert_eq!(stats.mutations, 20, "two seeds and eighteen deletes");
    assert!(stats.graph_bytes > 0, "operators are holding state");
    assert!(
        (1..=2).contains(&stats.window_refills),
        "eighteen deletes off the top should cost one refill, not eighteen; got {}",
        stats.window_refills
    );
    assert!(
        stats.window_refill_rows >= stats.window_refills,
        "a refill that returned nothing would be a refill that did not help"
    );
}

#[test]
fn a_query_that_asks_to_be_unbounded_gets_every_row_it_matches() {
    let db = seeded();
    let mut q = solstice_core::m0::query(1, 0, 2);
    q.allow_unbounded = true;
    let sub = db.subscribe(q.encode()).expect("it said allow_unbounded");
    let view = rows(&ViewDelta::decode(&sub.initial()).unwrap());
    assert_eq!(ids(&view), vec![5, 4, 3, 2, 1]);
}
