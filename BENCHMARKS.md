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

**This measures the engine half, on a laptop.** Two of the six M0 kill criteria
are not in here at all, and the two that are have a narrower meaning than the
plan's wording:

| Plan §5.1 says | This harness measures |
|---|---|
| p99 delta → **committed frame** | p99 delta → `pump` returned |
| RSS on a **mid-range Android phone** | RSS on the machine named below |
| Decode of a 1000-row view in Dart and Kotlin | measured, on the same laptop — [spike S1](#spike-s1--ffi-decode-cost) |
| APK size per ABI | the cdylib's size, built for x86-64 — [spike S2](#spike-s2--the-boundary-itself) |
| Jank on a Pixel 6a | — not yet |

So a number inside budget here is **necessary, not sufficient**. A number
*outside* budget here would already be fatal, which is the entire reason for
running this before building the FFI: if the engine cannot hold the budget on a
desktop with nothing else in the way, the ABI on top of it is irrelevant.

The physical-device gate stands (plan §9), and until it runs, the go decision is
provisional.

## Environment

| | |
|---|---|
| CPU | Intel Core i7-10870H @ 2.20GHz, 16 threads |
| RAM | 16 GB |
| OS | Arch Linux, kernel 7.2.2 |
| rustc | 1.95.0, `--release` |
| SQLite | 3.53.4 (bundled via `rusqlite`) |
| Date | 2026-09-12 |

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

| Metric | Budget | Measured | |
|---|---|---|---|
| p99 delta → `pump` returned | < 16ms | **190µs** | PASS |
| Engine operator state | < 60MB | **80.7 KB** | PASS |
| Peak anonymous RSS | < 60MB | **6.3 MB** | PASS |
| `TopK` refills/sec at 200 rows/sec | < 5 | **1.4** | PASS |
| Decode of a 1000-row view — Kotlin | < 5ms | **1.65ms** | PASS |
| Decode of a 1000-row view — Dart, `package:protobuf` | < 5ms | **6.14ms** | **FAIL** |
| Decode of a 1000-row view — Dart, zero-copy accessor | < 5ms | **8.30µs** | PASS |
| APK size per ABI | < 8MB | 1.9–2.0 MB of cdylib, x86-64 | — |

Peak *total* RSS is 68.2 MB, of which 61.9 MB is SQLite's reclaimable `mmap`
window over an 89 MB database. Those pages are clean and file-backed: the kernel
drops them under pressure and faults them back on the next read. The anonymous
number is the one the engine actually owns and the one that gets a process
killed, so that is the one measured against the budget. Reporting the total
alone would show a near-failure caused entirely by a page cache doing its job.

Refills are reported per *mutation* and then multiplied by the plan's 200
rows/sec write rate. The harness runs flat out, so its own refills-per-wall-
second would be a fact about this laptop's clock speed rather than about the
design.

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

**An early read on the APK budget.** With SQLite 3.53 bundled, under the `mobile`
profile (`opt-level=z`, `panic=abort`, stripped): **2.0 MB** for the Dart cdylib,
**1.9 MB** for the Kotlin one. x86-64 Linux, so an indication and not a
measurement, but the < 8MB budget is not obviously in danger.

**Two generator asymmetries, found by compiling the generated code.** UniFFI 0.32
cannot express an error field named `message` — it emits `val message` and
`override val message` in one class, and `kotlinc` rejects it; both adapters now
spell it `detail`. And `flutter_rust_bridge` renders a Rust enum-with-fields as a
`freezed` sealed class, so `build_runner` is part of any Flutter consumer's build
where UniFFI needs nothing extra.

## What this does not yet prove

- Nothing has run on a phone. A desktop has more cache, faster storage and no
  competition for either. Kotlin is measured on HotSpot, and the JNA upcall S2
  makes so much of is one of the things most likely to differ on ART.
- S2 stops at the host, not at the frame. `mutate → delta at the host` is not
  `delta → committed frame`; nothing here has laid out a list.
- Two phases of synthetic traffic are not eight months of a real app. The
  self-check makes the view *correct*; it does not make the workload
  *representative*.
