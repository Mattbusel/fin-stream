//! # Module: latency
//!
//! ## Responsibility
//! HDR-style latency histogram for measuring operation latencies in microseconds.
//! No external dependencies; uses powers-of-2 buckets with 4 sub-buckets each.
//!
//! ## Guarantees
//! - No panics on any input (saturating arithmetic for out-of-range values).
//! - All percentile queries run in O(B) where B = number of buckets.

use std::collections::HashMap;

// ─── constants ────────────────────────────────────────────────────────────────

/// Sub-buckets per power-of-2 bucket.
const SUB_BUCKETS: usize = 4;
/// Minimum latency bucket: 1 µs.
const MIN_US: u64 = 1;
/// Maximum latency bucket: 10 seconds = 10_000_000 µs.
const MAX_US: u64 = 10_000_000;

/// Total number of main buckets (powers of 2 from 1 to 10_000_000).
/// ceil(log2(10_000_000)) = 24
const N_MAIN_BUCKETS: usize = 24;
/// Total bucket count including sub-buckets.
const TOTAL_BUCKETS: usize = N_MAIN_BUCKETS * SUB_BUCKETS;

// ─── histogram ────────────────────────────────────────────────────────────────

/// HDR-style latency histogram.
///
/// Buckets cover powers of 2 from 1 µs to 10 s, each subdivided into 4
/// sub-buckets for finer resolution. Values outside the range are clamped
/// to the nearest boundary bucket.
#[derive(Debug, Clone)]
pub struct LatencyHistogram {
    counts: [u64; TOTAL_BUCKETS],
    total_count: u64,
    sum_us: u64,
    max_us: u64,
    min_us: u64,
}

impl LatencyHistogram {
    /// Create a new empty histogram.
    pub fn new() -> Self {
        Self {
            counts: [0; TOTAL_BUCKETS],
            total_count: 0,
            sum_us: 0,
            max_us: 0,
            min_us: u64::MAX,
        }
    }

    /// Record a latency measurement in microseconds.
    pub fn record(&mut self, latency_us: u64) {
        let clamped = latency_us.clamp(MIN_US, MAX_US);
        let idx = Self::bucket_index(clamped);
        self.counts[idx] = self.counts[idx].saturating_add(1);
        self.total_count = self.total_count.saturating_add(1);
        self.sum_us = self.sum_us.saturating_add(latency_us);
        if latency_us > self.max_us {
            self.max_us = latency_us;
        }
        if latency_us < self.min_us {
            self.min_us = latency_us;
        }
    }

    /// Compute the p-th percentile latency in microseconds.
    ///
    /// `p` is in [0.0, 100.0]. For example, `p=99.9` returns the p99.9 value.
    /// Returns 0 if no values have been recorded.
    pub fn percentile(&self, p: f64) -> u64 {
        if self.total_count == 0 {
            return 0;
        }
        let p = p.clamp(0.0, 100.0);
        let target = ((p / 100.0) * self.total_count as f64).ceil() as u64;
        let mut cumulative: u64 = 0;
        for (idx, &count) in self.counts.iter().enumerate() {
            cumulative = cumulative.saturating_add(count);
            if cumulative >= target {
                return Self::bucket_upper_us(idx);
            }
        }
        self.max_us
    }

    /// Mean latency in microseconds. Returns 0.0 if no values recorded.
    pub fn mean_us(&self) -> f64 {
        if self.total_count == 0 {
            return 0.0;
        }
        self.sum_us as f64 / self.total_count as f64
    }

    /// Maximum recorded latency in microseconds.
    pub fn max_us(&self) -> u64 {
        if self.total_count == 0 { 0 } else { self.max_us }
    }

    /// Minimum recorded latency in microseconds.
    pub fn min_us(&self) -> u64 {
        if self.total_count == 0 { 0 } else { self.min_us }
    }

    /// Total number of recorded samples.
    pub fn count(&self) -> u64 {
        self.total_count
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

    /// Map a latency value to a bucket index.
    fn bucket_index(us: u64) -> usize {
        // Main bucket: floor(log2(us)), clamped to [0, N_MAIN_BUCKETS-1]
        let us = us.max(1);
        let main = (63 - us.leading_zeros()) as usize;
        let main = main.min(N_MAIN_BUCKETS - 1);

        // Sub-bucket within the main bucket
        // The main bucket covers [2^main, 2^(main+1))
        // Divide that range into SUB_BUCKETS equal parts.
        let bucket_start = 1_u64 << main;
        let bucket_width = bucket_start; // width = 2^main
        let sub_width = (bucket_width / SUB_BUCKETS as u64).max(1);
        let offset = us.saturating_sub(bucket_start);
        let sub = ((offset / sub_width) as usize).min(SUB_BUCKETS - 1);

        main * SUB_BUCKETS + sub
    }

    /// Return the upper bound of a bucket (used as the representative value).
    fn bucket_upper_us(idx: usize) -> u64 {
        let main = idx / SUB_BUCKETS;
        let sub = idx % SUB_BUCKETS;
        let bucket_start = 1_u64 << main;
        let bucket_width = bucket_start;
        let sub_width = (bucket_width / SUB_BUCKETS as u64).max(1);
        bucket_start + sub_width * (sub as u64 + 1)
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
        h.record(MAX_US * 10); // should not panic, clamped to MAX_US bucket
        assert_eq!(h.count(), 1);
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
