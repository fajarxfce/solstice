#!/usr/bin/env bash
#
# Everything the app needs that a generator writes, plus the Rust it loads.
#
# Three of the four generators from `spikes/s2-bridge/generate.sh` run again
# here, against the same inputs, writing into the app instead of the spike:
#
#   protoc                      → lib/gen           the payload types
#   flutter_rust_bridge_codegen → lib/src/rust      the bridge
#   cargo ndk                   → android/.../jniLibs/<abi>/libsolstice_ffi_dart.so
#
# The duplication is the byte ABI paying off rather than a smell: no generator
# here knows this application's schema, so a second consumer of the same engine
# costs a second run of the same commands and no new Rust. Everything written
# is gitignored and reproducible from this script.
#
# Requires: protoc (+ `dart pub global activate protoc_plugin`), cargo,
# `cargo install flutter_rust_bridge_codegen --version 2.14.0-beta.2`,
# `cargo install cargo-ndk`, and an NDK — see `spikes/s3-android/build.sh`,
# which this delegates the Android build to.

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"

export PATH="$PATH:$HOME/.pub-cache/bin:$HOME/.cargo/bin"

# `mobile` is the profile that ships: opt-level=z, panic=abort, strip. S3
# measured what that costs against `release` (p99 434µs → 584µs, hydration
# 3.51ms → 5.22ms) and the answer was "a third of the speed for a third of the
# size". The demo runs the shipping one by default, because the number plan
# §5.1 wants is the one a user would get.
PROFILE="${PROFILE:-mobile}"
ABIS="${ABIS:-arm64-v8a}"

need() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "error: $1 not found on PATH" >&2
    exit 1
  }
}
need protoc
need cargo

# --- protobuf ---
#
# The app encodes with these and decodes without them. Plan §4.1's fallback —
# "accessor zero-copy yang di-generate" — is `lib/src/view.dart`, and S1 is the
# reason it exists: `package:protobuf` spent 6.14ms materialising a 1000-row
# view against a 5ms budget, 93% of it building objects nobody had asked for.
# Writing a 40-byte mutation has no such problem, so the generated types stay
# for the outbound direction.
need protoc-gen-dart
rm -rf "$here/lib/gen"
mkdir -p "$here/lib/gen"
protoc --proto_path="$root/proto" --dart_out="$here/lib/gen" \
  solstice/v1/view.proto solstice/v1/query.proto solstice/v1/mutation.proto
echo "dart proto:   lib/gen/solstice/v1/"

# --- the bridge ---
#
# Driven by `flutter_rust_bridge.yaml` in this directory, which differs from the
# crate's only in where the Dart goes. Both configs write the same
# `crates/solstice-ffi-dart/src/frb_generated.rs`; the generator is
# deterministic, so running either leaves the workspace identical.
need flutter_rust_bridge_codegen
(cd "$here" && flutter_rust_bridge_codegen generate)
echo "dart bridge:  lib/src/rust/"

# freezed, because the bridge renders `SolsticeError` as a sealed class. It is
# a `build_runner` pass over generated code, so it has to follow the generator
# and cannot be folded into it.
(cd "$here" && flutter pub get >/dev/null &&
  dart run build_runner build --delete-conflicting-outputs >/dev/null)
echo "freezed:      lib/src/rust/api.freezed.dart"

# --- the library ---
#
# jniLibs rather than a Gradle task: the demo is not shipping a plugin, and
# plan §7's mitigation for the cross-target risk is prebuilt binaries published
# to Releases, not a Rust toolchain in every consumer's build. A directory the
# APK packager already looks in is the closest thing to that a demo can have.
jni="$here/android/app/src/main/jniLibs"
targets=()
for abi in $ABIS; do targets+=(-t "$abi"); done
cargo ndk "${targets[@]}" -P 24 -o "$jni" build \
  --profile "$PROFILE" -p solstice-ffi-dart --manifest-path "$root/Cargo.toml"
echo "libraries:    android/app/src/main/jniLibs/"
for so in "$jni"/*/libsolstice_ffi_dart.so; do
  [ -f "$so" ] || continue
  printf '  %-12s %s bytes\n' "$(basename "$(dirname "$so")")" "$(stat -c %s "$so")"
done

# --- the world ---
#
# Seeded on the host, not in the app, for plan §1.4's reason: there is exactly
# one writer, and it is the engine. See the crate docs on `demo-fixture`.
echo
echo "next: cargo run --release -p solstice-bench --bin demo-fixture"
echo "      ./push-fixture.sh"
