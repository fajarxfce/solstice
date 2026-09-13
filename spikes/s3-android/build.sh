#!/usr/bin/env bash
#
# Cross-build both FFI adapters for the Android ABIs and print what they cost.
#
# This is spike S3 — plan §7's "ukuran binary: Rust + SQLite + protobuf per ABI"
# — and also the first half of the risk plan §7 names second: the cross-target
# build pipeline, which it says to settle in M0 rather than later, because it
# eats weeks and is not interesting.
#
# `mobile` is the profile that ships (see the root Cargo.toml): opt-level=z,
# panic=abort, strip=symbols. `--release` builds the same libraries at
# opt-level=3 so the size/speed trade has a number rather than a belief; the
# README records what that trade is worth on real hardware.
#
# NDK r28 or newer is wanted, not just tolerated: it aligns load segments to
# 16 KB by default, and Android 15 ships devices whose page size is 16 KB. A
# library aligned to 4 KB does not load there at all. The check at the end is
# cheap and the failure it catches is total, so it runs every time.

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"

PROFILE="${PROFILE:-mobile}"
# arm64 is every phone sold since ~2017; armv7 is what remains of the long tail.
# x86_64 is emulators, so it is opt-in rather than a third of every build.
TARGETS="${TARGETS:-arm64-v8a armeabi-v7a}"
# Plan §5.1 does not name a minSdk. 24 is Flutter's floor and AGP's default for
# new projects, so it is the number a consumer will actually be on.
PLATFORM="${PLATFORM:-24}"

# Dropping SQLite's full-text and R-tree modules. Off by default, measured by
# `--trim`, and the README explains why it is a lever and not a setting: plan
# §1.1 excludes full-text from the IVM graph, but §1.1 also keeps
# `queryOnce(rawSql)` as an escape hatch, and silently removing FTS from under
# that is a different promise than the one the plan makes.
TRIM_FLAGS="-USQLITE_ENABLE_FTS3 -USQLITE_ENABLE_FTS3_PARENTHESIS \
-USQLITE_ENABLE_FTS5 -USQLITE_ENABLE_RTREE -USQLITE_SOUNDEX \
-DSQLITE_OMIT_DEPRECATED -DSQLITE_DQS=0"

trim=0
for arg in "$@"; do
  case "$arg" in
  --trim) trim=1 ;;
  -h | --help)
    sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
    echo
    echo "usage: build.sh [--trim]"
    echo "  env: PROFILE=$PROFILE TARGETS='$TARGETS' PLATFORM=$PLATFORM"
    exit 0
    ;;
  *)
    echo "error: unknown argument $arg" >&2
    exit 2
    ;;
  esac
done

# Asking cargo, not the PATH. Cargo resolves its own subcommands out of its bin
# directory whether or not that directory is on the PATH, so `command -v
# cargo-ndk` reports a missing tool on a perfectly working install.
cargo ndk --version >/dev/null 2>&1 || {
  echo "error: cargo-ndk not found — cargo install cargo-ndk" >&2
  exit 1
}

# cargo-ndk finds the NDK itself, but its error when it cannot is a panic report
# with the whole environment in it. Resolving it here fails readably instead.
if [ -z "${ANDROID_NDK_HOME:-}" ]; then
  sdk="${ANDROID_SDK_ROOT:-${ANDROID_HOME:-$HOME/Android/Sdk}}"
  # Highest version present, and `sort -V` so 28 beats 9 rather than losing to it.
  newest="$(ls -1 "$sdk/ndk" 2>/dev/null | sort -V | tail -1 || true)"
  [ -n "$newest" ] || {
    echo "error: no NDK under $sdk/ndk — install one, or set ANDROID_NDK_HOME" >&2
    exit 1
  }
  export ANDROID_NDK_HOME="$sdk/ndk/$newest"
fi
echo "ndk:     $ANDROID_NDK_HOME"

llvm="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin"
readelf="$llvm/llvm-readelf"

declare -A TRIPLE=(
  [arm64-v8a]=aarch64-linux-android
  [armeabi-v7a]=armv7-linux-androideabi
  [x86_64]=x86_64-linux-android
  [x86]=i686-linux-android
)

targets=()
for abi in $TARGETS; do
  triple="${TRIPLE[$abi]:-}"
  [ -n "$triple" ] || {
    echo "error: unknown ABI $abi" >&2
    exit 2
  }
  rustup target list --installed | grep -qx "$triple" || {
    echo "error: rust target $triple missing — rustup target add $triple" >&2
    exit 1
  }
  targets+=(-t "$abi")
done

echo "profile: $PROFILE · abis: $TARGETS · minSdk: $PLATFORM"
[ "$trim" = 1 ] && echo "sqlite:  trimmed (no fts3/fts5/rtree/soundex)"
echo

if [ "$trim" = 1 ]; then
  # libsqlite3-sys appends LIBSQLITE3_FLAGS after its own hardcoded -D list, so
  # the only way to remove one of its defines is to -U it afterwards. Setting
  # -DSQLITE_ENABLE_FTS5=0 would not do it; the source tests `#ifdef`.
  export LIBSQLITE3_FLAGS="$TRIM_FLAGS"
fi

cargo ndk "${targets[@]}" -P "$PLATFORM" build \
  --profile "$PROFILE" \
  -p solstice-ffi-dart -p solstice-ffi-kotlin \
  --manifest-path "$root/Cargo.toml"

echo
echo "| abi | library | bytes | 16 KB aligned |"
echo "|---|---|---|---|"
for abi in $TARGETS; do
  triple="${TRIPLE[$abi]}"
  for name in dart kotlin; do
    so="$root/target/$triple/$PROFILE/libsolstice_ffi_$name.so"
    [ -f "$so" ] || continue
    size="$(stat -c %s "$so")"
    case "$abi" in
    arm64-v8a | x86_64)
      # Every PT_LOAD has to be aligned to at least 0x4000, or the loader on a
      # 16 KB-page device rejects the library outright. NDK r28 does this by
      # default; r27 needs `-Wl,-z,max-page-size=16384` passed explicitly, which
      # is why this is checked rather than assumed.
      bad="$("$readelf" -l "$so" | awk '$1=="LOAD" { if (strtonum($NF) < 16384) n++ } END { print n+0 }')"
      aligned=$([ "$bad" = 0 ] && echo "yes" || echo "**NO — $bad segment(s)**")
      ;;
    *)
      # 16 KB pages are a 64-bit-only concern: the 32-bit ABIs run on devices
      # whose page size is 4 KB and always will be. Checking them anyway reports
      # a failure that is not one, which is worse than not checking.
      aligned="n/a — 32-bit"
      ;;
    esac
    printf '| %s | `libsolstice_ffi_%s.so` | %s | %s |\n' "$abi" "$name" "$size" "$aligned"
  done
done
