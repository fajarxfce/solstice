// Plan §5.1's Flutter half of M0.
//
// > Aplikasi Flutter dan aplikasi Compose merender list yang di-scroll, dengan
// > thread latar memutasi ~200 row/detik.
//
// Three numbers from the kill-criteria table can only be produced here, and
// none of them by a benchmark that stops at the FFI boundary:
//
//   p99 delta → committed frame   `lib/src/probe.dart`
//   decode of a 1000-row view     the hydration below, on the phone
//   jank on a mid-range device    frame timings, while the driver runs
//
// Everything else — engine latency, operator state, RSS, TopK refills — has a
// number in `BENCHMARKS.md` already. What has never been measured is what
// happens between a `ViewDelta` arriving on the isolate and the pixels being
// on the glass, and that is the half of the question plan §5.1 says the
// project's premise rests on.
//
// # What this app deliberately does not do
//
// No `AnimatedList`. Plan §4.2 argues for positional diffs precisely so item
// animations are possible, and they are — the diff this applies carries
// `Moved{from,to}`. But an animation running at 200 rows/sec would put
// Flutter's animation system inside every frame measured, and the number
// wanted here is the engine's. The diff still does its real work: the list is
// patched in place, so scroll position is stable while rows move underneath.

import 'dart:async';
import 'dart:ui' as ui;

import 'package:flutter/foundation.dart' show ValueListenable;
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_rust_bridge/flutter_rust_bridge_for_generated.dart'
    show ExternalLibrary;
import 'package:path_provider/path_provider.dart';

import 'src/driver.dart';
import 'src/probe.dart';
import 'src/rust/api.dart';
import 'src/rust/frb_generated.dart';
import 'src/schema.dart';
import 'src/view.dart';
import 'src/world.dart';

/// Override with `--dart-define=SOLSTICE_DB=/path/to/issues.db`.
const _dbOverride = String.fromEnvironment('SOLSTICE_DB');
const _project = int.fromEnvironment('SOLSTICE_PROJECT', defaultValue: defaultProject);
const _k = int.fromEnvironment('SOLSTICE_K', defaultValue: defaultK);

/// Walk the whole window after every delta and assert it is still sorted.
///
/// On by default, because a demo that renders the wrong list quickly is worth
/// nothing and this is the only invariant the host can check without the
/// engine's help. Turn it off with
/// `--dart-define=SOLSTICE_VERIFY=false` when the number being published is a
/// latency: the check is O(k) on the delta path and it is not something a real
/// application would ever run. Both numbers belong in `BENCHMARKS.md` — the
/// verified one says the app is correct, the unverified one says what it costs.
const _verify = bool.fromEnvironment('SOLSTICE_VERIFY', defaultValue: true);

void main() {
  runApp(const DemoApp());
}

class DemoApp extends StatelessWidget {
  const DemoApp({super.key});

  @override
  Widget build(BuildContext context) => MaterialApp(
        title: 'Solstice — issues',
        debugShowCheckedModeBanner: false,
        theme: ThemeData(
          useMaterial3: true,
          colorScheme: ColorScheme.fromSeed(
            seedColor: const Color(0xFF6C8CFF),
            brightness: Brightness.dark,
          ),
        ),
        home: const IssuesPage(),
      );
}

class IssuesPage extends StatefulWidget {
  const IssuesPage({super.key});

  @override
  State<IssuesPage> createState() => _IssuesPageState();
}

class _IssuesPageState extends State<IssuesPage> {
  Database? _db;

  /// Held, not read. There is no `unsubscribe` on the handle — dropping it
  /// *is* the unsubscribe, because the Rust side tears the pipeline down in
  /// `Drop`. A local that went out of scope would leave Dart's finalizer free
  /// to collect a subscription this app is still rendering, and the list would
  /// simply stop updating with nothing in any log to say why.
  // ignore: unused_field
  Subscription? _sub;

  IssueView? _view;
  Driver? _driver;

  /// Bumped once per delta, listened to by the list and by nothing else.
  ///
  /// `setState` on this page would be correct and was what this app did, but it
  /// rebuilds the HUD too — ninety times a second, to redraw numbers that only
  /// change once a second. Scoping the rebuild to the widget the delta actually
  /// changes is what a real host would do (plan §4.4's `SolsticeQueryBuilder`
  /// is exactly this), and it is worth doing here because the alternative is
  /// publishing a frame time that measures the instrumentation.
  final ValueNotifier<int> _tick = ValueNotifier<int>(0);
  StreamSubscription<Uint8List>? _sink;
  FrameProbe? _probe;
  Timer? _hud;

  String? _error;

  // What the hydration cost, which is S1's kill criterion measured on device
  // rather than in a harness.
  Duration _openTook = Duration.zero;
  Duration _subscribeTook = Duration.zero;
  Duration _initialTook = Duration.zero;
  Duration _hydrateTook = Duration.zero;
  int _initialBytes = 0;

  DeltaCounts _totals = const DeltaCounts();
  int _deltasForOtherSubs = 0;
  int _disorders = 0;
  String? _firstDisorder;

  /// Deltas that reached this isolate, counted before anything can filter one.
  ///
  /// Shown next to `EngineStats.events`, which counts what the engine handed to
  /// the sink. The engine and the applier have each been proved correct on
  /// their own — `crates/solstice-core/tests/churn_convergence.rs` and
  /// `test/view_test.dart` — so if the view still drifts, the bytes are being
  /// lost or reordered *between* them, and these two counters are the only
  /// place in the system where that becomes visible.
  int _deltasSeen = 0;

  /// Deltas whose `version` did not advance.
  ///
  /// The engine's version is global and strictly increasing, and a delta is
  /// stamped with the version of the pump that produced it. So a version that
  /// repeats or goes backwards means the stream arrived out of order — the one
  /// failure that would make a correct engine and a correct applier still
  /// disagree. It also catches the subscribe/`initial()` race, where a delta
  /// emitted before the snapshot was taken gets replayed on top of it.
  int _versionRegressions = 0;

  /// The HUD's view of the world, refreshed on a timer rather than on every
  /// delta, and holding plain numbers rather than the live probe.
  ///
  /// Both halves of that matter. `FrameProbe.summarize` sorts its whole sample,
  /// so calling it at the delta rate would make the HUD the most expensive
  /// thing on the screen — and it did, for as long as the widgets read the
  /// probe directly: a build that rebuilds a hundred times a second was paying
  /// for four sorts of a few thousand ints each time, and charging it to the
  /// frame timings it was drawing.
  _Snapshot? _snapshot;

  @override
  void initState() {
    super.initState();
    _boot();
  }

  @override
  void dispose() {
    _hud?.cancel();
    _probe?.stop();
    _tick.dispose();
    // Order matters, and spike S2 found it the hard way: clear the sink first,
    // let the engine see it, and only then cancel the Dart subscription. The
    // other order leaves the engine holding a port that has closed, and
    // `flutter_rust_bridge` complains on the next event.
    unawaited(_shutdown());
    super.dispose();
  }

  Future<void> _shutdown() async {
    await _driver?.stop();
    _db?.clearEventSink();
    await Future<void>.delayed(const Duration(milliseconds: 50));
    await _sink?.cancel();
  }

  Future<void> _boot() async {
    try {
      // Named explicitly rather than left to the generated loader, whose
      // default `ioDirectory` points at a host build tree. On Android the
      // library is inside the APK and the system linker finds it by soname.
      await RustLib.init(
        externalLibrary: ExternalLibrary.open('libsolstice_ffi_dart.so'),
      );

      final path = await _fixturePath();

      final sw = Stopwatch()..start();
      final db = Database.open(path: path);
      _openTook = sw.elapsed;

      // Installed *before* subscribing. `set_event_sink` reaches the engine
      // thread as a command; a subscription created first could produce a
      // delta that arrived before the sink did, and it would be lost with no
      // trace but a row that stopped updating.
      final view = IssueView();
      _sink = db.setEventSink().listen(_onDelta);

      sw.reset();
      final sub = db.subscribe(
        queryIr: buildQuery(project: _project, k: _k, comments: defaultComments),
      );
      _subscribeTook = sw.elapsed;
      view.subId = sub.subId().toInt();

      sw.reset();
      final initial = sub.initial();
      _initialTook = sw.elapsed;

      sw.reset();
      view.hydrate(initial);
      _hydrateTook = sw.elapsed;
      _initialBytes = initial.length;

      final rate = ui.PlatformDispatcher.instance.views.isEmpty
          ? 60.0
          : ui.PlatformDispatcher.instance.views.first.display.refreshRate;
      final probe = FrameProbe(
        // Plan §5.1's budget, written for a 60Hz frame.
        budget: const Duration(milliseconds: 16),
        framePeriod: Duration(microseconds: (1e6 / (rate <= 0 ? 60 : rate)).round()),
      );
      probe.start();

      setState(() {
        _db = db;
        _sub = sub;
        _view = view;
        _probe = probe;
        _driver = Driver(db: db, view: view, project: _project);
        _snapshot = _take();
      });

      _hud = Timer.periodic(const Duration(seconds: 1), (_) {
        if (mounted) setState(() => _snapshot = _take());
      });
    } catch (e) {
      setState(() => _error = '$e');
    }
  }

  /// Where `push-fixture.sh` put the world.
  Future<String> _fixturePath() async {
    if (_dbOverride.isNotEmpty) return _dbOverride;
    final dir = await getExternalStorageDirectory();
    if (dir == null) {
      throw StateError('no external storage directory on this device');
    }
    return '${dir.path}/issues.db';
  }

  void _onDelta(Uint8List bytes) {
    _deltasSeen++;
    final view = _view;
    if (view == null) return;
    // Demultiplex first. One stream serves the whole database (plan §4.2), and
    // a host with two lists open would be handed both. This app has one, so
    // anything else is a bug worth counting rather than a case to handle.
    if (IssueView.subIdOf(bytes) != view.subId) {
      _deltasForOtherSubs++;
      return;
    }
    _probe?.deltaArrived();
    final wasAt = view.version;
    final counts = view.apply(bytes);
    // Stamped here and not after the check below: the check is this demo's own
    // scaffolding, and charging it to the engine's apply leg would publish a
    // number no application will ever pay.
    _probe?.deltaApplied();
    if (view.version <= wasAt) _versionRegressions++;
    _totals = _totals + counts;
    if (_verify) _checkOrder(view, bytes);
    // Coalesced by the scheduler: a hundred of these a second still produce at
    // most one build per frame.
    _tick.value++;
  }

  /// The one invariant this app can check on its own, checked on every delta.
  ///
  /// The query is `ORDER BY priority DESC`, ties broken by key, so a list that
  /// is not in that order means the host and the engine have diverged — and the
  /// point of checking here rather than reading it off the screen is that the
  /// *first* offending delta is the only one that says anything. Every later
  /// one is describing a list the engine no longer recognises.
  void _checkOrder(IssueView view, Uint8List bytes) {
    for (var i = 1; i < view.length; i++) {
      final a = view[i - 1], b = view[i];
      final pa = a.priority ?? -1, pb = b.priority ?? -1;
      if (pa > pb || (pa == pb && a.id < b.id)) continue;
      _disorders++;
      if (_firstDisorder != null) return;
      _firstDisorder = 'row $i (#${b.id} p$pb) after #${a.id} p$pa';
      debugPrint('DISORDER at v${view.version}: $_firstDisorder\n'
          'list: ${[
        for (var j = 0; j < view.length; j++) '${view[j].id}/${view[j].priority}'
      ].join(' ')}\n'
          'delta: ${bytes.map((x) => x.toRadixString(16).padLeft(2, '0')).join()}');
      return;
    }
  }

  _Snapshot _take() {
    final probe = _probe;
    final driver = _driver;
    return _Snapshot(
      probe: probe?.summarize(),
      driver: driver?.stats,
      running: driver?.running ?? false,
      rate: driver?.rowsPerSecond ?? 0,
      stats: _db?.stats(),
      rows: _view?.length ?? 0,
      version: _view?.version ?? 0,
      totals: _totals,
      strays: _deltasForOtherSubs,
      disorders: _disorders,
      firstDisorder: _firstDisorder,
      deltasSeen: _deltasSeen,
      versionRegressions: _versionRegressions,
    );
  }

  void _toggleDriver() {
    final driver = _driver;
    if (driver == null) return;
    setState(() {
      if (driver.running) {
        unawaited(driver.stop());
      } else {
        // Measure steady state, not the first seconds of it. Starting the
        // driver resets the probe so the hydration, the shader warm-up and the
        // cold heap are not in the sample that gets published.
        _probe?.reset();
        _totals = const DeltaCounts();
        driver.start();
      }
      _snapshot = _take();
    });
  }

  @override
  Widget build(BuildContext context) {
    final view = _view;
    final snapshot = _snapshot;

    return Scaffold(
      appBar: AppBar(
        title: Text('project $_project · top $_k by priority'),
        actions: [
          IconButton(
            tooltip: 'copy the markdown table',
            onPressed: snapshot == null ? null : () => _copyReport(snapshot),
            icon: const Icon(Icons.table_chart_outlined),
          ),
        ],
      ),
      body: _error != null
          ? _ErrorPane(message: _error!)
          : view == null || snapshot == null
              ? const Center(child: CircularProgressIndicator())
              : Column(
                  children: [
                    _Hud(
                      snapshot: snapshot,
                      openTook: _openTook,
                      subscribeTook: _subscribeTook,
                      initialTook: _initialTook,
                      hydrateTook: _hydrateTook,
                      initialBytes: _initialBytes,
                    ),
                    const Divider(height: 1),
                    Expanded(
                      child: _IssueList(
                        view: view,
                        tick: _tick,
                        // Every delta that has arrived since the last build is
                        // going into *this* frame. Filing it from the build
                        // that consumes it is what joins the two clocks; see
                        // `probe.dart`.
                        onBuild: () => _probe?.willPaint(),
                      ),
                    ),
                  ],
                ),
      floatingActionButton: _driver == null
          ? null
          : FloatingActionButton.extended(
              onPressed: _toggleDriver,
              icon: Icon(_driver!.running ? Icons.stop : Icons.play_arrow),
              label: Text(_driver!.running
                  ? 'stop ${_driver!.rowsPerSecond}/s'
                  : 'churn ${_driver!.rowsPerSecond}/s'),
            ),
    );
  }

  void _copyReport(_Snapshot s) {
    final text = s.markdown(
      project: _project,
      k: _k,
      openTook: _openTook,
      subscribeTook: _subscribeTook,
      initialTook: _initialTook,
      hydrateTook: _hydrateTook,
      initialBytes: _initialBytes,
    );
    // Both, on purpose: the clipboard is for a phone in the hand, and the log
    // is for `flutter run` — which is where the number actually gets copied
    // into `BENCHMARKS.md` from.
    Clipboard.setData(ClipboardData(text: text));
    debugPrint('\n$text');
    ScaffoldMessenger.of(context).showSnackBar(
      const SnackBar(content: Text('copied — also printed to the log')),
    );
  }
}

/// Everything the HUD shows, sampled at one instant.
class _Snapshot {
  final ProbeSummary? probe;
  final DriverStats? driver;
  final bool running;
  final int rate;
  final EngineStats? stats;
  final int rows;
  final int version;
  final DeltaCounts totals;
  final int strays;

  /// Deltas after which the list was not in the order the query asked for.
  /// Must be zero; see `_IssuesPageState._checkOrder`.
  final int disorders;
  final String? firstDisorder;

  /// Deltas the isolate received, against `EngineStats.events` sent.
  final int deltasSeen;
  final int versionRegressions;

  const _Snapshot({
    required this.probe,
    required this.driver,
    required this.running,
    required this.rate,
    required this.stats,
    required this.rows,
    required this.version,
    required this.totals,
    required this.strays,
    required this.disorders,
    required this.firstDisorder,
    required this.deltasSeen,
    required this.versionRegressions,
  });

  String markdown({
    required int project,
    required int k,
    required Duration openTook,
    required Duration subscribeTook,
    required Duration initialTook,
    required Duration hydrateTook,
    required int initialBytes,
  }) {
    final p = probe;
    final d = driver;
    final b = StringBuffer()
      ..writeln('# Solstice — Flutter demo')
      ..writeln()
      ..writeln('project $project · top $k by priority · $defaultComments '
          'comments each · $rows rows on screen · v$version')
      ..writeln()
      ..writeln('| what | measured | budget | |')
      ..writeln('|---|---|---|---|')
      ..writeln('| open | ${fmtDuration(openTook)} | — | — |')
      ..writeln('| subscribe | ${fmtDuration(subscribeTook)} | — | — |')
      ..writeln('| initial() — ${_bytes(initialBytes)} | '
          '${fmtDuration(initialTook)} | — | — |')
      ..writeln('| index $rows rows (lazy accessor) | '
          '${fmtDuration(hydrateTook)} | < 5ms | '
          '${hydrateTook.inMicroseconds < 5000 ? "PASS" : "**FAIL**"} |');

    if (p != null) {
      final p99 = p.p99;
      b
        ..writeln('| p50 delta → committed frame | ${fmtUs(p.p50)} | — | — |')
        ..writeln('| **p99 delta → committed frame** | ${fmtUs(p99)} | < 16ms | '
            '${p99 == null ? "—" : (p99 < 16000 ? "PASS" : "**FAIL**")} |')
        ..writeln('| max delta → committed frame | ${fmtUs(p.max)} | — | — |')
        // The three legs of that same p99, each taken at its own p99. They do
        // not add up to the line above and are not meant to: the question they
        // answer is which leg is capable of being slow, not what one unlucky
        // delta did.
        ..writeln('| ⤷ p99 apply (decode + index, this isolate)'
            '${_verify ? "" : " *"} | ${fmtUs(p.applyP99)} '
            '| — | — |')
        ..writeln('| ⤷ p99 wait (scheduling + vsync) | '
            '${fmtUs(p.waitP99)} | — | — |')
        ..writeln('| ⤷ p99 pipeline (build → raster done) | '
            '${fmtUs(p.pipelineP99)} | — | — |')
        ..writeln('| frames over 16ms | ${p.jankyFrames} / ${p.frames} | — | — |')
        ..writeln('| frames over the display period '
            '(${fmtDuration(p.framePeriod)}) | ${p.overPeriod} / ${p.frames} '
            '| — | — |')
        ..writeln('| worst frame | ${fmtDuration(p.worstFrame)} | — | — |')
        ..writeln('| worst build / raster | ${fmtDuration(p.worstBuild)} / '
            '${fmtDuration(p.worstRaster)} | — | — |');
    }
    if (d != null && d.transactions > 0) {
      b
        ..writeln('| driver | $rate rows/s requested, '
            '${d.operations} ops in ${d.transactions} txns | — | — |')
        ..writeln('| encode on the UI isolate | '
            '${fmtUs(d.encodeUs ~/ d.transactions)} / txn | — | — |')
        ..writeln('| await mutateAsync | '
            '${fmtUs(d.awaitUs ~/ d.transactions)} / txn | — | — |')
        ..writeln('| refused writes | ${d.refused} | — | — |');
    }
    final s = stats;
    if (s != null) {
      b
        ..writeln()
        ..writeln('engine: ${s.subscriptions} sub(s) · ${s.viewRows} view rows '
            '· ${_bytes(s.viewBytes.toInt())} view · '
            '${_bytes(s.graphBytes.toInt())} graph · ${s.mutations} mutations '
            '· ${s.events} events · v${s.version}')
        ..writeln('refills: ${s.windowRefills} window '
            '(${s.windowRefillRows} rows) · ${s.childRefills} child '
            '(${s.childRefillRows} rows)')
        ..writeln('applied: $totals · ${totals.keyMismatches} key mismatch · '
            '${totals.outOfRange} out of range · $strays delta(s) for another '
            'subscription')
        // The two row counts are computed on opposite sides of the FFI
        // boundary from the same stream of changes. They disagreeing is a
        // correctness failure, and one no latency number makes up for.
        ..writeln('rows: $rows here, ${s.viewRows} in the engine'
            '${rows == s.viewRows.toInt() ? "" : "  ← DIVERGED"}')
        ..writeln('deltas: $deltasSeen received, ${s.events} sent'
            '${deltasSeen == s.events.toInt() ? "" : "  ← LOST IN TRANSIT"}'
            '${versionRegressions > 0 ? " · $versionRegressions out of order" : ""}')
        ..writeln('order: ${_verify ? (disorders == 0 ? "sorted after every delta" : "$disorders delta(s) left the window unsorted — first at $firstDisorder") : "not checked (SOLSTICE_VERIFY=false)"}');
    }
    if (!_verify) {
      b
        ..writeln()
        ..writeln('\\* built with `SOLSTICE_VERIFY=false`: the per-delta sort '
            'check is off, so the apply leg is the cost an application would '
            'actually pay. Correctness in this configuration is asserted by '
            'the verified run, not by this one.');
    }
    if (p != null && !p.clockUsable) {
      b
        ..writeln()
        ..writeln('**No latency reported**: `rasterFinishWallTime` is '
            '${p.clockSkewUs}µs from `DateTime.now()`, so the two are not the '
            'same clock on this device and the pairing would be meaningless.');
    }
    return b.toString();
  }
}

String _bytes(int n) {
  if (n >= 1 << 20) return '${(n / (1 << 20)).toStringAsFixed(1)} MB';
  if (n >= 1024) return '${(n / 1024).toStringAsFixed(1)} KB';
  return '$n B';
}

class _Hud extends StatelessWidget {
  final _Snapshot snapshot;
  final Duration openTook;
  final Duration subscribeTook;
  final Duration initialTook;
  final Duration hydrateTook;
  final int initialBytes;

  const _Hud({
    required this.snapshot,
    required this.openTook,
    required this.subscribeTook,
    required this.initialTook,
    required this.hydrateTook,
    required this.initialBytes,
  });

  @override
  Widget build(BuildContext context) {
    final p = snapshot.probe;
    final d = snapshot.driver;
    final s = snapshot.stats;
    final theme = Theme.of(context);
    final p99 = p?.p99;

    return Container(
      width: double.infinity,
      padding: const EdgeInsets.fromLTRB(12, 8, 12, 10),
      color: theme.colorScheme.surfaceContainerHighest,
      child: DefaultTextStyle(
        style: theme.textTheme.bodySmall!.copyWith(
          fontFamily: 'monospace',
          fontFamilyFallback: const ['Roboto Mono', 'monospace'],
        ),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Row(
              crossAxisAlignment: CrossAxisAlignment.end,
              children: [
                _Big(
                  label: 'p99 delta → frame',
                  value: fmtUs(p99),
                  ok: p99 == null ? null : p99 < 16000,
                ),
                const SizedBox(width: 16),
                _Big(
                  label: 'p50',
                  value: fmtUs(p?.p50),
                ),
                const SizedBox(width: 16),
                _Big(
                  label: 'max',
                  value: fmtUs(p?.max),
                ),
                const Spacer(),
                _Big(
                  label: 'samples',
                  value: '${p?.samples ?? 0}',
                ),
              ],
            ),
            const SizedBox(height: 6),
            if (p != null && !p.clockUsable)
              Text(
                'rasterFinishWallTime is ${p.clockSkewUs}µs from DateTime.now() '
                '— latency not reported on this device',
                style: TextStyle(color: theme.colorScheme.error),
              ),
            Text('hydrate  ${snapshot.rows} rows · open ${fmtDuration(openTook)}'
                ' · subscribe ${fmtDuration(subscribeTook)}'
                ' · initial() ${fmtDuration(initialTook)}'
                ' (${_bytes(initialBytes)})'
                ' · index ${fmtDuration(hydrateTook)}'),
            if (p != null && p.samples > 0)
              Text('p99 legs apply ${fmtUs(p.applyP99)} · '
                  'wait ${fmtUs(p.waitP99)} · '
                  'pipeline ${fmtUs(p.pipelineP99)}'
                  '${_verify ? " · verifying" : ""}'),
            if (p != null)
              Text('frames   ${p.frames} · ${p.jankyFrames} over 16ms · '
                  '${p.overPeriod} over ${fmtDuration(p.framePeriod)} · '
                  'worst ${fmtDuration(p.worstFrame)} '
                  '(build ${fmtDuration(p.worstBuild)}, '
                  'raster ${fmtDuration(p.worstRaster)})'),
            Text('applied  ${snapshot.totals}'
                '${snapshot.strays > 0 ? " · ${snapshot.strays} stray" : ""}'
                '${snapshot.totals.keyMismatches > 0 ? " · ${snapshot.totals.keyMismatches} KEY MISMATCH" : ""}'
                '${snapshot.totals.outOfRange > 0 ? " · ${snapshot.totals.outOfRange} OUT OF RANGE" : ""}'
                '${(p?.orphaned ?? 0) > 0 ? " · ${p!.orphaned} unpaired" : ""}'),
            if (d != null)
              Text('driver   ${snapshot.running ? "running" : "stopped"} '
                  '${snapshot.rate}/s · ${d.operations} ops / '
                  '${d.transactions} txns · ${d.refused} refused'
                  '${d.transactions > 0 ? " · encode ${fmtUs(d.encodeUs ~/ d.transactions)}"
                      " · await ${fmtUs(d.awaitUs ~/ d.transactions)}" : ""}'),
            if (s != null)
              Text('engine   v${s.version} · ${s.mutations} mutations · '
                  '${s.events} events · ${_bytes(s.viewBytes.toInt())} view + '
                  '${_bytes(s.graphBytes.toInt())} graph · '
                  '${s.windowRefills} refills (${s.windowRefillRows} rows)'),
            // Two independent answers to the same two questions — how many
            // rows are in this view, and how many diffs described it — one
            // from each side of the FFI boundary. Shown side by side because
            // a drift between them localises the bug to the delivery path
            // instantly, and neither side's own tests can see it.
            if (s != null)
              Text('stream   ${snapshot.deltasSeen} deltas here / ${s.events} '
                  'sent · ${snapshot.rows} rows here / ${s.viewRows} in the '
                  'engine'),
            if (s != null &&
                (s.viewRows.toInt() != snapshot.rows ||
                    snapshot.versionRegressions > 0))
              Text(
                [
                  if (s.viewRows.toInt() != snapshot.rows) 'ROWS DIVERGED',
                  if (snapshot.versionRegressions > 0)
                    '${snapshot.versionRegressions} DELTAS OUT OF ORDER',
                ].join(' · '),
                style: TextStyle(color: theme.colorScheme.error),
              ),
            if (snapshot.disorders > 0)
              Text(
                '${snapshot.disorders} UNSORTED · first: ${snapshot.firstDisorder}',
                maxLines: 2,
                overflow: TextOverflow.ellipsis,
                style: TextStyle(color: theme.colorScheme.error),
              ),
            if (d?.lastError != null)
              Text(
                'last refusal: ${d!.lastError}',
                maxLines: 1,
                overflow: TextOverflow.ellipsis,
                style: TextStyle(color: theme.colorScheme.error),
              ),
          ],
        ),
      ),
    );
  }
}

class _Big extends StatelessWidget {
  final String label;
  final String value;
  final bool? ok;

  const _Big({required this.label, required this.value, this.ok});

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final colour = switch (ok) {
      true => const Color(0xFF6BD08A),
      false => theme.colorScheme.error,
      null => theme.colorScheme.onSurface,
    };
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      mainAxisSize: MainAxisSize.min,
      children: [
        Text(label, style: theme.textTheme.labelSmall),
        Text(
          value,
          style: theme.textTheme.titleMedium!.copyWith(
            color: colour,
            fontFeatures: const [ui.FontFeature.tabularFigures()],
          ),
        ),
      ],
    );
  }
}

class _IssueList extends StatelessWidget {
  final IssueView view;
  final ValueListenable<int> tick;
  final VoidCallback onBuild;

  const _IssueList({
    required this.view,
    required this.tick,
    required this.onBuild,
  });

  @override
  Widget build(BuildContext context) => ValueListenableBuilder<int>(
        valueListenable: tick,
        builder: (context, _, _) {
          onBuild();
          return ListView.builder(
            // The scroll position that matters. `ListView.builder` paints about
            // eight of these, which is the whole argument for the lazy
            // accessor: the other 42 rows are indexed and never decoded.
            itemCount: view.length,
            itemExtent: 96,
            itemBuilder: (context, i) {
              if (i >= view.length) return const SizedBox.shrink();
              return _IssueTile(row: view[i], position: i);
            },
          );
        },
      );
}

class _IssueTile extends StatelessWidget {
  final IssueRow row;
  final int position;

  const _IssueTile({required this.row, required this.position});

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final comments = row.comments;
    return Padding(
      padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 6),
      child: Row(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          SizedBox(
            width: 44,
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Text(
                  '${row.priority ?? "—"}',
                  style: theme.textTheme.titleMedium!.copyWith(
                    fontFeatures: const [ui.FontFeature.tabularFigures()],
                  ),
                ),
                Text('#$position', style: theme.textTheme.labelSmall),
              ],
            ),
          ),
          Expanded(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Text(
                  row.title,
                  maxLines: 1,
                  overflow: TextOverflow.ellipsis,
                  style: theme.textTheme.titleSmall,
                ),
                Text(
                  'id ${row.id} · updated ${row.updatedAt} · '
                  '${comments.length} comment(s)',
                  style: theme.textTheme.labelSmall,
                ),
                for (var c = 0; c < comments.length && c < 2; c++)
                  Text(
                    '${comments[c].author}: ${comments[c].body}',
                    maxLines: 1,
                    overflow: TextOverflow.ellipsis,
                    style: theme.textTheme.bodySmall!.copyWith(
                      color: theme.colorScheme.onSurfaceVariant,
                    ),
                  ),
              ],
            ),
          ),
        ],
      ),
    );
  }
}

class _ErrorPane extends StatelessWidget {
  final String message;

  const _ErrorPane({required this.message});

  @override
  Widget build(BuildContext context) => Padding(
        padding: const EdgeInsets.all(24),
        child: Column(
          mainAxisAlignment: MainAxisAlignment.center,
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text(
              'could not open the world',
              style: Theme.of(context).textTheme.titleMedium,
            ),
            const SizedBox(height: 8),
            SelectableText(message),
            const SizedBox(height: 16),
            const SelectableText(
              'seed it and push it:\n\n'
              '  cargo run --release -p solstice-bench --bin demo-fixture\n'
              '  ./push-fixture.sh',
              style: TextStyle(fontFamily: 'monospace'),
            ),
          ],
        ),
      );
}
