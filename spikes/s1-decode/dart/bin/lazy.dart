// Does plan §4.1's fallback actually work?
//
// Spike S1 said Dart fails the 5ms budget and said why: `package:protobuf`
// builds ~28,000 objects for a list that shows eight rows at a time. `probe.dart`
// then measured two floors on the same bytes — 422µs to walk every scalar, 8µs
// to index 1000 row offsets — which is evidence the fallback *should* work.
//
// "Should" is not a measurement. Those floors materialise nothing, and a real
// accessor has to hand the widget an `int` and a `String`. This runs the actual
// thing: `package:s1_decode/lazy_view.dart`, the prototype of what codegen would
// emit, doing the work a `ListView.builder` would really ask of it.
//
// Three numbers matter, and the third is the one that could still sink it:
//
//   1. **Index.** What `subscribe()` returns before the first frame. This is what
//      the kill criterion actually measures once decoding is lazy.
//   2. **One screen.** Index plus the rows a phone paints. This has to fit in a
//      16ms frame alongside layout, raster and everything else in the app.
//   3. **Everything.** Index plus all 1000 rows, fully read. Laziness moves cost
//      rather than removing it, so if this is *worse* than the 6.14ms eager
//      decode, the fallback has bought a good first frame at the price of a bad
//      fling — which on a scrolling list is a poor trade.

import 'dart:io';
import 'dart:typed_data';

import 'package:s1_decode/gen/solstice/v1/view.pb.dart' as eager;
import 'package:s1_decode/lazy_view.dart';

/// A phone paints about eight tiles; `ListView.builder` keeps a cache extent
/// either side, so a dozen is the honest number for one screen.
const screen = 12;

void main(List<String> args) {
  final dir = args.isEmpty ? '../fixtures' : args[0];
  final bytes = _read('$dir/view-1000.bin');

  // Correctness before timing. The hand-written walker in `probe.dart` was
  // wrong once — a compound assignment that read its left side too early — and
  // the lesson stuck: a fast wrong answer is worth less than a slow right one,
  // and an accessor is much easier to get subtly wrong than a decoder that
  // parses everything.
  //
  // Both fixtures, because they check different things — and against different
  // oracles, for a reason worth reading.
  //
  // The 1000-row view is real traffic, and there the generated decoder is the
  // oracle: two independent readers of the same bytes, agreeing field by field.
  //
  // `edge.bin` is four rows the hydration will never produce, and there the
  // generated decoder *cannot* be the oracle, because on two of those values it
  // is the one that is wrong. See `_verifyEdges`.
  _verify(bytes, 'view-1000.bin');
  _verifyEdges(_read('$dir/edge.bin'));

  print('# spike S1 — does the fallback work?');
  print('');
  print('Dart ${Platform.version.split(' ').first} · ${Platform.operatingSystem}'
      ' · ${_bytes(bytes.length)}, 1000 rows');
  print('');
  print('| what the host does | best | vs 5ms budget |');
  print('|---|---|---|');

  _row('eager decode, whole view (`package:protobuf`)',
      () => eager.ViewDelta.fromBuffer(bytes).changes.length);

  _row('lazy: index only — what `subscribe()` returns',
      () => LazyViewDelta(bytes).length);

  _row('lazy: index + $screen tiles (id, title, priority, closed)', () {
    final v = LazyViewDelta(bytes);
    var acc = 0;
    for (var i = 0; i < screen; i++) {
      final r = v[i];
      acc += r.id + r.title.length + (r.priority ?? 0) + (r.closed ? 1 : 0);
    }
    return acc;
  });

  _row('lazy: index + $screen tiles, comments included', () {
    final v = LazyViewDelta(bytes);
    var acc = 0;
    for (var i = 0; i < screen; i++) {
      final r = v[i];
      acc += r.id + r.title.length;
      final cs = r.comments;
      for (var j = 0; j < cs.length; j++) {
        final c = cs[j];
        acc += c.author.length + c.body.length;
      }
    }
    return acc;
  });

  _row('lazy: index + **all 1000** rows, comments included', () {
    final v = LazyViewDelta(bytes);
    var acc = 0;
    for (var i = 0; i < v.length; i++) {
      final r = v[i];
      acc += r.id + r.title.length + (r.priority ?? 0);
      final cs = r.comments;
      for (var j = 0; j < cs.length; j++) {
        final c = cs[j];
        acc += c.author.length + c.body.length;
      }
    }
    return acc;
  });
}

/// Every row, both ways, compared.
///
/// Not a spot check on the first row: an accessor that miscounts a column would
/// pass that and fail on the first row that differs. Every field of every row,
/// against the decoder `protoc` generated.
void _verify(Uint8List bytes, String label) {
  final e = eager.ViewDelta.fromBuffer(bytes);
  final l = LazyViewDelta(bytes);

  final problems = <String>[];
  void check(bool ok, String what) {
    if (!ok && problems.length < 5) problems.add(what);
  }

  check(l.length == e.changes.length, 'length ${l.length} != ${e.changes.length}');
  check(l.subId == e.subId.toInt(), 'sub_id ${l.subId} != ${e.subId}');
  check(l.version == e.version.toInt(), 'version ${l.version} != ${e.version}');

  var nulls = 0;
  final n = l.length < e.changes.length ? l.length : e.changes.length;
  for (var i = 0; i < n; i++) {
    final want = e.changes[i].added.row;
    final got = l[i];

    check(got.id == want.values[0].integer.toInt(), 'row $i id');
    check(got.projectId == want.values[1].integer.toInt(), 'row $i project_id');
    check(got.closed == (want.values[3].integer.toInt() != 0), 'row $i closed');
    check(got.title == want.values[4].text, 'row $i title');
    check(got.updatedAt == want.values[5].integer.toInt(), 'row $i updated_at');

    final wantNull =
        want.values[2].whichKind() == eager.Value_Kind.notSet;
    if (wantNull) nulls++;
    check((got.priority == null) == wantNull, 'row $i priority nullness');
    if (!wantNull) {
      check(got.priority == want.values[2].integer.toInt(), 'row $i priority');
    }

    final wantKids = want.values[6].rows.rows;
    final gotKids = got.comments;
    check(gotKids.length == wantKids.length, 'row $i comment count');
    for (var j = 0; j < wantKids.length && j < gotKids.length; j++) {
      check(gotKids[j].id == wantKids[j].values[0].integer.toInt(),
          'row $i comment $j id');
      check(gotKids[j].author == wantKids[j].values[3].text,
          'row $i comment $j author');
      check(gotKids[j].body == wantKids[j].values[4].text,
          'row $i comment $j body');
    }
  }

  if (problems.isNotEmpty) {
    stderr.writeln('$label: lazy accessor disagrees with the generated decoder:');
    for (final p in problems) {
      stderr.writeln('  - $p');
    }
    exit(1);
  }
  print('$label: verified $n rows field by field '
      '($nulls with a NULL priority)');
}

/// The edge fixture, checked against what Rust wrote rather than against
/// `package:protobuf`.
///
/// # `package:protobuf` decodes `sint64` wrongly at the extremes
///
/// Found here, and the reason this fixture exists. `zigzag(i64::MIN)` is
/// `u64::MAX`, the only value needing all ten varint bytes;
/// `zigzag(i64::MAX)` is `u64::MAX - 1`. Un-zigzagging needs a *logical* right
/// shift, and an arithmetic one collapses the result — which is a mistake this
/// accessor also made on its first draft, caught by this fixture.
///
/// `package:protobuf` 6.1.0 makes it too, and not in formatting: the bits are
/// wrong. It reports `i64::MIN` as `0` and `i64::MAX` as `-1`
/// (`toHexString()` confirms `FFFFFFFFFFFFFFFF`). Rust, `protoc`-generated Java
/// on protobuf-javalite, and this accessor all agree on the correct values, so
/// it is not the encoder.
///
/// It matters beyond a spike. A SQLite column holds an `i64`, so these are
/// values a real row may legitimately carry, and the failure is silent — a
/// corrupted number, not an exception. It is an argument for the generated
/// accessor over `package:protobuf` that has nothing to do with speed, and
/// worth reporting upstream.
void _verifyEdges(Uint8List bytes) {
  const i64min = -9223372036854775808;
  const i64max = 9223372036854775807;

  final v = LazyViewDelta(bytes);
  final problems = <String>[];
  void check(bool ok, String what) {
    if (!ok) problems.add(what);
  }

  check(v.length == 4, 'length ${v.length} != 4');
  check(v.subId == 7 && v.version == 3, 'sub ${v.subId} v${v.version} != 7/3');

  // NULL as the unset oneof, and an empty child collection.
  final a = v[0];
  check(a.priority == null, 'row 0 priority ${a.priority} should be NULL');
  check(a.title == 'no priority, no comments', 'row 0 title');
  check(a.comments.length == 0, 'row 0 should have no comments');

  // A zero that must still write its tag, and an empty string. If `Int(0)` were
  // omitted the way proto3 omits zero-valued scalars, it would read back as
  // NULL — which is a different value, and the one above.
  final b = v[1];
  check(b.priority == 0, 'row 1 priority ${b.priority} should be 0, not null');
  check(b.id == 0 && b.title == '', 'row 1 zero id / empty title');

  // Negatives and ten-byte varints.
  final c = v[2];
  check(c.id == -1, 'row 2 id ${c.id}');
  check(c.projectId == -2000000, 'row 2 project_id ${c.projectId}');
  check(c.priority == i64min, 'row 2 priority ${c.priority} != i64::MIN');
  check(c.updatedAt == i64max, 'row 2 updated_at ${c.updatedAt} != i64::MAX');
  check(c.closed, 'row 2 closed');

  // Multi-byte UTF-8, decoded out of a range rather than a copy, and a child
  // whose text field is empty.
  final d = v[3];
  check(d.id == 9007199254740993, 'row 3 id ${d.id}');
  check(d.title == 'judul — panjang ünïcödé ✓', 'row 3 title "${d.title}"');
  check(d.comments.length == 1, 'row 3 comment count');
  final kid = d.comments[0];
  check(kid.author == '', 'row 3 comment author should be empty');
  check(kid.body == 'emoji: 🌒 solstice', 'row 3 comment body "${kid.body}"');

  if (problems.isNotEmpty) {
    stderr.writeln('edge.bin: lazy accessor disagrees with what Rust encoded:');
    for (final p in problems) {
      stderr.writeln('  - $p');
    }
    exit(1);
  }
  print('edge.bin: verified 4 rows — NULL, Int(0), i64 extremes, multi-byte UTF-8');

  // And the finding, restated as a live check rather than a comment, so that
  // the day `package:protobuf` fixes this the spike says so instead of quietly
  // continuing to warn about it.
  final e = eager.ViewDelta.fromBuffer(bytes).changes[2].added.row;
  final theirMin = e.values[2].integer.toInt();
  final theirMax = e.values[5].integer.toInt();
  if (theirMin == i64min && theirMax == i64max) {
    print('note: `package:protobuf` now decodes the i64 extremes correctly — '
        'the warning in this file is out of date');
  } else {
    print('note: `package:protobuf` decodes i64::MIN as $theirMin and '
        'i64::MAX as $theirMax — wrong bits, not formatting');
  }
  print('');
}

/// Best-of-N over batches.
///
/// Batched because `Stopwatch` resolves to a microsecond here and the index is
/// expected to land near 8µs: timing it one call at a time would quantise the
/// answer into "8 or 9" and call that a measurement.
void _row(String label, int Function() work) {
  const warmup = 300;
  const batches = 200;
  const batch = 20;

  for (var i = 0; i < warmup; i++) {
    if (work() == 0) throw StateError('empty');
  }

  var best = double.infinity;
  for (var i = 0; i < batches; i++) {
    final sw = Stopwatch()..start();
    for (var j = 0; j < batch; j++) {
      if (work() == 0) throw StateError('empty');
    }
    sw.stop();
    final per = sw.elapsedMicroseconds / batch;
    if (per < best) best = per;
  }

  final pass = best <= 5000;
  print('| $label | ${_us(best)} | ${pass ? "PASS" : "**FAIL**"} |');
}

Uint8List _read(String path) {
  final f = File(path);
  if (!f.existsSync()) {
    stderr.writeln(
      'missing $path — run `cargo run --release -p solstice-bench --bin s1-fixture` first',
    );
    exit(2);
  }
  return f.readAsBytesSync();
}

String _us(double us) =>
    us >= 1000 ? '${(us / 1000).toStringAsFixed(2)}ms' : '${us.toStringAsFixed(2)}µs';

String _bytes(int n) =>
    n >= 1024 ? '${(n / 1024).toStringAsFixed(1)} KB' : '$n B';
