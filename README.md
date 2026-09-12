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

That is what M0 exists to answer, and the answer is not yet in. The question:

> Can a Rust IVM engine behind a schema-independent byte ABI deliver sub-16ms,
> jank-free updates for a **joined, sorted, limited** list over a 100k-row local
> dataset on a mid-tier Android phone, from **both** Flutter and Compose, within a
> reasonable binary-size budget?

If not, the premise collapses and this repository will say so. The kill criteria are
fixed in advance and published as measured numbers, pass or fail:

| Metric | Budget | Measured |
|---|---|---|
| p99 delta → committed frame | < 16 ms | 190 µs *(engine half only)* |
| Engine steady-state RSS | < 60 MB | 6.3 MB anonymous |
| `TopK` refills/sec under an adversarial delete-the-top workload | < 5 | 1.4 |
| Decode of an initial 1000-row view (Dart *and* Kotlin) | < 5 ms | not yet measured |
| APK growth per ABI | < 8 MB | not yet measured |

Measured over 100k issues and 1M comments in real SQLite — **on a laptop, and only up
to the point where the engine hands the diff over.** The FFI decode and the render are
the other half of that first budget, and a desktop is not a phone. See
[BENCHMARKS.md](BENCHMARKS.md) for what is and is not being claimed; the go/no-go gate
is a physical Android device and it has not run yet.

## Repository

Three crates so far, all of them M0 scaffolding.

```
crates/solstice-ivm/      incremental view maintenance — no IO, no time, no threads
crates/solstice-store/    SQLite: schemas, bounded scans, the IR → SQL translation
crates/solstice-bench/    the M0 kill criteria, measured
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
```

## License

Apache-2.0. Contributions by DCO sign-off; there is no CLA.
