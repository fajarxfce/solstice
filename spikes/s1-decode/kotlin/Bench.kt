// Spike S1, Kotlin half. See `../dart/bin/bench.dart` for why this exists.
//
// Against protobuf-javalite, not the full runtime: an Android app ships lite,
// and full protobuf-java carries descriptor and reflection machinery that is
// both large and awkward under R8. Measuring the full runtime would be
// measuring a decoder no Compose app would ever run.
//
// # This is HotSpot, and Compose runs on ART
//
// A desktop JVM C2-compiles a hot loop far more aggressively than ART will, and
// ART's allocation and GC behaviour under a real app's heap pressure is
// different again. So a pass here is *necessary and not sufficient*, exactly as
// with the engine benchmark, and a failure here is already fatal.

import dev.solstice.proto.v1.Value
import dev.solstice.proto.v1.ViewDelta
import java.io.File
import kotlin.system.exitProcess

const val WARMUP = 2000
const val REPS = 500

fun main(args: Array<String>) {
    val dir = if (args.isEmpty()) "../fixtures" else args[0]
    val view = read("$dir/view-1000.bin")
    val delta = read("$dir/delta-5.bin")

    println("# spike S1 — Kotlin")
    println()
    println(
        "JVM ${System.getProperty("java.version")} " +
            "(${System.getProperty("java.vm.name")}) · " +
            "${System.getProperty("os.name")}",
    )
    println()
    println("| payload | bytes | rows | decode (best) | decode (p50) | budget | |")
    println("|---|---|---|---|---|---|---|")

    measure("initial view", view, 1000, 5_000_000L)
    measure("5-row delta", delta, 5, 100_000L)

    println()
    crossCheck(view)
    edgeCheck(read("$dir/edge.bin"))
}

/// The tie-breaker.
///
/// `edge.bin` carries values a real hydration never produces but a real row is
/// allowed to hold — including `i64::MIN` and `i64::MAX`, the only two values
/// whose zigzag encoding needs all ten varint bytes. Rust round-trips them and
/// the Dart accessor in `lazy_view.dart` agrees; `package:protobuf` reports 0
/// and -1. Two against one is not an argument, so this is the third opinion,
/// from a decoder written by neither party.
fun edgeCheck(bytes: ByteArray) {
    val rows = ViewDelta.parseFrom(bytes).changesList.map { it.added.row }
    val failures = buildList {
        if (rows.size != 4) add("expected 4 rows, got ${rows.size}")
        if (rows[0].getValues(2).kindCase != Value.KindCase.KIND_NOT_SET) {
            add("row 0 priority should be NULL (the unset oneof)")
        }
        if (rows[1].getValues(2).kindCase != Value.KindCase.INTEGER) {
            add("row 1 priority is Int(0) and must still carry its tag")
        }
        if (rows[2].getValues(2).integer != Long.MIN_VALUE) {
            add("row 2 priority: ${rows[2].getValues(2).integer} != Long.MIN_VALUE")
        }
        if (rows[2].getValues(5).integer != Long.MAX_VALUE) {
            add("row 2 updated_at: ${rows[2].getValues(5).integer} != Long.MAX_VALUE")
        }
        if (rows[3].getValues(4).text != "judul — panjang ünïcödé ✓") {
            add("row 3 title: multi-byte UTF-8 did not survive")
        }
    }
    if (failures.isNotEmpty()) {
        System.err.println("edge-check FAILED — ${failures.joinToString("; ")}")
        exitProcess(1)
    }
    println("edge-check: NULL, Int(0), i64 extremes and multi-byte UTF-8 all survive")
}

fun read(path: String): ByteArray {
    val f = File(path)
    if (!f.exists()) {
        System.err.println(
            "missing $path — run `cargo run --release -p solstice-bench --bin s1-fixture` first",
        )
        exitProcess(2)
    }
    return f.readBytes()
}

fun measure(label: String, bytes: ByteArray, rows: Int, budgetNanos: Long) {
    // Long enough for C2 to have made up its mind. Too short and this measures
    // the interpreter, which would be a number about nothing.
    repeat(WARMUP) { sink(ViewDelta.parseFrom(bytes)) }

    val samples = LongArray(REPS)
    for (i in 0 until REPS) {
        val t = System.nanoTime()
        val decoded = ViewDelta.parseFrom(bytes)
        samples[i] = System.nanoTime() - t
        sink(decoded)
    }
    samples.sort()

    val best = samples.first()
    val p50 = samples[samples.size / 2]
    val pass = best <= budgetNanos
    println(
        "| $label | ${fmtBytes(bytes.size)} | $rows | ${fmtNanos(best)} | ${fmtNanos(p50)} " +
            "| < ${fmtNanos(budgetNanos)} | ${if (pass) "PASS" else "FAIL"} |",
    )
}

/// The oracle half: generated Java, reading bytes a hand-written Rust encoder
/// produced, agreeing with the Dart run on every count.
///
/// It asserts rather than prints, for the same reason the Dart side does: the
/// fixture comes from a fixed seed, so every count below is a constant, and an
/// encoder drifting from the normative `.proto` should fail a build rather than
/// print a line nobody reads.
fun crossCheck(bytes: ByteArray) {
    val d = ViewDelta.parseFrom(bytes)
    var parents = 0
    var children = 0
    var scalars = 0
    var nulls = 0

    for (change in d.changesList) {
        if (!change.hasAdded()) continue
        parents++
        for (v in change.added.row.valuesList) {
            if (v.hasRows()) {
                for (child in v.rows.rowsList) {
                    children++
                    scalars += child.valuesCount
                }
            } else {
                scalars++
                if (v.kindCase == Value.KindCase.KIND_NOT_SET) nulls++
            }
        }
    }

    println(
        "cross-check: $parents parents · $children children · $scalars scalars " +
            "· $nulls nulls · sub ${d.subId} v${d.version}",
    )
    val first = d.changesList.first().added.row
    println("first row: id=${first.getValues(0).integer} title=\"${first.getValues(4).text}\"")

    // 1000 parents × 6 columns, 3000 children × 5 columns.
    val failures = buildList {
        if (parents != 1000) add("parents: $parents != 1000")
        if (children != 3000) add("children: $children != 3000")
        if (scalars != 21000) add("scalars: $scalars != 21000")
        if (d.subId != 1L) add("sub_id: ${d.subId} != 1")
        if (d.version != 1L) add("version: ${d.version} != 1")
        if (first.getValues(0).integer != 19L) add("first id: != 19")
    }
    if (failures.isNotEmpty()) {
        System.err.println("cross-check FAILED — ${failures.joinToString("; ")}")
        System.err.println(
            "the Rust encoder and the generated Java decoder disagree about the " +
                "wire format, or the fixture was regenerated with different parameters",
        )
        exitProcess(1)
    }
}

var blackhole = 0

fun sink(d: ViewDelta) {
    // Keeps the decode from being optimised into nothing.
    blackhole += d.changesCount
}

fun fmtNanos(ns: Long): String =
    when {
        ns >= 1_000_000 -> "%.2fms".format(ns / 1e6)
        ns >= 1_000 -> "%.0fµs".format(ns / 1e3)
        else -> "${ns}ns"
    }

fun fmtBytes(n: Int): String =
    if (n >= 1024) "%.1f KB".format(n / 1024.0) else "$n B"
