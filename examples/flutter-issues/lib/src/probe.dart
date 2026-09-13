// Delta → committed frame, measured rather than approximated.
//
// This is the M0 kill criterion plan §5.1 puts first, and the one no headless
// benchmark can reach. `solstice-bench` measures "delta → pump returned" and
// spike S2 measures "mutate → delta on the stream"; both stop at the isolate's
// event loop. What plan §5.1 asks for is the number that ends on the glass.
//
// # How the two ends are joined
//
// The problem is that the two timestamps are taken on different threads by
// different subsystems, and there is no handle to pass between them. Flutter
// gives exactly the two hooks needed:
//
//   `PlatformDispatcher.instance.frameData.frameNumber`
//       valid during build — the frame the change is going into.
//   `FrameTiming.frameNumber`
//       the same number, reported after that frame has been rasterized.
//
// So a delta stamps a wall clock on arrival, the build that consumes it files
// that stamp under the frame number being built, and the timings callback
// closes the pair. No sampling, no proxy, no "we assume it made the next
// frame" — when several deltas coalesce into one frame, every one of them gets
// its own honest, larger, time-to-glass.
//
// # Why the wall clock, and why it is checked
//
// Every `FramePhase` except one is on a monotonic clock whose epoch the docs
// say "may not match [DateTime]'s". `rasterFinishWallTime` is the exception,
// and it exists to be correlated with the system clock. That correlation is
// the only thing that makes this measurement possible from Dart — so rather
// than trust it, the probe measures the skew on the first frame it sees and
// refuses to report latencies if it is implausible. A silently wrong number
// here would be worse than no number, because it would be published.

import 'dart:ui';

import 'package:flutter/scheduler.dart';

/// Everything the HUD and the report need, computed once.
///
/// This type exists because the probe was distorting its own measurement. The
/// HUD rebuilds on every delta — a hundred times a second — and each rebuild
/// read `p50`, `p99` and `max`, every one of which copies and sorts the whole
/// sample. At 2800 samples that is three sorts per frame of work that exists
/// only to draw the number, charged to the build it is reporting on. Adding
/// three more series made it visible: worst build went from 21ms to 30ms and
/// the count of frames over 16ms quadrupled, with nothing else changed.
///
/// So the sorting happens once a second in `_take`, and the widgets are handed
/// a plain record of ints. An instrument that perturbs what it measures is not
/// an instrument.
class ProbeSummary {
  final int samples;
  final int? p50, p99, max;
  final int? applyP99, waitP99, pipelineP99;
  final int frames, jankyFrames, overPeriod, deltas, orphaned;
  final Duration framePeriod, worstFrame, worstBuild, worstRaster;
  final bool clockUsable;
  final int? clockSkewUs;

  const ProbeSummary({
    required this.samples,
    required this.p50,
    required this.p99,
    required this.max,
    required this.applyP99,
    required this.waitP99,
    required this.pipelineP99,
    required this.frames,
    required this.jankyFrames,
    required this.overPeriod,
    required this.deltas,
    required this.orphaned,
    required this.framePeriod,
    required this.worstFrame,
    required this.worstBuild,
    required this.worstRaster,
    required this.clockUsable,
    required this.clockSkewUs,
  });
}

/// A sorted-on-demand latency sample plus the frame health around it.
class FrameProbe {
  /// One display refresh. Plan §5.1's budget is 16ms, which is 60Hz; the phone
  /// this runs on may be faster, and a frame that misses a 90Hz deadline is
  /// jank the user sees even though it is inside the plan's number. Both are
  /// reported: `budget` for the plan's verdict, the display's own period for
  /// whether it actually looked smooth.
  final Duration budget;
  final Duration framePeriod;

  FrameProbe({required this.budget, required this.framePeriod});

  /// Deltas that have arrived but have not yet been picked up by a build,
  /// flattened as `[arrived, applied, arrived, applied, …]`.
  final List<int> _pending = <int>[];

  /// Frame number → `[arrived, applied, painted, …]` for every delta in it.
  final Map<int, List<int>> _inFlight = <int, List<int>>{};

  final List<int> _latenciesUs = <int>[];

  // The same sample, cut into the three legs it is actually made of. A single
  // end-to-end number can only ever produce an argument about whose fault it
  // is; these three settle it. They sum to the total by construction, because
  // each one ends exactly where the next begins.
  //
  //   apply     the bytes arriving and being turned into rows, on this isolate
  //   wait      applied → the build that picks it up: scheduling and vsync
  //   pipeline  that build → the frame finishing on the GPU
  //
  // Only the first is this project's code. That is the point of measuring them
  // apart: plan §5.1's budget is end-to-end and stays end-to-end, but a verdict
  // that does not say which leg spends the time is not actionable.
  final List<int> _applyUs = <int>[];
  final List<int> _waitUs = <int>[];
  final List<int> _pipelineUs = <int>[];

  int frames = 0;
  int jankyFrames = 0;
  int overPeriod = 0;
  int deltas = 0;

  /// Deltas that were filed under a frame that never reported a timing.
  ///
  /// Not an error on its own — a build can be discarded — but a large number
  /// would mean the pairing above is not finding its other half, and that is
  /// the failure mode that would quietly bias the sample towards fast frames.
  int orphaned = 0;

  Duration worstFrame = Duration.zero;
  Duration worstBuild = Duration.zero;
  Duration worstRaster = Duration.zero;

  /// `DateTime.now()` minus `rasterFinishWallTime`, measured once.
  int? clockSkewUs;
  bool get clockUsable {
    final s = clockSkewUs;
    // A frame reported after it finished, by less than a second. Anything else
    // means the two clocks are not the same clock.
    return s != null && s >= -1000000 && s <= 1000000;
  }

  bool _listening = false;

  void start() {
    if (_listening) return;
    _listening = true;
    SchedulerBinding.instance.addTimingsCallback(_onTimings);
  }

  void stop() {
    if (!_listening) return;
    _listening = false;
    SchedulerBinding.instance.removeTimingsCallback(_onTimings);
  }

  /// Throw away the sample, keeping the clock check.
  ///
  /// The first seconds after launch contain a cold JIT, an unexpanded heap, a
  /// shader warm-up and the hydration itself. Publishing those alongside steady
  /// state would be measuring the launch and calling it the engine.
  void reset() {
    _pending.clear();
    _inFlight.clear();
    _latenciesUs.clear();
    _applyUs.clear();
    _waitUs.clear();
    _pipelineUs.clear();
    frames = 0;
    jankyFrames = 0;
    overPeriod = 0;
    deltas = 0;
    orphaned = 0;
    worstFrame = Duration.zero;
    worstBuild = Duration.zero;
    worstRaster = Duration.zero;
  }

  /// Called the moment a `ViewDelta` lands on the isolate, before it is applied.
  void deltaArrived() {
    deltas++;
    final now = DateTime.now().microsecondsSinceEpoch;
    _pending..add(now)..add(now);
  }

  /// Called once the delta has been turned into rows, to close the apply leg.
  void deltaApplied() {
    if (_pending.isEmpty) return;
    _pending[_pending.length - 1] = DateTime.now().microsecondsSinceEpoch;
  }

  /// Called from `build`, to attach everything pending to the frame being built.
  void willPaint() {
    if (_pending.isEmpty) return;
    final n = PlatformDispatcher.instance.frameData.frameNumber;
    // -1 is `FrameData`'s "not provided". Filing under it would pool unrelated
    // deltas into one bucket that no timing will ever match.
    if (n < 0) {
      orphaned += _pending.length ~/ 2;
      _pending.clear();
      return;
    }
    final painted = DateTime.now().microsecondsSinceEpoch;
    final waiting = _inFlight[n] ??= <int>[];
    for (var i = 0; i < _pending.length; i += 2) {
      waiting..add(_pending[i])..add(_pending[i + 1])..add(painted);
    }
    _pending.clear();
  }

  void _onTimings(List<FrameTiming> timings) {
    for (final t in timings) {
      frames++;
      if (t.totalSpan > budget) jankyFrames++;
      if (t.totalSpan > framePeriod) overPeriod++;
      if (t.totalSpan > worstFrame) worstFrame = t.totalSpan;
      if (t.buildDuration > worstBuild) worstBuild = t.buildDuration;
      if (t.rasterDuration > worstRaster) worstRaster = t.rasterDuration;

      final done = t.timestampInMicroseconds(FramePhase.rasterFinishWallTime);
      clockSkewUs ??= DateTime.now().microsecondsSinceEpoch - done;

      final waiting = _inFlight.remove(t.frameNumber);
      if (waiting == null) continue;
      if (!clockUsable) continue;
      for (var i = 0; i < waiting.length; i += 3) {
        final t0 = waiting[i], t1 = waiting[i + 1], t2 = waiting[i + 2];
        final us = done - t0;
        // A negative latency means the frame finished before the delta that is
        // supposedly in it, which can only be a pairing mistake. Dropping it
        // silently would flatter the percentiles, so it is counted instead.
        if (us < 0) {
          orphaned++;
        } else {
          _latenciesUs.add(us);
          _applyUs.add(t1 - t0);
          _waitUs.add(t2 - t1);
          _pipelineUs.add(done - t2);
        }
      }
    }
    _sweep(timings.isEmpty ? 0 : timings.last.frameNumber);
  }

  /// Forget deltas filed under frames that are now well in the past.
  ///
  /// Without this the map grows for the life of the app whenever a build is
  /// discarded before it reaches the rasterizer. 120 frames is over a second at
  /// any refresh rate this runs at, which is far longer than a timing report is
  /// ever delayed.
  void _sweep(int newest) {
    if (_inFlight.length < 64) return;
    final cutoff = newest - 120;
    _inFlight.removeWhere((n, waiting) {
      if (n > cutoff) return false;
      orphaned += waiting.length ~/ 3;
      return true;
    });
  }

  int get samples => _latenciesUs.length;

  /// Sort the four series and read every number off them.
  ///
  /// Four sorts, and they belong here rather than behind four getters so that
  /// the cost is paid once per call and the caller can see that it is. Call it
  /// on a timer, never on the delta path.
  ///
  /// The three legs are each taken at their own percentile, so they do not add
  /// up to `p99` and are not meant to: the delta with the worst wait is rarely
  /// the one with the worst apply. Each answers "how bad does this leg get",
  /// not "what did the p99 delta do" — the latter would describe one unlucky
  /// event and call it the shape of the system.
  ProbeSummary summarize() {
    final sorted = List<int>.from(_latenciesUs)..sort();
    return ProbeSummary(
      samples: sorted.length,
      p50: _at(sorted, 0.50),
      p99: _at(sorted, 0.99),
      max: sorted.isEmpty ? null : sorted.last,
      applyP99: _at(List<int>.from(_applyUs)..sort(), 0.99),
      waitP99: _at(List<int>.from(_waitUs)..sort(), 0.99),
      pipelineP99: _at(List<int>.from(_pipelineUs)..sort(), 0.99),
      frames: frames,
      jankyFrames: jankyFrames,
      overPeriod: overPeriod,
      deltas: deltas,
      orphaned: orphaned,
      framePeriod: framePeriod,
      worstFrame: worstFrame,
      worstBuild: worstBuild,
      worstRaster: worstRaster,
      clockUsable: clockUsable,
      clockSkewUs: clockSkewUs,
    );
  }

  static int? _at(List<int> sorted, double p) =>
      sorted.isEmpty ? null : sorted[((sorted.length - 1) * p).round()];
}

String fmtUs(int? us) {
  if (us == null) return '—';
  if (us >= 10000) return '${(us / 1000).toStringAsFixed(1)}ms';
  if (us >= 1000) return '${(us / 1000).toStringAsFixed(2)}ms';
  return '$usµs';
}

String fmtDuration(Duration d) => fmtUs(d.inMicroseconds);
