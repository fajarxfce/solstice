#!/usr/bin/env bash
#
# Build and run the Dart decode benchmark.
#
# `dart compile exe`, not `dart run`. `dart run` is the JIT VM, and Flutter ships
# AOT on both Android and iOS, so the JIT number would be a number about a
# runtime the product never uses. They are not close enough to substitute: the
# JIT's p50 on this fixture is ~1.6× its own best, because the optimiser is still
# making up its mind while the benchmark is running.
#
# Requires: dart, and `./generate.sh` already run.

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
out="$here/build"

if [ ! -f "$here/lib/gen/solstice/v1/view.pb.dart" ]; then
  echo "error: generated Dart missing — run ../generate.sh first" >&2
  exit 1
fi

if [ ! -d "$here/.dart_tool" ]; then
  (cd "$here" && dart pub get)
fi

mkdir -p "$out"
target="${1:-bench}"
dart compile exe -o "$out/$target" "$here/bin/$target.dart" >/dev/null
exec "$out/$target" "$here/../fixtures"
