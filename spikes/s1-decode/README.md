# Spike S1 — what does a view diff cost to decode?

Plan §4.1 makes the FFI ABI **byte-based**: query IR, view diffs, mutation bodies
and events all cross as protobuf `Vec<u8>`. That choice buys a great deal — one
encoding shared with the wire protocol, both binding generators handling `Vec<u8>`
perfectly, schema evolution across FFI for free, and the decisive one: **the ABI
does not depend on the application's schema**, so no app ever regenerates Rust FFI
glue.

It has exactly one cost, and the plan names it as M0's primary kill criterion:

> Decode of an initial 1000-row view: **< 5ms**, in Dart **and** Kotlin, on a
> mid-range phone. A 5-row delta: **< 100µs**.
>
> On failure: columnar diff encoding with generated zero-copy accessors.

This spike answers it. It runs **before** `flutter_rust_bridge` and UniFFI are
wired up, because the bridging is mechanical (~500 LOC each) and it is precisely
the work that would be thrown away if the encoding is wrong.

## Result

| | 1000-row view (266.4 KB) | 5-row delta (1.3 KB) | |
|---|---|---|---|
| **Rust** — `solstice-proto`, hand-written | 0.99ms | 4µs | control |
| **Kotlin** — JVM 21, protobuf-javalite 4.36.1 | **1.65ms** | 8µs | **PASS** |
| **Dart** — 3.12.2 AOT, `package:protobuf` 6.1.0 | **6.14ms** | 31µs | **FAIL** |

Best-of-N on the machine in [BENCHMARKS.md](../../BENCHMARKS.md), which is a
laptop. Kotlin's pass is on HotSpot and Compose runs on ART; Dart's failure is
already fatal at this budget, and a phone will not be kinder.

**The delta case passes everywhere, with two orders of magnitude to spare.** That
matters more than it first looks: the steady state of a running app is deltas.
The 5ms budget is about the *first* frame after a subscribe.

**The failure has a fix, and the fix is measured, not assumed** — the plan named
it in advance and half of it turns out to be enough. A prototype of generated
zero-copy accessors does the same job in **8.30µs**, and reading all 1000 rows
through it is still 8× cheaper than decoding them eagerly. Details below; the
short version is that the `.proto` does not change.

## The diagnosis

A FAIL is not yet a finding. The plan's fallback is a large piece of work, and it
is only the right answer if the cost is *materialising objects*. So, two
hypotheses, tested in order (`dart/bin/probe.dart`).

**Hypothesis 1 — `Int64` boxing. Rejected.** `package:protobuf` maps every 64-bit
field to `Int64` from `package:fixnum`, because Dart compiled to JavaScript has no
64-bit integer. `Int64` is a heap object with three `int` fields, where Java uses a
primitive `long`, and this fixture carries ~14,000 of them. Decoding the same
bytes against a schema differing only in `sint64` → `sint32` — wire-compatible,
and the generated accessor returns a native `int`:

| integer field | accessor type | decode |
|---|---|---|
| `sint64` (what the schema says) | `Int64` | 6.33ms |
| `sint32` (diagnostic only) | `int` | 6.03ms |

**5%.** Worth measuring rather than assuming: this hypothesis is plausible enough
that acting on it would have meant a narrower integer type on the wire — a real
correctness loss, since SQLite integers are 64-bit — in exchange for nothing.

**Hypothesis 2 — object materialisation. Confirmed.** 1000 rows is 1000 `Added`,
1000 `ViewChange`, 4000 `Row`, 1000 `RowList` and 21,000 `Value`: about **28,000
`GeneratedMessage` instances** for a list that shows eight rows at a time.

| work done on the same 266.4 KB | best |
|---|---|
| full decode (`package:protobuf`) | 6.33ms |
| walk every scalar, materialise nothing | **422µs** |
| index 1000 row offsets, parse no row | **8µs** |

**93% of the time is building objects, not reading bytes.** Dart walks this
payload at roughly 630 MB/s. The encoding is not the problem.

## The fallback, measured rather than assumed

Plan §4.1's fallback has two separable halves, and the diagnosis says only one is
load-bearing: **zero-copy accessors, without the columnar re-encoding.**

`lazy_view.dart` is that half, prototyped — hand-written, but written as a
prototype of what codegen would emit, with the column indices baked in because a
generator knows the query's shape. Run it with `./dart/run.sh lazy`:

| what the host does | best | vs 5ms budget |
|---|---|---|
| eager decode, whole view (`package:protobuf`) | 8.17ms | **FAIL** |
| lazy: index only — what `subscribe()` returns | **8.30µs** | PASS, 600× under |
| lazy: index + 12 tiles (id, title, priority, closed) | 10.50µs | PASS |
| lazy: index + 12 tiles, comments included | 20.55µs | PASS |
| lazy: index + **all 1000** rows, comments included | **984µs** | PASS |

Three things worth reading off that table.

**The first frame stops being a problem.** What `subscribe()` has to return before
the first paint is the index: 8.30µs. The kill criterion is no longer close.

**One screen is free.** A dozen tiles with their comments is 20.55µs, inside a
16ms frame by three orders of magnitude, which leaves the budget to layout and
raster where it belongs.

**Laziness is not just moving the cost.** That was the real risk — a good first
frame paid for with a bad fling. Reading *all* 1000 rows and every comment through
the accessor costs 984µs, **8× less than decoding them eagerly**, because it never
builds the 28,000 intermediate messages. There is no scroll position that makes
the eager decoder the better choice.

<sub>The eager row reads 8.17ms here against 6.14ms in `bench.dart`. Same bytes,
different harness: this one times batches of 20 consecutive decodes, so the
allocator pressure from one lands inside the next, where `bench.dart` measures an
isolated best case. The batched figure is the more honest one for a scrolling
list, and both fail. The comparison that matters is within one harness: 8.17ms
against 8.30µs.</sub>

So the `.proto` survives S1 unchanged, the columnar half of the fallback is not
needed, and the work moves to the Dart binding. Kotlin needs nothing — javalite is
inside budget as it stands, and the same accessor can follow if ART says
otherwise.

The honest caveat: 6.14ms against a 5ms budget is not a rout, and a reader could
reasonably ask whether tuning `package:protobuf` would close it. Two reasons not
to bother. Its p50 is 9.91ms, so the typical frame is 2× over, not 20%. And the
budget is a laptop standing in for a phone — plan §9's device gate is still ahead
and only moves one direction.

## A correctness finding, not a performance one

`package:protobuf` 6.1.0 **decodes `sint64` wrongly at the extremes of the
type.** It reports `i64::MIN` as `0` and `i64::MAX` as `-1`, and the bits are
wrong, not the formatting — `toHexString()` gives `FFFFFFFFFFFFFFFF`.

`zigzag(i64::MIN)` is `u64::MAX`, the only value that needs all ten varint bytes,
and un-zigzagging it needs a *logical* right shift where an arithmetic one
collapses the result. This accessor made the same mistake on its first draft.
What caught both is `edge.bin` — four rows the hydration will never produce, which
is exactly why they had to be generated rather than found: the 1000-row window
holds the top issues by priority, so it contains no NULL, no empty child
collection, no empty string and no large integer. Its cross-check line reads
`0 nulls`, and a decoder that mishandled every one of those cases would pass it.

Three independent decoders — Rust, `protoc`-generated Java on protobuf-javalite,
and this accessor — agree on the correct values, so it is not the encoder.

It matters past the spike. A SQLite column holds an `i64`, so these are values a
real row may legitimately carry, and the failure is silent: a corrupted number,
not an exception. It is an argument for the generated accessor that has nothing to
do with speed, and it is worth reporting upstream.

The check is live rather than a comment — if `package:protobuf` fixes this, the
spike will say so instead of going on warning about it.

## Reproducing

```sh
# 1. Generate the fixtures — a real hydration through real SQLite.
cargo run --release -p solstice-bench --bin s1-fixture

# 2. Generate the Dart and Kotlin decoders from the normative .proto.
#    Needs protoc, and `dart pub global activate protoc_plugin`.
./generate.sh

# 3. Run them.
./dart/run.sh          # AOT-compiled, which is what Flutter ships
./dart/run.sh probe    # the diagnosis: boxing, the walk, the index
./dart/run.sh lazy     # the fallback, and the package:protobuf finding
./kotlin/run.sh        # needs kotlinc; found inside Android Studio or IDEA
```

### The fixture is not synthetic

`crates/solstice-bench/src/bin/s1-fixture.rs` lives in the benchmark crate on
purpose. It seeds 20,000 issues across 4 projects with the same generators the
engine benchmark uses, builds the real `Source → Filter → TopK → Join(1:N)`
pipeline, and encodes **what an actual hydration returned**: 1000 parents, each
with 3 children, 21,000 scalars, 266.4 KB. It errors out rather than pad if
hydration yields fewer than 1000 rows.

Measuring a decode of rows this crate invented would only prove that the invented
rows were easy to decode. Row shape drives this benchmark entirely — string
lengths, null density, child fanout — so the payload has to come from the engine.

### The generated decoders are oracles, not conveniences

The Rust encoder in `crates/solstice-proto` is hand-written (its `wire` module
explains why prost was rejected). Checking it against a decoder from the same hand
would prove only that the hand is consistent. `protoc` reading the normative
`.proto` gives two independent implementations, in two other languages, and both
benchmarks **assert** on the counts rather than printing them:

```
cross-check: 1000 parents · 3000 children · 21000 scalars · 0 nulls · sub 1 v1
first row: id=19 title="scroll crash in theme"
```

Rust, Dart and Java all agree, down to the first row's title. An encoder that
drifts from the spec fails these runs with exit code 1.

## Files

| | |
|---|---|
| `generate.sh` | `protoc` → Dart and Java (javalite, because Android ships lite) |
| `dart/bin/bench.dart` | the budget measurement + cross-check assertion |
| `dart/bin/probe.dart` | the diagnosis: boxing, the allocation-free walk, the index |
| `dart/lib/lazy_view.dart` | the fallback: a prototype of generated zero-copy accessors |
| `dart/bin/lazy.dart` | does the fallback work, and every field verified both ways |
| `kotlin/Bench.kt` | same budget measurement on the JVM, plus the edge fixture |
| `kotlin/run.sh` | no Gradle — one Kotlin file and generated Java do not need one |

## What S1 does not settle

- **It ran on a laptop.** Kotlin's pass is HotSpot, not ART; Dart's AOT is x86-64,
  not arm64. Plan §9's device gate is still the real one.
- **Nothing has crossed FFI.** This measures decode, not the round trip. The copy
  `flutter_rust_bridge` and UniFFI make at the boundary is unmeasured, and it is
  the next thing to build.
- **The accessor is a prototype, not a generator.** It is hand-written against one
  query's shape. Turning it into codegen driven by the query IR is M1 work, and
  the interesting part — strings, nested collections, NULL — is what this file
  already had to solve.
