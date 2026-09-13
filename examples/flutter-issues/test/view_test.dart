// The host half of plan §4.2, against an oracle.
//
// `crates/solstice-core/tests/churn_convergence.rs` proves the engine emits a
// diff a host can replay. This proves *this* host replays it — and the two are
// not the same claim, because the Dart applier reads the wire bytes by hand
// (S1's zero-copy fallback) rather than through the generated classes. A
// decoder written a second time is a decoder that can disagree a second way.
//
// The oracle is a plain `List<int>` of ids maintained with the obvious
// operations. The payloads are built with `package:protobuf`, so the encoder
// under test here is the one that actually ships.

import 'dart:math';
import 'dart:typed_data';

import 'package:fixnum/fixnum.dart';

import 'package:flutter_issues/gen/solstice/v1/view.pb.dart' as pb;
import 'package:flutter_issues/src/view.dart';
import 'package:flutter_test/flutter_test.dart';

/// One issue row in the shape `m0::schemas()` declares: six scalars and the
/// joined collection.
pb.Row issueRow(int id, int priority, {int comments = 0}) {
  final row = pb.Row()
    ..values.addAll([
      pb.Value()..integer = Int64(id),
      pb.Value()..integer = Int64(7),
      pb.Value()..integer = Int64(priority),
      pb.Value()..integer = Int64(0),
      pb.Value()..text = 'issue $id',
      pb.Value()..integer = Int64(id * 7),
    ]);
  final children = pb.RowList();
  for (var c = 0; c < comments; c++) {
    children.rows.add(pb.Row()
      ..values.addAll([
        pb.Value()..integer = Int64(id * 10 + c),
        pb.Value()..integer = Int64(id),
        pb.Value()..integer = Int64(c),
        pb.Value()..text = 'ana',
        pb.Value()..text = 'comment $c',
      ]));
  }
  row.values.add(pb.Value()..rows = children);
  return row;
}

/// A length-delimited field, written the way the wire wants it.
void _lenField(BytesBuilder b, int field, List<int> payload) {
  _varint(b, (field << 3) | 2);
  _varint(b, payload.length);
  b.add(payload);
}

void _varint(BytesBuilder b, int v) {
  while (v >= 0x80) {
    b.addByte((v & 0x7f) | 0x80);
    v >>>= 7;
  }
  b.addByte(v);
}

void main() {
  // The bug this is here for: `_Buf.skip` advanced by the *length* of a
  // length-delimited field without also advancing past the length prefix, so
  // every skip landed one or two bytes inside the payload. Nothing in the repo
  // could see it. The engine's own tests decode with the Rust decoder; the
  // tests below all parse fields this build knows, and `apply` resets the
  // cursor at the end of each change, which hides a short skip of the last
  // field. It took a crash on the device — `cols: [6]`, whose bytes read as
  // wire type 6 — to surface it, after several thousand deltas had already
  // silently gone missing.
  group('fields this build does not read', () {
    test('a trailing cols list is skipped whatever it contains', () {
      // 6 is the children column, which is the *common* case: every comment
      // added or dropped changes it. Its byte lands on wire type 6, which the
      // old skip walked into and threw on — aborting `apply` in the middle of
      // the batch and leaving the list half-patched.
      for (final cols in [
        <int>[6],
        [5, 6],
        [2, 5, 6],
        [0, 4, 9],
      ]) {
        final delta = pb.ViewDelta()
          ..subId = Int64(1)
          ..version = Int64(1);
        delta.changes.add(pb.ViewChange()
          ..added = (pb.Added()
            ..index = 0
            ..row = issueRow(1, 500)));
        delta.changes.add(pb.ViewChange()
          ..changed = (pb.Changed()
            ..index = 0
            ..row = issueRow(1, 700, comments: 2)
            ..cols.addAll(cols)));
        // A second change *after* the one carrying `cols`, because that is what
        // a mis-skip actually costs: not the row it stumbled on, but every
        // change behind it in the same delta.
        delta.changes.add(pb.ViewChange()
          ..added = (pb.Added()
            ..index = 1
            ..row = issueRow(2, 100)));

        final view = IssueView();
        final counts = view.apply(Uint8List.fromList(delta.writeToBuffer()));

        expect(counts.added, 2, reason: 'cols $cols');
        expect(counts.changed, 1, reason: 'cols $cols');
        expect(counts.outOfRange, 0, reason: 'cols $cols');
        expect([for (var i = 0; i < view.length; i++) view[i].id], [1, 2],
            reason: 'cols $cols');
        expect(view[0].priority, 700, reason: 'cols $cols');
      }
    });

    test('an unknown field in front of the known ones is stepped over', () {
      // Plan §3.5's forward-compatibility promise, which is the reason `skip`
      // exists at all: a newer engine adds a field, an older host walks past
      // it. Put in *front* of `index` and `row` on purpose — behind them, a
      // short skip is masked by the cursor reset at the end of the change, and
      // that masking is how the bug survived.
      final row = issueRow(4, 800, comments: 1).writeToBuffer();
      final changed = BytesBuilder();
      _lenField(changed, 9, List<int>.filled(200, 0xff)); // two-byte length
      _lenField(changed, 8, const [0x01]); // one-byte length
      _varint(changed, (1 << 3) | 0);
      _varint(changed, 0); // index = 0
      _lenField(changed, 2, row);

      final change = BytesBuilder();
      _lenField(change, 3, changed.takeBytes());

      final delta = BytesBuilder();
      _varint(delta, (1 << 3) | 0);
      _varint(delta, 1); // sub_id
      _varint(delta, (2 << 3) | 0);
      _varint(delta, 7); // version
      _lenField(delta, 3, change.takeBytes());

      final view = IssueView();
      view.hydrate(Uint8List.fromList((pb.ViewDelta()
            ..subId = Int64(1)
            ..changes.add(pb.ViewChange()
              ..added = (pb.Added()
                ..index = 0
                ..row = issueRow(4, 1))))
          .writeToBuffer()));

      final counts = view.apply(delta.takeBytes());
      expect(counts.changed, 1);
      expect(counts.outOfRange, 0);
      expect(view.version, 7);
      expect(view[0].priority, 800);
      expect(view[0].comments.length, 1);
    });
  });

  test('an initial view hydrates in order', () {
    final delta = pb.ViewDelta()
      ..subId = Int64(3)
      ..version = Int64(1);
    for (var i = 0; i < 5; i++) {
      delta.changes.add(pb.ViewChange()
        ..added = (pb.Added()
          ..index = i
          ..row = issueRow(100 + i, 900 - i, comments: 2)));
    }

    final view = IssueView();
    final counts = view.hydrate(Uint8List.fromList(delta.writeToBuffer()));

    expect(counts.added, 5);
    expect(counts.keyMismatches, 0);
    expect([for (var i = 0; i < view.length; i++) view[i].id],
        [100, 101, 102, 103, 104]);
    expect(view[0].priority, 900);
    expect(view[0].comments.length, 2);
    expect(view[0].comments[1].author, 'ana');
  });

  test('sub_id is readable without decoding the rest', () {
    final delta = pb.ViewDelta()..subId = Int64(9);
    delta.changes.add(pb.ViewChange()
      ..added = (pb.Added()
        ..index = 0
        ..row = issueRow(1, 1)));
    expect(IssueView.subIdOf(Uint8List.fromList(delta.writeToBuffer())), 9);
  });

  test('a long random diff stream lands where the oracle does', () {
    // Seeded, so a failure is one `flutter test` away from being reproduced.
    final rng = Random(0x501571CE);
    final view = IssueView();
    final oracle = <int>[];
    final priorities = <int, int>{};
    var nextId = 1;
    var mismatches = 0;

    for (var round = 0; round < 400; round++) {
      final delta = pb.ViewDelta()
        ..subId = Int64(1)
        ..version = Int64(round + 1);

      // Several changes per delta, each indexed relative to the list after
      // every earlier change in the same batch — the contract `view.rs`
      // documents and the one an off-by-one violates silently.
      final n = 1 + rng.nextInt(4);
      for (var c = 0; c < n; c++) {
        final roll = oracle.isEmpty ? 0 : rng.nextInt(100);
        if (roll < 30) {
          final id = nextId++;
          final at = rng.nextInt(oracle.length + 1);
          final priority = rng.nextInt(1000);
          oracle.insert(at, id);
          priorities[id] = priority;
          delta.changes.add(pb.ViewChange()
            ..added = (pb.Added()
              ..index = at
              ..row = issueRow(id, priority, comments: rng.nextInt(4))));
        } else if (roll < 55) {
          final at = rng.nextInt(oracle.length);
          final id = oracle.removeAt(at);
          delta.changes.add(pb.ViewChange()
            ..removed = (pb.Removed()
              ..index = at
              ..key = (pb.Value()..integer = Int64(id))));
        } else if (roll < 80) {
          final at = rng.nextInt(oracle.length);
          final id = oracle[at];
          final priority = rng.nextInt(1000);
          priorities[id] = priority;
          delta.changes.add(pb.ViewChange()
            ..changed = (pb.Changed()
              ..index = at
              ..row = issueRow(id, priority, comments: rng.nextInt(4))
              ..cols.addAll([2, 5])));
        } else {
          final from = rng.nextInt(oracle.length);
          final id = oracle.removeAt(from);
          final to = rng.nextInt(oracle.length + 1);
          oracle.insert(to, id);
          delta.changes.add(pb.ViewChange()
            ..moved = (pb.Moved()
              ..from = from
              ..to = to));
        }
      }

      final counts = view.apply(Uint8List.fromList(delta.writeToBuffer()));
      mismatches += counts.keyMismatches;

      expect([for (var i = 0; i < view.length; i++) view[i].id], oracle,
          reason: 'diverged in round $round');
    }

    expect(mismatches, 0);
    expect(view.version, 400);
    // The rows, not just their order: a `Changed` that kept the id but dropped
    // the payload would pass every check above.
    for (var i = 0; i < view.length; i++) {
      expect(view[i].priority, priorities[view[i].id],
          reason: 'row ${view[i].id} is carrying the wrong priority');
    }
  });
}
