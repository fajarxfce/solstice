#!/usr/bin/env bash
#
# Everything generated for spike S2, in the order the pieces depend on each
# other. Four generators run here and none of them knows about the others:
#
#   protoc                      → the payload types, in Dart and in Java
#   flutter_rust_bridge_codegen → the Dart bridge, and `src/frb_generated.rs`
#   cargo build                 → the two cdylibs the hosts load
#   uniffi-bindgen              → the Kotlin bindings, read back out of the .so
#
# That protobuf and the bridge are generated *separately* is the whole shape of
# plan §4.1 made visible. The bridge carries `Vec<u8>` and has no idea what is
# in it; the protobuf types describe what is in it and have no idea how it
# travelled. Neither generator has to be re-run when the other's input changes,
# and — the point of the byte ABI — neither has to be re-run when an
# application's schema changes.
#
# Requires: protoc, dart (+ `dart pub global activate protoc_plugin`), cargo,
# and `cargo install flutter_rust_bridge_codegen --version 2.14.0-beta.2`.

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"
proto_dir="$root/proto"

# `protoc-gen-dart` and `flutter_rust_bridge_codegen` install into per-tool bin
# directories that a login shell does not necessarily have.
export PATH="$PATH:$HOME/.pub-cache/bin:$HOME/.cargo/bin"

need() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "error: $1 not found on PATH" >&2
    exit 1
  }
}
need protoc
need cargo

# The request half as well as the payload half. S1 needed only `view.proto`,
# because it decoded bytes someone else had produced; S2's hosts have to *build*
# a query IR and a mutation body, which is the direction across the boundary
# that S1 never exercised at all.
protos=(solstice/v1/view.proto solstice/v1/query.proto solstice/v1/mutation.proto)

# --- protobuf: Dart ---
if command -v protoc-gen-dart >/dev/null 2>&1; then
  rm -rf "$here/dart/lib/gen"
  mkdir -p "$here/dart/lib/gen"
  protoc --proto_path="$proto_dir" --dart_out="$here/dart/lib/gen" "${protos[@]}"
  echo "dart proto:   $here/dart/lib/gen/solstice/v1/"
else
  echo "skip dart proto: protoc-gen-dart not found (dart pub global activate protoc_plugin)" >&2
fi

# --- protobuf: Java lite ---
#
# Lite for the reason S1 gives: Android ships javalite, and the full runtime's
# descriptor and reflection machinery would measure a decoder no Compose app
# would ever run.
rm -rf "$here/kotlin/gen/dev"
mkdir -p "$here/kotlin/gen"
protoc --proto_path="$proto_dir" --java_out=lite:"$here/kotlin/gen" "${protos[@]}"
echo "java proto:   $here/kotlin/gen/dev/solstice/proto/v1/"

# --- the Dart bridge ---
#
# Writes two things from one parse: `crates/solstice-ffi-dart/src/frb_generated.rs`
# beside the hand-written api module, and the Dart half under `dart/lib/src/rust`.
# The two agree on a protocol and a checksum, so they are generated together and
# a version skew between them is a runtime failure rather than a compile error.
if command -v flutter_rust_bridge_codegen >/dev/null 2>&1; then
  (cd "$root/crates/solstice-ffi-dart" && flutter_rust_bridge_codegen generate)
  echo "dart bridge:  $here/dart/lib/src/rust/"
else
  echo "skip dart bridge: flutter_rust_bridge_codegen not found" >&2
  echo "  cargo install flutter_rust_bridge_codegen --version 2.14.0-beta.2" >&2
fi

# --- the libraries ---
#
# Release, and not for the usual reason. A debug cdylib would make the boundary
# look cheap relative to an engine compiled without optimisation, and S2's whole
# output is the ratio between those two.
cargo build --release --manifest-path "$root/Cargo.toml" \
  -p solstice-ffi-dart -p solstice-ffi-kotlin
echo "libraries:    $root/target/release/"

# --- the Kotlin bindings ---
#
# Read out of the built `.so` rather than out of the source: UniFFI writes its
# metadata into the library, so the bindings are generated from the artefact
# that will actually be loaded. The bindgen binary lives *inside*
# `solstice-ffi-kotlin` so that the generator and the scaffolding are one
# version — see that crate's Cargo.toml.
rm -rf "$here/kotlin/gen/uniffi"
cargo run --release -q --manifest-path "$root/Cargo.toml" \
  -p solstice-ffi-kotlin --bin uniffi-bindgen -- \
  generate --library "$root/target/release/libsolstice_ffi_kotlin.so" \
  --language kotlin --no-format --out-dir "$here/kotlin/gen"
echo "kotlin bind:  $here/kotlin/gen/uniffi/solstice_ffi_kotlin/"
