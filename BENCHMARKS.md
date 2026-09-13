# Benchmarks

Plan §9 makes M0 the project's go/no-go gate and asks for the kill-criteria
numbers to be recorded here and tracked every release. This is the first entry.

Reproduce it with:

```
cargo run --release -p solstice-bench
```

Every number below is deterministic from the seed printed in the header. There
is no `rand` dependency and no wall clock in the generator, so a regression can
be bisected rather than argued about.

## Read this before reading the numbers

**This measures the engine half.** As of [spike S3](#spike-s3--android-size-and-the-device-gate)
it measures it on a physical Android phone as well as on the laptop, which
closes two of the gaps this section used to list and leaves the ones a UI owns:

| Plan §5.1 says | What is measured |
|---|---|
| p99 delta → **committed frame** | p99 delta → `pump` returned — the frame is still the demo apps' half |
| RSS on a **mid-range Android phone** | measured there — [spike S3](#spike-s3--android-size-and-the-device-gate) |
| Decode of a 1000-row view in Dart and Kotlin | measured, on the laptop — [spike S1](#spike-s1--ffi-decode-cost) |
| APK size per ABI | the cdylib, built for the real ABIs — [spike S3](#spike-s3--android-size-and-the-device-gate) |
| Jank on a Pixel 6a | — not yet, and it needs a UI |

So a number inside budget here is **necessary, not sufficient**. A number
*outside* budget here would already be fatal, which is the entire reason for
running this before building the FFI: if the engine cannot hold the budget on a
desktop with nothing else in the way, the ABI on top of it is irrelevant.

Plan §9's go/no-go gate is a physical device, and the engine half of it now
passes on one. The decision stays provisional until the demo apps close the gap
between "the engine has the diff" and "the frame is on screen".

## Environment

Two machines. The laptop is where the detail sections below were measured; the
phone is where the kill criteria were re-run, and it is the one the budgets were
written for.

| | laptop | phone |
|---|---|---|
| CPU | Intel Core i7-10870H @ 2.20GHz, 16 threads | Snapdragon 720G — 2× Kryo 465 Gold @ 2.3GHz, 6× Silver @ 1.8GHz |
| Device | — | Samsung SM-A725F (Galaxy A72) |
| RAM | 16 GB | 7 GB |
| OS | Arch Linux, kernel 7.2.2 | Android 16 (API 36), arm64-v8a |
| rustc | 1.95.0, `--release` | 1.95.0, `--release`, NDK r28, minSdk 24 |
| SQLite | 3.53.4 (bundled via `rusqlite`) | same, cross-compiled |
| Date | 2026-09-12 | 2026-09-13 |

The A72 is a 2020 mid-ranger and deliberately slower than the Pixel 6a plan §5.1
names, so its numbers read the criterion pessimistically rather than
generously.

## The workload

Plan §7's own example — the query it names as the project's most likely way to
fail:

> the top 50 open issues in one project by priority, each with its 3 latest
> comments

over 100,000 issues and 1,000,000 comments across 50 projects (89.1 MB of
SQLite, `journal_mode=WAL`, `mmap_size=64MB`). The subscribed project holds
2,014 open issues, so the 50-row window has ~40× more candidates below it than
it can show — the window has to actually work.

The pipeline is the four operators M0 ships, in the order that keeps the memory
bound: `Source → Filter → TopK → Join(1:N)`, with the window **below** the join
so the join only ever holds children for parents on screen.

Two phases, 2,000 transactions each, timing exactly `apply` then `pump`:

- **churn** — ordinary app traffic: issues edited, comments added and deleted,
  issues created. 25% of it aimed at the subscribed project.
- **delete-the-top** — plan §7 mitigation 4, the adversarial one: delete the
  first row of the window, every single transaction, forever.

After each phase the maintained view is compared against SQLite answering the
same question directly — plan §6's second oracle, at scale. **A divergence
suppresses every timing**, because a fast wrong answer is worth less than a slow
right one.

## Kill criteria

| Metric | Budget | Laptop | **Galaxy A72** | |
|---|---|---|---|---|
| p99 delta → `pump` returned | < 16ms | 190µs | **410–437µs** | PASS |
| Engine operator state | < 60MB | 80.7 KB | **80.7 KB** | PASS |
| Peak anonymous RSS | < 60MB | 6.3 MB | **6.5–6.6 MB** | PASS |
| `TopK` refills/sec at 200 rows/sec | < 5 | 1.4 | **1.4** | PASS |
| Decode of a 1000-row view — Kotlin | < 5ms | **1.65ms** | — | PASS |
| Decode of a 1000-row view — Dart, `package:protobuf` | < 5ms | **6.14ms** | — | **FAIL** |
| Decode of a 1000-row view — Dart, zero-copy accessor | < 5ms | **8.30µs** | — | PASS |
| APK size per ABI | < 8MB | — | **1.73–1.84 MB** of cdylib, arm64-v8a | PASS |
| Worst single pump | — | 409µs | **21.3–21.6ms** | see S3 |

The decode rows are laptop-only because the hosts have not been cross-compiled
yet; the phone columns are the engine, measured by
[spike S3](#spike-s3--android-size-and-the-device-gate) on the device named
above. The last row is not a criterion the plan states, and it is here because
it is the only number in the table that behaves differently on a phone than on a
laptop — 53× worse, where everything else is 2.5× worse. It is SQLite's WAL
checkpoint running on the engine thread; S3 has the diagnosis.

Peak *total* RSS is 68.2 MB on the laptop, of which 61.9 MB is SQLite's
reclaimable `mmap` window over an 89 MB database (20.7 MB / 14.1 MB on the
phone, which has less page cache to give). Those pages are clean and
file-backed: the kernel drops them under pressure and faults them back on the
next read. The anonymous number is the one the engine actually owns and the one
that gets a process killed, so that is the one measured against the budget.
Reporting the total alone would show a near-failure caused entirely by a page
cache doing its job.

Refills are reported per *mutation* and then multiplied by the plan's 200
rows/sec write rate. The harness runs flat out, so its own refills-per-wall-
second would be a fact about the machine's clock speed rather than about the
design. That the phone and the laptop agree at 1.4 is the point: refills are a
property of the algorithm, not of the hardware.

### The first frame

Hydration produces 50 rows in **1.71ms**, reading **567 of 1,100,000 rows —
0.05% of the store**: one bounded 67-row scan for the window, then one bounded
read per parent on screen for its comments.

The 67 is `k + slack + 1`, and the `+ 1` is load-bearing. A `TopK` only learns
that rows exist below its window by discarding one, so a scan of exactly
`k + slack` would leave it believing it held the whole relation, and it would
never refill.

### Operator state

| Operator | After hydration | After 2,000 adversarial deletes |
|---|---|---|
| `Source` (issues) | 0 B | 0 B |
| `Filter` | 0 B | 0 B |
| `TopK` | 13.3 KB | — |
| `Source` (comments) | 0 B | 0 B |
| `Join1N` | 116.9 KB | — |
| **total** | **130.1 KB** | **80.7 KB** |

Per-operator rather than a total, because a total cannot distinguish a join
fanning out — a bug — from a window that grew its slack, which is the design
working. State goes *down* over the adversarial phase: replacement issues are
new and have no comments yet, so the join holds smaller child collections.

### The adversarial phase

2,000 deletes of the top row produced **27 window refills** — one per 74 deletes,
not one per delete. `slack` is what buys that: the window holds `k + slack` rows
so ordinary deletes are absorbed without touching the store, and the adaptive
growth (plan §1.3) doubles it under sustained pressure, which is why the rate
falls as the phase goes on.

The 2,000 child-window reads in the same phase are **not** refills in the
kill-criterion sense and are counted separately. One parent entered the view per
delete, and its comments had to come from somewhere; that rate is pinned to how
often the window changes, not to how hard it is working to stay full. Summing
the two makes a healthy join look like a failing window — which it did, in the
first version of this harness, until the counter learned to say which operator
issued the read.

## Indexes are not an optimisation here

The same engine, the same workload, the same data — only the indexes differ:

| Indexes | p99 (adversarial) | vs budget |
|---|---|---|
| `seek` — `(issue_id, created_at DESC, id)` | **190µs** | PASS |
| `none` | 34.5ms | 2× over |
| `order` — `(created_at DESC, id)` | **826ms** | **52× over** |

<sub>`order` and `none` were run with 100 and 20 transactions respectively; at
2,000 they do not finish in a reasonable time, which is itself the result.</sub>

A per-parent child window asks for
`WHERE issue_id = ? ORDER BY created_at DESC LIMIT 4`. An index on the sort
columns alone answers it by walking the whole index newest-first and doing a
table lookup on every entry to test `issue_id`, until it happens to find four
rows for this parent — **the bound lost to the filter instead of to the sort**.
Over a million comments that is most of the table, per parent, and it is *worse
than having no index at all*, because a full scan is at least sequential.

Leading with the equality column makes the same statement a seek: find the one
parent's slice, read four entries off it, stop. That is `create_seek_index`, and
the test that covers it asserts on `EXPLAIN QUERY PLAN` rather than on a
stopwatch — "it was fast on my machine" does not distinguish a seek from a scan
of a table that is still small.

The lesson for the planner, which M1 will have to encode: **an `ORDER BY` index
is not enough for a query that also filters.** The equality columns have to come
first, and the planner has to know which they are.

## Spike S1 — FFI decode cost

The other half of the `delta → frame` budget: plan §4.1 sends view diffs across
FFI as protobuf bytes, and the host has to decode them. Full write-up and the
diagnosis in [`spikes/s1-decode/`](spikes/s1-decode/); the summary is that **one
of the two bindings fails**.

The payload is a real hydration — 1000 parents, 3 children each, 21,000 scalars,
266.4 KB — encoded by `solstice-proto` and read back by decoders `protoc`
generated from the normative `.proto`.

| | 1000-row view | 5-row delta | |
|---|---|---|---|
| Rust (`solstice-proto`) | 0.99ms | 4µs | control |
| Kotlin, protobuf-javalite 4.36.1 | **1.65ms** | 8µs | PASS |
| Dart 3.12.2 AOT, `package:protobuf` 6.1.0 | **6.14ms** | 31µs | **FAIL** |

Deltas pass everywhere by two orders of magnitude, which is the steady state of a
running app. The 5ms budget is about the first frame after a subscribe.

**The encoding is not what fails.** Walking the same bytes and materialising
nothing takes **422µs**; indexing 1000 row offsets takes **8µs**. 93% of Dart's
6.14ms is building ~28,000 `GeneratedMessage` objects for a list that shows eight
rows at a time. (`Int64` boxing from `package:fixnum` — the obvious suspect — was
measured at 5% and rejected as the cause.)

So plan §4.1's named fallback is right, and the measurement narrows it to half:
**generated zero-copy accessors, without the columnar re-encoding.** Reordering
bytes that already index in 8µs would solve nothing and would cost the shared
encoding with the wire protocol. The `.proto` survives S1 unchanged.

That fallback is then built and measured rather than taken on faith, because the
real risk in going lazy is trading a good first frame for a bad fling:

| what the host does | best |
|---|---|
| eager decode, whole view | 8.17ms |
| lazy: index only — what `subscribe()` returns | **8.30µs** |
| lazy: index + 12 tiles, comments included | 20.55µs |
| lazy: index + all 1000 rows, comments included | **984µs** |

Reading *everything* through the accessor is 8× cheaper than decoding it eagerly,
so there is no scroll position at which the eager decoder wins. <sub>The eager row
reads 8.17ms here against 6.14ms above because this harness times batches of 20
consecutive decodes, so one decode's allocator pressure lands inside the next.
Both fail; the comparison that matters is within one harness.</sub>

Kotlin is measured on HotSpot; Compose runs on ART. That pass is provisional in
the same way every number here is.

**A correctness finding fell out of it.** `package:protobuf` 6.1.0 decodes
`sint64` wrongly at the extremes of the type — `i64::MIN` reads back as `0` and
`i64::MAX` as `-1`, wrong bits rather than wrong formatting. Rust, generated Java
and the accessor all agree on the correct values. A SQLite column holds an `i64`,
so a real row may carry these, and the failure is silent. Details and the
reproduction in [`spikes/s1-decode/`](spikes/s1-decode/).

## Spike S2 — the boundary itself

S1 decoded bytes without crossing FFI. S2 crosses it: two adapter crates, two
generated bindings, two host benchmarks, one engine. Full write-up in
[`spikes/s2-bridge/`](spikes/s2-bridge/).

Rust is the control — the same engine called in-process, so a host's column minus
Rust's is what the binding charges.

| | rust | dart | kotlin |
|---|---|---|---|
| `initial()` k=1000 — 266.4 KB | 7µs | **532µs** | **194µs** |
| `subId()` — no payload at all | — | **<1µs** | **7µs** |
| `initial()` k=50 — 13.3 KB | 137ns | 17µs | 21µs |
| `mutate` — no sink installed | 385µs | 388µs | 422µs |
| `mutate` — 1 update, 2 views live | 386µs | 411µs | **516µs** |
| `mutate` → delta arrives at the host | — | 409µs | 524µs |

**The boundary charges per byte, not per call.** Dart's per-call floor is under a
microsecond and its 266.4 KB payload is 532µs. Neither binding moves bytes at
memory speed — ~0.5 GB/s on Dart, ~1.4 GB/s on Kotlin, against 38 GB/s for the
Rust clone — because both copy twice. An ABI of few large calls is therefore the
right shape, which is what plan §4.1 already chose; now for a measured reason.

**Put S1 and S2 together and the first frame comes in under budget, but the copy
becomes the whole cost.** Dart today is 532µs + 6.29ms = 6.82ms, over. With S1's
zero-copy accessor it is 532µs + 8µs = **540µs**, 9× under — and 98% of that is
`flutter_rust_bridge` copying a buffer. Kotlin needs nothing: 194µs + 1.64ms =
1.8ms.

**The callback is where the two generators genuinely differ.** `mutate` measured
with and without a sink installed isolates it, since `Engine::pump` maintains
every view either way. Ranges below are two full runs:

| `mutate`, 2 deltas per write | no sink | with sink | delivery |
|---|---|---|---|
| Rust — virtual call into a counter | 385–394µs | 386–403µs | in the noise |
| Dart — FRB `StreamSink`, posts to the isolate's port | 388–414µs | 411–420µs | in the noise |
| Kotlin — UniFFI foreign trait, upcall into the JVM via JNA | 421–422µs | 515–516µs | **+94µs** |

UniFFI's callback is synchronous and runs on the engine thread, so it lands in
every writer's latency — 47µs per delta, reproducible to the microsecond, making
Kotlin's write path 22% slower with a sink than without. Dart's and Rust's cost
less than the measurement's own run-to-run spread. That makes plan §4.3's
"`trySend` and get out" load-bearing rather than advisory, and makes per-pump
batching (plan §4.2's one stream per `Database`) the Kotlin fix rather than a
nicety.

**An early read on the APK budget**, since superseded by S3: 2.0 MB and 1.9 MB
of cdylib under the `mobile` profile, on x86-64 Linux.

**Two generator asymmetries, found by compiling the generated code.** UniFFI 0.32
cannot express an error field named `message` — it emits `val message` and
`override val message` in one class, and `kotlinc` rejects it; both adapters now
spell it `detail`. And `flutter_rust_bridge` renders a Rust enum-with-fields as a
`freezed` sealed class, so `build_runner` is part of any Flutter consumer's build
where UniFFI needs nothing extra.

## Spike S3 — Android size, and the device gate

S1 and S2 both ran on a laptop and both said so. S3 cross-compiles and runs the
harness on a physical phone, which is where plan §9 puts the M0 gate, and
measures the per-ABI size plan §7 lists as S3. Full write-up in
[`spikes/s3-android/`](spikes/s3-android/).

**Size, per ABI**, `mobile` profile, NDK r28, minSdk 24, SQLite bundled:

| abi | `libsolstice_ffi_dart.so` | `libsolstice_ffi_kotlin.so` |
|---|---|---|
| arm64-v8a | **1.84 MB** | **1.73 MB** |
| armeabi-v7a | **1.46 MB** | **1.40 MB** |

Native libraries are stored uncompressed in an APK at every minSdk this project
will support, so per-ABI APK growth is that number rather than a compressed
fraction of it. It passes the < 8MB budget by more than 4×.

**The engine is 3.7% of what ships.** By symbol bytes: SQLite 35% (of which
13.5% is fts3/fts5/rtree, which plan §1.1 excludes from the query language),
Rust std and dependencies 50%, `std::backtrace`'s gimli/addr2line/miniz\_oxide
10%, all six `solstice-*` crates 3.7%, UniFFI scaffolding 1.7%. Writing less
Rust is not how this library gets smaller. `build.sh --trim` removes the SQLite
modules for a measured 14–18%, and is a flag rather than a default because
§1.1's `queryOnce(rawSql)` escape hatch would stop being able to run a full-text
query at all.

**opt-level=z buys a third of the size for a third of the speed.** Against the
same code at opt-level=3, both stripped, on the phone: 33–38% smaller, p99
434µs → 584µs, hydration 3.51ms → 5.22ms. Both sides are inside both budgets, so
at M0 the choice is free; the exchange rate is recorded because it will stop
being free later.

**The phone is 2.5× the laptop everywhere except one number, where it is 53×.**
p50, p99 and hydration all scale by about 2.5×. The worst single pump goes from
409µs to 21.3–21.6ms. Two experiments name it: in memory the outlier drops to
904µs, and on disk with `wal_autocheckpoint=0` it drops to 1.09ms. It is
SQLite's WAL checkpoint — 1000 pages by default — copying the WAL back and
fsyncing **synchronously on the engine thread**, the thread plan §4.3 protects
from long hydrations but not from this.

The pragma is the diagnosis, not the fix: disabling autocheckpoint costs 15% on
p99 and lets the WAL grow without bound. Moving the checkpoint onto a thread
that is allowed to block is M1 work. Against the criterion as written nothing
fails — p99 is 434µs against 16ms, and this is one pump in 2000 — but 21ms is a
dropped frame and a half, it is reproducible rather than random, and it was
invisible on a laptop.

**16 KB pages.** Android 15 ships devices with 16 KB pages, where a library
aligned to 4 KB does not load at all. NDK r28 aligns by default and r27 needs a
linker flag, so `build.sh` verifies every 64-bit library it produces instead of
trusting the toolchain version.

## What this does not yet prove

- The engine has run on a phone; the *hosts* have not. Dart and Kotlin are still
  measured on a laptop, and Kotlin on HotSpot rather than ART — the JNA upcall
  S2 makes so much of is one of the things most likely to differ there.
- Nothing stops at the frame. `mutate → delta at the host` is not `delta →
  committed frame`; nothing here has laid out a list. The WAL finding is a
  reason to measure that rather than to extrapolate it.
- The APK is a cdylib, not an APK. Nothing yet checks what Gradle and AGP add
  around it.
- Two phases of synthetic traffic are not eight months of a real app. The
  self-check makes the view *correct*; it does not make the workload
  *representative*.
