//! # Module: latency
//!
//! ## Responsibility
//! Latency histograms for measuring operation latencies in microseconds, backed
//! by [`hdrhistogram`] (the Rust port of Gil Tene's HdrHistogram).
//!
//! ## Guarantees
//! - No panics on any input: values above one hour are clamped into the top bucket.
//! - Percentiles are within 1% of the true sample value (two significant digits),
//!   and exact below 256 µs. `min`, `max` and `mean` are exact.
//! - Memory is fixed per histogram, whatever the sample count.

use hdrhistogram::Histogram;
use std::collections::HashMap;

// ─── constants ────────────────────────────────────────────────────────────────

/// Lowest trackable latency: 1 µs (0 is recorded as 1).
const MIN_US: u64 = 1;
/// Highest trackable latency: one hour, in µs. Larger values are clamped.
const MAX_US: u64 = 3_600_000_000;
/// Significant decimal digits kept by the histogram (1% resolution).
const SIG_FIGS: u8 = 2;

// ─── histogram ────────────────────────────────────────────────────────────────

/// Latency histogram with percentile queries.
///
/// Before 2.12 this was a hand-rolled histogram with 4 buckets per power of two:
/// every percentile came back as a bucket's upper edge, up to 25% above the real
/// sample (a single 100 µs sample reported p50 = 112 µs), and anything over 10 s
/// was clamped. It now keeps every value to within 1% from 1 µs to one hour.
#[derive(Debug, Clone)]
pub struct LatencyHistogram {
    hist: Histogram<u64>,
    sum_us: u64,
    max_us: u64,
    min_us: u64,
}

impl LatencyHistogram {
    /// Create a new empty histogram.
    pub fn new() -> Self {
        // The bounds are constants that hdrhistogram accepts (low >= 1, high >= 2 * low,
        // sigfig <= 5), so the first constructor cannot fail; the fallbacks only exist
        // to keep this function free of `unwrap`.
        let hist = Histogram::new_with_bounds(MIN_US, MAX_US, SIG_FIGS)
            .or_else(|_| Histogram::new(SIG_FIGS))
            .unwrap_or_else(|_| unreachable!("2 significant figures is always valid"));
        Self {
            hist,
            sum_us: 0,
            max_us: 0,
            min_us: u64::MAX,
        }
    }

    /// Record a latency measurement in microseconds.
    pub fn record(&mut self, latency_us: u64) {
        self.hist.saturating_record(latency_us.clamp(MIN_US, MAX_US));
        self.sum_us = self.sum_us.saturating_add(latency_us);
        self.max_us = self.max_us.max(latency_us);
        self.min_us = self.min_us.min(latency_us);
    }

    /// Add every sample from `other` into this histogram (for example, one
    /// histogram per worker thread merged for reporting).
    pub fn merge(&mut self, other: &LatencyHistogram) {
        if other.count() == 0 {
            return;
        }
        // Both histograms share the same bounds, so `add` cannot fail.
        let _ = self.hist.add(&other.hist);
        self.sum_us = self.sum_us.saturating_add(other.sum_us);
        self.max_us = self.max_us.max(other.max_us);
        self.min_us = self.min_us.min(other.min_us);
    }

    /// Compute the p-th percentile latency in microseconds.
    ///
    /// `p` is in [0.0, 100.0]. For example, `p=99.9` returns the p99.9 value.
    /// The result is always between [`min_us`](Self::min_us) (or 1, if the
    /// minimum is 0) and [`max_us`](Self::max_us). Returns 0 if no values have
    /// been recorded.
    pub fn percentile(&self, p: f64) -> u64 {
        if self.count() == 0 {
            return 0;
        }
        let p = if p.is_nan() { 0.0 } else { p.clamp(0.0, 100.0) };
        let hi = self.max_us.max(MIN_US);
        let lo = self.min_us.clamp(MIN_US, hi);
        self.hist.value_at_quantile(p / 100.0).clamp(lo, hi)
    }

    /// Mean latency in microseconds. Returns 0.0 if no values recorded.
    pub fn mean_us(&self) -> f64 {
        if self.count() == 0 {
            return 0.0;
        }
        self.sum_us as f64 / self.count() as f64
    }

    /// Maximum recorded latency in microseconds.
    pub fn max_us(&self) -> u64 {
        if self.count() == 0 { 0 } else { self.max_us }
    }

    /// Minimum recorded latency in microseconds.
    pub fn min_us(&self) -> u64 {
        if self.count() == 0 { 0 } else { self.min_us }
    }

    /// Total number of recorded samples.
    pub fn count(&self) -> u64 {
        self.hist.len()
    }

    /// Produce a snapshot of key percentiles.
    pub fn snapshot(&self) -> HistogramSnapshot {
        HistogramSnapshot {
            p50: self.percentile(50.0),
            p90: self.percentile(90.0),
            p99: self.percentile(99.0),
            p999: self.percentile(99.9),
            mean: self.mean_us(),
            max: self.max_us(),
            min: self.min_us(),
            count: self.count(),
        }
    }
}

impl Default for LatencyHistogram {
    fn default() -> Self {
        Self::new()
    }
}

// ─── snapshot ────────────────────────────────────────────────────────────────

/// A point-in-time snapshot of histogram statistics.
#[derive(Debug, Clone, Copy)]
pub struct HistogramSnapshot {
    /// 50th percentile latency in microseconds.
    pub p50: u64,
    /// 90th percentile latency in microseconds.
    pub p90: u64,
    /// 99th percentile latency in microseconds.
    pub p99: u64,
    /// 99.9th percentile latency in microseconds.
    pub p999: u64,
    /// Mean latency in microseconds.
    pub mean: f64,
    /// Maximum recorded latency in microseconds.
    pub max: u64,
    /// Minimum recorded latency in microseconds.
    pub min: u64,
    /// Total number of recorded samples.
    pub count: u64,
}

// ─── tracker ──────────────────────────────────────────────────────────────────

/// Per-operation named latency histograms.
///
/// Maintains one `LatencyHistogram` per named operation. Thread-unsafe;
/// wrap in a `Mutex` for concurrent use.
#[derive(Debug, Default)]
pub struct LatencyTracker {
    histograms: HashMap<String, LatencyHistogram>,
}

impl LatencyTracker {
    /// Create a new empty tracker.
    pub fn new() -> Self {
        Self { histograms: HashMap::new() }
    }

    /// Record a latency for the named operation (creates histogram on first call).
    pub fn record(&mut self, op: &str, latency_us: u64) {
        self.histograms
            .entry(op.to_owned())
            .or_default()
            .record(latency_us);
    }

    /// Return a snapshot for the named operation, or `None` if unknown.
    pub fn snapshot(&self, op: &str) -> Option<HistogramSnapshot> {
        self.histograms.get(op).map(|h| h.snapshot())
    }

    /// Return snapshots for all tracked operations.
    pub fn all_ops(&self) -> HashMap<String, HistogramSnapshot> {
        self.histograms
            .iter()
            .map(|(k, v)| (k.clone(), v.snapshot()))
            .collect()
    }

    /// Return the raw histogram for an operation, or `None` if unknown.
    pub fn histogram(&self, op: &str) -> Option<&LatencyHistogram> {
        self.histograms.get(op)
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── basic ──

    #[test]
    fn empty_histogram_count_zero() {
        let h = LatencyHistogram::new();
        assert_eq!(h.count(), 0);
    }

    #[test]
    fn empty_histogram_min_max_zero() {
        let h = LatencyHistogram::new();
        assert_eq!(h.min_us(), 0);
        assert_eq!(h.max_us(), 0);
    }

    #[test]
    fn empty_histogram_mean_zero() {
        let h = LatencyHistogram::new();
        assert_eq!(h.mean_us(), 0.0);
    }

    #[test]
    fn single_record_count_one() {
        let mut h = LatencyHistogram::new();
        h.record(100);
        assert_eq!(h.count(), 1);
    }

    #[test]
    fn single_record_min_max() {
        let mut h = LatencyHistogram::new();
        h.record(500);
        assert_eq!(h.min_us(), 500);
        assert_eq!(h.max_us(), 500);
    }

    #[test]
    fn mean_of_uniform_values() {
        let mut h = LatencyHistogram::new();
        for _ in 0..100 {
            h.record(1000);
        }
        assert!((h.mean_us() - 1000.0).abs() < 1e-3);
    }

    // ── percentiles ──

    #[test]
    fn p50_of_two_values() {
        let mut h = LatencyHistogram::new();
        h.record(100);
        h.record(1000);
        // p50 should resolve to the bucket containing 100
        let p50 = h.percentile(50.0);
        assert!(p50 > 0, "p50 should be positive: {p50}");
    }

    #[test]
    fn p100_equals_or_exceeds_max() {
        let mut h = LatencyHistogram::new();
        h.record(50);
        h.record(200);
        h.record(5000);
        let p100 = h.percentile(100.0);
        assert!(p100 >= h.max_us(), "p100 {p100} should >= max {}", h.max_us());
    }

    #[test]
    fn percentile_monotone() {
        let mut h = LatencyHistogram::new();
        for v in [10, 50, 100, 500, 1000, 5000, 10000, 50000] {
            h.record(v);
        }
        let p50 = h.percentile(50.0);
        let p90 = h.percentile(90.0);
        let p99 = h.percentile(99.0);
        assert!(p50 <= p90, "p50 ({p50}) > p90 ({p90})");
        assert!(p90 <= p99, "p90 ({p90}) > p99 ({p99})");
    }

    #[test]
    fn p0_is_positive_when_non_empty() {
        let mut h = LatencyHistogram::new();
        h.record(100);
        assert!(h.percentile(0.0) > 0);
    }

    #[test]
    fn large_batch_p99_within_bucket() {
        let mut h = LatencyHistogram::new();
        // 1000 values at 100µs, 10 at 10000µs
        for _ in 0..1000 {
            h.record(100);
        }
        for _ in 0..10 {
            h.record(10000);
        }
        let p99 = h.percentile(99.0);
        // p99 should be around 100µs bucket (990th value)
        assert!(p99 < 500, "p99 should be near 100µs: {p99}");
    }

    #[test]
    fn boundary_min_value() {
        let mut h = LatencyHistogram::new();
        h.record(1); // MIN_US
        assert_eq!(h.min_us(), 1);
        assert_eq!(h.count(), 1);
    }

    #[test]
    fn boundary_max_value() {
        let mut h = LatencyHistogram::new();
        h.record(MAX_US);
        assert_eq!(h.max_us(), MAX_US);
    }

    #[test]
    fn above_max_clamped() {
        let mut h = LatencyHistogram::new();
        h.record(MAX_US * 10); // should not panic, clamped into the top bucket
        assert_eq!(h.count(), 1);
        assert_eq!(h.max_us(), MAX_US * 10);
        h.record(u64::MAX);
        assert_eq!(h.count(), 2);
    }

    #[test]
    fn single_sample_percentile_is_the_sample() {
        // The old bucketed histogram answered 112 here (the bucket's upper edge).
        let mut h = LatencyHistogram::new();
        h.record(100);
        assert_eq!(h.percentile(50.0), 100);
        assert_eq!(h.percentile(99.9), 100);
    }

    #[test]
    fn percentiles_within_one_percent_of_exact() {
        // 1..=100_000 µs: the exact p-th percentile is p * 1000.
        let mut h = LatencyHistogram::new();
        for v in 1..=100_000u64 {
            h.record(v);
        }
        for (p, exact) in [(50.0, 50_000.0), (90.0, 90_000.0), (99.0, 99_000.0), (99.9, 99_900.0)] {
            let got = h.percentile(p) as f64;
            assert!((got - exact).abs() / exact < 0.01, "p{p}: got {got}, exact {exact}");
        }
        assert_eq!(h.min_us(), 1);
        assert_eq!(h.max_us(), 100_000);
        assert!((h.mean_us() - 50_000.5).abs() < 1e-9);
    }

    #[test]
    fn latencies_above_ten_seconds_are_kept() {
        // The old histogram clamped everything above 10 s into one bucket.
        let mut h = LatencyHistogram::new();
        for _ in 0..99 {
            h.record(1_000);
        }
        h.record(60_000_000); // one 60 s stall
        let p999 = h.percentile(99.9) as f64;
        assert!((p999 - 60_000_000.0).abs() / 60_000_000.0 < 0.01, "p99.9 = {p999}");
    }

    #[test]
    fn merge_combines_samples() {
        let mut a = LatencyHistogram::new();
        let mut b = LatencyHistogram::new();
        for v in 1..=50u64 {
            a.record(v);
        }
        for v in 51..=100u64 {
            b.record(v);
        }
        a.merge(&b);
        assert_eq!(a.count(), 100);
        assert_eq!(a.min_us(), 1);
        assert_eq!(a.max_us(), 100);
        assert_eq!(a.percentile(50.0), 50);
        a.merge(&LatencyHistogram::new());
        assert_eq!(a.count(), 100);
    }

    #[test]
    fn zero_latency_clamped_to_min() {
        let mut h = LatencyHistogram::new();
        h.record(0); // should be clamped to MIN_US = 1
        assert_eq!(h.count(), 1);
    }

    // ── snapshot ──

    #[test]
    fn snapshot_count_matches() {
        let mut h = LatencyHistogram::new();
        for i in 1..=50 {
            h.record(i * 100);
        }
        let snap = h.snapshot();
        assert_eq!(snap.count, 50);
    }

    #[test]
    fn snapshot_p50_le_p99() {
        let mut h = LatencyHistogram::new();
        for v in [100, 200, 300, 1000, 5000, 10000] {
            h.record(v);
        }
        let snap = h.snapshot();
        assert!(snap.p50 <= snap.p99);
    }

    // ── tracker ──

    #[test]
    fn tracker_unknown_op_returns_none() {
        let t = LatencyTracker::new();
        assert!(t.snapshot("unknown").is_none());
    }

    #[test]
    fn tracker_records_and_retrieves() {
        let mut t = LatencyTracker::new();
        t.record("order_submit", 150);
        t.record("order_submit", 200);
        let snap = t.snapshot("order_submit").unwrap();
        assert_eq!(snap.count, 2);
    }

    #[test]
    fn tracker_multiple_ops_independent() {
        let mut t = LatencyTracker::new();
        t.record("submit", 100);
        t.record("fill", 500);
        let sub = t.snapshot("submit").unwrap();
        let fill = t.snapshot("fill").unwrap();
        assert_eq!(sub.count, 1);
        assert_eq!(fill.count, 1);
    }

    #[test]
    fn tracker_all_ops_returns_all() {
        let mut t = LatencyTracker::new();
        t.record("a", 100);
        t.record("b", 200);
        t.record("c", 300);
        let all = t.all_ops();
        assert_eq!(all.len(), 3);
        assert!(all.contains_key("a"));
        assert!(all.contains_key("b"));
        assert!(all.contains_key("c"));
    }

    #[test]
    fn tracker_same_op_accumulates() {
        let mut t = LatencyTracker::new();
        for _ in 0..10 {
            t.record("op", 1000);
        }
        let snap = t.snapshot("op").unwrap();
        assert_eq!(snap.count, 10);
    }
}
