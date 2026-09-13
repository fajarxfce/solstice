#!/usr/bin/env bash
#
# Build and run the Kotlin bridge benchmark.
#
# No Gradle, for the reason S1 gives: one Kotlin file, generated Java and
# generated Kotlin do not need an Android toolchain, a daemon and a wrapper
# distribution to print eight numbers. The Compose demo app at M0 brings its own
# build.
#
# Two jars are fetched rather than vendored:
#
#   protobuf-javalite  what an Android app ships — the full runtime's descriptor
#                      and reflection machinery would measure a decoder no
#                      Compose app would ever run.
#   jna                what UniFFI's generated Kotlin binds through on the JVM.
#                      Android uses the same generated code over JNA's Android
#                      build, so this is the real mechanism, not a stand-in.
#
# `kotlinc` is usually not on PATH — it ships inside IntelliJ and Android Studio
# — so this looks for it in the usual places.

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../../.." && pwd)"
gen="$here/gen"
out="$here/build"
lib="$here/lib"

PROTOBUF_VERSION="${PROTOBUF_VERSION:-4.36.1}"
JNA_VERSION="${JNA_VERSION:-5.18.1}"
pb_jar="$lib/protobuf-javalite-$PROTOBUF_VERSION.jar"
jna_jar="$lib/jna-$JNA_VERSION.jar"

find_kotlinc() {
  if command -v kotlinc >/dev/null 2>&1; then
    command -v kotlinc
    return
  fi
  for candidate in \
    /opt/android-studio*/plugins/Kotlin/kotlinc/bin/kotlinc \
    /opt/intellij-idea*/plugins/Kotlin/kotlinc/bin/kotlinc \
    "$HOME"/.local/share/JetBrains/Toolbox/apps/*/plugins/Kotlin/kotlinc/bin/kotlinc; do
    [ -x "$candidate" ] && {
      echo "$candidate"
      return
    }
  done
  echo "error: kotlinc not found — install it or set KOTLINC" >&2
  exit 1
}

KOTLINC="${KOTLINC:-$(find_kotlinc)}"

if [ ! -f "$gen/dev/solstice/proto/v1/ViewDelta.java" ] ||
  [ ! -f "$gen/uniffi/solstice_ffi_kotlin/solstice_ffi_kotlin.kt" ]; then
  echo "error: generated code missing — run ../generate.sh first" >&2
  exit 1
fi
if [ ! -f "$root/target/release/libsolstice_ffi_kotlin.so" ]; then
  echo "error: libsolstice_ffi_kotlin.so missing — run ../generate.sh first" >&2
  exit 1
fi
if [ ! -f "$here/../fixtures/s2.db" ]; then
  echo "error: the seeded database is missing — run" >&2
  echo "  cargo run --release -p solstice-bench --bin s2-fixture" >&2
  exit 1
fi

mkdir -p "$lib"
fetch() {
  [ -f "$2" ] && return
  echo "fetching $(basename "$2")"
  curl -fsSL -o "$2" "$1"
}
fetch "https://repo1.maven.org/maven2/com/google/protobuf/protobuf-javalite/$PROTOBUF_VERSION/protobuf-javalite-$PROTOBUF_VERSION.jar" "$pb_jar"
fetch "https://repo1.maven.org/maven2/net/java/dev/jna/jna/$JNA_VERSION/jna-$JNA_VERSION.jar" "$jna_jar"

cp="$pb_jar:$jna_jar"

mkdir -p "$out"
echo "compiling generated java"
javac -nowarn -cp "$cp" -d "$out" "$gen"/dev/solstice/proto/v1/*.java

echo "compiling bindings and Bench.kt"
"$KOTLINC" -nowarn -cp "$cp:$out" -d "$out" \
  "$gen/uniffi/solstice_ffi_kotlin/solstice_ffi_kotlin.kt" "$here/Bench.kt" \
  2>&1 | grep -v '^warning:' || true

# The stdlib ships beside the compiler; `kotlinc` puts it on the compile
# classpath implicitly but `java` will not find it on its own.
stdlib="$(dirname "$(dirname "$KOTLINC")")/lib/kotlin-stdlib.jar"

# `jna.library.path` is how the generated `Native.register` finds the cdylib.
exec java -Djna.library.path="$root/target/release" \
  -cp "$cp:$stdlib:$out" BenchKt "${@:-$here/../fixtures}"
