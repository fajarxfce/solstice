// The fallback plan §4.1 names, prototyped.
//
// Spike S1 measured `package:protobuf` decoding a 1000-row view in 6.14ms
// against a 5ms budget, and located the cost precisely: 93% of it is
// materialising ~28,000 `GeneratedMessage` objects, not reading bytes. The same
// payload can be walked in 422µs and indexed in 8µs.
//
// So the fix is not a different encoding. It is to stop building objects for
// rows nobody is looking at. A `ListView.builder` on a phone paints about eight
// tiles; the other 992 rows are work done for a scroll position the user has not
// reached and may never reach.
//
// This file is hand-written, and it is a prototype of *generated* code. The
// column indices below are constants because the query's shape is known when the
// code is generated — which is the property that makes the byte ABI worth
// having. The Rust FFI glue never changes; only this layer does, per query, per
// app, without anybody recompiling Rust.
//
// # What is lazy, and what is not
//
// Two levels:
//
//   1. **Rows.** Construction indexes the top-level `changes` and stops. No row
//      is looked at until it is subscripted.
//   2. **Columns.** A row indexes its own column offsets on first access, then
//      decodes individual fields on demand. A tile that shows a title does not
//      pay for the comment bodies underneath it.
//
// What is *not* lazy: strings. `utf8.decode` has to build a `String`, and Dart
// has no view type over a byte range that `Text()` would accept. That is the
// floor, and it is the right floor — you cannot render a string you have not
// decoded.

import 'dart:convert';
import 'dart:typed_data';

const _utf8 = Utf8Decoder();

/// Field numbers from `proto/solstice/v1/view.proto`.
///
/// Duplicated here rather than imported from the generated code on purpose:
/// depending on `package:protobuf` to avoid `package:protobuf` would defeat the
/// exercise, and these are the numbers the `.proto` freezes at 1.0 anyway.
abstract final class _F {
  // ViewDelta
  static const subId = 1;
  static const version = 2;
  static const changes = 3;
  // ViewChange
  static const added = 1;
  // Added
  static const row = 2;
  // Row / RowList
  static const values = 1;
  static const rowListRows = 1;
  // Value oneof
  static const integer = 1;
  static const real = 2;
  static const text = 3;
  static const blob = 4;
  static const rows = 5;
}

/// A view diff that has been indexed but not decoded.
///
/// Construction is the operation the M0 kill criterion measures: after it
/// returns, `length` is known and any row can be reached in constant time.
class LazyViewDelta {
  final Uint8List _b;

  /// Start and end of each top-level `ViewChange` *body*, parallel arrays.
  ///
  /// Two `Int32List`s rather than a list of objects, because a list of 1000
  /// two-field objects is the very thing this class exists to avoid.
  Int32List _start;
  Int32List _end;
  int _length = 0;

  int subId = 0;
  int version = 0;

  LazyViewDelta(this._b)
      : _start = Int32List(1024),
        _end = Int32List(1024) {
    _index();
  }

  int get length => _length;

  /// The row at [i], still undecoded.
  ///
  /// Descends `ViewChange → Added → Row` on each call. That is three tag reads
  /// over a handful of bytes; hoisting it into the index would make the index
  /// slower for rows that are never asked for, which is most of them.
  IssueRow operator [](int i) {
    if (i < 0 || i >= _length) throw RangeError.index(i, this, 'index');
    var p = _start[i];
    final changeEnd = _end[i];

    // ViewChange.added
    var rowStart = -1, rowEnd = -1;
    while (p < changeEnd) {
      final tag = _varint(p);
      p = _pos;
      final field = tag >> 3;
      final wire = tag & 7;
      if (field == _F.added && wire == 2) {
        final len = _varint(p);
        p = _pos;
        final addedEnd = p + len;
        // Added.row
        while (p < addedEnd) {
          final t = _varint(p);
          p = _pos;
          if (t >> 3 == _F.row && t & 7 == 2) {
            final l = _varint(p);
            p = _pos;
            rowStart = p;
            rowEnd = p + l;
            p = rowEnd;
          } else {
            p = _skip(p, t & 7);
          }
        }
        p = addedEnd;
      } else {
        p = _skip(p, wire);
      }
    }
    if (rowStart < 0) {
      throw StateError('change $i is not an Added — this view is not an '
          'initial hydration');
    }
    return IssueRow._(_b, rowStart, rowEnd);
  }

  void _index() {
    var p = 0;
    final n = _b.length;
    while (p < n) {
      final tag = _varint(p);
      p = _pos;
      final field = tag >> 3;
      final wire = tag & 7;
      if (field == _F.changes && wire == 2) {
        final len = _varint(p);
        p = _pos;
        if (_length == _start.length) _grow();
        _start[_length] = p;
        _end[_length] = p + len;
        _length++;
        p += len;
      } else if (field == _F.subId && wire == 0) {
        subId = _varint(p);
        p = _pos;
      } else if (field == _F.version && wire == 0) {
        version = _varint(p);
        p = _pos;
      } else {
        p = _skip(p, wire);
      }
    }
  }

  void _grow() {
    final start = Int32List(_start.length * 2)..setRange(0, _length, _start);
    final end = Int32List(_end.length * 2)..setRange(0, _length, _end);
    _start = start;
    _end = end;
  }

  // --- varint plumbing -----------------------------------------------------
  //
  // Dart has no multiple return and no out-parameter, and returning a record
  // from the hottest function in the file allocates. So these read from an
  // explicit position and leave the new one in `_pos`, which the caller picks
  // up immediately. Ugly, and measurably cheaper than the alternatives.

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

/// One joined issue, decoded field by field as something asks for it.
///
/// The column numbers match `solstice_bench::fixture`'s issue schema plus the
/// collection the join attaches — which is exactly what a code generator knows
/// from the query IR.
class IssueRow {
  static const _id = 0;
  static const _projectId = 1;
  static const _priority = 2;
  static const _closed = 3;
  static const _title = 4;
  static const _updatedAt = 5;
  static const _comments = 6;
  static const _columns = 7;

  final Uint8List _b;
  final int _start;
  final int _end;

  /// Start and end of each column's `Value` *body*, filled on first access.
  ///
  /// One allocation per row that is actually looked at. The eager decoder
  /// allocates 28 for the same row.
  Int32List? _cols;

  IssueRow._(this._b, this._start, this._end);

  int get id => _int(_id);
  int get projectId => _int(_projectId);
  int? get priority => _isNull(_priority) ? null : _int(_priority);
  bool get closed => _int(_closed) != 0;
  String get title => _string(_title);
  int get updatedAt => _int(_updatedAt);

  /// The nested collection the 1:N join attached. Indexed on access, not now.
  CommentList get comments {
    final c = _index();
    final lo = c[_comments * 2], hi = c[_comments * 2 + 1];
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
        // zigzag
        // `>>>`, not `>>`. Dart's `>>` is arithmetic, and a ten-byte varint
        // fills the sign bit: `zigzag(i64::MIN)` is `u64::MAX`, which Dart holds
        // as `-1`, and `-1 >> 1` is `-1` — so the whole value collapses to 0.
        // Caught by `edge.bin`, which exists for exactly this.
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

  /// Shared, because a parent with no children is common and there is nothing
  /// to distinguish one empty list from another.
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
    if (i < 0 || i >= _length) throw RangeError.index(i, this, 'index');
    final r = _rows!;
    return CommentRow._(_b!, r[i * 2], r[i * 2 + 1]);
  }

  /// Bounded by the query's `limit`, which is why a fixed 8 is enough here and
  /// why growth is a fallback rather than the common path. Plan §1.1 makes
  /// `limit` mandatory on 1:N traversal precisely so this array has a bound.
  void _index() {
    final b = _b;
    if (b == null) {
      _length = 0;
      return;
    }
    var r = Int32List(16);
    var n = 0;
    var p = _start;
    // ViewChange.added → Value.rows body is a RowList; walk its `rows`.
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
  static const _id = 0;
  static const _issueId = 1;
  static const _createdAt = 2;
  static const _author = 3;
  static const _body = 4;
  static const _columns = 5;

  final Uint8List _b;
  final int _start;
  final int _end;
  Int32List? _cols;

  CommentRow._(this._b, this._start, this._end);

  int get id => _int(_id);
  int get issueId => _int(_issueId);
  int get createdAt => _int(_createdAt);
  String get author => _string(_author);
  String get body => _string(_body);

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
        // `>>>`, not `>>`. Dart's `>>` is arithmetic, and a ten-byte varint
        // fills the sign bit: `zigzag(i64::MIN)` is `u64::MAX`, which Dart holds
        // as `-1`, and `-1 >> 1` is `-1` — so the whole value collapses to 0.
        // Caught by `edge.bin`, which exists for exactly this.
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
