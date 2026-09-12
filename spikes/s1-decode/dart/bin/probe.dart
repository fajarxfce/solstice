// Why is Dart 3.7× slower than Kotlin on the same 266 KB?
//
// A number that just says FAIL is not yet a finding. Plan §4.1's fallback —
// columnar diff plus generated zero-copy accessors — is a large piece of work,
// and it is only the right answer if the cost is *materialising objects*. If
// the cost is something narrower, the fix is narrower.
//
// The hypothesis worth testing first: `package:protobuf` maps every 64-bit
// integer field to `Int64` from `package:fixnum`, because Dart compiled to
// JavaScript has no 64-bit integer. `Int64` is a class — three `int` fields on
// the heap — so a row with five integers allocates five objects that Java's
// `long` primitive does not. The fixture carries ~14,000 such integers.
//
// The experiment: decode the *same bytes* against a schema that differs only in
// `sint64` → `sint32`. Zigzag varints are identical for values that fit in 32
// bits, so this is wire-compatible for this fixture and changes exactly one
// thing — whether the generated accessor returns `Int64` or a native `int`.
//
// It is a diagnostic, not a proposal. `sint32` cannot ship: SQLite integers are
// 64-bit and a row id above 2^31 would silently truncate.

import 'dart:io';
import 'dart:typed_data';

import 'package:s1_decode/gen/solstice/v1/view.pb.dart' as wide;
import 'package:s1_decode/gen32/solstice/v1/view.pb.dart' as narrow;

const warmup = 200;
const reps = 300;

void main(List<String> args) {
  final dir = args.isEmpty ? '../fixtures' : args[0];
  final bytes = File('$dir/view-1000.bin').readAsBytesSync();

  // Same bytes, same 21,000 scalars, same object graph — one schema apart.
  final wideUs = _best(bytes, (b) => wide.ViewDelta.fromBuffer(b).changes.length);
  final narrowUs =
      _best(bytes, (b) => narrow.ViewDelta.fromBuffer(b).changes.length);

  print('# spike S1 — where Dart spends it');
  print('');
  print('| integer field | accessor type | decode (best) |');
  print('|---|---|---|');
  print('| `sint64` (what the schema says) | `Int64` (package:fixnum) | ${_us(wideUs)} |');
  print('| `sint32` (diagnostic only) | `int` (native) | ${_us(narrowUs)} |');
  print('');

  final saved = wideUs - narrowUs;
  print(
    'Int64 boxing accounts for ${_us(saved)} of ${_us(wideUs)} '
    '— ${(100 * saved / wideUs).toStringAsFixed(0)}% of the decode.',
  );
  print('');

  // Second hypothesis, and the one the plan's fallback is aimed at: the cost is
  // *materialising the object graph*. 1000 rows is 1000 `Added`, 1000
  // `ViewChange`, 4000 `Row`, 1000 `RowList` and 21,000 `Value` — about 28,000
  // `GeneratedMessage` instances, each with its own field set, for a list that
  // shows eight rows at a time.
  //
  // The floor: walk the same bytes, find every scalar, build nothing. This is
  // what a generated zero-copy accessor pays up front, deferring strings and
  // objects until a row is actually rendered. If the floor is small, the
  // fallback works; if the floor is already over budget, the encoding itself is
  // wrong and columnar would not save it either.
  // The walker is hand-written and was wrong once already, so its answer is
  // checked before its timing is believed: 1000 parents × 6 columns plus
  // 3000 children × 5 columns is 21,000 scalars, the same number the generated
  // Dart and Kotlin decoders report.
  final scalars = _scanScalars(bytes);
  if (scalars != 21000) {
    stderr.writeln('walker found $scalars scalars, expected 21000 — timing it '
        'would be timing a bug');
    exit(1);
  }
  final scanUs = _best(bytes, _scanScalars);

  // And the shape a fallback would actually take: index the top level only —
  // one offset per row — and leave every row unparsed until it is scrolled to.
  final indexUs = _best(bytes, _indexRows);

  print('| payload | work done | best |');
  print('|---|---|---|');
  print('| 266.4 KB | full decode (`package:protobuf`) | ${_us(wideUs)} |');
  print('| 266.4 KB | walk every scalar, materialise nothing | ${_us(scanUs)} |');
  print('| 266.4 KB | index 1000 row offsets, parse no row | ${_us(indexUs)} |');
  print('');
  print(
    'So ${_us(wideUs - scanUs)} of ${_us(wideUs)} '
    '(${(100 * (wideUs - scanUs) / wideUs).toStringAsFixed(0)}%) is building the '
    '~28,000 objects, not reading the bytes.',
  );

  // Same count from both, or the comparison was between two different things.
  final a = wide.ViewDelta.fromBuffer(bytes);
  final b = narrow.ViewDelta.fromBuffer(bytes);
  print(
    'cross-check: ${a.changes.length} == ${b.changes.length} changes, '
    'first id ${a.changes.first.added.row.values[0].integer} == '
    '${b.changes.first.added.row.values[0].integer}',
  );
}

int _best(Uint8List bytes, int Function(Uint8List) decode) {
  for (var i = 0; i < warmup; i++) {
    decode(bytes);
  }
  var best = 1 << 30;
  for (var i = 0; i < reps; i++) {
    final sw = Stopwatch()..start();
    final n = decode(bytes);
    sw.stop();
    if (n == 0) throw StateError('empty');
    if (sw.elapsedMicroseconds < best) best = sw.elapsedMicroseconds;
  }
  return best;
}

String _us(int us) =>
    us >= 1000 ? '${(us / 1000).toStringAsFixed(2)}ms' : '${us}µs';

// ---------------------------------------------------------------------------
// A protobuf walker that allocates nothing.
//
// Hand-written against `view.proto` — which is what codegen would emit for the
// columnar/zero-copy fallback, so this is a prototype of it rather than a
// strawman. It reads varints and lengths, descends the schema, and counts the
// scalars it passes. It does not decode strings, does not build rows, and does
// not keep anything.
// ---------------------------------------------------------------------------

int _pos = 0;

int _varint(Uint8List b) {
  var result = 0;
  var shift = 0;
  while (true) {
    final byte = b[_pos++];
    result |= (byte & 0x7f) << shift;
    if (byte < 0x80) return result;
    shift += 7;
  }
}

/// Counts scalar leaves in the whole frame.
int _scanScalars(Uint8List b) {
  _pos = 0;
  var scalars = 0;
  while (_pos < b.length) {
    final tag = _varint(b);
    final field = tag >> 3;
    final wire = tag & 7;
    if (field == 3 && wire == 2) {
      final end = _varint(b) + _pos;
      scalars += _scanChange(b, end); // ViewChange
    } else {
      _skip(b, wire);
    }
  }
  return scalars;
}

int _scanChange(Uint8List b, int end) {
  var scalars = 0;
  while (_pos < end) {
    final tag = _varint(b);
    if (tag >> 3 == 1 && tag & 7 == 2) {
      final inner = _varint(b) + _pos;
      scalars += _scanAdded(b, inner);
    } else {
      _skip(b, tag & 7);
    }
  }
  return scalars;
}

int _scanAdded(Uint8List b, int end) {
  var scalars = 0;
  while (_pos < end) {
    final tag = _varint(b);
    if (tag >> 3 == 2 && tag & 7 == 2) {
      final inner = _varint(b) + _pos;
      scalars += _scanRow(b, inner);
    } else {
      _skip(b, tag & 7);
    }
  }
  return scalars;
}

int _scanRow(Uint8List b, int end) {
  var scalars = 0;
  while (_pos < end) {
    final tag = _varint(b);
    if (tag >> 3 == 1 && tag & 7 == 2) {
      final inner = _varint(b) + _pos;
      scalars += _scanValue(b, inner);
    } else {
      _skip(b, tag & 7);
    }
  }
  return scalars;
}

int _scanValue(Uint8List b, int end) {
  // An empty body is NULL — still a scalar.
  if (_pos >= end) return 1;
  var scalars = 0;
  var leaf = true;
  while (_pos < end) {
    final tag = _varint(b);
    final field = tag >> 3;
    if (field == 5 && tag & 7 == 2) {
      leaf = false;
      final inner = _varint(b) + _pos;
      while (_pos < inner) {
        final t = _varint(b);
        if (t >> 3 == 1 && t & 7 == 2) {
          final row = _varint(b) + _pos;
          scalars += _scanRow(b, row);
        } else {
          _skip(b, t & 7);
        }
      }
    } else {
      _skip(b, tag & 7);
    }
  }
  return leaf ? 1 : scalars;
}

/// The fallback's actual shape: one offset per row, nothing parsed.
///
/// A `ListView.builder` asks for the rows it is about to paint, which on a phone
/// is eight of them. Decoding the other 992 up front is work done for a scroll
/// position the user has not reached and may never reach. This records where
/// each row starts and stops; `itemBuilder` would decode from there on demand.
///
/// Returns the number of rows indexed.
int _indexRows(Uint8List b) {
  _pos = 0;
  final offsets = Int32List(1024);
  var n = 0;
  while (_pos < b.length) {
    final tag = _varint(b);
    if (tag >> 3 == 3 && tag & 7 == 2) {
      final len = _varint(b);
      if (n < offsets.length) offsets[n] = _pos;
      n++;
      _pos += len;
    } else {
      _skip(b, tag & 7);
    }
  }
  return n;
}

void _skip(Uint8List b, int wire) {
  switch (wire) {
    case 0:
      _varint(b);
    case 1:
      _pos += 8;
    case 2:
      // Not `_pos += _varint(b)`: compound assignment reads `_pos` before
      // evaluating the right-hand side, so the length varint's own bytes would
      // be un-consumed and the next tag would be read from inside the payload.
      final len = _varint(b);
      _pos += len;
    case 5:
      _pos += 4;
    default:
      throw StateError('wire type $wire');
  }
}
