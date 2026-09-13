# Spike S2 — what does the boundary itself cost?

[S1](../s1-decode/) measured what it costs a host to *decode* a view diff. It
ended on an admission:

> **Nothing has crossed FFI.** This measures decode, not the round trip. The copy
> `flutter_rust_bridge` and UniFFI make at the boundary is unmeasured, and it is
> the next thing to build.

S2 builds it. Plan §4.1 claims one Rust core can sit behind a byte ABI and serve
**both** generators without either of them bending it; plan §5.1 calls that half
of the combined assumption M0 exists to kill. Here it is wired up for real: two
adapter crates, two generated bindings, two host benchmarks, one engine.

The thing to read for is not the round-trip number. It is **which of the four
costs at a boundary is the expensive one**, because that is what decides where
the next piece of work goes.

## Result

Rust is the control — the same engine, called in-process, so a host's column
minus Rust's column is what the binding charges.

| | rust | dart | kotlin | budget |
|---|---|---|---|---|
| `open` | — | 1.60ms | 91.47ms<sup>†</sup> | — |
| `subscribe` k=1000 (hydration included) | 35.26ms | 33.41ms | 34.88ms | — |
| `initial()` k=1000 — **266.4 KB** | 7µs | **532µs** | **194µs** | — |
| decode k=1000 | 1.07ms | 6.29ms **FAIL** | 1.64ms PASS | < 5ms |
| `subId()` — no payload at all | — | **<1µs** | **7µs** | — |
| `initial()` k=50 — 13.3 KB | 137ns | 17µs | 21µs | — |
| `mutate` — no sink installed | 385µs | 388µs | 422µs | — |
| `mutate` — 1 update, 2 views live | 386µs | 411µs | **516µs** | — |
| `mutate` → delta arrives at the host | — | 409µs PASS | 524µs PASS | < 16ms |

Best-of-N after warm-up, on the laptop in [BENCHMARKS.md](../../BENCHMARKS.md).
Dart is AOT-compiled, which is what Flutter ships; Kotlin is HotSpot, which is
not what Compose ships.

<sub>† `open` is timed once, and on the JVM it is mostly one-time cost that has
nothing to do with this library: class loading, and JNA extracting and `dlopen`ing
the shared object. It is in the table because a reader would otherwise wonder
where it went, not because 91ms is a number to design around.</sub>

**The byte ABI holds.** Both hosts build the query IR themselves, out of
`protoc`-generated classes, and hand the engine bytes. Nothing in either Rust
adapter knows this application has a table called `issues` — that is the whole
claim of plan §4.1, and it is the part S1 could not show because S1 never called
into Rust.

## Four costs, measured apart

A round-trip number would have hidden the answer. There are four separable things
a boundary charges for, so the benchmarks charge for them separately:

**`subId()` is the per-call floor.** Same handle, same machinery, no payload —
a `u64` in, a `u64` out. Whatever `initial()` costs above this is bytes.

**`initial()` at two sizes is the per-byte cost.** 266.4 KB and 13.3 KB, the same
call on the same kind of object.

**`mutate()` is the inbound direction**, which S1 never touched at all.

**The sink is the callback path**, and it is the one place plan §4.1 predicted
the two generators would not agree.

## Finding 1 — the boundary charges per byte, not per call

Dart's per-call floor is **under a microsecond**. Its 266.4 KB `initial()` is
**532µs**. The call is free; the payload is not.

| | bytes moved | time | effective |
|---|---|---|---|
| Rust — `Vec<u8>` clone | 266.4 KB | 7µs | ~38 GB/s |
| Kotlin — UniFFI `RustBuffer` → `ByteArray` | 266.4 KB | 194µs | ~1.4 GB/s |
| Dart — FRB SSE codec → `Uint8List` | 266.4 KB | 532µs | ~0.5 GB/s |

Neither binding moves bytes at memory speed, and neither is doing anything
exotic: both make two copies (Rust-side into a transfer buffer, host-side out of
it) plus one host allocation. Dart's is the slower of the two by 2.7×; its `sync`
path runs the value through the SSE codec, which length-prefixes and re-copies
where UniFFI hands over a pointer and a length.

This is worth stating plainly because it points the opposite way from intuition.
**An ABI of few large calls is the right shape here, and a chatty one would be
wrong** — which is what the byte ABI already is, and now for a measured reason
rather than an assumed one. The corollary also holds: shrinking the payload buys
more than removing calls ever could.

<sub>Dart's per-byte cost is mildly superlinear: 20× the bytes costs 31× the
time, 782 MB/s at 13.3 KB against 501 MB/s at 266.4 KB. Kotlin's k=50 figure is
mostly its 7µs call floor, and what is left over is ~950 MB/s against 1.4 GB/s at
the larger size — the same direction, less of it. Either way the k=50 row is the
one a real list query produces, and it is 17–21µs.</sub>

## Finding 2 — once S1's accessor lands, the copy *is* the cost

S1 found Dart's eager protobuf decode is over the 5ms budget — 6.14ms there,
6.29ms here, same bytes and same decoder — and that a generated zero-copy
accessor does the same job in **8.30µs**. Put S1 and S2
together and the first frame after a `subscribe` looks like this:

| Dart, k=1000 | today | with S1's accessor |
|---|---|---|
| boundary copy | 532µs | 532µs |
| decode | 6.29ms | 8µs |
| **total** | **6.82ms — over budget** | **540µs — 9× under** |

So the fallback plan §4.1 named works, with room. But note what it does to the
shape of the problem: **the 532µs copy goes from 8% of the cost to 98% of it.**
After S1's fix, the only remaining thing between the engine and the first frame
is `flutter_rust_bridge` copying a buffer — code this project does not own.

That is not a problem at 540µs. It is the thing to look at first if the phone
gate in plan §9 says otherwise, and it has an obvious shape: an accessor that
reads out of a buffer does not need that buffer to be a Dart-heap `Uint8List`, so
the copy is removable in principle rather than intrinsic.

Kotlin needs none of this. 194µs + 1.64ms = **1.8ms**, inside budget as it
stands, on HotSpot.

## Finding 3 — the callback is where the generators actually differ

Plan §4.1 predicted the callback would be the one thing outside the intersection
of the two generators. It is, and the difference is not only in spelling.

`mutate` measured twice — once with no sink installed, once with — isolates it.
`Engine::pump` maintains every graph and applies every view either way; a sink
adds encoding the `ViewDelta` (272 B) and handing it over.

| `mutate`, 2 deltas per write | no sink | with sink | cost of delivery |
|---|---|---|---|
| Rust — virtual call into a counter | 385–394µs | 386–403µs | in the noise |
| Dart — FRB `StreamSink`, posts to the isolate's port | 388–414µs | 411–420µs | in the noise |
| Kotlin — UniFFI foreign trait, upcall into the JVM via JNA | 421–422µs | 515–516µs | **+94µs, both runs** |

Ranges are two full runs, which is the honest way to report this: on a ~400µs
write, Rust's and Dart's sinks cost less than the run-to-run spread of the
measurement, and Kotlin's costs **+94µs to the microsecond, twice**. That is 47µs
per delta, and it is not a payload cost — the delta is 272 bytes.

**UniFFI's callback is a synchronous upcall and it happens on the engine thread,
inside `mutate`.** FRB's `StreamSink.add` is a port post: it hands the bytes to
the isolate's event loop and returns, which is why Dart's write path cannot tell
whether a sink is installed and Kotlin's gets 22% slower.

Two consequences, both actionable now rather than at M4:

1. **Plan §4.3's rule for Compose is not advice, it is load-bearing.** "`trySend`
   into a `Channel` and get out" is already the shape `Collector` uses here, and
   even doing nothing but that costs 47µs a delta. Anything a host does in
   `onEvent` is added directly to every writer's latency, for every subscription
   on the device.
2. **Batching belongs on the Kotlin side first.** Plan §4.2 gives one stream per
   `Database` precisely so a pump's deltas can be coalesced; this measurement
   turns that from a convenience into the fix. One upcall carrying N deltas costs
   what one upcall costs — the 47µs is the crossing, not the payload — so a pump
   that touches 10 views should cross once.

At M0's demo load (200 writes/sec, 2 live views) the current cost is 19ms/sec of
engine thread on Kotlin: real but not alarming. It scales with
writes × live views, which is exactly the axis a busy screen moves along.

## The `subId()` floor, and why Kotlin's is 7µs

Sub-microsecond on Dart, 7–8µs on Kotlin, for the same do-nothing call. Reading
the generated `Subscription.subId()` accounts for it. One call is:

1. a compare-and-set loop on an `AtomicLong` call counter, to keep the object
   alive for the duration;
2. `uniffiCloneHandle()` — which is itself a **native call**, into
   `uniffi_..._fn_clone_subscription`, allocating a `UniffiRustCallStatus` JNA
   `Structure` as its out-parameter;
3. the actual native call, allocating a second `UniffiRustCallStatus`;
4. a decrement, and a cleaner check.

Two crossings and two JNA structures per method, because UniFFI keeps the object
alive across the call by cloning the `Arc` through FFI first. That is a
defensible safety design and it is invisible next to a 194µs payload copy. It
would not be invisible behind a per-field accessor API, which is another reason
the ABI is shaped the way it is.

## Two generator asymmetries, found by compiling the output

Both of these are the kind of thing that only shows up when the generated code is
actually built, which is the argument for a spike that runs `kotlinc` rather than
one that stops at "the bindings generated".

**UniFFI 0.32 cannot express an error field named `message`.** It emits each
variant as a class extending `kotlin.Exception` with the field as a constructor
property *and* an `override val message` formatting it:

```
solstice_ffi_kotlin.kt:2435:59: error: overload resolution ambiguity between candidates:
val message: String
val message: String
```

The Rust compiles, `cargo test` passes, the bindings generate, and the failure
arrives in the Kotlin compiler. Both adapters now spell the field `detail`.

**`flutter_rust_bridge` needs a second Dart codegen pass that UniFFI does not.**
A Rust enum with fields — `SolsticeError` — is rendered as a `freezed` sealed
class, so `freezed`, `freezed_annotation` and `build_runner` are dependencies of
any app using the Dart binding, and `build_runner` is part of its build. UniFFI
renders the same enum with nothing extra. Neither is a defect; they are a real
difference in what the two ecosystems ask of a consumer, and the Flutter one asks
more.

## Binary size — an early read on S3

Plan §5.1 budgets **< 8MB of APK growth per ABI**, and plan §7 lists cross-target
builds as the second most underestimated risk. Not the answer, but a first
bracket, with SQLite 3.53 bundled in:

| | `release` | `mobile` (`opt-level=z`, `panic=abort`, stripped) |
|---|---|---|
| `libsolstice_ffi_dart.so` | 3.6 MB | **2.0 MB** |
| `libsolstice_ffi_kotlin.so` | 3.4 MB | **1.9 MB** |

x86-64 Linux, so this is an indication and not a measurement: arm64 codegen,
Android's linker and the Flutter or Compose half of the APK are all still ahead.
It does say the budget is not obviously in danger.

## The cross-checks

Every run asserts before it prints, for the reason S1 gives: a benchmark that
measures the wrong thing quietly is worse than one that fails.

**The query IR is compared byte-for-byte against a fixture the Rust side wrote.**
Both hosts build plan §7's query from generated protobuf classes and check the
result against `query-1000.bin` / `query-50.bin`:

```
issues WHERE project_id = $0 AND closed = 0  ORDER BY priority DESC  LIMIT k
  RELATED comments  ORDER BY created_at DESC  LIMIT 3  AS comments
```

A misspelled field would be a compile error and needs no check. What this catches
is the silent version: an `ORDER BY` that sorted the other way, or a `project`
folded in as a literal instead of bound as a parameter. Both would measure a
perfectly real query that is not the one under test — and the parameter case
would also break plan §1.2's `PipelineId = hash(IR without params)`, which is
what lets N users share one server-side pipeline.

**The view is counted, on both sides:**

```
cross-check: 1000 parents · 3000 children · 21000 scalars · 2 subs ·
             550 mutations (250 before the sink) · 600 events · v550
```

and every write made with a sink installed is asserted to have produced exactly
two deltas — two live views, one total order. Kotlin additionally asserts that
what its `EventSink` received equals what the engine counted as sent, since a
dropped delta is a correctness bug plan §4.2 forbids outright, and it would
otherwise look like a fast callback.

## Reproducing

```sh
# 1. Seed the database and print the Rust control column.
cargo run --release -p solstice-bench --bin s2-fixture

# 2. protoc → Dart and Java; flutter_rust_bridge_codegen; cargo build;
#    uniffi-bindgen. Needs protoc, `dart pub global activate protoc_plugin`, and
#    `cargo install flutter_rust_bridge_codegen --version 2.14.0-beta.2`. The
#    UniFFI bindgen is a bin target of solstice-ffi-kotlin, so it needs nothing.
./generate.sh

# 3. Run them.
./dart/run.sh      # AOT-compiled, which is what Flutter ships
./kotlin/run.sh    # needs kotlinc; found inside Android Studio or IDEA
```

### Why a seeded `.db` and not a seeding API

The engine's only way in is `mutate`, so a host that seeded its own fixture would
spend the benchmark measuring 20,000 mutations instead of the boundary. The
fixture binary seeds through the real `SqliteStore` with S1's seed, scale and
generators, so the k=1000 view is the same 266.4 KB in both spikes and the two
sets of numbers subtract.

It also answers a question M0 has to answer anyway: the Flutter and Compose demo
apps need a 100k-row database, and `solstice-core` has no RNG in it by design
(plan §6). They will ship a seeded file too.

## Files

| | |
|---|---|
| `../../crates/solstice-ffi-dart/` | the `flutter_rust_bridge` adapter — no logic, only wrapping |
| `../../crates/solstice-ffi-kotlin/` | the UniFFI adapter, deliberately the same shape |
| `../../crates/solstice-bench/src/bin/s2-fixture.rs` | seeds the database, prints the Rust control |
| `generate.sh` | protoc → Dart/Java, FRB codegen, cargo build, uniffi-bindgen |
| `dart/bin/bench.dart` | the four costs, separated, plus the cross-checks |
| `kotlin/Bench.kt` | the twin, with a `Channel`-shaped `EventSink` |
| `kotlin/run.sh` | no Gradle — one Kotlin file and generated Java do not need one |

## What S2 does not settle

- **Still a laptop.** Kotlin is HotSpot, not ART, and the JNA upcall this spike
  makes so much of is one of the things most likely to differ there. Dart's AOT
  is x86-64, not arm64. Plan §9's device gate is still the real one.
- **No UI in the loop.** `mutate → delta at the host` is not `delta → committed
  frame`. Nothing here has laid out a list, and a `Uint8List` arriving on the
  event loop is not a rebuilt widget.
- **One shape of payload.** 266.4 KB of parents-with-children, twice. A view of
  wide rows without relations, or a delta of five rows, would stress the copy
  differently — S1 measured the five-row delta's decode, not its crossing.
- **The size numbers are not APK numbers.** No arm64, no `cargo-ndk`, no
  xcframework, and plan §7 warns that this is the part that eats weeks.
