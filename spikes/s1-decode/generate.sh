#!/usr/bin/env bash
#
# Generate the Dart and Kotlin decoders from `proto/solstice/v1/view.proto`.
#
# Generated, not hand-written, and that is the point. The Rust encoder in
# `solstice-proto` is hand-written (see its `wire` module for why), so checking
# it against a decoder from the same hand would prove only that the hand is
# consistent. These two come from `protoc` reading the normative `.proto`, which
# makes them independent oracles: if the encoder drifts from the spec, code
# generated in two other languages stops parsing its bytes.
#
# Requires: protoc, dart (with `dart pub global activate protoc_plugin`), javac.

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"
proto_dir="$root/proto"
proto="solstice/v1/view.proto"

export PATH="$PATH:$HOME/.pub-cache/bin"

need() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "error: $1 not found on PATH" >&2
    exit 1
  }
}
need protoc

# --- Dart ---
if command -v protoc-gen-dart >/dev/null 2>&1; then
  mkdir -p "$here/dart/lib/gen"
  protoc --proto_path="$proto_dir" --dart_out="$here/dart/lib/gen" "$proto"
  echo "dart:   $here/dart/lib/gen/solstice/v1/"

  # A second Dart decoder from a schema that differs in exactly one token:
  # `sint64 integer` becomes `sint32 integer`. Zigzag varints agree for anything
  # that fits in 32 bits, so it reads the same fixture bytes, and the generated
  # accessor returns a native `int` instead of a `package:fixnum` `Int64`.
  #
  # It exists to answer "is boxing what makes Dart slow?" with a measurement
  # rather than a guess — see `dart/bin/probe.dart`. It is a diagnostic and
  # cannot ship: SQLite integers are 64-bit and a row id past 2^31 would
  # silently truncate.
  narrow="$(mktemp -d)"
  mkdir -p "$narrow/solstice/v1"
  sed 's/sint64 integer = 1;/sint32 integer = 1;/' \
    "$proto_dir/$proto" >"$narrow/$proto"
  mkdir -p "$here/dart/lib/gen32"
  protoc --proto_path="$narrow" --dart_out="$here/dart/lib/gen32" "$proto"
  rm -rf "$narrow"
  echo "dart:   $here/dart/lib/gen32/solstice/v1/  (sint32 diagnostic)"
else
  echo "skip dart: protoc-gen-dart not found (dart pub global activate protoc_plugin)" >&2
fi

# --- Kotlin ---
#
# `--java_out=lite` rather than the full runtime. Android apps ship javalite —
# full protobuf-java carries a reflection and descriptor machinery that is both
# large and unwelcome under R8 — so the full runtime would measure a decoder no
# Compose app would ever run.
mkdir -p "$here/kotlin/gen"
protoc --proto_path="$proto_dir" --java_out=lite:"$here/kotlin/gen" "$proto"
# Java lands under `java_package`, not under the proto path.
echo "kotlin: $here/kotlin/gen/dev/solstice/proto/v1/"
