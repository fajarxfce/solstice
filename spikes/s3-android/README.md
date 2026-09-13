# Spike S3 — what does it cost to ship, and does it hold on a phone?

[S1](../s1-decode/) measured decode. [S2](../s2-bridge/) measured the boundary.
Both ran on a laptop, and both said so at the top, because plan §9 is explicit
about where the M0 answer has to come from:

> **M0:** `cargo bench` + aplikasi benchmark di HP Android fisik […] **Ini
> gerbang go/no-go proyeknya.**

S3 is the other half of plan §7's spike list — "ukuran binary: Rust + SQLite +
protobuf per ABI" — and it cannot be answered without a cross-compile, which is
the second risk that same section names:

> **Risiko kedua yang sering diremehkan: pipeline build lintas target.** […]
> Mitigasi: beresin di M0 (bukan belakangan).

So this does both at once: build for the ABIs, then run the kill criteria on a
real phone, because the binary that answers the size question is built by the
same pipeline as the binary that answers the latency one.

The demo apps are still owed. What they add is the half of the latency question
a UI owns — delta → *committed frame*, and visible jank. What is here is
everything the engine owns, on the hardware the budgets were written for.

## Result

**Size, per ABI.** `mobile` profile (opt-level=z, panic=abort, strip), NDK r28,
minSdk 24, SQLite bundled.

| abi | `libsolstice_ffi_dart.so` | `libsolstice_ffi_kotlin.so` | budget |
|---|---|---|---|
| **arm64-v8a** | **1.84 MB** | **1.73 MB** | < 8 MB |
| **armeabi-v7a** | **1.46 MB** | **1.40 MB** | < 8 MB |
| x86-64 Linux, for reference | 1.92 MB | 1.79 MB | — |

Native libraries are stored uncompressed in an APK on every minSdk this project
will support, so per-ABI APK growth is this number, not a compressed fraction of
it. It passes by more than 4×.

**The kill criteria, on a phone.** Same seed, same 100k issues / 1M comments,
same query, same harness, `--release`.

| Metric | Budget | Laptop | **Galaxy A72** | |
|---|---|---|---|---|
| p99 delta → pump returned | < 16 ms | 156 µs | **410–437 µs** | PASS |
| engine operator state | < 60 MB | 80.7 KB | **80.7 KB** | PASS |
| peak anonymous RSS | < 60 MB | 6.3 MB | **6.5–6.6 MB** | PASS |
| `TopK` refills/sec, adversarial @200 rows/s | < 5 | 1.4 | **1.4** | PASS |
| hydration, 50 rows + 150 comments | — | 1.70 ms | **3.51–3.68 ms** | — |
| worst single pump | — | 409 µs | **21.3–21.6 ms** | see below |

Ranges are across runs, which is also the answer to whether they are stable:
everything but the last row is.

## The device

`SM-A725F` — Galaxy A72, Snapdragon 720G, Android 16, arm64-v8a, 7 GB RAM. A
2020 8 nm mid-ranger: two Kryo 465 Gold at 2.3 GHz and six Silver at 1.8 GHz.

That is deliberately *below* the Pixel 6a the plan names. Tensor G1 is a
generation and a half newer and roughly twice the single-core throughput, so
every number here is a pessimistic reading of the criterion as written, and the
phone most likely to be the floor of a supported device list is the one it ran
on.

The bench script prints the model, SoC and Android version above the table, for
the reason a table of budgets met on unnamed hardware is not a measurement.

## Finding 1 — the phone is 2.5× the laptop, except where it is 50×

| | laptop | A72 | ratio |
|---|---|---|---|
| p50, churn | 33 µs | 76–79 µs | 2.4× |
| p99, churn | 85 µs | 197–226 µs | 2.5× |
| p50, delete-the-top | 56 µs | 125–138 µs | 2.3× |
| p99, delete-the-top | 156 µs | 410–437 µs | 2.7× |
| hydration | 1.70 ms | 3.51–3.68 ms | 2.1× |
| **max, delete-the-top** | **409 µs** | **21.3–21.6 ms** | **53×** |

The laptop column is a fresh run on the machine [BENCHMARKS.md](../../BENCHMARKS.md)
describes, taken for this comparison. Where it differs from the published run it
is a second sample rather than a correction: p99 delete-the-top was 190 µs there
and 156 µs here, so the laptop's spread across sessions is wider than the
phone's within one, and the honest ratio for that row is 2.2–2.8× rather than
the single number the table prints. BENCHMARKS.md keeps the original figures;
swapping them for a second sample would be churn dressed up as precision.

Five rows say the same thing — this CPU is about 2.5× slower, which is what a
2020 mid-range ARM core against a desktop x86 should say. One row does not, and
a row that breaks the pattern by 20× is either a measurement artefact or the
finding.

It is the finding, and it is only visible here. On the laptop the worst pump in
the entire adversarial phase was 409 µs, so nothing in S1, S2 or any previous
`cargo bench` run could have shown it. This is the argument for plan §9's gate
being physical hardware, made concrete: not "the phone is slower", which anyone
could have assumed, but "the phone has a cost the laptop does not have at all".

## Finding 2 — the outlier is SQLite's WAL checkpoint, on the engine thread

Two experiments on the same phone, same seed, same workload:

| | max, churn | max, delete-the-top | p50 | p99 |
|---|---|---|---|---|
| on disk, as shipped | 33–37 ms | 21.3–21.6 ms | 125–138 µs | 410–437 µs |
| **in memory** | 877 µs | **904 µs** | 94 µs | 303 µs |
| **on disk, `wal_autocheckpoint=0`** | 1.10 ms | **1.09 ms** | 156 µs | 473 µs |

The first row isolates it to storage: taking the disk away removes 95% of the
worst case while leaving p50 and p99 roughly where they were. The second names
it exactly. With autocheckpoint disabled the database is still on flash, still
fsyncing, still doing every write it did before — and the outlier is gone. It is
not write latency. It is the checkpoint: SQLite's default `wal_autocheckpoint` is
1000 pages, and when a commit crosses that line it copies the WAL back into the
main database and fsyncs, **synchronously, on whatever thread committed**.

That thread is the one from plan §4.3:

> **Thread `dq-engine`** — satu OS thread menjalankan command loop. Memiliki
> koneksi SQLite dan seluruh graf dataflow. Nol lock di hot path. Hydration besar
> dipotong per 500 row lalu yield ke command queue, supaya query besar tidak bisa
> memblokir mutasi.

The plan was careful that a large *hydration* cannot block a mutation. A
checkpoint blocks one for 21 ms and nothing in the design saw it coming, because
it is not the engine's work — it is SQLite's, charged to the caller, at a moment
the caller did not choose.

**The third row is also why the fix is not the pragma.** Turning autocheckpoint
off costs 15% on p99 (410→473 µs) and 20% on p50, because reads walk an
ever-longer WAL index, and the WAL then grows without bound, which on a phone is
a different way to fail. The number in that row is a diagnosis, not a
configuration. The fix is to move the checkpoint off the engine thread —
`wal_checkpoint(PASSIVE)` driven deliberately, from a thread that is allowed to
block — and it belongs with the threading work in M1, not smuggled into a
measurement.

Against the criterion as written, none of this fails: p99 is 434 µs against
16 ms, and one pump in 2000 exceeding a frame is 0.05%. But 21 ms is a dropped
frame and a half, it is reproducible rather than random, and "p99 passes" is
exactly the kind of true statement that a scroll jank bug report is made of. It
is written down here in week three instead.

## Finding 3 — half the library is neither SQLite nor Solstice

Symbol bytes in the unstripped arm64 `libsolstice_ffi_kotlin.so`:

| | bytes | share |
|---|---|---|
| SQLite — fts3, fts5, rtree, soundex | 188,394 | 13.5% |
| SQLite — everything else | 299,824 | 21.5% |
| `std::backtrace` → gimli, addr2line, miniz_oxide | 138,249 | 9.9% |
| Rust std and the rest of the dependency graph | 690,844 | 49.6% |
| **`solstice-*`, all six crates** | **51,076** | **3.7%** |
| UniFFI scaffolding | 23,344 | 1.7% |

The engine is 3.7% of what ships. That reframes the size budget: there is no
version of this project where writing less Rust is how the library gets smaller.

Two line items are removable, and only one of them should be removed today.

**Full-text and R-tree, 188 KB.** `rusqlite`'s `bundled` feature hardcodes
`-DSQLITE_ENABLE_FTS3 -DSQLITE_ENABLE_FTS5 -DSQLITE_ENABLE_RTREE` and appends
`LIBSQLITE3_FLAGS` *after* them, so removing one means `-U`-ing it, not
redefining it. `build.sh --trim` does that:

| abi | as shipped | trimmed | |
|---|---|---|---|
| arm64-v8a, dart | 1,925,824 | 1,650,512 | −14.3% |
| arm64-v8a, kotlin | 1,814,056 | 1,538,680 | −15.2% |
| armeabi-v7a, dart | 1,530,316 | 1,267,908 | −17.1% |
| armeabi-v7a, kotlin | 1,467,876 | 1,205,412 | −17.9% |

It is a flag rather than the default on purpose. Plan §1.1 excludes full-text
*from the IVM graph*, and the same section keeps `db.queryOnce(rawSql)` as a
documented escape hatch. Deleting FTS from the build turns "you cannot subscribe
to a full-text query" into "you cannot run one at all", which is a different
promise than the plan makes. 269 KB against a budget passing by 4× does not buy
the right to quietly narrow it; the lever is measured, named, and left up to
whoever is actually short of space.

**The backtrace machinery, 138 KB.** `panic = "abort"` stops unwinding but not
*formatting*: std still links gimli, addr2line and an inflate implementation so
that a panic can symbolise itself. Removing it needs `-Z build-std` with
`panic_immediate_abort`, which means nightly, which is a real cost to consumers
of a library that otherwise builds on stable. Noted, not taken.

## The size/speed trade has a price, and it is affordable

`mobile` is opt-level=z. Measured against the same code at opt-level=3, both
stripped, on the same phone:

| | opt-level=3 | opt-level=z | |
|---|---|---|---|
| arm64-v8a, dart | 2,879,840 | 1,925,824 | −33.1% |
| arm64-v8a, kotlin | 2,699,272 | 1,814,056 | −32.8% |
| armeabi-v7a, dart | 2,484,392 | 1,530,316 | −38.4% |
| armeabi-v7a, kotlin | 2,375,304 | 1,467,876 | −38.2% |
| hydration | 3.51 ms | 5.22 ms | +49% |
| p50, delete-the-top | 128 µs | 210 µs | +64% |
| **p99, delete-the-top** | **434 µs** | **584 µs** | **+35%** |

A third of the size for a third of the speed, and the slow side still passes p99
by 27×. Both numbers are inside both budgets, so at M0 this is a genuinely free
choice — which is worth knowing precisely because it will stop being free later,
and the exchange rate is now recorded rather than guessed at.

## 16 KB pages

Android 15 ships devices whose page size is 16 KB, and a shared object whose
`PT_LOAD` segments are aligned to 4 KB does not load on them — not slowly, not
at all. NDK r28 aligns to 16 KB by default; r27 needs
`-Wl,-z,max-page-size=16384` passed by hand.

`build.sh` checks every 64-bit library it produces rather than trusting the NDK
version, since the check costs one `readelf` and the failure it catches is total.
The 32-bit ABIs are reported as `n/a`: 16 KB pages are a 64-bit concern, and
flagging armeabi-v7a would report a failure that does not exist, which is worse
than not checking.

## Two things this found in the harness itself

**`rss()` would have under-reported by 4× on the phones that matter.** It read
`/proc/self/statm`, whose fields are page *counts*, and multiplied by a
hardcoded 4096 with a comment saying that was true of every target this project
builds for. It stopped being true the first time the harness ran on the platform
the criterion is about. It now reads `/proc/self/smaps_rollup`, which reports kB
so the page size never enters, and which reports `Anonymous` directly instead of
inferring it as resident minus shared. On the laptop the new metric agrees with
the old to 0.1 MB, so the previously published RSS numbers stand.

Worth noting what the failure mode was: not a crash, not a wrong-looking number,
but a number 4× too small — in the direction that makes a memory budget look
met.

**`cargo run --release -p solstice-bench` had stopped working.** Adding the S1
and S2 fixture binaries made the package ambiguous, and cargo refuses to guess.
The command is written in the crate docs, the root README and BENCHMARKS.md, and
it had been broken since the S1 commit. `default-run` fixes it.

## What this measures, and what it does not

`solstice-bench` is a plain binary with no Android dependencies, so it
cross-compiles and runs from `/data/local/tmp` against a real file on the
device's real flash. Everything above is therefore the real storage path, the
real CPU and the real scheduler.

What is missing is the UI. `p99 delta → pump returned` is not `p99 delta →
committed frame`: it stops when the engine has the diff, and the budget in plan
§5.1 is written for the frame. The gap contains the FFI copy S2 measured, the
decode S1 measured, and everything Flutter or Compose does afterwards — which is
the part no benchmark here can reach and the demo apps exist to close.

So the honest summary is that the engine half of the M0 gate passes on real
hardware with three orders of magnitude to spare on memory and one and a half on
latency, and that the remaining risk in the latency criterion is now
concentrated in code this project does not own.

## Reproducing

```sh
# Size, all ABIs, with the alignment check.
./spikes/s3-android/build.sh
./spikes/s3-android/build.sh --trim        # without SQLite's fts/rtree

# Kill criteria on a connected device. Prints what it ran on.
./spikes/s3-android/bench.sh
PROFILE=mobile ./spikes/s3-android/bench.sh        # the profile that ships
./spikes/s3-android/bench.sh --memory              # isolate storage
```

Needs `cargo install cargo-ndk`, `rustup target add aarch64-linux-android
armv7-linux-androideabi`, an NDK (r28 or newer preferred), and for `bench.sh`
exactly one device visible to `adb`.

Neither script writes anything into the repository, so there is nothing here to
`.gitignore`; both leave the device as they found it.

## Files

```
build.sh    cross-build both adapters for the Android ABIs; size + alignment table
bench.sh    cross-build the harness, push it, run the kill criteria on the device
```

## What S3 does not settle

- **iOS, macOS, Windows, Linux.** Plan §7 lists six target families and this
  covers one. The xcframework half of that risk is untouched, and UniFFI on
  Kotlin Multiplatform is its own open spike.
- **Prebuilt binaries on GitHub Releases**, which plan §7 names as the actual
  mitigation — "supaya konsumen tidak pernah perlu meng-compile Rust". Two
  scripts on one developer's machine is not that.
- **APK growth measured as an APK.** The per-ABI number here is the library it
  would contain. Nothing yet checks what Gradle and AGP do around it.
- **The frame.** Both halves of plan §5.1's latency criterion that involve a UI
  are still unmeasured, and the WAL checkpoint finding above is a reason to
  measure them rather than assume they follow.
- **The checkpoint fix.** Diagnosed, not repaired. Moving it off the engine
  thread is M1 work and needs a design, not a pragma.
