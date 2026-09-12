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
| Decode of a 1000-row view in Dart and Kotlin | — not yet; needs the FFI |
| APK size per ABI | — not yet |
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
| Decode of a 1000-row view (Dart, Kotlin) | < 5ms | not yet measured | — |
| APK size per ABI | < 8MB | not yet measured | — |

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

## What this does not yet prove

- Nothing has crossed an FFI boundary. Decode cost in Dart and Kotlin (spike S1)
  is unmeasured, and it is the other half of the `delta → frame` budget.
- Nothing has run on a phone. A desktop has more cache, faster storage and no
  competition for either.
- Two phases of synthetic traffic are not eight months of a real app. The
  self-check makes the view *correct*; it does not make the workload
  *representative*.
