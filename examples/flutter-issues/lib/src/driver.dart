// Plan §5.1's "thread latar memutasi ~200 row/detik", in Dart.
//
// # Where the work happens
//
// Not on this isolate. `Database.mutate_async` exists for this driver: it is
// the same engine call as `mutate` without `#[frb(sync)]`, so the write is
// dispatched to a worker and the engine applies it on its own thread while the
// isolate that paints frames stays free. Spike S3 is why that matters — a write
// that lands on SQLite's WAL checkpoint takes 21ms on this phone, and taking
// that out of the UI thread would drop a frame and a half for a reason having
// nothing to do with the view.
//
// What the isolate does pay is the timer and the encode: building a 40-byte
// `Patch` with `package:protobuf` a hundred times a second. The HUD reports it,
// because a background driver that quietly costs 3% of the UI thread would
// flatter every other number on the screen.
//
// # How it differs from `solstice-bench`'s `Churn`, and why
//
// The mix is the same — 1–3 operations per transaction, 55% touch an issue
// (30% of those also move it in the sort order), 25% add a comment, 12% drop
// one, 8% add an issue. The *targets* are not, and cannot be.
//
// `Churn` scans the store at startup and picks from every issue in the project,
// open or closed, in or out of the window. An app has no such list: plan §1.4
// gives it one way in, `mutate`, and no way to enumerate. So this driver aims
// at the rows it can see — the ones in the view — and otherwise at uniformly
// random ids across the whole table.
//
// That is a *harder* workload for the window, not an easier one, and the
// direction is deliberate. Writes concentrated on the 50 rows on screen are
// exactly the traffic that pushes rows across the window boundary and makes
// `TopK` refill, which plan §7 names as the place this project lives or dies.
// A driver that spread its writes over 2,014 issues would mostly be measuring
// the dispatch index deciding the view does not care.

import 'dart:async';

import '../gen/solstice/v1/mutation.pb.dart' show Op;
import 'rust/api.dart';
import 'schema.dart';
import 'view.dart';
import 'world.dart';

/// splitmix64, the generator `solstice_bench::rng` uses.
///
/// Dart's `int` is 64-bit two's complement on the platforms this runs on and
/// its arithmetic wraps, so the constants and the shifts transfer directly —
/// `>>>` where Rust has `>>` on a `u64`, because Dart's `>>` is arithmetic.
class Rng {
  int _s;

  Rng(this._s);

  int nextU64() {
    _s += 0x9E3779B97F4A7C15;
    var z = _s;
    z = (z ^ (z >>> 30)) * -0x40A7B892E31B1A47; // 0xBF58476D1CE4E5B9
    z = (z ^ (z >>> 27)) * -0x6B2FB644ECCEEE15; // 0x94D049BB133111EB
    return z ^ (z >>> 31);
  }

  /// A number in `0..n`.
  ///
  /// Modulo over 63 bits rather than `Rng::below`'s multiply-shift, because
  /// Lemire's method needs a 128-bit product and Dart has no 128-bit integer.
  /// The bias that buys back is under one part in 2^54 at the sizes used here
  /// (n ≤ 10^6) — invisible next to the fact that this driver picks different
  /// targets than the Rust one in the first place.
  int below(int n) => (nextU64() >>> 1) % n;

  int range(int lo, int hi) => lo + below(hi - lo + 1);

  bool chance(int percent) => below(100) < percent;
}

/// What the driver has done, for the HUD.
class DriverStats {
  int transactions = 0;
  int operations = 0;

  /// Writes the engine refused.
  ///
  /// Expected and not zero: plan §1.4's engine rejects an update or delete of a
  /// row that is not there (`engine.rs`: "a write that silently does nothing is
  /// the hardest kind of bug to see"), and this driver deletes comments it may
  /// have already deleted in a transaction whose delta has not come back yet.
  /// The count is shown rather than hidden so the rate stays interpretable.
  int refused = 0;
  String? lastError;

  /// Microseconds this isolate spent encoding, cumulative.
  int encodeUs = 0;

  /// Microseconds spent inside `await mutateAsync`, cumulative. Includes the
  /// engine's own work, so it is an upper bound on the boundary, not the
  /// boundary.
  int awaitUs = 0;

  /// How far behind the requested rate the driver is, in operations.
  double behind = 0;
}

/// Issues transactions at a fixed row rate for as long as it is running.
class Driver {
  final Database db;
  final IssueView view;

  /// Row mutations per second. Plan §5.1 says 200.
  int rowsPerSecond;

  /// Percent of writes aimed at rows the view is showing. The harness calls
  /// the same knob `--bias`.
  int bias;

  /// The project the subscription is for, so an inserted issue can be aimed
  /// into it.
  final int project;

  final Rng _rng;
  final DriverStats stats = DriverStats();

  /// The logical clock the fixture's generator left off at, so `updated_at`
  /// values the driver writes sort after the ones it seeded.
  int _now = 2000001;

  /// Ids for rows this driver creates.
  ///
  /// Started past anything `demo-fixture` can have produced (its `Scale::M0` is
  /// 100k issues and a million comments) so that an insert can never collide
  /// with a seeded row, which the engine would treat as an update.
  int _nextIssueId = 1 << 31;
  int _nextCommentId = 1 << 31;

  bool _running = false;
  Future<void>? _loop;

  Driver({
    required this.db,
    required this.view,
    this.project = defaultProject,
    this.rowsPerSecond = 200,
    this.bias = 80,
    int seed = 0x501571CE,
  }) : _rng = Rng(seed);

  bool get running => _running;

  void start() {
    if (_running) return;
    _running = true;
    _loop = _run();
  }

  Future<void> stop() async {
    _running = false;
    await _loop;
    _loop = null;
  }

  /// Self-pacing: the deadline for operation *n* is `n / rowsPerSecond`
  /// seconds after the start, so a transaction that took too long is made up
  /// for by the next ones rather than permanently shifting the rate down.
  Future<void> _run() async {
    final clock = Stopwatch()..start();
    final startedAt = stats.operations;
    while (_running) {
      final due = ((stats.operations - startedAt) / rowsPerSecond * 1e6).round();
      final waitUs = due - clock.elapsedMicroseconds;
      if (waitUs > 0) {
        await Future<void>.delayed(Duration(microseconds: waitUs));
        continue;
      }
      stats.behind = -waitUs / 1e6 * rowsPerSecond;
      await _transaction();
    }
  }

  Future<void> _transaction() async {
    _now++;
    final encode = Stopwatch()..start();
    final n = _rng.range(1, 3);
    final ops = <Op>[for (var i = 0; i < n; i++) _nextOp()];
    final body = buildMutation(mutationId: _now, ops: ops);
    encode.stop();
    stats.encodeUs += encode.elapsedMicroseconds;

    final call = Stopwatch()..start();
    try {
      // Awaited, so at most one transaction is in flight. The engine is a
      // single command loop (plan §4.3) and would serialise them anyway; what
      // awaiting adds is that a slow engine slows the driver down instead of
      // growing a queue nobody can see.
      await db.mutateAsync(body: body);
      stats.transactions++;
      stats.operations += ops.length;
    } on SolsticeError catch (e) {
      stats.refused++;
      stats.lastError = e.toString();
      // Counted against the rate even though it did not land: the driver asked
      // for the work, and pretending it did not would make it speed up to
      // compensate for writes it is choosing to attempt.
      stats.operations += ops.length;
    } finally {
      call.stop();
      stats.awaitUs += call.elapsedMicroseconds;
    }
  }

  Op _nextOp() {
    final roll = _rng.below(100);
    if (roll < 55) return _touchIssue();
    if (roll < 80) return _addComment();
    if (roll < 92) return _dropComment();
    return _addIssue();
  }

  /// The common write: someone edits an issue. Thirty percent of the time it
  /// also changes the priority, which is the sort column — so it may move the
  /// row, push it out of the window, or pull a replacement in from below. That
  /// is the expensive case, and the one plan §7 is about.
  Op _touchIssue() {
    final set = <String, int>{Issue.colUpdatedAt: _now};
    if (_rng.chance(30)) set[Issue.colPriority] = _rng.range(0, 999);
    return updateIssue(_pickIssue(), set);
  }

  Op _addComment() {
    final id = _nextCommentId++;
    return insertComment(
      id: id,
      issueId: _pickIssue(),
      createdAt: _now,
      author: authors[_rng.below(authors.length)],
      body: '${_word()} ${_word()} — ${_word()} ${_word()} ${_word()}',
    );
  }

  /// Deletes a comment the view is showing, most of the time.
  ///
  /// A child leaving is the delta a 1:N join has to turn into a `Changed` on
  /// the *parent* — the reason `view.proto` puts children in a column of the
  /// parent row — and it forces the child window to go back to the store for a
  /// replacement. Aiming at random ids across a million rows would almost never
  /// produce one.
  Op _dropComment() {
    if (_rng.chance(bias)) {
      final id = _pickComment();
      if (id != null) return deleteComment(id);
    }
    return deleteComment(_rng.below(seededComments));
  }

  /// A new issue, which lands in the subscribed project `bias` percent of the
  /// time — and then competes for a place in the window on priority alone.
  Op _addIssue() {
    final id = _nextIssueId++;
    final mine = _rng.chance(bias);
    return insertIssue(
      id: id,
      projectId: mine ? project : _rng.below(seededProjects),
      priority: _rng.range(0, 999),
      closed: mine ? false : _rng.chance(30),
      title: '${_word()} ${_word()} in ${_word()}',
      updatedAt: _now,
    );
  }

  /// An issue id: one the view is showing, or any id in the table.
  ///
  /// The uniform branch is not filler. Most of those rows are in other
  /// projects, so the write reaches the engine, is applied to the store, and is
  /// dropped by the dispatch index before it reaches this view's pipeline
  /// (plan §1.3). That path has a cost, and a driver that only wrote rows the
  /// view cares about would never measure it.
  int _pickIssue() {
    if (view.length > 0 && _rng.chance(bias)) {
      return view[_rng.below(view.length)].id;
    }
    return _rng.below(seededIssues);
  }

  int? _pickComment() {
    if (view.length == 0) return null;
    final row = view[_rng.below(view.length)];
    final comments = row.comments;
    if (comments.length == 0) return null;
    return comments[_rng.below(comments.length)].id;
  }

  String _word() => words[_rng.below(words.length)];
}
