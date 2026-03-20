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
//! ## Precision
//!
//! Inputs and window storage use [`rust_decimal::Decimal`] to preserve the
//! same exact arithmetic guarantees as the rest of the pipeline ("never use
//! f64 for prices"). The normalized output is `f64` because downstream ML
//! models expect floating-point features and the [0, 1] range does not require
//! financial precision.
//!
//! ## Guarantees
//!
//! - Non-panicking: construction returns `Result`; all operations return
//!   `Result` or `Option`.
//! - The window is a fixed-size ring buffer; once full, the oldest value is
//!   evicted on each new observation.
//! - `MinMaxNormalizer` is `Send` but not `Sync`; wrap in `Mutex` for shared
//!   multi-thread access.

use crate::error::StreamError;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use std::collections::VecDeque;

/// Rolling min-max normalizer over a sliding window of [`Decimal`] observations.
///
/// # Example
///
/// ```rust
/// use fin_stream::norm::MinMaxNormalizer;
/// use rust_decimal_macros::dec;
///
/// let mut norm = MinMaxNormalizer::new(4).unwrap();
/// norm.update(dec!(10));
/// norm.update(dec!(20));
/// norm.update(dec!(30));
/// norm.update(dec!(40));
///
/// // 40 is the current max; 10 is the current min
/// let v = norm.normalize(dec!(40)).unwrap();
/// assert!((v - 1.0).abs() < 1e-10);
/// ```
pub struct MinMaxNormalizer {
    window_size: usize,
    window: VecDeque<Decimal>,
    cached_min: Decimal,
    cached_max: Decimal,
    dirty: bool,
}

impl MinMaxNormalizer {
    /// Create a new normalizer with the given rolling window size.
    ///
    /// # Errors
    ///
    /// Returns [`StreamError::ConfigError`] if `window_size == 0`.
    pub fn new(window_size: usize) -> Result<Self, StreamError> {
        if window_size == 0 {
            return Err(StreamError::ConfigError {
                reason: "MinMaxNormalizer window_size must be > 0".into(),
            });
        }
        Ok(Self {
            window_size,
            window: VecDeque::with_capacity(window_size),
            cached_min: Decimal::MAX,
            cached_max: Decimal::MIN,
            dirty: false,
        })
    }

    /// Add a new observation to the rolling window.
    ///
    /// If the window is full, the oldest value is evicted. After the call,
    /// the internal min/max cache is marked dirty and will be recomputed lazily
    /// on the next call to [`normalize`](Self::normalize) or
    /// [`min_max`](Self::min_max).
    ///
    /// # Complexity: O(1) amortized
    pub fn update(&mut self, value: Decimal) {
        if self.window.len() == self.window_size {
            self.window.pop_front();
            self.dirty = true;
        }
        self.window.push_back(value);
        // Eager update is cheaper than a full recompute when we don't evict.
        if !self.dirty {
            if value < self.cached_min {
                self.cached_min = value;
            }
            if value > self.cached_max {
                self.cached_max = value;
            }
        }
    }

    /// Recompute min and max from the full window.
    ///
    /// Called lazily when `dirty` is set (eviction occurred). O(W).
    fn recompute(&mut self) {
        self.cached_min = Decimal::MAX;
        self.cached_max = Decimal::MIN;
        for &v in &self.window {
            if v < self.cached_min {
                self.cached_min = v;
            }
            if v > self.cached_max {
                self.cached_max = v;
            }
        }
        self.dirty = false;
    }

    /// Return the current `(min, max)` of the window.
    ///
    /// Returns `None` if the window is empty.
    ///
    /// # Complexity: O(1) when the cache is clean; O(W) after an eviction.
    pub fn min_max(&mut self) -> Option<(Decimal, Decimal)> {
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
    /// Returns [`StreamError::NormalizationError`] if the window is empty (no
    /// observations have been fed yet), or if the normalized Decimal cannot be
    /// converted to `f64`.
    ///
    /// # Complexity: O(1) when cache is clean; O(W) after an eviction.
    pub fn normalize(&mut self, value: Decimal) -> Result<f64, StreamError> {
        let (min, max) = self
            .min_max()
            .ok_or_else(|| StreamError::NormalizationError {
                reason: "window is empty; call update() before normalize()".into(),
            })?;
        if max == min {
            // Degenerate: all values in the window are identical.
            return Ok(0.0);
        }
        let normalized = (value - min) / (max - min);
        // Clamp to [0, 1] in Decimal space before converting to f64.
        let clamped = normalized.clamp(Decimal::ZERO, Decimal::ONE);
        clamped.to_f64().ok_or_else(|| StreamError::NormalizationError {
            reason: "Decimal-to-f64 conversion failed for normalized value".into(),
        })
    }

    /// Reset the normalizer, clearing all observations and the cache.
    pub fn reset(&mut self) {
        self.window.clear();
        self.cached_min = Decimal::MAX;
        self.cached_max = Decimal::MIN;
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

    /// Feed a slice of values into the window and return normalized forms of each.
    ///
    /// Each value in `values` is first passed through [`update`](Self::update) to
    /// advance the rolling window, then normalized against the current window state.
    /// The output has the same length as `values`.
    ///
    /// # Errors
    /// Propagates the first [`StreamError`] returned by [`normalize`](Self::normalize).
    pub fn normalize_batch(
        &mut self,
        values: &[rust_decimal::Decimal],
    ) -> Result<Vec<f64>, crate::error::StreamError> {
        values
            .iter()
            .map(|&v| {
                self.update(v);
                self.normalize(v)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn norm(w: usize) -> MinMaxNormalizer {
        MinMaxNormalizer::new(w).unwrap()
    }

    // ── Construction ─────────────────────────────────────────────────────────

    #[test]
    fn test_new_normalizer_is_empty() {
        let n = norm(4);
        assert!(n.is_empty());
        assert_eq!(n.len(), 0);
    }

    #[test]
    fn test_new_zero_window_returns_error() {
        let result = MinMaxNormalizer::new(0);
        assert!(matches!(result, Err(StreamError::ConfigError { .. })));
    }

    // ── Normalization range [0, 1] ────────────────────────────────────────────

    #[test]
    fn test_normalize_min_is_zero() {
        let mut n = norm(4);
        n.update(dec!(10));
        n.update(dec!(20));
        n.update(dec!(30));
        n.update(dec!(40));
        let v = n.normalize(dec!(10)).unwrap();
        assert!(
            (v - 0.0).abs() < 1e-10,
            "min should normalize to 0.0, got {v}"
        );
    }

    #[test]
    fn test_normalize_max_is_one() {
        let mut n = norm(4);
        n.update(dec!(10));
        n.update(dec!(20));
        n.update(dec!(30));
        n.update(dec!(40));
        let v = n.normalize(dec!(40)).unwrap();
        assert!(
            (v - 1.0).abs() < 1e-10,
            "max should normalize to 1.0, got {v}"
        );
    }

    #[test]
    fn test_normalize_midpoint_is_half() {
        let mut n = norm(4);
        n.update(dec!(0));
        n.update(dec!(100));
        let v = n.normalize(dec!(50)).unwrap();
        assert!((v - 0.5).abs() < 1e-10);
    }

    #[test]
    fn test_normalize_result_clamped_below_zero() {
        let mut n = norm(4);
        n.update(dec!(50));
        n.update(dec!(100));
        // 10 is below the window min of 50
        let v = n.normalize(dec!(10)).unwrap();
        assert!(v >= 0.0);
        assert_eq!(v, 0.0);
    }

    #[test]
    fn test_normalize_result_clamped_above_one() {
        let mut n = norm(4);
        n.update(dec!(50));
        n.update(dec!(100));
        // 200 is above the window max of 100
        let v = n.normalize(dec!(200)).unwrap();
        assert!(v <= 1.0);
        assert_eq!(v, 1.0);
    }

    #[test]
    fn test_normalize_all_same_values_returns_zero() {
        let mut n = norm(4);
        n.update(dec!(5));
        n.update(dec!(5));
        n.update(dec!(5));
        let v = n.normalize(dec!(5)).unwrap();
        assert_eq!(v, 0.0);
    }

    // ── Empty window error ────────────────────────────────────────────────────

    #[test]
    fn test_normalize_empty_window_returns_error() {
        let mut n = norm(4);
        let err = n.normalize(dec!(1)).unwrap_err();
        assert!(matches!(err, StreamError::NormalizationError { .. }));
    }

    #[test]
    fn test_min_max_empty_returns_none() {
        let mut n = norm(4);
        assert!(n.min_max().is_none());
    }

    // ── Rolling window eviction ───────────────────────────────────────────────

    /// After the window fills and the minimum is evicted, the new min must
    /// reflect the remaining values.
    #[test]
    fn test_rolling_window_evicts_oldest() {
        let mut n = norm(3);
        n.update(dec!(1)); // will be evicted
        n.update(dec!(5));
        n.update(dec!(10));
        n.update(dec!(20)); // evicts 1
        let (min, max) = n.min_max().unwrap();
        assert_eq!(min, dec!(5));
        assert_eq!(max, dec!(20));
    }

    #[test]
    fn test_rolling_window_len_does_not_exceed_capacity() {
        let mut n = norm(3);
        for i in 0..10 {
            n.update(Decimal::from(i));
        }
        assert_eq!(n.len(), 3);
    }

    // ── Reset behavior ────────────────────────────────────────────────────────

    #[test]
    fn test_reset_clears_window() {
        let mut n = norm(4);
        n.update(dec!(10));
        n.update(dec!(20));
        n.reset();
        assert!(n.is_empty());
        assert!(n.min_max().is_none());
    }

    #[test]
    fn test_normalize_works_after_reset() {
        let mut n = norm(4);
        n.update(dec!(10));
        n.reset();
        n.update(dec!(0));
        n.update(dec!(100));
        let v = n.normalize(dec!(100)).unwrap();
        assert!((v - 1.0).abs() < 1e-10);
    }

    // ── Streaming update ──────────────────────────────────────────────────────

    #[test]
    fn test_streaming_updates_monotone_sequence() {
        let mut n = norm(5);
        let prices = [dec!(100), dec!(101), dec!(102), dec!(103), dec!(104), dec!(105)];
        for &p in &prices {
            n.update(p);
        }
        // Window now holds [101, 102, 103, 104, 105]; min=101, max=105
        let v_min = n.normalize(dec!(101)).unwrap();
        let v_max = n.normalize(dec!(105)).unwrap();
        assert!((v_min - 0.0).abs() < 1e-10);
        assert!((v_max - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_normalization_monotonicity_in_window() {
        let mut n = norm(10);
        for i in 0..10 {
            n.update(Decimal::from(i * 10));
        }
        // Values 0, 10, 20, ..., 90 in window; min=0, max=90
        let v0 = n.normalize(dec!(0)).unwrap();
        let v50 = n.normalize(dec!(50)).unwrap();
        let v90 = n.normalize(dec!(90)).unwrap();
        assert!(v0 < v50, "normalized values should be monotone");
        assert!(v50 < v90, "normalized values should be monotone");
    }

    #[test]
    fn test_high_precision_input_preserved() {
        // Verify that a value like 50000.12345678 is handled without f64 loss.
        let mut n = norm(2);
        n.update(dec!(50000.00000000));
        n.update(dec!(50000.12345678));
        let (min, max) = n.min_max().unwrap();
        assert_eq!(min, dec!(50000.00000000));
        assert_eq!(max, dec!(50000.12345678));
    }
}
