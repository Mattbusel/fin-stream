//! Rolling-window coordinate normalization for financial time series.
//!
//! ## Purpose
//!
//! Machine-learning pipelines require features in a bounded numeric range.
//! For streaming market data, a global min/max is unavailable; this module
//! maintains a **rolling window** of the last `W` observations and maps each
//! new sample into `[0.0, 1.0]` using the running min and max.
//!
//! ## Formula
//!
//! Given a window of observations `x_1 ... x_W` with minimum `m` and maximum
//! `M`, the normalized value of a new sample `x` is:
//!
//! ```text
//!     x_norm = (x - m) / (M - m)    if M != m
//!     x_norm = 0.0                  if M == m  (degenerate; single-valued window)
//! ```
//!
//! The result is clamped to `[0.0, 1.0]` to handle the case where `x` falls
//! outside the current window range.
//!
//! ## Guarantees
//!
//! - Non-panicking: all operations return `Result` or `Option`.
//! - The window is a fixed-size ring buffer; once full, the oldest value is
//!   evicted on each new observation.
//! - `MinMaxNormalizer` is `Send` but not `Sync`; wrap in `Mutex` for shared
//!   multi-thread access.

use crate::error::StreamError;
use std::collections::VecDeque;

/// Rolling min-max normalizer over a sliding window of `f64` observations.
///
/// # Example
///
/// ```rust
/// use fin_stream::norm::MinMaxNormalizer;
///
/// let mut norm = MinMaxNormalizer::new(4);
/// norm.update(10.0);
/// norm.update(20.0);
/// norm.update(30.0);
/// norm.update(40.0);
///
/// // 40.0 is the current max; 10.0 is the current min
/// let v = norm.normalize(40.0).unwrap();
/// assert!((v - 1.0).abs() < 1e-10);
/// ```
pub struct MinMaxNormalizer {
    window_size: usize,
    window: VecDeque<f64>,
    cached_min: f64,
    cached_max: f64,
    dirty: bool,
}

impl MinMaxNormalizer {
    /// Create a new normalizer with the given rolling window size.
    ///
    /// `window_size` must be at least 1. Passing 0 is a programming error;
    /// this function will panic in debug mode and is flagged by the
    /// `clippy::panic` lint configured in `Cargo.toml`.
    ///
    /// # Panics
    ///
    /// Panics if `window_size == 0`.
    pub fn new(window_size: usize) -> Self {
        // This is an API misuse guard; the window size of 0 makes the
        // normalizer semantically undefined. The panic is intentional and
        // documented. Production callers should validate before calling.
        if window_size == 0 {
            panic!("MinMaxNormalizer::new: window_size must be > 0");
        }
        Self {
            window_size,
            window: VecDeque::with_capacity(window_size),
            cached_min: f64::MAX,
            cached_max: f64::MIN,
            dirty: false,
        }
    }

    /// Add a new observation to the rolling window.
    ///
    /// If the window is full, the oldest value is evicted. After the call,
    /// the internal min/max cache is marked dirty and will be recomputed lazily
    /// on the next call to [`normalize`](Self::normalize) or
    /// [`min_max`](Self::min_max).
    ///
    /// # Complexity: O(1) amortized
    pub fn update(&mut self, value: f64) {
        if self.window.len() == self.window_size {
            self.window.pop_front();
            self.dirty = true;
        }
        self.window.push_back(value);
        // Eager update is cheaper than a full recompute when we don't evict.
        if !self.dirty {
            if value < self.cached_min { self.cached_min = value; }
            if value > self.cached_max { self.cached_max = value; }
        }
    }

    /// Recompute min and max from the full window.
    ///
    /// Called lazily when `dirty` is set (eviction occurred). O(W).
    fn recompute(&mut self) {
        self.cached_min = f64::MAX;
        self.cached_max = f64::MIN;
        for &v in &self.window {
            if v < self.cached_min { self.cached_min = v; }
            if v > self.cached_max { self.cached_max = v; }
        }
        self.dirty = false;
    }

    /// Return the current `(min, max)` of the window.
    ///
    /// Returns `None` if the window is empty.
    ///
    /// # Complexity: O(1) when the cache is clean; O(W) after an eviction.
    pub fn min_max(&mut self) -> Option<(f64, f64)> {
        if self.window.is_empty() {
            return None;
        }
        if self.dirty {
            self.recompute();
        }
        Some((self.cached_min, self.cached_max))
    }

    /// Normalize `value` into `[0.0, 1.0]` using the current window.
    ///
    /// The value is clamped so that even if `value` falls outside the window
    /// range the result is always in `[0.0, 1.0]`.
    ///
    /// # Errors
    ///
    /// Returns `NormalizationError` if the window is empty (no observations
    /// have been fed yet).
    ///
    /// # Complexity: O(1) when cache is clean; O(W) after an eviction.
    pub fn normalize(&mut self, value: f64) -> Result<f64, StreamError> {
        let (min, max) = self.min_max().ok_or_else(|| StreamError::NormalizationError {
            reason: "window is empty; call update() before normalize()".into(),
        })?;
        if (max - min).abs() < f64::EPSILON {
            // Degenerate: all values in the window are identical.
            return Ok(0.0);
        }
        let normalized = (value - min) / (max - min);
        Ok(normalized.clamp(0.0, 1.0))
    }

    /// Reset the normalizer, clearing all observations and the cache.
    pub fn reset(&mut self) {
        self.window.clear();
        self.cached_min = f64::MAX;
        self.cached_max = f64::MIN;
        self.dirty = false;
    }

    /// Number of observations currently in the window.
    pub fn len(&self) -> usize {
        self.window.len()
    }

    /// Returns `true` if no observations have been added since construction or
    /// the last reset.
    pub fn is_empty(&self) -> bool {
        self.window.is_empty()
    }

    /// The configured window size.
    pub fn window_size(&self) -> usize {
        self.window_size
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Construction ─────────────────────────────────────────────────────────

    #[test]
    fn test_new_normalizer_is_empty() {
        let n = MinMaxNormalizer::new(4);
        assert!(n.is_empty());
        assert_eq!(n.len(), 0);
    }

    // ── Normalization range [0, 1] ────────────────────────────────────────────

    #[test]
    fn test_normalize_min_is_zero() {
        let mut n = MinMaxNormalizer::new(4);
        n.update(10.0);
        n.update(20.0);
        n.update(30.0);
        n.update(40.0);
        let v = n.normalize(10.0).unwrap();
        assert!((v - 0.0).abs() < 1e-10, "min should normalize to 0.0, got {v}");
    }

    #[test]
    fn test_normalize_max_is_one() {
        let mut n = MinMaxNormalizer::new(4);
        n.update(10.0);
        n.update(20.0);
        n.update(30.0);
        n.update(40.0);
        let v = n.normalize(40.0).unwrap();
        assert!((v - 1.0).abs() < 1e-10, "max should normalize to 1.0, got {v}");
    }

    #[test]
    fn test_normalize_midpoint_is_half() {
        let mut n = MinMaxNormalizer::new(4);
        n.update(0.0);
        n.update(100.0);
        let v = n.normalize(50.0).unwrap();
        assert!((v - 0.5).abs() < 1e-10);
    }

    #[test]
    fn test_normalize_result_clamped_below_zero() {
        let mut n = MinMaxNormalizer::new(4);
        n.update(50.0);
        n.update(100.0);
        // 10.0 is below the window min of 50.0
        let v = n.normalize(10.0).unwrap();
        assert!(v >= 0.0);
        assert_eq!(v, 0.0);
    }

    #[test]
    fn test_normalize_result_clamped_above_one() {
        let mut n = MinMaxNormalizer::new(4);
        n.update(50.0);
        n.update(100.0);
        // 200.0 is above the window max of 100.0
        let v = n.normalize(200.0).unwrap();
        assert!(v <= 1.0);
        assert_eq!(v, 1.0);
    }

    #[test]
    fn test_normalize_all_same_values_returns_zero() {
        let mut n = MinMaxNormalizer::new(4);
        n.update(5.0);
        n.update(5.0);
        n.update(5.0);
        let v = n.normalize(5.0).unwrap();
        assert_eq!(v, 0.0);
    }

    // ── Empty window error ────────────────────────────────────────────────────

    #[test]
    fn test_normalize_empty_window_returns_error() {
        let mut n = MinMaxNormalizer::new(4);
        let err = n.normalize(1.0).unwrap_err();
        assert!(matches!(err, StreamError::NormalizationError { .. }));
    }

    #[test]
    fn test_min_max_empty_returns_none() {
        let mut n = MinMaxNormalizer::new(4);
        assert!(n.min_max().is_none());
    }

    // ── Rolling window eviction ───────────────────────────────────────────────

    /// After the window fills and the minimum is evicted, the new min must
    /// reflect the remaining values.
    #[test]
    fn test_rolling_window_evicts_oldest() {
        let mut n = MinMaxNormalizer::new(3);
        n.update(1.0); // will be evicted
        n.update(5.0);
        n.update(10.0);
        n.update(20.0); // evicts 1.0
        let (min, max) = n.min_max().unwrap();
        assert_eq!(min, 5.0);
        assert_eq!(max, 20.0);
    }

    #[test]
    fn test_rolling_window_len_does_not_exceed_capacity() {
        let mut n = MinMaxNormalizer::new(3);
        for i in 0..10 {
            n.update(i as f64);
        }
        assert_eq!(n.len(), 3);
    }

    // ── Reset behavior ────────────────────────────────────────────────────────

    #[test]
    fn test_reset_clears_window() {
        let mut n = MinMaxNormalizer::new(4);
        n.update(10.0);
        n.update(20.0);
        n.reset();
        assert!(n.is_empty());
        assert!(n.min_max().is_none());
    }

    #[test]
    fn test_normalize_works_after_reset() {
        let mut n = MinMaxNormalizer::new(4);
        n.update(10.0);
        n.reset();
        n.update(0.0);
        n.update(100.0);
        let v = n.normalize(100.0).unwrap();
        assert!((v - 1.0).abs() < 1e-10);
    }

    // ── Streaming update ──────────────────────────────────────────────────────

    #[test]
    fn test_streaming_updates_monotone_sequence() {
        let mut n = MinMaxNormalizer::new(5);
        let prices = [100.0, 101.0, 102.0, 103.0, 104.0, 105.0];
        for &p in &prices {
            n.update(p);
        }
        // Window now holds [101, 102, 103, 104, 105]; min=101, max=105
        let v_min = n.normalize(101.0).unwrap();
        let v_max = n.normalize(105.0).unwrap();
        assert!((v_min - 0.0).abs() < 1e-10);
        assert!((v_max - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_normalization_monotonicity_in_window() {
        let mut n = MinMaxNormalizer::new(10);
        for i in 0..10 {
            n.update(i as f64 * 10.0);
        }
        // Values 0, 10, 20, ..., 90 in window; min=0, max=90
        let v0 = n.normalize(0.0).unwrap();
        let v50 = n.normalize(50.0).unwrap();
        let v90 = n.normalize(90.0).unwrap();
        assert!(v0 < v50, "normalized values should be monotone");
        assert!(v50 < v90, "normalized values should be monotone");
    }
}
