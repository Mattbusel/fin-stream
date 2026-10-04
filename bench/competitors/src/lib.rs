//! The `LatencyHistogram` from fin-stream 2.11.3, copied verbatim (constants and the
//! histogram only) so the benchmark can compare it with the hdrhistogram-backed one.
#![allow(dead_code, clippy::all)]

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
pub struct OldLatencyHistogram {
    counts: [u64; TOTAL_BUCKETS],
    total_count: u64,
    sum_us: u64,
    max_us: u64,
    min_us: u64,
}

impl OldLatencyHistogram {
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

impl Default for OldLatencyHistogram {
    fn default() -> Self {
        Self::new()
    }
}


/// 100,000 latency-shaped samples in microseconds: 99% between 50 and 500 us,
/// 1% in a long tail up to about 0.5 s. Seeded, so every run sees the same data.
pub fn samples() -> Vec<u64> {
    let mut state = 7u64;
    (0..100_000)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let u = state % 10_000;
            if u < 9_900 { 50 + u / 22 } else { 1_000 + (u - 9_900) * 5_000 }
        })
        .collect()
}
