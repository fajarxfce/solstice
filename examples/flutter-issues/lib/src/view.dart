// The list model: positional diffs in, lazily-decoded rows out.
//
// Two things are being proved here at once, and they are the two halves of
// plan §5.1's Flutter question.
//
// **The accessor.** Spike S1 measured `package:protobuf` decoding a 1000-row
// view in 6.14ms against a 5ms budget and found 93% of it was materialising
// ~28,000 `GeneratedMessage` objects — for rows nobody was looking at. Its
// prototype fallback (`spikes/s1-decode/dart/lib/lazy_view.dart`) indexed the
// same payload in 8µs by not building them. That prototype handled `Added` and
// nothing else, because a hydration is all inserts. This is the same idea
// carried to a live view, which needs all four change kinds.
//
// **The contract.** `solstice-core`'s `View` documents it precisely: *each
// change's index is relative to the list after every earlier change in the same
// batch has been applied*. That is also what `ListView`, `AnimatedList` and
// `LazyColumn` implement, so honouring it is a single pass with no diffing. A
// host that got this wrong would not crash; it would drift, slowly, in a way
// that only shows up as rows in the wrong order after a few thousand writes.
// So `Removed` is checked against the key the engine sends — `view.proto` puts
// the key in that message for exactly this reason — and any mismatch is
// counted and surfaced in the HUD rather than swallowed.
//
// # What is not lazy
//
// Strings. `utf8.decode` has to build a `String`, and Dart has no view type
// over a byte range that `Text()` accepts. That is the floor, and it is the
// right floor: you cannot render a string you have not decoded.
//
// # What zero-copy costs
//
// A row holds a range inside the buffer the delta arrived in, so a view of 50
// rows can be pinning 50 small buffers plus whatever is left of the initial
// hydration. At M0's sizes — 13 KB for k=50, 271 B per delta — that is a
// rounding error against the 60 MB budget, and `Database.stats()` reports the
// Rust side separately so the two never get confused. At k=1000 the hydration
// buffer is 266 KB and stays alive until the last row from it is replaced,
// which is a real trade and worth knowing about before it is a surprise.

import 'dart:convert';
import 'dart:typed_data';

const _utf8 = Utf8Decoder();

/// Field numbers from `proto/solstice/v1/view.proto`.
///
/// Written out rather than imported from the generated code, for the reason S1
/// gives: depending on `package:protobuf` in order to avoid `package:protobuf`
/// would defeat the exercise. These are frozen at 1.0 (plan §3.5) and until
/// then the `.proto` is the source of truth this file is checked against.
abstract final class _F {
  // ViewDelta
  static const subId = 1;
  static const version = 2;
  static const changes = 3;
  // ViewChange oneof
  static const added = 1;
  static const removed = 2;
  static const changed = 3;
  static const moved = 4;
  // Added / Removed / Changed
  static const index = 1;
  static const row = 2;
  static const key = 2;
  // Moved
  static const from = 1;
  static const to = 2;
  // Row / RowList
  static const values = 1;
  static const rowListRows = 1;
  // Value oneof
  static const integer = 1;
  static const text = 3;
  static const rows = 5;
}

/// What one delta did, for the HUD.
class DeltaCounts {
  final int added;
  final int removed;
  final int changed;
  final int moved;

  /// Removals the host could not match: an index outside the list, or a key
  /// that is not the key of the row at it.
  ///
  /// Must be zero. Counted rather than asserted because an assertion would be
  /// compiled out of the release build, and release is the only build whose
  /// latency means anything.
  final int keyMismatches;

  /// Changes whose index was outside the list, which this applier clamps or
  /// drops rather than throwing.
  ///
  /// Separate from [keyMismatches] because they say different things. A key
  /// mismatch means the row at that index is not the row the engine meant; an
  /// out-of-range index means the two lists are not even the same length. The
  /// second is the louder symptom and usually the earlier one.
  final int outOfRange;

  const DeltaCounts({
    this.added = 0,
    this.removed = 0,
    this.changed = 0,
    this.moved = 0,
    this.keyMismatches = 0,
    this.outOfRange = 0,
  });

  int get total => added + removed + changed + moved;

  DeltaCounts operator +(DeltaCounts o) => DeltaCounts(
        added: added + o.added,
        removed: removed + o.removed,
        changed: changed + o.changed,
        moved: moved + o.moved,
        keyMismatches: keyMismatches + o.keyMismatches,
        outOfRange: outOfRange + o.outOfRange,
      );

  @override
  String toString() => '+$added -$removed ~$changed ↕$moved';
}

/// The rows of one subscription, kept in sync by applying diffs in order.
class IssueView {
  final List<IssueRow> _rows = <IssueRow>[];

  int subId = 0;

  /// The engine's global version, not this view's. Plan §1.4: a transaction
  /// touching five tables produces one pump, and every affected view emits at
  /// the same version — which is what lets a host render five lists without
  /// ever showing a torn read.
  int version = 0;

  int get length => _rows.length;
  IssueRow operator [](int i) => _rows[i];

  /// Out-of-range indices seen while applying the current delta.
  ///
  /// A field rather than a return value because all four appliers can produce
  /// one, and threading a second count out of each of them would bury the one
  /// line of logic each contains.
  int _oor = 0;

  /// Which subscription a delta belongs to, without decoding the rest of it.
  ///
  /// Plan §4.2 puts one event stream on the `Database` rather than one per
  /// subscription — fewer FFI objects, and one total order, so a host can
  /// observe that two lists moved together — and leaves the demultiplexing to
  /// the host. This is that, and it is deliberately cheap: `sub_id` is field 1,
  /// so in practice it is the first varint in the message and nothing else is
  /// touched.
  static int subIdOf(Uint8List bytes) {
    final b = _Buf(bytes);
    while (!b.done) {
      final tag = b.varint();
      if (tag == _F.subId << 3) return b.varint();
      b.skip(tag & 7);
    }
    return 0;
  }

  /// The whole view as inserts, from `Subscription.initial()`.
  ///
  /// Not a special case in the parser — a hydration *is* a delta whose changes
  /// happen to all be `Added` — but it clears first, so a resync (plan §4.2's
  /// backpressure valve) lands on an empty list rather than doubling it.
  DeltaCounts hydrate(Uint8List bytes) {
    _rows.clear();
    return apply(bytes);
  }

  /// Apply one `ViewDelta`, returning what it did.
  DeltaCounts apply(Uint8List bytes) {
    final b = _Buf(bytes);
    var added = 0, removed = 0, changed = 0, moved = 0, mismatches = 0;
    _oor = 0;

    while (!b.done) {
      final tag = b.varint();
      switch (tag) {
        case const (_F.subId << 3): // varint
          subId = b.varint();
        case const (_F.version << 3):
          version = b.varint();
        case const ((_F.changes << 3) | 2):
          final end = b.varint() + b.p;
          // Exactly one field is set — it is a oneof — so the first one wins
          // and the rest of the submessage is skipped by the loop below.
          while (b.p < end) {
            final t = b.varint();
            switch (t) {
              case const ((_F.added << 3) | 2):
                final n = _applyAdded(b);
                added += n;
              case const ((_F.removed << 3) | 2):
                mismatches += _applyRemoved(b);
                removed++;
              case const ((_F.changed << 3) | 2):
                _applyChanged(b);
                changed++;
              case const ((_F.moved << 3) | 2):
                _applyMoved(b);
                moved++;
              default:
                b.skip(t & 7);
            }
          }
          b.p = end;
        default:
          b.skip(tag & 7);
      }
    }

    return DeltaCounts(
      added: added,
      removed: removed,
      changed: changed,
      moved: moved,
      keyMismatches: mismatches,
      outOfRange: _oor,
    );
  }

  int _applyAdded(_Buf b) {
    final end = b.varint() + b.p;
    var index = 0, start = -1, rowEnd = -1;
    while (b.p < end) {
      final t = b.varint();
      switch (t) {
        case const (_F.index << 3):
          index = b.varint();
        case const ((_F.row << 3) | 2):
          final len = b.varint();
          start = b.p;
          rowEnd = b.p + len;
          b.p = rowEnd;
        default:
          b.skip(t & 7);
      }
    }
    b.p = end;
    if (start < 0) return 0;
    // Clamped rather than trusted, and counted rather than clamped quietly. An
    // index past the end can only mean the host and the engine disagree about
    // the contract above, and crashing the demo would hide the disagreement
    // behind a stack trace.
    if (index < 0 || index > _rows.length) _oor++;
    _rows.insert(index.clamp(0, _rows.length), IssueRow(b.b, start, rowEnd));
    return 1;
  }

  int _applyRemoved(_Buf b) {
    final end = b.varint() + b.p;
    var index = 0, keyStart = -1, keyEnd = -1;
    while (b.p < end) {
      final t = b.varint();
      switch (t) {
        case const (_F.index << 3):
          index = b.varint();
        case const ((_F.key << 3) | 2):
          final len = b.varint();
          keyStart = b.p;
          keyEnd = b.p + len;
          b.p = keyEnd;
        default:
          b.skip(t & 7);
      }
    }
    b.p = end;
    if (index < 0 || index >= _rows.length) {
      _oor++;
      return 0;
    }
    var mismatch = 0;
    if (keyStart >= 0) {
      final expected = _Buf(b.b, keyStart).valueInt(keyEnd);
      if (expected != _rows[index].id) mismatch = 1;
    }
    _rows.removeAt(index);
    return mismatch;
  }

  void _applyChanged(_Buf b) {
    final end = b.varint() + b.p;
    var index = 0, start = -1, rowEnd = -1;
    while (b.p < end) {
      final t = b.varint();
      switch (t) {
        case const (_F.index << 3):
          index = b.varint();
        case const ((_F.row << 3) | 2):
          final len = b.varint();
          start = b.p;
          rowEnd = b.p + len;
          b.p = rowEnd;
        // `cols` is the one field this app ignores. It exists for a host that
        // rebuilds a widget per changed field; this one rebuilds the tile, and
        // `Changed.row` is the whole row, not a patch.
        default:
          b.skip(t & 7);
      }
    }
    b.p = end;
    if (start < 0) return;
    if (index < 0 || index >= _rows.length) {
      _oor++;
      return;
    }
    _rows[index] = IssueRow(b.b, start, rowEnd);
  }

  void _applyMoved(_Buf b) {
    final end = b.varint() + b.p;
    var from = 0, to = 0;
    while (b.p < end) {
      final t = b.varint();
      switch (t) {
        case const (_F.from << 3):
          from = b.varint();
        case const (_F.to << 3):
          to = b.varint();
        default:
          b.skip(t & 7);
      }
    }
    b.p = end;
    if (from < 0 || from >= _rows.length) {
      _oor++;
      return;
    }
    final row = _rows.removeAt(from);
    // `to` is an index into the list *after* the removal — the contract
    // `solstice-core`'s `View` documents, and what `AnimatedList` implements.
    if (to < 0 || to > _rows.length) _oor++;
    _rows.insert(to.clamp(0, _rows.length), row);
  }
}

/// A cursor over a protobuf message: a position plus the primitives to move it.
///
/// One allocation per parse rather than per field. S1's prototype avoided even
/// that by leaving the new position in a mutable member — which it needed at
/// 28,000 field reads per hydration, and which this does not: a delta carries a
/// handful of changes, and the rows underneath keep S1's shape unchanged.
final class _Buf {
  final Uint8List b;
  int p;

  _Buf(this.b, [this.p = 0]);

  bool get done => p >= b.length;

  int varint() {
    var x = b[p++];
    if (x < 0x80) return x;
    var result = x & 0x7f;
    var shift = 7;
    while (true) {
      x = b[p++];
      result |= (x & 0x7f) << shift;
      if (x < 0x80) return result;
      shift += 7;
    }
  }

  void skip(int wire) {
    switch (wire) {
      case 0:
        varint();
      case 1:
        p += 8;
      case 2:
        // `p += varint()` is wrong here and was wrong for a week: Dart reads
        // the left operand *before* evaluating the right, so the assignment
        // puts back the `p` from before `varint()` consumed the length prefix,
        // and the skip lands one or two bytes inside the payload. The next byte
        // read as a tag is then whatever the data happens to be.
        final len = varint();
        p += len;
      case 5:
        p += 4;
      default:
        throw StateError('wire type $wire');
    }
  }

  /// The integer inside a `Value` whose body ends at [end], or 0 for NULL.
  int valueInt(int end) {
    while (p < end) {
      final tag = varint();
      if (tag == _F.integer << 3) {
        final n = varint();
        return (n >>> 1) ^ -(n & 1); // zigzag
      }
      skip(tag & 7);
    }
    return 0;
  }
}

/// One joined issue, decoded field by field as something asks for it.
///
/// Unchanged in shape from S1's prototype, down to the `_pos` member: this is
/// the class the 8µs number was measured on, and the point of the demo is to
/// run *that* code in a frame rather than a tidier relative of it.
class IssueRow {
  static const _columns = 7; // six scalars plus the joined collection

  final Uint8List _b;
  final int _start;
  final int _end;

  /// Start and end of each column's `Value` body, filled on first access.
  /// One allocation per row that is actually looked at; the eager decoder
  /// allocates 28 for the same row.
  Int32List? _cols;

  IssueRow(this._b, this._start, this._end);

  int get id => _int(0);
  int get projectId => _int(1);
  int? get priority => _isNull(2) ? null : _int(2);
  bool get closed => _int(3) != 0;
  String get title => _string(4);
  int get updatedAt => _int(5);

  /// The nested collection the 1:N join attached. Indexed on access, not now.
  CommentList get comments {
    final c = _index();
    final lo = c[6 * 2], hi = c[6 * 2 + 1];
    if (lo < 0) return CommentList.empty;
    return CommentList._(_b, lo, hi);
  }

  Int32List _index() {
    var c = _cols;
    if (c != null) return c;
    c = Int32List(_columns * 2)..fillRange(0, _columns * 2, -1);
    var p = _start;
    var col = 0;
    while (p < _end && col < _columns) {
      final tag = _varint(p);
      p = _pos;
      if (tag >> 3 == _F.values && tag & 7 == 2) {
        final len = _varint(p);
        p = _pos;
        c[col * 2] = p;
        c[col * 2 + 1] = p + len;
        col++;
        p += len;
      } else {
        p = _skip(p, tag & 7);
      }
    }
    _cols = c;
    return c;
  }

  /// A `Value` whose body is empty is NULL — the unset oneof case.
  bool _isNull(int col) {
    final c = _index();
    return c[col * 2] < 0 || c[col * 2] == c[col * 2 + 1];
  }

  int _int(int col) {
    final c = _index();
    var p = c[col * 2];
    final end = c[col * 2 + 1];
    while (p < end) {
      final tag = _varint(p);
      p = _pos;
      if (tag >> 3 == _F.integer && tag & 7 == 0) {
        final n = _varint(p);
        // `>>>`, not `>>`. Dart's `>>` is arithmetic, and a ten-byte varint
        // fills the sign bit: `zigzag(i64::MIN)` is `u64::MAX`, which Dart
        // holds as `-1`, and `-1 >> 1` is `-1` — so the value collapses to 0.
        return (n >>> 1) ^ -(n & 1);
      }
      p = _skip(p, tag & 7);
    }
    return 0;
  }

  String _string(int col) {
    final c = _index();
    var p = c[col * 2];
    final end = c[col * 2 + 1];
    while (p < end) {
      final tag = _varint(p);
      p = _pos;
      if (tag >> 3 == _F.text && tag & 7 == 2) {
        final len = _varint(p);
        // `convert` over a range, not `sublist` then decode: the sublist would
        // copy the bytes only to throw the copy away.
        return _utf8.convert(_b, _pos, _pos + len);
      }
      p = _skip(p, tag & 7);
    }
    return '';
  }

  int _pos = 0;

  int _varint(int p) {
    var b = _b[p++];
    if (b < 0x80) {
      _pos = p;
      return b;
    }
    var result = b & 0x7f;
    var shift = 7;
    while (true) {
      b = _b[p++];
      result |= (b & 0x7f) << shift;
      if (b < 0x80) {
        _pos = p;
        return result;
      }
      shift += 7;
    }
  }

  int _skip(int p, int wire) {
    switch (wire) {
      case 0:
        _varint(p);
        return _pos;
      case 1:
        return p + 8;
      case 2:
        final len = _varint(p);
        return _pos + len;
      case 5:
        return p + 4;
      default:
        throw StateError('wire type $wire');
    }
  }
}

/// The children of one issue, indexed lazily and capped by the query's `limit`.
class CommentList {
  final Uint8List? _b;
  final int _start;
  final int _end;
  Int32List? _rows;
  int _length = -1;

  /// Shared: a parent with no children is common and there is nothing to
  /// distinguish one empty list from another.
  static final empty = CommentList._empty();

  CommentList._empty()
      : _b = null,
        _start = 0,
        _end = 0,
        _length = 0;

  CommentList._(this._b, this._start, this._end);

  int get length {
    if (_length < 0) _index();
    return _length;
  }

  CommentRow operator [](int i) {
    if (_length < 0) _index();
    final r = _rows!;
    return CommentRow._(_b!, r[i * 2], r[i * 2 + 1]);
  }

  /// Bounded by the query's `limit`, which is why 8 is enough and growth is a
  /// fallback. Plan §1.1 makes `limit` mandatory on 1:N traversal precisely so
  /// this array has a bound.
  void _index() {
    final b = _b;
    if (b == null) {
      _length = 0;
      return;
    }
    var r = Int32List(16);
    var n = 0;
    var p = _start;
    while (p < _end) {
      final tag = _varint(b, p);
      p = _pos;
      if (tag >> 3 == _F.rows && tag & 7 == 2) {
        final len = _varint(b, p);
        p = _pos;
        final listEnd = p + len;
        while (p < listEnd) {
          final t = _varint(b, p);
          p = _pos;
          if (t >> 3 == _F.rowListRows && t & 7 == 2) {
            final l = _varint(b, p);
            p = _pos;
            if (n * 2 == r.length) {
              r = Int32List(r.length * 2)..setRange(0, n * 2, r);
            }
            r[n * 2] = p;
            r[n * 2 + 1] = p + l;
            n++;
            p += l;
          } else {
            p = _skip(b, p, t & 7);
          }
        }
        p = listEnd;
      } else {
        p = _skip(b, p, tag & 7);
      }
    }
    _rows = r;
    _length = n;
  }

  int _pos = 0;

  int _varint(Uint8List b, int p) {
    var x = b[p++];
    if (x < 0x80) {
      _pos = p;
      return x;
    }
    var result = x & 0x7f;
    var shift = 7;
    while (true) {
      x = b[p++];
      result |= (x & 0x7f) << shift;
      if (x < 0x80) {
        _pos = p;
        return result;
      }
      shift += 7;
    }
  }

  int _skip(Uint8List b, int p, int wire) {
    switch (wire) {
      case 0:
        _varint(b, p);
        return _pos;
      case 1:
        return p + 8;
      case 2:
        final len = _varint(b, p);
        return _pos + len;
      case 5:
        return p + 4;
      default:
        throw StateError('wire type $wire');
    }
  }
}

/// One comment. Same shape as [IssueRow], five columns, no children.
class CommentRow {
  static const _columns = 5;

  final Uint8List _b;
  final int _start;
  final int _end;
  Int32List? _cols;

  CommentRow._(this._b, this._start, this._end);

  int get id => _int(0);
  int get issueId => _int(1);
  int get createdAt => _int(2);
  String get author => _string(3);
  String get body => _string(4);

  Int32List _index() {
    var c = _cols;
    if (c != null) return c;
    c = Int32List(_columns * 2)..fillRange(0, _columns * 2, -1);
    var p = _start;
    var col = 0;
    while (p < _end && col < _columns) {
      final tag = _varint(p);
      p = _pos;
      if (tag >> 3 == _F.values && tag & 7 == 2) {
        final len = _varint(p);
        p = _pos;
        c[col * 2] = p;
        c[col * 2 + 1] = p + len;
        col++;
        p += len;
      } else {
        p = _skip(p, tag & 7);
      }
    }
    _cols = c;
    return c;
  }

  int _int(int col) {
    final c = _index();
    var p = c[col * 2];
    final end = c[col * 2 + 1];
    while (p < end) {
      final tag = _varint(p);
      p = _pos;
      if (tag >> 3 == _F.integer && tag & 7 == 0) {
        final n = _varint(p);
        return (n >>> 1) ^ -(n & 1);
      }
      p = _skip(p, tag & 7);
    }
    return 0;
  }

  String _string(int col) {
    final c = _index();
    var p = c[col * 2];
    final end = c[col * 2 + 1];
    while (p < end) {
      final tag = _varint(p);
      p = _pos;
      if (tag >> 3 == _F.text && tag & 7 == 2) {
        final len = _varint(p);
        return _utf8.convert(_b, _pos, _pos + len);
      }
      p = _skip(p, tag & 7);
    }
    return '';
  }

  int _pos = 0;

  int _varint(int p) {
    var b = _b[p++];
    if (b < 0x80) {
      _pos = p;
      return b;
    }
    var result = b & 0x7f;
    var shift = 7;
    while (true) {
      b = _b[p++];
      result |= (b & 0x7f) << shift;
      if (b < 0x80) {
        _pos = p;
        return result;
      }
      shift += 7;
    }
  }

  int _skip(int p, int wire) {
    switch (wire) {
      case 0:
        _varint(p);
        return _pos;
      case 1:
        return p + 8;
      case 2:
        final len = _varint(p);
        return _pos + len;
      case 5:
        return p + 4;
      default:
        throw StateError('wire type $wire');
    }
  }
}
