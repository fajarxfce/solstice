#!/usr/bin/env bash
#
# Run the M0 kill criteria on a physical Android device.
#
# Plan §9 says the M0 gate is "`cargo bench` + aplikasi benchmark di HP Android
# fisik" and that it is the project's go/no-go. Two demo apps are a milestone's
# worth of work; the engine half of that table is not, because `solstice-bench`
# is a plain binary with no Android dependencies at all. It cross-compiles, it
# runs from `/data/local/tmp`, and it prints the same markdown it prints on a
# laptop — so the two columns can be put side by side.
#
# What this does not measure is the half that needs a UI: delta → *committed
# frame*, and visible jank. Those wait for the demo apps. What it does measure
# is everything the engine controls, on the hardware the budget was written for,
# and the difference between those two machines turned out not to be a scale
# factor — see the README.

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"

PROFILE="${PROFILE:-release}"
ABI="${ABI:-arm64-v8a}"
PLATFORM="${PLATFORM:-24}"
# Writable and executable without root, on every Android version that matters.
REMOTE="${REMOTE:-/data/local/tmp}"

# `type -P` rather than `command -v`, which reports a shell function by name and
# would have resolved this to itself. The SDK path is the fallback because adb
# is frequently installed and almost as frequently not on the PATH.
ADB="${ADB:-$(type -P adb || true)}"
ADB="${ADB:-${ANDROID_SDK_ROOT:-${ANDROID_HOME:-$HOME/Android/Sdk}}/platform-tools/adb}"
[ -x "$ADB" ] || {
  echo "error: adb not found at $ADB — set ADB or install platform-tools" >&2
  exit 1
}
adb() { "$ADB" "$@"; }

case "$ABI" in
arm64-v8a) triple=aarch64-linux-android ;;
armeabi-v7a) triple=armv7-linux-androideabi ;;
x86_64) triple=x86_64-linux-android ;;
*)
  echo "error: unknown ABI $ABI" >&2
  exit 2
  ;;
esac

# See build.sh: cargo finds its subcommands whether or not ~/.cargo/bin is on
# the PATH, so this asks cargo rather than the PATH.
cargo ndk --version >/dev/null 2>&1 || {
  echo "error: cargo-ndk not found — cargo install cargo-ndk" >&2
  exit 1
}
if [ -z "${ANDROID_NDK_HOME:-}" ]; then
  sdk="${ANDROID_SDK_ROOT:-${ANDROID_HOME:-$HOME/Android/Sdk}}"
  newest="$(ls -1 "$sdk/ndk" 2>/dev/null | sort -V | tail -1 || true)"
  [ -n "$newest" ] || {
    echo "error: no NDK under $sdk/ndk — install one, or set ANDROID_NDK_HOME" >&2
    exit 1
  }
  export ANDROID_NDK_HOME="$sdk/ndk/$newest"
fi

devices="$(adb devices | awk 'NR>1 && $2=="device" { print $1 }')"
count="$(echo "$devices" | grep -c . || true)"
[ "$count" = 1 ] || {
  echo "error: need exactly one device, found $count" >&2
  echo "$devices" >&2
  exit 1
}

# The device this ran on is worth printing next to its numbers. A "mid-range
# Android phone" is not a specification, and a table of budgets met on unnamed
# hardware is not a measurement.
model="$(adb shell getprop ro.product.model | tr -d '\r')"
soc="$(adb shell getprop ro.soc.model | tr -d '\r')"
release="$(adb shell getprop ro.build.version.release | tr -d '\r')"
echo "device:  $model · $soc · Android $release · $ABI"
echo "profile: $PROFILE"
echo

cargo ndk -t "$ABI" -P "$PLATFORM" build \
  --profile "$PROFILE" -p solstice-bench \
  --manifest-path "$root/Cargo.toml"

bin="$root/target/$triple/$PROFILE/solstice-bench"
[ -f "$bin" ] || {
  echo "error: $bin not built" >&2
  exit 1
}

remote_bin="$REMOTE/solstice-bench"
adb push "$bin" "$remote_bin" >/dev/null
adb shell chmod 755 "$remote_bin"

# `--db` is passed explicitly because the harness would otherwise fall back to
# `std::env::temp_dir()`, which on Android is `/tmp` — a path that does not
# exist. An in-memory run would hide the finding this spike exists to report.
db="$REMOTE/solstice-m0.db"
adb shell "rm -f $db $db-wal $db-shm"
adb shell "cd $REMOTE && ./solstice-bench --db $db $*" | tr -d '\r'
adb shell "rm -f $db $db-wal $db-shm"
