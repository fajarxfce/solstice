#!/usr/bin/env bash
#
# Build and run the Dart bridge benchmark.
#
# `dart compile exe`, not `dart run`, for the reason S1 gives: `dart run` is the
# JIT VM and Flutter ships AOT on both Android and iOS, so a JIT number would be
# a number about a runtime the product never uses.
#
# It matters more here than it did in S1. `flutter_rust_bridge`'s synchronous
# path goes through `dart:ffi`, and AOT and JIT do not compile an FFI trampoline
# the same way — the per-call floor this benchmark is built to isolate is
# precisely the part where they differ most.
#
# Requires: dart, and `../generate.sh` already run.

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../../.." && pwd)"
out="$here/build"
lib="$root/target/release/libsolstice_ffi_dart.so"

if [ ! -f "$here/lib/src/rust/api.dart" ] || [ ! -f "$here/lib/gen/solstice/v1/query.pb.dart" ]; then
  echo "error: generated Dart missing — run ../generate.sh first" >&2
  exit 1
fi
if [ ! -f "$lib" ]; then
  echo "error: $lib missing — run ../generate.sh first" >&2
  exit 1
fi
if [ ! -f "$here/../fixtures/s2.db" ]; then
  echo "error: the seeded database is missing — run" >&2
  echo "  cargo run --release -p solstice-bench --bin s2-fixture" >&2
  exit 1
fi

if [ ! -d "$here/.dart_tool" ]; then
  (cd "$here" && dart pub get)
fi

mkdir -p "$out"
target="${1:-bench}"
dart compile exe -o "$out/$target" "$here/bin/$target.dart" >/dev/null
exec "$out/$target" "$here/../fixtures" "$lib"
