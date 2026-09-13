# Solstice

A local-first sync engine for **Flutter and Jetpack Compose**, built on one Rust core.

You write queries. Solstice keeps their results live — updating incrementally as
local writes and server changes arrive, working offline, and converging when the
network comes back.

> **Status: pre-alpha. Nothing here is usable yet.**
>
> This repository is at milestone M0, whose entire purpose is to find out whether
> the central assumption holds (see [Is this even possible?](#is-this-even-possible)
> below). The API will change without warning, the protocol is explicitly unstable,
> and there are no published packages. Watch the repo if the idea interests you;
> don't depend on it.

## Why

Every serious local-first sync engine today lives in JavaScript — Zero, ElectricSQL,
TanStack DB, Triplit. On mobile there is essentially PowerSync and little else, and
for Jetpack Compose / Kotlin Multiplatform there is practically nothing at all.

Mobile developers have the strongest need for offline and the weakest tooling for it.

Solstice targets that gap directly:

| Existing | What Solstice does differently |
|---|---|
| **Zero** refuses offline writes by design, and stores in IndexedDB | Offline writes are the point; storage is SQLite |
| **ElectricSQL** shapes are single-table, and the write path is yours to build | Queries join and nest; writes go through the engine |
| **PowerSync** defines replication in a separate YAML sync-rules DSL | Your active queries *are* the replication spec |
| Dart-only offline libraries | One Rust core serves both Flutter and Compose |

## How it works

- **Queries are subscriptions.** Subscribing gives you a stream of row-level diffs —
  `Added` / `Removed` / `Changed` / `Moved` with positions — so `AnimatedList` and
  `LazyColumn` get real item animations and stable scrolling, not a new snapshot each
  time.
- **A restricted query language, on purpose.** Every operator admitted into the
  language has a delta rule computable in a bounded number of index probes against
  bounded state. Anything needing a full rescan or unbounded state is excluded. The
  restriction is what makes incremental maintenance possible on a phone.
- **Writes apply instantly, then rebase.** Local mutations hit an overlay and show up
  in the UI immediately. The server is authoritative; when its answer arrives, pending
  mutations are recomputed against the new base. No CRDTs.
- **The protocol is open.** Three HTTP endpoints and three semantic obligations. A
  conformant backend is a few hundred lines in any language, and a conformance suite
  proves it. A reference server is included.

## Is this even possible?

That is what M0 exists to answer. For Flutter the answer is now in, end to end,
and it is a qualified no. The question:

> Can a Rust IVM engine behind a schema-independent byte ABI deliver sub-16ms,
> jank-free updates for a **joined, sorted, limited** list over a 100k-row local
> dataset on a mid-tier Android phone, from **both** Flutter and Compose, within a
> reasonable binary-size budget?

If not, the premise collapses and this repository will say so. The kill criteria are
fixed in advance and published as measured numbers, pass or fail:

| Metric | Budget | Measured |
|---|---|---|
| p99 delta → committed frame, Flutter | < 16 ms | **23.4 ms — fails.** 24 µs of it is this engine |
| Jank under continuous churn | not visible | 0.8% of frames over 16 ms |
| Engine steady-state RSS | < 60 MB | 6.6 MB anonymous, on a phone |
| `TopK` refills/sec under an adversarial delete-the-top workload | < 5 | 1.4 |
| Decode of an initial 1000-row view — Kotlin | < 5 ms | 1.65 ms *(on a laptop)* |
| Decode of an initial 1000-row view — Dart | < 5 ms | **6.14 ms — fails**, 137 µs on the phone with the fallback |
| APK growth per ABI | < 8 MB | 1.73–1.84 MB of cdylib, arm64-v8a |

Measured over 100k issues and 1M comments in real SQLite, on a Galaxy A72 —
deliberately slower than the Pixel 6a the plan names. Everything except the Kotlin
row now comes from that phone. See [BENCHMARKS.md](BENCHMARKS.md) for what is and is
not being claimed.

**One criterion has already failed, and this is what that is for.** Dart decodes a
1000-row view in 6.14 ms against a 5 ms budget. The cause is not the encoding — Dart
walks those same bytes in 422 µs and indexes them in 8 µs — but `package:protobuf`
materialising ~28,000 objects for a list that shows eight rows at a time. The plan
named the fallback in advance (columnar diff + zero-copy accessors); the diagnosis
narrows it to half of that, and a prototype of the accessor half does the same job
in **8.30 µs**, with reading *all* 1000 rows through it still 8× cheaper than the
eager decode. The `.proto` does not change. Kotlin passes as-is. Full write-up in
[spikes/s1-decode](spikes/s1-decode/).

Finding this in week three, before two demo apps were written on top of the wrong
ABI, is the entire point of M0.

**The other half of that question is now answered: the byte ABI works, from both
sides.** Dart and Kotlin each build a query themselves, hand the engine bytes, and
get a view back — and neither Rust adapter knows this application has a table called
`issues`, which is the property the whole three-crate split exists for. What the
boundary costs is **bytes, not calls**: a payload-free call is under a microsecond in
Dart, while 266 KB costs 532 µs. Put that next to S1 and the Dart first frame lands at
540 µs once the accessor replaces the eager decode — 9× under budget, with 98% of it
now being `flutter_rust_bridge` copying a buffer. The one place the two generators
genuinely diverge is the callback: UniFFI's is a synchronous upcall on the engine
thread and makes a write 22% slower, where Dart's `StreamSink` posts and returns. Full
write-up in [spikes/s2-bridge](spikes/s2-bridge/).

**And it now runs on a phone, which found something a laptop could not.** Cross-built
for the Android ABIs, the shipping library is **1.73–1.84 MB** on arm64 — a budget
that passes by 4×, and of which the entire Solstice engine is 3.7%; the rest is
SQLite, Rust std, and 138 KB of panic-formatting machinery. On the device, every
CPU-bound number is about 2.5× the laptop, exactly as expected — except one. The
worst single pump is **53×** worse, 21 ms where the laptop saw 409 µs, reproducibly.
It is SQLite's WAL checkpoint copying pages back and fsyncing **synchronously on the
engine thread**. Nothing in the criteria fails — p99 is 437 µs against 16 ms — but
21 ms is a dropped frame and a half, and it was invisible on a desktop. Diagnosed
now, fixed in M1. Full write-up in [spikes/s3-android](spikes/s3-android/).

**The headline criterion fails, and the decomposition says the engine is 0.1% of the
miss.** A Flutter app over the same 89 MB fixture, with a driver mutating 200 rows/sec,
puts a change on the glass in **23.4 ms at p99** against a 16 ms budget. Cut into the
three legs it is made of: **24 µs** to turn FFI bytes into indexed rows, 12.5 ms
waiting for the scheduler and the next vsync, 14.1 ms for Flutter to build and
rasterize. Making the engine faster cannot fix this — the budget would still be
missed if `apply` were free. On a 90 Hz panel, 16 ms is 1.44 refresh intervals, and a
delta that arrives at an arbitrary moment waits up to one of them before a build can
even start. That is an explanation, not an excuse: the number stays a failure, and
what it redirects is *where to look* — at frame scheduling, not at the byte ABI.
Jank, separately, is fine: 0.8% of frames over 16 ms under continuous churn, with the
list correct after every one of 5,520 deltas. Full write-up in
[examples/flutter-issues](examples/flutter-issues/).

Compose has not answered the same question yet, and nothing here licenses assuming it
transfers.

## Repository

All of it M0 scaffolding.

```
proto/solstice/v1/        the normative wire format
crates/solstice-ivm/      incremental view maintenance — no IO, no time, no threads
crates/solstice-store/    SQLite: schemas, bounded scans, the IR → SQL translation
crates/solstice-proto/    protobuf encoding for the view diff — FFI payload and wire
crates/solstice-core/     the engine thread behind the byte ABI — zero FFI attributes
crates/solstice-ffi-dart/ the flutter_rust_bridge adapter — wrapping, no logic
crates/solstice-ffi-kotlin/ the UniFFI adapter, deliberately the same shape
crates/solstice-bench/    the M0 kill criteria, measured
examples/flutter-issues/  the Flutter demo — where delta → frame is measured
spikes/s1-decode/         spike S1: what that payload costs to decode in Dart/Kotlin
spikes/s2-bridge/         spike S2: what the boundary itself costs, both bindings
spikes/s3-android/        spike S3: size per ABI, and the kill criteria on a real phone
```

The crate is organised around a single law, which its property tests check directly
against a deliberately dumb reference evaluator:

```
for all plans P, relations R, deltas D:    P(R + D) == P(R) + P.apply(D)
```

Maintaining a result incrementally and recomputing it from scratch must always agree.
The law is checked against three oracles: a deliberately dumb reference evaluator,
SQLite itself, and the incremental path.

```sh
cargo test --workspace                    # the law, plus everything else
cargo run --release -p solstice-bench     # the kill criteria, on your machine
./spikes/s3-android/bench.sh              # the same, on a phone plugged into adb
./spikes/s3-android/build.sh              # what it costs to ship, per ABI
```

## License

Apache-2.0. Contributions by DCO sign-off; there is no CLA.
