#!/usr/bin/env bash
#
# Put the seeded world on the phone.
#
# The destination is the app's own external files directory, which is what
# `getExternalStorageDirectory()` returns on Android. Two other places look
# obvious and are not:
#
#   /data/local/tmp    adb can write it, the app's UID cannot read it.
#   /sdcard/Download   readable, but scoped storage means the app needs
#                      MANAGE_EXTERNAL_STORAGE to open it — a permission
#                      dialog in a benchmark is one more thing to get wrong.
#
# The app-specific directory needs no permission at all and `adb push` reaches
# it, which is the whole reason the fixture lives there.
#
# Requires the app to be installed once, because the system creates that
# directory at install time.

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"

PKG="${PKG:-dev.solstice.flutter_issues}"
SRC="${SRC:-$root/examples/fixtures/issues.db}"
DEST="/sdcard/Android/data/$PKG/files"

command -v adb >/dev/null 2>&1 || {
  echo "error: adb not found on PATH" >&2
  exit 1
}
[ -f "$SRC" ] || {
  echo "error: $SRC missing — run:" >&2
  echo "  cargo run --release -p solstice-bench --bin demo-fixture" >&2
  exit 1
}

adb shell "[ -d $DEST ]" 2>/dev/null || {
  echo "error: $DEST does not exist — install the app once first:" >&2
  echo "  flutter run --release -d \$(adb get-serialno)" >&2
  exit 1
}

# The siblings first, and unconditionally. A `-wal` from a previous push
# describes transactions against a file that is about to be replaced, and
# SQLite will happily replay it over the new one — which is a corrupt database
# presented as a working one. `push-fixture.sh` overwrites the `.db` but would
# leave the `-wal` untouched, so it has to be deleted rather than overwritten.
adb shell "rm -f $DEST/issues.db-wal $DEST/issues.db-shm"
adb push "$SRC" "$DEST/issues.db"
adb shell "ls -l $DEST"
