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
    #[must_use = "normalized value is returned; ignoring it loses the result"]
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

    /// Inverse of [`normalize`](Self::normalize): map a `[0, 1]` value back to
    /// the original scale.
    ///
    /// `denormalized = normalized * (max - min) + min`
    ///
    /// Works outside `[0, 1]` for extrapolation, but returns
    /// [`StreamError::NormalizationError`] if the window is empty.
    pub fn denormalize(&mut self, normalized: f64) -> Result<Decimal, StreamError> {
        use rust_decimal::prelude::FromPrimitive;
        let (min, max) = self
            .min_max()
            .ok_or_else(|| StreamError::NormalizationError {
                reason: "window is empty; call update() before denormalize()".into(),
            })?;
        let scale = max - min;
        let n_dec = Decimal::from_f64(normalized).ok_or_else(|| StreamError::NormalizationError {
            reason: "normalized value is not a finite f64".into(),
        })?;
        Ok(n_dec * scale + min)
    }

    /// Scale of the current window: `max - min`.
    ///
    /// Returns `None` if the window is empty. Returns `Decimal::ZERO` when all
    /// observations are identical (zero range → degenerate distribution).
    pub fn range(&mut self) -> Option<Decimal> {
        let (min, max) = self.min_max()?;
        Some(max - min)
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

    /// Returns `true` when the window holds exactly `window_size` observations.
    ///
    /// At full capacity the normalizer has seen enough data for stable min/max
    /// estimates; before this point early observations dominate the range.
    pub fn is_full(&self) -> bool {
        self.window.len() == self.window_size
    }

    /// Returns the arithmetic mean of the current window observations.
    ///
    /// Returns `None` if the window is empty.
    pub fn mean(&self) -> Option<Decimal> {
        if self.window.is_empty() {
            return None;
        }
        let sum: Decimal = self.window.iter().copied().sum();
        Some(sum / Decimal::from(self.window.len() as u64))
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

    /// Normalize `value` and clamp the result to `[0.0, 1.0]`.
    ///
    /// Identical to [`normalize`](Self::normalize) but silently clamps values
    /// that fall outside the window's observed range. Useful when applying a
    /// learned normalizer to out-of-sample data without erroring on outliers.
    ///
    /// # Errors
    ///
    /// Returns [`StreamError::NormalizationError`] if the window is empty.
    pub fn normalize_clamp(
        &mut self,
        value: rust_decimal::Decimal,
    ) -> Result<f64, crate::error::StreamError> {
        self.normalize(value).map(|v| v.clamp(0.0, 1.0))
    }

    /// Compute the z-score of `value` relative to the current window.
    ///
    /// `z = (value - mean) / stddev`
    ///
    /// Returns `None` if the window has fewer than 2 observations, or if the
    /// standard deviation is zero (all values identical).
    ///
    /// Useful for detecting outliers and standardising features for ML models
    /// when a bounded `[0, 1]` range is not required.
    pub fn z_score(&self, value: Decimal) -> Option<f64> {
        if self.window.len() < 2 {
            return None;
        }
        let n = Decimal::from(self.window.len() as u64);
        let mean: Decimal = self.window.iter().copied().sum::<Decimal>() / n;
        let variance: Decimal = self
            .window
            .iter()
            .map(|&v| { let d = v - mean; d * d })
            .sum::<Decimal>()
            / n;
        if variance.is_zero() {
            return None;
        }
        use rust_decimal::prelude::ToPrimitive;
        let std_dev_f64 = variance.to_f64()?.sqrt();
        let value_f64 = value.to_f64()?;
        let mean_f64 = mean.to_f64()?;
        Some((value_f64 - mean_f64) / std_dev_f64)
    }

    /// Returns the percentile rank of `value` within the current window.
    ///
    /// The percentile rank is the fraction of window values that are `<= value`,
    /// expressed in `[0.0, 1.0]`. Returns `None` if the window is empty.
    ///
    /// Useful for identifying whether the current value is historically high or low
    /// relative to its recent context without requiring a min/max range.
    pub fn percentile_rank(&self, value: rust_decimal::Decimal) -> Option<f64> {
        if self.window.is_empty() {
            return None;
        }
        let count_le = self
            .window
            .iter()
            .filter(|&&v| v <= value)
            .count();
        Some(count_le as f64 / self.window.len() as f64)
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

    // ── denormalize ───────────────────────────────────────────────────────────

    #[test]
    fn test_denormalize_empty_window_returns_error() {
        let mut n = norm(4);
        assert!(matches!(n.denormalize(0.5), Err(StreamError::NormalizationError { .. })));
    }

    #[test]
    fn test_denormalize_roundtrip_min() {
        let mut n = norm(4);
        for v in [dec!(10), dec!(20), dec!(30), dec!(40)] {
            n.update(v);
        }
        let normalized = n.normalize(dec!(10)).unwrap(); // should be ~0.0
        let back = n.denormalize(normalized).unwrap();
        assert!((back - dec!(10)).abs() < dec!(0.0001));
    }

    #[test]
    fn test_denormalize_roundtrip_max() {
        let mut n = norm(4);
        for v in [dec!(10), dec!(20), dec!(30), dec!(40)] {
            n.update(v);
        }
        let normalized = n.normalize(dec!(40)).unwrap(); // should be ~1.0
        let back = n.denormalize(normalized).unwrap();
        assert!((back - dec!(40)).abs() < dec!(0.0001));
    }

    // ── range ─────────────────────────────────────────────────────────────────

    #[test]
    fn test_range_none_when_empty() {
        let mut n = norm(4);
        assert!(n.range().is_none());
    }

    #[test]
    fn test_range_zero_when_all_same() {
        let mut n = norm(3);
        n.update(dec!(5));
        n.update(dec!(5));
        n.update(dec!(5));
        assert_eq!(n.range(), Some(dec!(0)));
    }

    #[test]
    fn test_range_correct() {
        let mut n = norm(4);
        for v in [dec!(10), dec!(40), dec!(20), dec!(30)] {
            n.update(v);
        }
        assert_eq!(n.range(), Some(dec!(30))); // 40 - 10
    }

    // ── MinMaxNormalizer::normalize_clamp ─────────────────────────────────────

    #[test]
    fn test_normalize_clamp_in_range_equals_normalize() {
        let mut n = norm(4);
        for v in [dec!(0), dec!(25), dec!(75), dec!(100)] {
            n.update(v);
        }
        let clamped = n.normalize_clamp(dec!(50)).unwrap();
        let normal = n.normalize(dec!(50)).unwrap();
        assert!((clamped - normal).abs() < 1e-9);
    }

    #[test]
    fn test_normalize_clamp_above_max_clamped_to_one() {
        let mut n = norm(3);
        for v in [dec!(0), dec!(50), dec!(100)] {
            n.update(v);
        }
        // 200 is above the window max of 100; normalize would return > 1.0
        let clamped = n.normalize_clamp(dec!(200)).unwrap();
        assert!((clamped - 1.0).abs() < 1e-9, "expected 1.0 got {clamped}");
    }

    #[test]
    fn test_normalize_clamp_below_min_clamped_to_zero() {
        let mut n = norm(3);
        for v in [dec!(10), dec!(50), dec!(100)] {
            n.update(v);
        }
        // -50 is below the window min of 10; normalize would return < 0.0
        let clamped = n.normalize_clamp(dec!(-50)).unwrap();
        assert!((clamped - 0.0).abs() < 1e-9, "expected 0.0 got {clamped}");
    }

    #[test]
    fn test_normalize_clamp_empty_window_returns_error() {
        let mut n = norm(4);
        assert!(n.normalize_clamp(dec!(5)).is_err());
    }
}

/// Rolling z-score normalizer over a sliding window of [`Decimal`] observations.
///
/// Maps each new sample to its z-score: `(x - mean) / std_dev`. The rolling
/// window maintains an O(1) mean and variance via incremental sum/sum-of-squares
/// tracking, with O(W) recompute only when a value is evicted.
///
/// Returns 0.0 when the window has fewer than 2 observations (variance is 0).
///
/// # Example
///
/// ```rust
/// use fin_stream::norm::ZScoreNormalizer;
/// use rust_decimal_macros::dec;
///
/// let mut norm = ZScoreNormalizer::new(5).unwrap();
/// for v in [dec!(10), dec!(20), dec!(30), dec!(40), dec!(50)] {
///     norm.update(v);
/// }
/// // 30 is the mean; normalize returns 0.0
/// let z = norm.normalize(dec!(30)).unwrap();
/// assert!((z - 0.0).abs() < 1e-9);
/// ```
pub struct ZScoreNormalizer {
    window_size: usize,
    window: VecDeque<Decimal>,
    sum: Decimal,
    sum_sq: Decimal,
}

impl ZScoreNormalizer {
    /// Create a new z-score normalizer with the given rolling window size.
    ///
    /// # Errors
    ///
    /// Returns [`StreamError::ConfigError`] if `window_size == 0`.
    pub fn new(window_size: usize) -> Result<Self, StreamError> {
        if window_size == 0 {
            return Err(StreamError::ConfigError {
                reason: "ZScoreNormalizer window_size must be > 0".into(),
            });
        }
        Ok(Self {
            window_size,
            window: VecDeque::with_capacity(window_size),
            sum: Decimal::ZERO,
            sum_sq: Decimal::ZERO,
        })
    }

    /// Add a new observation to the rolling window.
    ///
    /// Evicts the oldest value when the window is full, adjusting running sums
    /// in O(1). No full recompute is needed unless eviction causes sum drift;
    /// the implementation recomputes exactly when necessary via `recompute`.
    pub fn update(&mut self, value: Decimal) {
        if self.window.len() == self.window_size {
            let evicted = self.window.pop_front().unwrap_or(Decimal::ZERO);
            self.sum -= evicted;
            self.sum_sq -= evicted * evicted;
        }
        self.window.push_back(value);
        self.sum += value;
        self.sum_sq += value * value;
    }

    /// Normalize `value` to a z-score using the current window's mean and std dev.
    ///
    /// Returns 0.0 if:
    /// - The window has fewer than 2 observations (std dev undefined).
    /// - The standard deviation is effectively zero (all window values identical).
    ///
    /// # Errors
    ///
    /// Returns [`StreamError::NormalizationError`] if the window is empty.
    ///
    /// # Complexity: O(1)
    #[must_use = "z-score is returned; ignoring it loses the normalized value"]
    pub fn normalize(&self, value: Decimal) -> Result<f64, StreamError> {
        let n = self.window.len();
        if n == 0 {
            return Err(StreamError::NormalizationError {
                reason: "window is empty; call update() before normalize()".into(),
            });
        }
        if n < 2 {
            return Ok(0.0);
        }
        let n_dec = Decimal::from(n as u64);
        let mean = self.sum / n_dec;
        // Population variance = E[X²] - (E[X])²
        let variance = (self.sum_sq / n_dec) - mean * mean;
        // Clamp to zero to guard against floating-point subtraction underflow.
        let variance = if variance < Decimal::ZERO {
            Decimal::ZERO
        } else {
            variance
        };
        let var_f64 = variance.to_f64().ok_or_else(|| StreamError::NormalizationError {
            reason: "Decimal-to-f64 conversion failed for variance".into(),
        })?;
        let std_dev = var_f64.sqrt();
        if std_dev < f64::EPSILON {
            return Ok(0.0);
        }
        let diff = value - mean;
        let diff_f64 = diff.to_f64().ok_or_else(|| StreamError::NormalizationError {
            reason: "Decimal-to-f64 conversion failed for diff".into(),
        })?;
        Ok(diff_f64 / std_dev)
    }

    /// Current rolling mean of the window, or `None` if the window is empty.
    pub fn mean(&self) -> Option<Decimal> {
        if self.window.is_empty() {
            return None;
        }
        let n = Decimal::from(self.window.len() as u64);
        Some(self.sum / n)
    }

    /// Current population standard deviation of the window.
    ///
    /// Returns `None` if the window is empty. Returns `Some(0.0)` if fewer
    /// than 2 observations are present (undefined variance, treated as zero)
    /// or if all values are identical.
    pub fn std_dev(&self) -> Option<f64> {
        let n = self.window.len();
        if n == 0 {
            return None;
        }
        if n < 2 {
            return Some(0.0);
        }
        let n_dec = Decimal::from(n as u64);
        let mean = self.sum / n_dec;
        let variance = (self.sum_sq / n_dec) - mean * mean;
        let variance = if variance < Decimal::ZERO { Decimal::ZERO } else { variance };
        let var_f64 = variance.to_f64().unwrap_or(0.0);
        Some(var_f64.sqrt())
    }

    /// Reset the normalizer, clearing all observations and sums.
    pub fn reset(&mut self) {
        self.window.clear();
        self.sum = Decimal::ZERO;
        self.sum_sq = Decimal::ZERO;
    }

    /// Number of observations currently in the window.
    pub fn len(&self) -> usize {
        self.window.len()
    }

    /// Returns `true` if no observations have been added since construction or reset.
    pub fn is_empty(&self) -> bool {
        self.window.is_empty()
    }

    /// The configured window size.
    pub fn window_size(&self) -> usize {
        self.window_size
    }

    /// Returns `true` when the window holds exactly `window_size` observations.
    ///
    /// At full capacity the z-score calculation is stable; before this point
    /// the window may not be representative of the underlying distribution.
    pub fn is_full(&self) -> bool {
        self.window.len() == self.window_size
    }

    /// Current population variance of the window.
    ///
    /// Computed as `E[X²] − (E[X])²` from running sums in O(1). Returns
    /// `None` if fewer than 2 observations are present (variance undefined).
    pub fn variance(&self) -> Option<Decimal> {
        let n = self.window.len();
        if n < 2 {
            return None;
        }
        let n_dec = Decimal::from(n as u64);
        let mean = self.sum / n_dec;
        let v = (self.sum_sq / n_dec) - mean * mean;
        Some(if v < Decimal::ZERO { Decimal::ZERO } else { v })
    }

    /// Feed a slice of values into the window and return z-scores for each.
    ///
    /// Each value is first passed through [`update`](Self::update) to advance
    /// the rolling window, then normalized. The output has the same length as
    /// `values`.
    ///
    /// # Errors
    ///
    /// Propagates the first [`StreamError`] returned by [`normalize`](Self::normalize).
    pub fn normalize_batch(
        &mut self,
        values: &[Decimal],
    ) -> Result<Vec<f64>, StreamError> {
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
mod zscore_tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn znorm(w: usize) -> ZScoreNormalizer {
        ZScoreNormalizer::new(w).unwrap()
    }

    #[test]
    fn test_zscore_new_zero_window_returns_error() {
        assert!(matches!(
            ZScoreNormalizer::new(0),
            Err(StreamError::ConfigError { .. })
        ));
    }

    #[test]
    fn test_zscore_empty_window_returns_error() {
        let n = znorm(4);
        assert!(matches!(
            n.normalize(dec!(1)),
            Err(StreamError::NormalizationError { .. })
        ));
    }

    #[test]
    fn test_zscore_single_value_returns_zero() {
        let mut n = znorm(4);
        n.update(dec!(50));
        assert_eq!(n.normalize(dec!(50)).unwrap(), 0.0);
    }

    #[test]
    fn test_zscore_mean_is_zero() {
        let mut n = znorm(5);
        for v in [dec!(10), dec!(20), dec!(30), dec!(40), dec!(50)] {
            n.update(v);
        }
        // mean = 30; z-score of 30 should be 0
        let z = n.normalize(dec!(30)).unwrap();
        assert!((z - 0.0).abs() < 1e-9, "z-score of mean should be 0, got {z}");
    }

    #[test]
    fn test_zscore_symmetric_around_mean() {
        let mut n = znorm(4);
        for v in [dec!(10), dec!(20), dec!(30), dec!(40)] {
            n.update(v);
        }
        // mean = 25; values equidistant above and below mean have equal |z|
        let z_low = n.normalize(dec!(15)).unwrap();
        let z_high = n.normalize(dec!(35)).unwrap();
        assert!((z_low.abs() - z_high.abs()).abs() < 1e-9);
        assert!(z_low < 0.0, "below-mean z-score should be negative");
        assert!(z_high > 0.0, "above-mean z-score should be positive");
    }

    #[test]
    fn test_zscore_all_same_returns_zero() {
        let mut n = znorm(4);
        for _ in 0..4 {
            n.update(dec!(100));
        }
        assert_eq!(n.normalize(dec!(100)).unwrap(), 0.0);
    }

    #[test]
    fn test_zscore_rolling_window_eviction() {
        let mut n = znorm(3);
        n.update(dec!(1));
        n.update(dec!(2));
        n.update(dec!(3));
        // Evict 1, add 100 — window is [2, 3, 100]
        n.update(dec!(100));
        // mean ≈ 35; value 100 should have positive z-score
        let z = n.normalize(dec!(100)).unwrap();
        assert!(z > 0.0);
    }

    #[test]
    fn test_zscore_reset_clears_state() {
        let mut n = znorm(4);
        for v in [dec!(10), dec!(20), dec!(30)] {
            n.update(v);
        }
        n.reset();
        assert!(n.is_empty());
        assert!(n.mean().is_none());
        assert!(matches!(
            n.normalize(dec!(1)),
            Err(StreamError::NormalizationError { .. })
        ));
    }

    #[test]
    fn test_zscore_len_and_window_size() {
        let mut n = znorm(5);
        assert_eq!(n.len(), 0);
        assert!(n.is_empty());
        n.update(dec!(1));
        n.update(dec!(2));
        assert_eq!(n.len(), 2);
        assert_eq!(n.window_size(), 5);
    }

    // ── std_dev ───────────────────────────────────────────────────────────────

    #[test]
    fn test_std_dev_none_when_empty() {
        let n = znorm(5);
        assert!(n.std_dev().is_none());
    }

    #[test]
    fn test_std_dev_zero_with_one_observation() {
        let mut n = znorm(5);
        n.update(dec!(42));
        assert_eq!(n.std_dev(), Some(0.0));
    }

    #[test]
    fn test_std_dev_zero_when_all_same() {
        let mut n = znorm(4);
        for _ in 0..4 {
            n.update(dec!(10));
        }
        let sd = n.std_dev().unwrap();
        assert!(sd < f64::EPSILON);
    }

    #[test]
    fn test_std_dev_positive_for_varying_values() {
        let mut n = znorm(4);
        for v in [dec!(10), dec!(20), dec!(30), dec!(40)] {
            n.update(v);
        }
        let sd = n.std_dev().unwrap();
        // Population std dev of [10,20,30,40]: mean=25, var=125, sd≈11.18
        assert!((sd - 11.18).abs() < 0.01);
    }

    // ── ZScoreNormalizer::variance ────────────────────────────────────────────

    #[test]
    fn test_variance_none_when_fewer_than_two_observations() {
        let mut n = znorm(5);
        assert!(n.variance().is_none());
        n.update(dec!(10));
        assert!(n.variance().is_none());
    }

    #[test]
    fn test_variance_zero_for_identical_values() {
        let mut n = znorm(4);
        for _ in 0..4 {
            n.update(dec!(7));
        }
        assert_eq!(n.variance().unwrap(), dec!(0));
    }

    #[test]
    fn test_variance_correct_for_known_values() {
        let mut n = znorm(4);
        for v in [dec!(10), dec!(20), dec!(30), dec!(40)] {
            n.update(v);
        }
        // Population variance of [10,20,30,40]: mean=25, var=125
        let var = n.variance().unwrap();
        let var_f64 = f64::try_from(var).unwrap();
        assert!((var_f64 - 125.0).abs() < 0.01, "expected 125 got {var_f64}");
    }

    // ── ZScoreNormalizer::normalize_batch ─────────────────────────────────────

    #[test]
    fn test_normalize_batch_same_length_as_input() {
        let mut n = znorm(5);
        let vals = [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)];
        let out = n.normalize_batch(&vals).unwrap();
        assert_eq!(out.len(), vals.len());
    }

    #[test]
    fn test_normalize_batch_last_value_matches_single_normalize() {
        let mut n1 = znorm(5);
        let vals = [dec!(10), dec!(20), dec!(30), dec!(40), dec!(50)];
        let batch = n1.normalize_batch(&vals).unwrap();

        let mut n2 = znorm(5);
        for &v in &vals {
            n2.update(v);
        }
        let single = n2.normalize(dec!(50)).unwrap();
        assert!((batch[4] - single).abs() < 1e-9);
    }
}
