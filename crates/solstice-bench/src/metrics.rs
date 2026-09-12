//! Latency samples, percentiles, and resident memory.
//!
//! # Why percentiles and not a mean
//!
//! The M0 budget is `p99 delta → frame < 16ms` (plan §5.1), and a mean cannot
//! fail that budget: an operator that is instant 99 times and stalls for 200ms
//! on the hundredth has an excellent mean and a visibly broken list. Jank *is*
//! the tail. So every sample is kept and the tail is reported, rather than
//! summarised on the way in.
//!
//! Keeping every sample costs 8 bytes each, which for a run of a few hundred
//! thousand pumps is a couple of megabytes — cheaper than the argument about
//! whether a streaming quantile sketch is accurate enough at p99.

use std::time::Duration;

#[derive(Default)]
pub struct Samples {
    nanos: Vec<u64>,
    sorted: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct Summary {
    pub count: usize,
    pub p50: Duration,
    pub p99: Duration,
    pub max: Duration,
    pub total: Duration,
}

impl Samples {
    pub fn new() -> Self {
        Samples::default()
    }

    pub fn push(&mut self, d: Duration) {
        self.nanos.push(d.as_nanos().min(u64::MAX as u128) as u64);
        self.sorted = false;
    }

    pub fn len(&self) -> usize {
        self.nanos.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nanos.is_empty()
    }

    /// Nearest-rank percentile: the smallest sample at or above `p` percent of
    /// the data. No interpolation, so the number reported is one that actually
    /// happened — which is the right claim to make about a latency budget.
    pub fn percentile(&mut self, p: f64) -> Duration {
        if self.nanos.is_empty() {
            return Duration::ZERO;
        }
        self.sort();
        let n = self.nanos.len() as f64;
        let rank = (p / 100.0 * n).ceil().max(1.0) as usize;
        Duration::from_nanos(self.nanos[rank.min(self.nanos.len()) - 1])
    }

    pub fn summary(&mut self) -> Summary {
        let total: u64 = self.nanos.iter().sum();
        Summary {
            count: self.nanos.len(),
            p50: self.percentile(50.0),
            p99: self.percentile(99.0),
            max: Duration::from_nanos(self.nanos.iter().copied().max().unwrap_or(0)),
            total: Duration::from_nanos(total),
        }
    }

    fn sort(&mut self) {
        if !self.sorted {
            self.nanos.sort_unstable();
            self.sorted = true;
        }
    }
}

/// Resident memory, split by whether the kernel can take it back.
///
/// The split is not pedantry here. The store sets `mmap_size=64MB` (plan §1.4),
/// and every database page the engine touches through that window is resident,
/// file-backed, and counted in RSS — which on a 1.1M-row database is most of the
/// process. Those pages are clean and evictable: under pressure the kernel drops
/// them and the next read faults them back. The anonymous pages are the ones the
/// engine actually owns and the ones that get it killed.
///
/// So a single total cannot answer plan §5.1's 60MB question. It would read as a
/// near-failure caused entirely by a page cache doing its job, or — if measured
/// after the store is dropped — as a comfortable pass that omits the store
/// entirely. Both numbers are wrong in the direction of the reader's prior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rss {
    /// Everything resident, file-backed pages included.
    pub resident: u64,
    /// Resident pages backed by a file — here, overwhelmingly SQLite's mmap
    /// window. Reclaimable.
    pub file_backed: u64,
}

impl Rss {
    /// Heap, stacks, and operator state: what the engine cannot give back.
    pub fn anon(&self) -> u64 {
        self.resident.saturating_sub(self.file_backed)
    }
}

/// This process's resident memory, if the platform will say.
///
/// Linux only, by reading `/proc/self/statm`. Returning `None` elsewhere rather
/// than guessing: a number produced by a fallback nobody validated would be
/// worse than no number, because it would be believed.
#[cfg(target_os = "linux")]
pub fn rss() -> Option<Rss> {
    // Fields are page counts: size, resident, shared, ... The page size is 4096
    // on every Linux target this project builds for, and `sysconf` would mean a
    // libc dependency in a crate that has none.
    const PAGE: u64 = 4096;
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let mut fields = statm.split_whitespace().skip(1);
    let resident: u64 = fields.next()?.parse().ok()?;
    let shared: u64 = fields.next()?.parse().ok()?;
    Some(Rss {
        resident: resident * PAGE,
        file_backed: shared * PAGE,
    })
}

#[cfg(not(target_os = "linux"))]
pub fn rss() -> Option<Rss> {
    None
}

pub fn bytes(n: u64) -> String {
    const UNITS: [(&str, u64); 4] = [("GB", 1 << 30), ("MB", 1 << 20), ("KB", 1 << 10), ("B", 1)];
    for (unit, scale) in UNITS {
        if n >= scale {
            return if scale == 1 {
                format!("{n} B")
            } else {
                format!("{:.1} {unit}", n as f64 / scale as f64)
            };
        }
    }
    "0 B".to_string()
}

pub fn dur(d: Duration) -> String {
    let ns = d.as_nanos();
    if ns >= 1_000_000_000 {
        format!("{:.2}s", d.as_secs_f64())
    } else if ns >= 1_000_000 {
        format!("{:.2}ms", ns as f64 / 1e6)
    } else if ns >= 1_000 {
        format!("{:.0}µs", ns as f64 / 1e3)
    } else {
        format!("{ns}ns")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples(ms: &[u64]) -> Samples {
        let mut s = Samples::new();
        for m in ms {
            s.push(Duration::from_millis(*m));
        }
        s
    }

    #[test]
    fn a_percentile_is_a_sample_that_actually_happened() {
        let mut s = samples(&(1..=100).collect::<Vec<_>>());
        assert_eq!(s.percentile(50.0), Duration::from_millis(50));
        assert_eq!(s.percentile(99.0), Duration::from_millis(99));
        assert_eq!(s.percentile(100.0), Duration::from_millis(100));
    }

    #[test]
    fn a_slow_tail_moves_p99_and_leaves_the_mean_looking_fine() {
        // The reason this module reports a tail rather than an average: both
        // sets average a little under 5ms, and only one of them janks.
        let mut steady = samples(&[5; 100]);
        let mut spiky = {
            let mut v = vec![1u64; 98];
            v.extend([200, 200]);
            samples(&v)
        };
        assert_eq!(steady.percentile(99.0), Duration::from_millis(5));
        assert_eq!(spiky.percentile(99.0), Duration::from_millis(200));
    }

    #[test]
    fn p99_cannot_see_a_tail_thinner_than_one_percent() {
        // Nearest-rank p99 of 100 samples is the 99th smallest, so a single
        // 200ms stall hides behind it and only `max` reports it. Not a defect —
        // it is what a percentile means — but it sets the resolution of a run:
        // a phase of n transactions measures its p99 from the worst n/100
        // samples, so a phase of 200 is deciding a kill criterion on two
        // numbers. Run enough transactions that the tail has a shape.
        let mut one_in_a_hundred = {
            let mut v = vec![1u64; 99];
            v.push(200);
            samples(&v)
        };
        assert_eq!(one_in_a_hundred.percentile(99.0), Duration::from_millis(1));
        assert_eq!(one_in_a_hundred.summary().max, Duration::from_millis(200));
    }

    #[test]
    fn an_empty_sample_set_has_no_opinion() {
        let mut s = Samples::new();
        assert!(s.is_empty());
        assert_eq!(s.percentile(99.0), Duration::ZERO);
        assert_eq!(s.summary().count, 0);
    }

    #[test]
    fn samples_can_be_read_twice_without_being_disturbed() {
        let mut s = samples(&[9, 1, 5]);
        assert_eq!(s.percentile(50.0), Duration::from_millis(5));
        s.push(Duration::from_millis(2));
        assert_eq!(s.summary().max, Duration::from_millis(9));
        assert_eq!(s.len(), 4);
    }

    #[test]
    fn byte_and_duration_formatting_stays_readable() {
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(2048), "2.0 KB");
        assert_eq!(bytes(3 << 20), "3.0 MB");
        assert_eq!(dur(Duration::from_nanos(900)), "900ns");
        assert_eq!(dur(Duration::from_micros(1500)), "1.50ms");
        assert_eq!(dur(Duration::from_millis(2500)), "2.50s");
    }
}
