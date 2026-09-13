# flutter-issues

Plan §5.1's Flutter half of M0: a scrolling list over 100,000 issues and
1,000,000 comments, with a background driver mutating 200 rows/sec, measuring
how long a change takes to reach the glass.

Three of M0's kill criteria can only be produced here, and none of them by a
benchmark that stops at the FFI boundary:

- **p99 delta → committed frame** — the number plan §5.1 says the project's
  premise rests on.
- **decode of a 1000-row view, in Dart, on a phone** — spike S1's fallback,
  measured on arm64 rather than on a laptop.
- **jank on a mid-range device** — frame timings taken while the driver runs.

The results are in [`BENCHMARKS.md`](../../BENCHMARKS.md#the-flutter-demo--delta--committed-frame).
The short version: 23.4ms p99, which **fails** the 16ms budget, of which 24µs is
this engine and the rest is Flutter's frame scheduling. Read the decomposition
before reading the verdict.

## Running it

The app needs a seeded database on the device. It is 89 MB, so it is built
rather than committed:

```bash
cargo run --release -p solstice-bench --bin demo-fixture   # writes examples/fixtures/issues.db
cd examples/flutter-issues
flutter run --release -d $(adb get-serialno)               # once, to create the app directory
./push-fixture.sh                                          # then push the world into it
```

`push-fixture.sh` puts the fixture in the app's own external files directory,
which is the only place `adb push` can write and the app can read without asking
for a permission. It deletes any `-wal` and `-shm` siblings first: a WAL from a
previous push describes transactions against a file that is about to be
replaced, and SQLite will replay it over the new one, which is a corrupt
database presented as a working one.

Re-run `push-fixture.sh` between measurement runs. The driver mutates the
fixture in place and it does not undo itself.

**Release builds only for any number you intend to quote.** A debug build runs
Dart in the JIT and the accessor is the hot path.

### Switches

| `--dart-define=` | default | |
|---|---|---|
| `SOLSTICE_K=1000` | 50 | window size. 1000 is the plan's decode criterion |
| `SOLSTICE_PROJECT=7` | 7 | which project to subscribe to |
| `SOLSTICE_DB=/path` | the pushed fixture | run against some other file |
| `SOLSTICE_VERIFY=false` | true | turn off the per-delta sort check |

`SOLSTICE_VERIFY` is on by default because a demo that renders the wrong list
quickly is worth nothing. It walks all `k` rows after every delta asserting the
window is still ordered — the one invariant a host can check without the
engine's help, and the one that caught the bug described below. Turn it off when
the number being published is a latency; it measured as being below this
measurement's noise floor either way.

## Reading the HUD

Tap **churn 200/s** to start the driver, let it run, then tap the table icon in
the app bar. The report is copied to the clipboard *and* printed to the log, so
`adb logcat -s flutter` is the usual way to get it out.

```
p99 legs  apply 24µs · wait 12.5ms · pipeline 14.1ms · verifying
stream    5520 deltas here / 5520 sent · 50 rows here / 50 in the engine
```

The first line is the latency cut into the three legs it is made of; only
`apply` is this project's code. The second is the correctness check that
matters most: both halves of each pair are counted on opposite sides of the FFI
boundary from the same stream of changes, so a disagreement is a real bug and
not a rounding difference. The HUD turns red for a divergence, for deltas
arriving out of order and for a window that stopped being sorted.

## How the latency is actually measured

The two ends are on different threads, taken by different subsystems, with no
handle to pass between them. Flutter provides exactly the two hooks that join
them:

- `PlatformDispatcher.instance.frameData.frameNumber`, valid during build — the
  frame the change is going into.
- `FrameTiming.frameNumber`, the same number, reported after that frame has
  rasterized.

So a delta stamps a wall clock on arrival, the build that consumes it files that
stamp under the frame number being built, and the timings callback closes the
pair. When several deltas coalesce into one frame, every one of them gets its
own honest, larger time-to-glass — no sampling, no proxy, no "we assume it made
the next frame".

Every `FramePhase` except one is on a monotonic clock whose epoch the docs say
may not match `DateTime`'s. `rasterFinishWallTime` is the exception and exists
to be correlated with the system clock. Rather than trust that, the probe
measures the skew on the first frame it sees and refuses to report latencies at
all if it is implausible. A silently wrong number here would be worse than no
number, because it would be published.

See [`lib/src/probe.dart`](lib/src/probe.dart).

## What this app deliberately does not do

**No `AnimatedList`.** Plan §4.2 argues for positional diffs precisely so item
animations are possible, and they are — the diff this applies carries
`Moved{from,to}`. But an animation running at 200 rows/sec would put Flutter's
animation system inside every frame being measured, and the number wanted here
is the engine's. The diff still does its real work: the list is patched in
place, so scroll position is stable while rows move underneath it.

**No generated code.** M0 has no schema DSL and no codegen, so
[`lib/src/view.dart`](lib/src/view.dart) is the zero-copy accessor written by
hand, against the field numbers in `proto/solstice/v1/view.proto`. It is what
plan §4.1's fallback would generate. `lib/src/schema.dart` hardcodes the two
tables.

**No sync.** M0 is local-only. The driver writes through `mutate`, the same
single chokepoint plan §1.4 makes the whole IVM soundness argument rest on.

## The bug this app was written to find

An early run reported 62 key mismatches and a visibly unsorted list, with the
engine and the applier each already proved correct on their own — by
`crates/solstice-core/tests/churn_convergence.rs` and by
[`test/view_test.dart`](test/view_test.dart) respectively. Two correct halves
and a wrong whole.

What separated them was counting deltas on *both* sides of the boundary. 1,123
received against 1,123 sent said nothing was being lost in transit, which left
only "misapplied", which led to `adb logcat` and a `Bad state: wire type 6`
thrown from the middle of `apply`. The cause was one line:

```dart
p += varint();   // Dart evaluates the left operand first, so the `p` that
                 // `varint()` advanced past the length prefix is overwritten
```

Every skip of a length-delimited field therefore landed one or two bytes inside
the payload. The only such field in a delta is `Changed.cols`, and `cols` is
`[6]` whenever a comment is added or dropped — so the next byte read as a tag
was `0x06`, an invalid wire type, and the exception aborted the delta
half-applied.

Neither existing test could see it. The engine's tests decode with the Rust
decoder. The Dart tests only parsed fields the applier reads, and `apply` resets
its cursor at the end of each change, which hides a short skip of the last
field. `test/view_test.dart` now has both missing cases: a `cols` list that
reproduces the crash, and an unknown field placed *ahead* of the ones the
applier reads — which is plan §3.5's forward-compatibility promise and the
reason `skip` exists at all.
