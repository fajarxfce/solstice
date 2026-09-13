// Spike S2, Kotlin half: what does the FFI boundary cost?
//
// The twin of `dart/bin/bench.dart`, measuring the same four things through
// UniFFI instead of `flutter_rust_bridge`:
//
//   subId()     a u64 out, no payload      → the per-call floor
//   initial()   266.4 KB out, no work      → the per-byte cost, outbound
//   mutate()    42 B in, real work         → the direction S1 never measured
//   the sink    271 B out, unsolicited     → the callback path
//
// Every number has a Rust control from `s2-fixture` against the same database,
// and the difference between the two is the answer. The absolute figures are
// not, because they also contain an engine.
//
// The one structural difference from the Dart file is the callback. UniFFI hands
// Rust a foreign object and calls it **on the engine thread**, where FRB posts to
// the Dart isolate's event loop. Plan §4.1 predicted exactly this as the one
// thing outside the intersection of the two generators, and it is the one thing
// that came out different — so the Kotlin event number is measuring a different
// mechanism, not a slower version of the same one.
//
// Run it with `./run.sh` from this directory.

import dev.solstice.proto.v1.ColValue
import dev.solstice.proto.v1.Cmp
import dev.solstice.proto.v1.CmpOp
import dev.solstice.proto.v1.Expr
import dev.solstice.proto.v1.Mutation
import dev.solstice.proto.v1.Op
import dev.solstice.proto.v1.Order
import dev.solstice.proto.v1.Patch
import dev.solstice.proto.v1.Predicate
import dev.solstice.proto.v1.PredicateList
import dev.solstice.proto.v1.Query
import dev.solstice.proto.v1.Related
import dev.solstice.proto.v1.Update
import dev.solstice.proto.v1.Value
import dev.solstice.proto.v1.ViewDelta
import java.io.File
import java.util.concurrent.ArrayBlockingQueue
import java.util.concurrent.TimeUnit
import com.google.protobuf.ByteString
import uniffi.solstice_ffi_kotlin.Database
import uniffi.solstice_ffi_kotlin.EngineStats
import uniffi.solstice_ffi_kotlin.EventSink

/** Enough repetitions that HotSpot has finished deciding what this code is. */
const val WARMUP = 200
const val REPS = 500

lateinit var fixtures: String

fun main(args: Array<String>) {
    fixtures = if (args.isEmpty()) "../fixtures" else args[0]

    println("# spike S2 — Kotlin")
    println()
    println("JVM ${System.getProperty("java.version")} · ${System.getProperty("os.name")}")
    println()

    val dbPath = "$fixtures/s2.db"
    if (!File(dbPath).exists()) {
        System.err.println(
            "missing $dbPath — run `cargo run --release -p solstice-bench --bin s2-fixture` first"
        )
        kotlin.system.exitProcess(2)
    }

    // Built here, in Kotlin, from `protoc`-generated classes. Nothing in the
    // Rust library knows this application's schema, which is the byte ABI's
    // whole reason for existing and the thing S1 could not demonstrate.
    val ir1000 = buildQuery(project = 1, k = 1000, comments = 3)
    val ir50 = buildQuery(project = 1, k = 50, comments = 3)
    crossCheckIr(ir1000, "query-1000.bin")
    crossCheckIr(ir50, "query-50.bin")

    var t = System.nanoTime()
    val db = Database.open(dbPath)
    val opened = System.nanoTime() - t

    println("| what | kotlin | budget | |")
    println("|---|---|---|---|")
    row("open", opened)

    // --- outbound ---
    t = System.nanoTime()
    val sub = db.subscribe(ir1000)
    row("subscribe k=1000", System.nanoTime() - t)

    val view = sub.initial()
    row("initial() k=1000 — ${fmtBytes(view.size)}", best { sub.initial() })
    row("decode k=1000", best { ViewDelta.parseFrom(view) }, budget = 5_000_000L)

    // The floor. Same JNA machinery, same handle, no payload — so whatever
    // `initial()` costs above this is bytes, and whatever it shares with this is
    // the price of the call itself.
    row("subId() — no payload", best { sub.subId() })

    val sub50 = db.subscribe(ir50)
    val view50 = sub50.initial()
    row("initial() k=50 — ${fmtBytes(view50.size)}", best { sub50.initial() })

    // --- inbound ---
    //
    // Measured with no sink installed first, and that is not a throwaway row.
    // The engine pumps every graph and applies every view either way (see the
    // early return in `engine.rs::pump`); what it skips is encoding the
    // `ViewDelta` and handing it over. So the difference between this row and
    // the one below is the price of delivery, and UniFFI's way of delivering —
    // a synchronous upcall into the JVM, on the engine thread, inside the write
    // — is not `flutter_rust_bridge`'s.
    val key = firstId(view)
    var n = 0L
    row("mutate — no sink installed", best { db.mutate(buildMutation(key, ++n)) })

    // --- the callback ---
    //
    // One sink per database, not per subscription (plan §4.2): these deltas
    // carry two different `subId`s and the host demultiplexes on them.
    val deaf = db.stats().mutations
    val sink = Collector()
    db.setEventSink(sink)

    row("mutate — 1 update, 2 views live", best { db.mutate(buildMutation(key, ++n)) })

    // What a Compose collector waits for. `onEvent` runs **on the engine
    // thread**, so this is a hand-off into an `ArrayBlockingQueue` and back —
    // the same shape plan §4.3 prescribes (`trySend` into a `Channel`, never
    // work on the callback thread).
    row("mutate → delta at the sink", eventLatency(db, sink, key), budget = 16_000_000L)

    db.clearEventSink()
    println()
    crossCheck(view, db.stats(), sink.count, deaf)

    // Explicit, not left to the cleaner. These hold an engine thread and a
    // SQLite connection, and a benchmark that exits without closing them is a
    // benchmark that never exercises the teardown path a real app hits on every
    // navigation.
    sub.close()
    sub50.close()
    db.close()
}

/**
 * Plan §7's query, built the way an application would build it.
 *
 * `project` is a **parameter**, not a literal in the predicate. Plan §1.2 hashes
 * the IR twice — without params for the `PipelineId`, with them for the `ViewId`
 * — so N users running this query for N projects share one pipeline on the
 * server. Folding it in as a literal would encode a shape the real system never
 * runs, and would also not match `query-1000.bin`.
 */
fun buildQuery(project: Long, k: Int, comments: Int): ByteArray =
    Query.newBuilder()
        .setTable("issues")
        .setWhere(
            Predicate.newBuilder().setAnd(
                PredicateList.newBuilder()
                    .addPreds(
                        Predicate.newBuilder().setCmp(
                            Cmp.newBuilder()
                                .setLhs(Expr.newBuilder().setCol("project_id"))
                                .setOp(CmpOp.CMP_OP_EQ)
                                .setRhs(Expr.newBuilder().setParam(0))
                        )
                    )
                    .addPreds(
                        Predicate.newBuilder().setCmp(
                            Cmp.newBuilder()
                                .setLhs(Expr.newBuilder().setCol("closed"))
                                .setOp(CmpOp.CMP_OP_EQ)
                                .setRhs(Expr.newBuilder().setLit(Value.newBuilder().setInteger(0)))
                        )
                    )
            )
        )
        .addOrderBy(Order.newBuilder().setCol("priority").setDesc(true))
        .setLimit(k)
        .addRelated(
            Related.newBuilder()
                .setRelName("comments")
                .setAs("comments")
                .setSub(
                    Query.newBuilder()
                        .setTable("comments")
                        .addOrderBy(Order.newBuilder().setCol("created_at").setDesc(true))
                        .setLimit(comments)
                )
        )
        .addParams(Value.newBuilder().setInteger(project))
        .build()
        .toByteArray()

/**
 * One `updated_at` write to a row the window holds — see `s2-fixture`'s `touch`
 * for why it is that column and why the key comes out of the view.
 */
fun buildMutation(key: Long, n: Long): ByteArray =
    Mutation.newBuilder()
        .setClientId(ByteString.copyFrom(byteArrayOf(3)))
        .setMutationId(n)
        .setPatch(
            Patch.newBuilder().addOps(
                Op.newBuilder().setUpdate(
                    Update.newBuilder()
                        .setTable("issues")
                        .setKey(Value.newBuilder().setInteger(key))
                        .addSet(
                            ColValue.newBuilder()
                                .setCol("updated_at")
                                .setValue(Value.newBuilder().setInteger(n))
                        )
                )
            )
        )
        .build()
        .toByteArray()

/**
 * Asserts that the IR Kotlin built is the IR Rust would have built.
 *
 * Not a formality. A query naming `projectId` instead of `project_id` comes back
 * as a `SolsticeException.Compile` and announces itself; one that ordered
 * ascending, or set a field number off, would compile fine and quietly measure a
 * different query. Byte equality against the fixture is the only check that
 * catches the second kind — and it is the same check the Dart benchmark makes,
 * so all three encoders are pinned to one canonical encoding.
 */
fun crossCheckIr(built: ByteArray, name: String) {
    val expected = File("$fixtures/$name").readBytes()
    if (!built.contentEquals(expected)) {
        System.err.println(
            "the query Kotlin built is not the query Rust builds ($name): " +
                "${built.size} bytes vs ${expected.size}"
        )
        System.err.println(
            "canonical encoding matters here — field order is the .proto tag order, " +
                "and two encoders that disagree would also hash to two PipelineIds"
        )
        kotlin.system.exitProcess(1)
    }
}

/**
 * The same assertion S1 makes, against bytes that came through FFI this time.
 *
 * If these agree, the boundary is not merely fast — it is delivering exactly the
 * payload S1 decoded, so the two spikes' numbers are about the same thing.
 */
/**
 * `deaf` is the mutation count from before the sink was installed. Those writes
 * emitted nothing by design, so they are subtracted rather than allowed to
 * weaken the invariant below into "roughly two deltas per write".
 */
fun crossCheck(bytes: ByteArray, stats: EngineStats, events: Long, deaf: ULong) {
    val d = ViewDelta.parseFrom(bytes)
    var parents = 0
    var children = 0
    var scalars = 0
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
            }
        }
    }
    println(
        "cross-check: $parents parents · $children children · $scalars scalars · " +
            "${stats.subscriptions} subs · ${stats.mutations} mutations " +
            "($deaf before the sink) · ${stats.events} events · v${stats.version}"
    )

    val heard = stats.mutations - deaf
    val failures = buildList {
        if (parents != 1000) add("parents: $parents != 1000")
        if (children != 3000) add("children: $children != 3000")
        if (scalars != 21000) add("scalars: $scalars != 21000")
        if (stats.subscriptions != 2UL) add("subs: ${stats.subscriptions} != 2")
        // Two live views, so every write made with the sink installed emits two
        // deltas — and every write made without it emits none.
        if (stats.events != heard * 2UL) {
            add("events: ${stats.events} != 2 × $heard")
        }
        // What the sink saw and what the engine counted are two different
        // machines' opinions about the same events. They have to agree, or the
        // callback is dropping deltas — which plan §4.2 forbids outright.
        if (events != stats.events.toLong()) add("sink saw $events of ${stats.events}")
    }
    if (failures.isNotEmpty()) {
        System.err.println("cross-check FAILED — ${failures.joinToString("; ")}")
        kotlin.system.exitProcess(1)
    }
}

fun firstId(view: ByteArray): Long =
    ViewDelta.parseFrom(view).getChanges(0).added.row.getValues(0).integer

/**
 * The sink, written the way plan §4.3 says to write one: hand off and return.
 *
 * `onEvent` runs on the engine thread, so anything slow here stalls every other
 * subscription in the process. An `ArrayBlockingQueue` with `offer` is the
 * blocking-free equivalent of the `Channel.trySend` a Compose host would use.
 */
class Collector : EventSink {
    val queue = ArrayBlockingQueue<ByteArray>(1024)

    @Volatile
    var count = 0L

    @Volatile
    var listening = false

    override fun onEvent(event: ByteArray) {
        count++
        if (listening) queue.offer(event)
    }
}

/**
 * Write, then wait for the diff to come back through the sink.
 *
 * A round trip from one thread rather than a one-way latency, because there is
 * no shared clock to subtract across the boundary — a `ViewDelta` carries a
 * version, not a timestamp. It is an upper bound on the one-way cost and is what
 * a collector waits for anyway.
 */
fun eventLatency(db: Database, sink: Collector, key: Long): Long {
    sink.queue.clear()
    sink.listening = true
    var best = Long.MAX_VALUE
    for (i in 0 until 200) {
        val t = System.nanoTime()
        db.mutate(buildMutation(key, 1_000_000L + i))
        sink.queue.poll(1, TimeUnit.SECONDS) ?: error("no delta within a second")
        sink.queue.poll(1, TimeUnit.SECONDS) ?: error("only one of two deltas")
        val e = System.nanoTime() - t
        if (e < best) best = e
    }
    sink.listening = false
    return best
}

/**
 * Best-of, for the reason `s1-fixture` gives: the work is deterministic, so the
 * spread between repetitions is the machine's and the fastest run has least of
 * it in. Warmed first, because otherwise this measures the interpreter.
 */
inline fun best(f: () -> Any?): Long {
    for (i in 0 until WARMUP) sink(f())
    var best = Long.MAX_VALUE
    for (i in 0 until REPS) {
        val t = System.nanoTime()
        val r = f()
        val e = System.nanoTime() - t
        sink(r)
        if (e < best) best = e
    }
    return best
}

/** Touch the result, so the JIT cannot delete the work that produced it. */
fun sink(x: Any?) {
    if (x == null) throw IllegalStateException("null")
}

fun row(label: String, nanos: Long, budget: Long? = null) {
    val verdict = if (budget == null) "" else if (nanos <= budget) "PASS" else "FAIL"
    val b = if (budget == null) "—" else "< ${fmt(budget)}"
    println("| $label | ${fmt(nanos)} | $b | $verdict |")
}

fun fmt(nanos: Long): String = when {
    nanos >= 1_000_000 -> String.format("%.2fms", nanos / 1_000_000.0)
    nanos >= 1_000 -> "${nanos / 1000}µs"
    else -> "${nanos}ns"
}

fun fmtBytes(n: Int): String =
    if (n >= 1024) String.format("%.1f KB", n / 1024.0) else "$n B"
