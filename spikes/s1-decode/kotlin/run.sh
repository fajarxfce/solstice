#!/usr/bin/env bash
#
# Build and run the Kotlin decode benchmark.
#
# No Gradle. The whole thing is one Kotlin file plus generated Java, and a
# Gradle project would add an Android toolchain, a daemon and a wrapper
# distribution to a spike whose entire job is to print two numbers. When these
# numbers move into the Compose demo app at M0, that app brings its own build.
#
# `kotlinc` is usually not on PATH — it ships inside IntelliJ and Android
# Studio — so this looks for it in the usual places.

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
gen="$here/gen"
out="$here/build"
lib="$here/lib"

# javalite, because that is what an Android app ships.
PROTOBUF_VERSION="${PROTOBUF_VERSION:-4.36.1}"
jar="$lib/protobuf-javalite-$PROTOBUF_VERSION.jar"

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

if [ ! -f "$gen/dev/solstice/proto/v1/ViewDelta.java" ]; then
  echo "error: generated Java missing — run ../generate.sh first" >&2
  exit 1
fi

if [ ! -f "$jar" ]; then
  mkdir -p "$lib"
  echo "fetching protobuf-javalite $PROTOBUF_VERSION"
  curl -fsSL -o "$jar" \
    "https://repo1.maven.org/maven2/com/google/protobuf/protobuf-javalite/$PROTOBUF_VERSION/protobuf-javalite-$PROTOBUF_VERSION.jar"
fi

mkdir -p "$out"
echo "compiling generated java"
javac -nowarn -cp "$jar" -d "$out" "$gen"/dev/solstice/proto/v1/*.java

echo "compiling Bench.kt"
"$KOTLINC" -nowarn -cp "$jar:$out" -d "$out" "$here/Bench.kt" 2>&1 | grep -v '^warning:' || true

# The stdlib ships beside the compiler; `kotlinc` puts it on the compile
# classpath implicitly but `java` will not find it on its own.
stdlib="$(dirname "$(dirname "$KOTLINC")")/lib/kotlin-stdlib.jar"

exec java -cp "$jar:$stdlib:$out" BenchKt "${@:-$here/../fixtures}"
