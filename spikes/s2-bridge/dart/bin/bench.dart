// Spike S2, Dart half: what does the FFI boundary cost?
//
// S1 measured a decode. It handed Dart a `.bin` that Rust had written earlier
// and timed `ViewDelta.fromBuffer`, and its own closing section says what it
// left open:
//
// > **Nothing has crossed FFI.** This measures decode, not the round trip. The
// > copy `flutter_rust_bridge` and UniFFI make at the boundary is unmeasured,
// > and it is the next thing to build.
//
// This is that. Every number here comes from calling a live engine through
// `flutter_rust_bridge`, and every one of them has a Rust control printed by
// `s2-fixture` against the same database. **The difference between the two
// columns is the answer**; the absolute numbers are not, because they also
// contain an engine.
//
// Four things are worth separating, and the benchmark is built around the
// separation rather than around a single round-trip figure:
//
//   subId()     a u64 out, no payload      → the per-call floor
//   initial()   266.4 KB out, no work      → the per-byte cost, outbound
//   mutate()    42 B in, real work         → the direction S1 never measured
//   the stream  271 B out, unsolicited     → the callback path
//
// A single "subscribe took Xms" would hide all of it: at 266 KB you cannot tell
// a fixed per-call cost from a per-byte copy, and knowing which one dominates
// decides whether the answer to a slow list is fewer calls or fewer bytes.
//
// Run it with `./run.sh` from this directory.

import 'dart:async';
import 'dart:io';

import 'package:async/async.dart';
import 'package:fixnum/fixnum.dart';
// `ExternalLibrary` is not in the package's public surface — a Flutter app never
// names it, because the generated loader finds the library by platform
// convention. A command-line benchmark has no such convention and has to open
// the `.so` by path.
import 'package:flutter_rust_bridge/flutter_rust_bridge_for_generated.dart';
import 'package:s2_bridge/gen/solstice/v1/mutation.pb.dart' as mut;
import 'package:s2_bridge/gen/solstice/v1/query.pb.dart' as q;
import 'package:s2_bridge/gen/solstice/v1/view.pb.dart';
import 'package:s2_bridge/src/rust/api.dart';
import 'package:s2_bridge/src/rust/frb_generated.dart';

const warmup = 50;
const reps = 200;

late String fixtures;

Future<void> main(List<String> args) async {
  fixtures = args.isEmpty ? '../fixtures' : args[0];
  final lib = args.length > 1 ? args[1] : '../../../target/release/libsolstice_ffi_dart.so';

  await RustLib.init(externalLibrary: ExternalLibrary.open(lib));

  print('# spike S2 — Dart');
  print('');
  print('Dart ${Platform.version.split(' ').first} · ${Platform.operatingSystem}');
  print('');

  final dbPath = '$fixtures/s2.db';
  if (!File(dbPath).existsSync()) {
    stderr.writeln(
      'missing $dbPath — run `cargo run --release -p solstice-bench --bin s2-fixture` first',
    );
    exit(2);
  }

  // The query is built here, in Dart, from `protoc`-generated classes. That is
  // the point of the byte ABI and the thing S1 could not show: no Rust glue
  // knows this application's schema, so nothing was regenerated to run it.
  final ir1000 = buildQuery(project: 1, k: 1000, comments: 3);
  final ir50 = buildQuery(project: 1, k: 50, comments: 3);
  crossCheckIr(ir1000, 'query-1000.bin');
  crossCheckIr(ir50, 'query-50.bin');

  final sw = Stopwatch()..start();
  final db = Database.open(path: dbPath);
  final opened = sw.elapsed;

  print('| what | dart | budget | |');
  print('|---|---|---|---|');
  row('open', opened);

  // --- outbound ---
  sw.reset();
  final sub = db.subscribe(queryIr: ir1000);
  final subscribed = sw.elapsed;
  row('subscribe k=1000', subscribed);

  final view = sub.initial();
  row('initial() k=1000 — ${fmtBytes(view.length)}', best(() => sub.initial()));
  row(
    'decode k=1000',
    best(() => ViewDelta.fromBuffer(view)),
    budget: const Duration(milliseconds: 5),
  );

  // The floor. Same FFI machinery, same handle, no payload at all — so whatever
  // `initial()` costs above this is bytes, and whatever it shares with this is
  // the price of the call itself.
  row('subId() — no payload', best(() => sub.subId()));

  final sub50 = db.subscribe(queryIr: ir50);
  final view50 = sub50.initial();
  row('initial() k=50 — ${fmtBytes(view50.length)}', best(() => sub50.initial()));

  // --- inbound ---
  //
  // Measured with no sink installed first, and that is not a throwaway row. The
  // engine pumps every graph and applies every view either way (see the early
  // return in `engine.rs::pump`); what it skips is encoding the `ViewDelta` and
  // handing it over. So the difference between this row and the one below is
  // the price of delivery, and the two bindings do not deliver the same way.
  final key = firstId(view);
  row('mutate — no sink installed', await bestAsync(() async {
    final body = buildMutation(key: key, at: DateTime.now().microsecondsSinceEpoch);
    db.mutate(body: body);
  }));

  // --- the stream ---
  //
  // Subscribed before the next mutate, because `setEventSink` installs the sink
  // on the engine thread asynchronously and a write that raced it would lose its
  // event. One stream per database, not per subscription (plan §4.2) — these
  // deltas carry two different `subId`s and the host demultiplexes.
  final deaf = db.stats().mutations;
  final events = StreamController<Uint8List>();
  final sinkSub = db.setEventSink().listen(events.add);
  await pump();

  row('mutate — 1 update, 2 views live', await bestAsync(() async {
    final body = buildMutation(key: key, at: DateTime.now().microsecondsSinceEpoch);
    db.mutate(body: body);
  }));

  // What a widget actually waits for: the write returns, and some time later a
  // diff arrives on the isolate's event loop. Unlike everything above, this one
  // has no Rust control that means anything — the Rust sink is called *on the
  // engine thread, inside* `mutate`, so its latency is zero by construction.
  // The number below is the event loop hop, and it is a cost only the host has.
  final latency = await eventLatency(db, events.stream, key);
  row('mutate → delta on the stream', latency, budget: const Duration(milliseconds: 16));

  // Clear first, then pump, then cancel. The other order makes the engine hold
  // a sink whose Dart port has already closed, and `flutter_rust_bridge` prints
  // "Fail to post message to Dart" on the next event. Harmless — the Rust side
  // ignores a failed `add` precisely so a disposed widget cannot take down the
  // engine — but it is the shape of teardown a host should copy.
  db.clearEventSink();
  await pump();
  await sinkSub.cancel();

  print('');
  crossCheck(view, db.stats(), deaf);
}

/// Plan §7's query, built the way an application would build it.
///
/// `project` is a **parameter**, not a literal in the predicate. Plan §1.2 hashes
/// the IR twice — without params for the `PipelineId`, with them for the `ViewId`
/// — so that N users running this query for N projects share one dataflow
/// pipeline on the server. Folding it in as a literal would encode a query shape
/// the real system never runs, and would also not match `query-1000.bin`.
Uint8List buildQuery({required int project, required int k, required int comments}) {
  final query = q.Query()
    ..table = 'issues'
    ..where = (q.Predicate()
      ..and = (q.PredicateList()
        ..preds.addAll([
          q.Predicate()
            ..cmp = (q.Cmp()
              ..lhs = (q.Expr()..col = 'project_id')
              ..op = q.CmpOp.CMP_OP_EQ
              ..rhs = (q.Expr()..param = 0)),
          q.Predicate()
            ..cmp = (q.Cmp()
              ..lhs = (q.Expr()..col = 'closed')
              ..op = q.CmpOp.CMP_OP_EQ
              ..rhs = (q.Expr()..lit = (Value()..integer = Int64.ZERO))),
        ])))
    ..orderBy.add(q.Order()
      ..col = 'priority'
      ..desc = true)
    ..limit = k
    // Built through the factory rather than a cascade: the `as` field is named
    // after a Dart keyword, so `..as = 'comments'` parses as a cast.
    ..related.add(q.Related(
      relName: 'comments',
      as: 'comments',
      sub: q.Query()
        ..table = 'comments'
        ..orderBy.add(q.Order()
          ..col = 'created_at'
          ..desc = true)
        ..limit = comments,
    ))
    ..params.add(Value()..integer = Int64(project));
  return query.writeToBuffer();
}

/// One `updated_at` write to a row the window holds — see `s2-fixture`'s
/// `touch` for why it is that column and why the key comes from the view.
Uint8List buildMutation({required int key, required int at}) {
  final m = mut.Mutation()
    ..clientId = Uint8List.fromList([1])
    ..mutationId = Int64(at)
    ..patch = (mut.Patch()
      ..ops.add(mut.Op()
        ..update = (mut.Update()
          ..table = 'issues'
          ..key = (Value()..integer = Int64(key))
          ..set.add(mut.ColValue()
            ..col = 'updated_at'
            ..value = (Value()..integer = Int64(at))))));
  return m.writeToBuffer();
}

/// Asserts that the IR Dart built is the IR Rust would have built.
///
/// This is a real check and not a formality. The engine resolves column names
/// against the schema at subscribe, so a query that named `projectId` instead of
/// `project_id` would come back as a `SolsticeError.compile` — but one that set
/// `limit` a field number off, or ordered ascending, would compile fine and
/// quietly measure a different query. Byte equality against the fixture is the
/// only version of this check that catches the second kind.
void crossCheckIr(Uint8List built, String name) {
  final expected = File('$fixtures/$name').readAsBytesSync();
  if (built.length != expected.length ||
      !Iterable.generate(built.length).every((i) => built[i] == expected[i])) {
    stderr.writeln(
      'the query Dart built is not the query Rust builds ($name): '
      '${built.length} bytes vs ${expected.length}',
    );
    stderr.writeln(
      'canonical encoding matters here — field order is the .proto tag order, '
      'and two encoders that disagree would also hash to two PipelineIds',
    );
    exit(1);
  }
}

/// The same assertion S1 makes, against bytes that came through FFI this time.
///
/// If these agree, the boundary is not merely fast — it is delivering exactly
/// the payload S1 decoded, and the two spikes' numbers are about the same thing.
/// `deaf` is the mutation count from before the sink was installed. Those writes
/// emitted nothing by design, so they are subtracted rather than allowed to
/// weaken the invariant below into "roughly two deltas per write".
void crossCheck(Uint8List bytes, EngineStats stats, BigInt deaf) {
  final d = ViewDelta.fromBuffer(bytes);
  var parents = 0, children = 0, scalars = 0;
  for (final change in d.changes) {
    if (!change.hasAdded()) continue;
    parents++;
    for (final v in change.added.row.values) {
      if (v.hasRows()) {
        for (final child in v.rows.rows) {
          children++;
          scalars += child.values.length;
        }
      } else {
        scalars++;
      }
    }
  }
  print(
    'cross-check: $parents parents · $children children · $scalars scalars · '
    '${stats.subscriptions} subs · ${stats.mutations} mutations '
    '($deaf before the sink) · ${stats.events} events · v${stats.version}',
  );

  final heard = stats.mutations - deaf;
  final failures = <String>[
    if (parents != 1000) 'parents: $parents != 1000',
    if (children != 3000) 'children: $children != 3000',
    if (scalars != 21000) 'scalars: $scalars != 21000',
    if (stats.subscriptions != BigInt.two) 'subs: ${stats.subscriptions} != 2',
    // Two live views, so every write made with the sink installed emits two
    // deltas — and every write made without it emits none.
    if (stats.events != heard * BigInt.two)
      'events: ${stats.events} != 2 × $heard',
  ];
  if (failures.isNotEmpty) {
    stderr.writeln('cross-check FAILED — ${failures.join('; ')}');
    exit(1);
  }
}

int firstId(Uint8List view) =>
    ViewDelta.fromBuffer(view).changes.first.added.row.values[0].integer.toInt();

/// Best-of, for the reason `s1-fixture` gives: the work is deterministic, so the
/// spread between repetitions is the machine's and the fastest run has least of
/// it in. Warmed first, because Dart AOT still has a cold instruction cache and
/// an unexpanded heap.
Duration best(void Function() f) {
  for (var i = 0; i < warmup; i++) {
    f();
  }
  var bestUs = 1 << 62;
  for (var i = 0; i < reps; i++) {
    final sw = Stopwatch()..start();
    f();
    sw.stop();
    if (sw.elapsedMicroseconds < bestUs) bestUs = sw.elapsedMicroseconds;
  }
  return Duration(microseconds: bestUs);
}

Future<Duration> bestAsync(Future<void> Function() f) async {
  for (var i = 0; i < warmup; i++) {
    await f();
  }
  var bestUs = 1 << 62;
  for (var i = 0; i < reps; i++) {
    final sw = Stopwatch()..start();
    await f();
    sw.stop();
    if (sw.elapsedMicroseconds < bestUs) bestUs = sw.elapsedMicroseconds;
  }
  return Duration(microseconds: bestUs);
}

/// Write, then wait for the diff to come back up the stream.
///
/// Measured as a round trip from the same thread rather than as a one-way
/// latency, because there is no shared clock to subtract across the boundary —
/// the `ViewDelta` carries a version, not a timestamp. A round trip is an upper
/// bound on the one-way cost and is what a widget waits for anyway.
Future<Duration> eventLatency(Database db, Stream<Uint8List> events, int key) async {
  final queue = StreamQueue<Uint8List>(events);
  var bestUs = 1 << 62;
  for (var i = 0; i < 50; i++) {
    final at = DateTime.now().microsecondsSinceEpoch;
    final sw = Stopwatch()..start();
    db.mutate(body: buildMutation(key: key, at: at));
    await queue.next;
    await queue.next; // two live views, two deltas
    sw.stop();
    if (sw.elapsedMicroseconds < bestUs) bestUs = sw.elapsedMicroseconds;
  }
  await queue.cancel(immediate: true);
  return Duration(microseconds: bestUs);
}

/// Let the isolate's event loop run, so a `setEventSink` posted to the engine
/// thread has actually landed before the first write.
Future<void> pump() => Future<void>.delayed(const Duration(milliseconds: 50));

void row(String label, Duration d, {Duration? budget}) {
  final verdict = budget == null ? '' : (d <= budget ? 'PASS' : 'FAIL');
  print('| $label | ${fmt(d)} | '
      '${budget == null ? '—' : '< ${fmt(budget)}'} | $verdict |');
}

String fmt(Duration d) {
  final us = d.inMicroseconds;
  if (us >= 1000) return '${(us / 1000).toStringAsFixed(2)}ms';
  if (us == 0) return '<1µs';
  return '${us}µs';
}

String fmtBytes(int n) =>
    n >= 1024 ? '${(n / 1024).toStringAsFixed(1)} KB' : '$n B';
