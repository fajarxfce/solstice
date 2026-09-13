// What the app knows about the world `demo-fixture` seeded.
//
// Four numbers and two word lists, and the app needs them for one reason:
// plan §1.4 gives it exactly one way into the store, `mutate`, and no way to
// enumerate. A driver that wants to write to a row it is not looking at has to
// be able to name an id, and naming one means knowing how many there are.
//
// They are constants rather than a read from the database because the app has
// no read path that is not a subscription, and adding one to support a
// benchmark would be adding a feature to make a measurement look easier.
// `demo-fixture` prints every one of these when it runs; if the two ever
// disagree, the driver aims at ids that are not there and the HUD's `refused`
// counter goes up, which is a visible failure rather than a silent one.

/// `Scale::M0` from `solstice_bench::fixture` — plan §5.1's "100k issue /
/// 1M comment".
const int seededIssues = 100000;
const int seededComments = 1000000;
const int seededProjects = 50;

/// `fixture::Query::default().project`, which is the project every number in
/// `BENCHMARKS.md` was measured against. `demo-fixture` defaults to the same
/// one so the demo's latency can be put next to the harness's.
const int defaultProject = 7;

/// Plan §7's query: the top 50 by priority, three comments each.
const int defaultK = 50;
const int defaultComments = 3;

/// `solstice_bench::fixture`'s vocabulary, so rows the driver inserts are the
/// same shape and the same length as the ones it was seeded with. A driver
/// writing 4-character titles into a table of 30-character ones would slowly
/// make the payload smaller and the numbers better.
const List<String> words = [
  'crash', 'layout', 'timeout', 'retry', 'cache', 'scroll', 'sync', 'token', //
  'upload', 'parser', 'theme', 'locale', 'export', 'search', 'badge', 'toast',
];

const List<String> authors = [
  'ana', 'bruno', 'chen', 'dewi', 'elif', 'farid', 'gita', 'hugo', //
];
