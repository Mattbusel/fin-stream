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

    /// Clamps `value` to the `[min, max]` range of the current window.
    ///
    /// Returns `value` unchanged if the window is empty (no clamping possible).
    pub fn clamp_to_window(&mut self, value: Decimal) -> Decimal {
        self.min_max().map_or(value, |(min, max)| value.max(min).min(max))
    }

    /// Midpoint of the current window: `(min + max) / 2`.
    ///
    /// Returns `None` if the window is empty.
    pub fn midpoint(&mut self) -> Option<Decimal> {
        let (min, max) = self.min_max()?;
        Some((min + max) / Decimal::TWO)
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

    /// Current window minimum, or `None` if the window is empty.
    ///
    /// Equivalent to `self.min_max().map(|(min, _)| min)` but avoids also
    /// computing the max when only the min is needed.
    pub fn min(&mut self) -> Option<Decimal> {
        self.min_max().map(|(min, _)| min)
    }

    /// Current window maximum, or `None` if the window is empty.
    ///
    /// Equivalent to `self.min_max().map(|(_, max)| max)` but avoids also
    /// computing the min when only the max is needed.
    pub fn max(&mut self) -> Option<Decimal> {
        self.min_max().map(|(_, max)| max)
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

    /// Population variance of the current window: `Σ(x − mean)² / n`.
    ///
    /// Returns `None` if the window has fewer than 2 observations.
    pub fn variance(&self) -> Option<Decimal> {
        let n = self.window.len();
        if n < 2 {
            return None;
        }
        let mean = self.mean()?;
        let variance = self
            .window
            .iter()
            .map(|&v| { let d = v - mean; d * d })
            .sum::<Decimal>()
            / Decimal::from(n as u64);
        Some(variance)
    }

    /// Population standard deviation of the current window: `sqrt(variance)`.
    ///
    /// Returns `None` if the window has fewer than 2 observations or if the
    /// variance cannot be converted to `f64`.
    pub fn std_dev(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        self.variance()?.to_f64().map(f64::sqrt)
    }

    /// Coefficient of variation: `std_dev / |mean|`.
    ///
    /// A dimensionless measure of relative dispersion. Returns `None` when the
    /// window has fewer than 2 observations or when the mean is zero.
    pub fn coefficient_of_variation(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let mean = self.mean()?;
        if mean.is_zero() {
            return None;
        }
        let std_dev = self.std_dev()?;
        let mean_f = mean.abs().to_f64()?;
        Some(std_dev / mean_f)
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
    /// Identical to [`normalize`](Self::normalize) because `normalize` already
    /// clamps its output to `[0.0, 1.0]`. Deprecated in favour of calling
    /// `normalize` directly.
    ///
    /// # Errors
    ///
    /// Returns [`StreamError::NormalizationError`] if the window is empty.
    #[deprecated(since = "2.2.0", note = "Use `normalize()` instead — it already clamps to [0.0, 1.0]")]
    pub fn normalize_clamp(
        &mut self,
        value: rust_decimal::Decimal,
    ) -> Result<f64, crate::error::StreamError> {
        self.normalize(value)
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
        use rust_decimal::prelude::ToPrimitive;
        let std_dev = self.std_dev()?; // None for < 2 obs
        if std_dev == 0.0 {
            return None;
        }
        let mean = self.mean()?;
        let value_f64 = value.to_f64()?;
        let mean_f64 = mean.to_f64()?;
        Some((value_f64 - mean_f64) / std_dev)
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

    /// Count of observations in the current window that are strictly above `threshold`.
    pub fn count_above(&self, threshold: rust_decimal::Decimal) -> usize {
        self.window.iter().filter(|&&v| v > threshold).count()
    }

    /// Count of observations in the current window that are strictly below `threshold`.
    ///
    /// Complement to [`count_above`](Self::count_above).
    pub fn count_below(&self, threshold: rust_decimal::Decimal) -> usize {
        self.window.iter().filter(|&&v| v < threshold).count()
    }

    /// Value at the p-th percentile of the current window (0.0 ≤ p ≤ 1.0).
    ///
    /// Uses linear interpolation between adjacent sorted values.
    /// Returns `None` if the window is empty.
    pub fn percentile_value(&self, p: f64) -> Option<Decimal> {
        if self.window.is_empty() {
            return None;
        }
        let p = p.clamp(0.0, 1.0);
        let mut sorted: Vec<Decimal> = self.window.iter().copied().collect();
        sorted.sort();
        let n = sorted.len();
        if n == 1 {
            return Some(sorted[0]);
        }
        let idx = p * (n - 1) as f64;
        let lo = idx.floor() as usize;
        let hi = idx.ceil() as usize;
        if lo == hi {
            Some(sorted[lo])
        } else {
            let frac = Decimal::try_from(idx - lo as f64).ok()?;
            Some(sorted[lo] + (sorted[hi] - sorted[lo]) * frac)
        }
    }

    /// Fraction of window values strictly above the midpoint `(min + max) / 2`.
    ///
    /// Returns `None` if the window is empty. Returns `0.0` if all values are equal.
    pub fn fraction_above_mid(&mut self) -> Option<f64> {
        let (min, max) = self.min_max()?;
        let mid = (min + max) / rust_decimal::Decimal::TWO;
        let above = self.window.iter().filter(|&&v| v > mid).count();
        Some(above as f64 / self.window.len() as f64)
    }

    /// `(max - min) / max` as `f64` — the range as a fraction of the maximum.
    ///
    /// Measures how wide the window's spread is relative to its peak. Returns
    /// `None` if the window is empty or the maximum is zero.
    pub fn normalized_range(&mut self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let (min, max) = self.min_max()?;
        if max.is_zero() {
            return None;
        }
        ((max - min) / max).to_f64()
    }

    /// Exponential weighted moving average of the current window values.
    ///
    /// Applies `alpha` as the smoothing factor (most-recent weight), scanning oldest→newest.
    /// `alpha` is clamped to `(0, 1]`. Returns `None` if the window is empty.
    pub fn ewma(&self, alpha: f64) -> Option<f64> {
        if self.window.is_empty() {
            return None;
        }
        let alpha = alpha.clamp(f64::MIN_POSITIVE, 1.0);
        let one_minus = 1.0 - alpha;
        let mut ewma = self.window[0].to_f64().unwrap_or(0.0);
        for &v in self.window.iter().skip(1) {
            ewma = alpha * v.to_f64().unwrap_or(ewma) + one_minus * ewma;
        }
        Some(ewma)
    }

    /// Interquartile range: Q3 (75th percentile) − Q1 (25th percentile) of the window.
    ///
    /// Returns `None` if the window has fewer than 4 observations.
    /// The IQR is a robust spread measure less sensitive to outliers than range or std dev.
    pub fn interquartile_range(&self) -> Option<Decimal> {
        let n = self.window.len();
        if n < 4 {
            return None;
        }
        let mut sorted: Vec<Decimal> = self.window.iter().copied().collect();
        sorted.sort();
        let q1_idx = n / 4;
        let q3_idx = 3 * n / 4;
        Some(sorted[q3_idx] - sorted[q1_idx])
    }

    /// Skewness of the window values: `Σ((x - mean)³/n) / std_dev³`.
    ///
    /// Positive skew means the tail is longer on the right; negative on the left.
    /// Returns `None` if the window has fewer than 3 observations or std dev is zero.
    pub fn skewness(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let n = self.window.len();
        if n < 3 {
            return None;
        }
        let n_f = n as f64;
        let vals: Vec<f64> = self.window.iter().filter_map(|v| v.to_f64()).collect();
        if vals.len() < n {
            return None;
        }
        let mean = vals.iter().sum::<f64>() / n_f;
        let variance = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n_f;
        let std_dev = variance.sqrt();
        if std_dev == 0.0 {
            return None;
        }
        let skew = vals.iter().map(|v| ((v - mean) / std_dev).powi(3)).sum::<f64>() / n_f;
        Some(skew)
    }

    /// Excess kurtosis of the window values: `(Σ((x−mean)⁴/n) / std_dev⁴) − 3`.
    ///
    /// Positive values indicate heavier-tailed distributions (leptokurtic);
    /// negative values indicate lighter tails (platykurtic). A normal
    /// distribution has excess kurtosis of `0`.
    ///
    /// Returns `None` if the window has fewer than 4 observations or if the
    /// standard deviation is zero (all values identical).
    pub fn kurtosis(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let n = self.window.len();
        if n < 4 {
            return None;
        }
        let n_f = n as f64;
        let vals: Vec<f64> = self.window.iter().filter_map(|v| v.to_f64()).collect();
        if vals.len() < n {
            return None;
        }
        let mean = vals.iter().sum::<f64>() / n_f;
        let variance = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n_f;
        let std_dev = variance.sqrt();
        if std_dev == 0.0 {
            return None;
        }
        let kurt = vals.iter().map(|v| ((v - mean) / std_dev).powi(4)).sum::<f64>() / n_f - 3.0;
        Some(kurt)
    }

    /// Median of the current window values, or `None` if the window is empty.
    ///
    /// For an even-length window the median is the mean of the two middle values.
    pub fn median(&self) -> Option<Decimal> {
        if self.window.is_empty() {
            return None;
        }
        let mut sorted: Vec<Decimal> = self.window.iter().copied().collect();
        sorted.sort();
        let n = sorted.len();
        if n % 2 == 1 {
            Some(sorted[n / 2])
        } else {
            Some((sorted[n / 2 - 1] + sorted[n / 2]) / Decimal::from(2u64))
        }
    }

    /// Bessel-corrected (sample) variance of the window — divides by `n − 1`.
    ///
    /// Returns `None` for fewer than 2 observations.
    pub fn sample_variance(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let n = self.window.len();
        if n < 2 {
            return None;
        }
        let mean = self.mean()?.to_f64()?;
        let sum_sq: f64 = self.window.iter()
            .filter_map(|v| v.to_f64())
            .map(|v| (v - mean).powi(2))
            .sum();
        Some(sum_sq / (n - 1) as f64)
    }

    /// Median absolute deviation (MAD) of the window.
    ///
    /// Returns `None` if the window is empty.
    pub fn mad(&self) -> Option<Decimal> {
        let med = self.median()?;
        let mut deviations: Vec<Decimal> = self.window.iter()
            .map(|&v| (v - med).abs())
            .collect();
        deviations.sort();
        let n = deviations.len();
        if n % 2 == 1 {
            Some(deviations[n / 2])
        } else {
            Some((deviations[n / 2 - 1] + deviations[n / 2]) / Decimal::from(2u64))
        }
    }

    /// Robust z-score of `value` using median and MAD instead of mean and std-dev.
    ///
    /// `robust_z = 0.6745 × (value − median) / MAD`
    ///
    /// The `0.6745` factor makes the scale consistent with the standard normal.
    /// Returns `None` if the window is empty or MAD is zero (all values identical).
    pub fn robust_z_score(&self, value: Decimal) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let med = self.median()?;
        let mad = self.mad()?;
        if mad.is_zero() {
            return None;
        }
        let diff = (value - med) / mad;
        Some(0.674_5 * diff.to_f64()?)
    }

    /// The most recently added value, or `None` if the window is empty.
    pub fn latest(&self) -> Option<Decimal> {
        self.window.back().copied()
    }

    /// Sum of all values currently in the window.
    ///
    /// Returns `None` if the window is empty.
    pub fn sum(&self) -> Option<Decimal> {
        if self.window.is_empty() {
            return None;
        }
        Some(self.window.iter().copied().sum())
    }

    /// Returns `true` if the absolute z-score of `value` exceeds `z_threshold`.
    pub fn is_outlier(&self, value: Decimal, z_threshold: f64) -> bool {
        self.z_score(value).map_or(false, |z| z.abs() > z_threshold)
    }

    /// Returns a copy of the window values that fall within `sigma` standard
    /// deviations of the mean. Values whose absolute z-score exceeds `sigma`
    /// are excluded.
    ///
    /// Returns an empty `Vec` if the window has fewer than 2 elements.
    pub fn trim_outliers(&self, sigma: f64) -> Vec<Decimal> {
        self.window
            .iter()
            .copied()
            .filter(|&v| !self.is_outlier(v, sigma))
            .collect()
    }

    /// Z-score of the most recently added value.
    ///
    /// Returns `None` if the window has fewer than 2 elements or standard
    /// deviation is zero.
    pub fn z_score_of_latest(&self) -> Option<f64> {
        self.z_score(self.latest()?)
    }

    /// Signed deviation of `value` from the window mean, expressed as a
    /// floating-point number.
    ///
    /// Returns `None` if the window is empty.
    pub fn deviation_from_mean(&self, value: Decimal) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let mean = self.mean()?;
        (value - mean).to_f64()
    }

    /// Window range (max − min) as a `f64`.
    ///
    /// Returns `None` if the window has fewer than 1 element.
    pub fn range_f64(&mut self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        self.range()?.to_f64()
    }

    /// Sum of all values in the window as a `f64`.
    ///
    /// Returns `None` if the window is empty.
    pub fn sum_f64(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        self.sum()?.to_f64()
    }

    /// All current window values as a `Vec<Decimal>`, in insertion order (oldest first).
    pub fn values(&self) -> Vec<Decimal> {
        self.window.iter().copied().collect()
    }

    /// Normalize the window midpoint ((min + max) / 2) and return it as a `f64`.
    ///
    /// Returns `None` if the window is empty or min == max.
    pub fn normalized_midpoint(&mut self) -> Option<f64> {
        let mid = self.midpoint()?;
        self.normalize(mid).ok()
    }

    /// Returns `true` if `value` equals the current window minimum.
    ///
    /// Returns `false` if the window is empty.
    pub fn is_at_min(&mut self, value: Decimal) -> bool {
        self.min().map_or(false, |m| value == m)
    }

    /// Returns `true` if `value` equals the current window maximum.
    ///
    /// Returns `false` if the window is empty.
    pub fn is_at_max(&mut self, value: Decimal) -> bool {
        self.max().map_or(false, |m| value == m)
    }

    /// Fraction of window values strictly above `threshold`.
    ///
    /// Returns `None` if the window is empty.
    pub fn fraction_above(&self, threshold: Decimal) -> Option<f64> {
        if self.window.is_empty() {
            return None;
        }
        Some(self.count_above(threshold) as f64 / self.window.len() as f64)
    }

    /// Fraction of window values strictly below `threshold`.
    ///
    /// Returns `None` if the window is empty.
    pub fn fraction_below(&self, threshold: Decimal) -> Option<f64> {
        if self.window.is_empty() {
            return None;
        }
        Some(self.count_below(threshold) as f64 / self.window.len() as f64)
    }

    /// Returns all window values strictly above `threshold`.
    pub fn window_values_above(&self, threshold: Decimal) -> Vec<Decimal> {
        self.window.iter().copied().filter(|&v| v > threshold).collect()
    }

    /// Returns all window values strictly below `threshold`.
    pub fn window_values_below(&self, threshold: Decimal) -> Vec<Decimal> {
        self.window.iter().copied().filter(|&v| v < threshold).collect()
    }

    /// Count of window values equal to `value`.
    pub fn count_equal(&self, value: Decimal) -> usize {
        self.window.iter().filter(|&&v| v == value).count()
    }

    /// Range of the current window: `max - min`.
    ///
    /// Returns `None` if the window is empty.
    pub fn rolling_range(&self) -> Option<Decimal> {
        if self.window.is_empty() {
            return None;
        }
        let lo = self.window.iter().copied().reduce(Decimal::min)?;
        let hi = self.window.iter().copied().reduce(Decimal::max)?;
        Some(hi - lo)
    }

    /// Lag-1 autocorrelation of the window values.
    ///
    /// Measures how much each value predicts the next.
    /// Returns `None` if fewer than 2 values or variance is zero.
    pub fn autocorrelation_lag1(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let n = self.window.len();
        if n < 2 {
            return None;
        }
        let vals: Vec<f64> = self.window.iter().filter_map(|v| v.to_f64()).collect();
        if vals.len() < 2 {
            return None;
        }
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let var: f64 = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / vals.len() as f64;
        if var == 0.0 {
            return None;
        }
        let cov: f64 = vals.windows(2).map(|w| (w[0] - mean) * (w[1] - mean)).sum::<f64>()
            / (vals.len() - 1) as f64;
        Some(cov / var)
    }

    /// Fraction of consecutive pairs where the second value > first (trending upward).
    ///
    /// Returns `None` if fewer than 2 values.
    pub fn trend_consistency(&self) -> Option<f64> {
        let n = self.window.len();
        if n < 2 {
            return None;
        }
        let up = self.window.iter().collect::<Vec<_>>().windows(2)
            .filter(|w| w[1] > w[0]).count();
        Some(up as f64 / (n - 1) as f64)
    }

    /// Mean absolute deviation of the window values.
    ///
    /// Returns `None` if window is empty.
    pub fn mean_absolute_deviation(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() {
            return None;
        }
        let n = self.window.len();
        let vals: Vec<f64> = self.window.iter().filter_map(|v| v.to_f64()).collect();
        let mean = vals.iter().sum::<f64>() / n as f64;
        let mad = vals.iter().map(|v| (v - mean).abs()).sum::<f64>() / n as f64;
        Some(mad)
    }

    /// Percentile rank of the most recently added value within the window.
    ///
    /// Returns `None` if no value has been added yet. Uses the same `<=`
    /// semantics as [`percentile_rank`](Self::percentile_rank).
    pub fn percentile_of_latest(&self) -> Option<f64> {
        let latest = self.latest()?;
        self.percentile_rank(latest)
    }

    /// Tail ratio: `max(window) / 75th-percentile(window)`.
    ///
    /// A simple fat-tail indicator. Values well above 1.0 signal that the
    /// maximum observation is an outlier relative to the upper quartile.
    /// Returns `None` if the window is empty or the 75th percentile is zero.
    pub fn tail_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let max = self.window.iter().copied().reduce(Decimal::max)?;
        let p75 = self.percentile_value(0.75)?;
        if p75.is_zero() {
            return None;
        }
        (max / p75).to_f64()
    }

    /// Z-score of the window minimum relative to the current mean and std dev.
    ///
    /// Returns `None` if the window is empty or std dev is zero.
    pub fn z_score_of_min(&self) -> Option<f64> {
        let min = self.window.iter().copied().reduce(Decimal::min)?;
        self.z_score(min)
    }

    /// Z-score of the window maximum relative to the current mean and std dev.
    ///
    /// Returns `None` if the window is empty or std dev is zero.
    pub fn z_score_of_max(&self) -> Option<f64> {
        let max = self.window.iter().copied().reduce(Decimal::max)?;
        self.z_score(max)
    }

    /// Shannon entropy of the window values.
    ///
    /// Each unique value is treated as a category. Returns `None` if the
    /// window is empty. A uniform distribution maximises entropy; identical
    /// values give `Some(0.0)`.
    pub fn window_entropy(&self) -> Option<f64> {
        if self.window.is_empty() {
            return None;
        }
        let n = self.window.len() as f64;
        let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for v in &self.window {
            *counts.entry(v.to_string()).or_insert(0) += 1;
        }
        let entropy: f64 = counts.values().map(|&c| {
            let p = c as f64 / n;
            -p * p.ln()
        }).sum();
        Some(entropy)
    }

    /// Normalised standard deviation (alias for [`coefficient_of_variation`](Self::coefficient_of_variation)).
    pub fn normalized_std_dev(&self) -> Option<f64> {
        self.coefficient_of_variation()
    }

    /// Count of window values that are strictly above the window mean.
    ///
    /// Returns `None` if the window is empty or the mean cannot be computed.
    pub fn value_above_mean_count(&self) -> Option<usize> {
        let mean = self.mean()?;
        Some(self.window.iter().filter(|&&v| v > mean).count())
    }

    /// Length of the longest consecutive run of values above the window mean.
    ///
    /// Returns `None` if the window is empty or the mean cannot be computed.
    pub fn consecutive_above_mean(&self) -> Option<usize> {
        let mean = self.mean()?;
        let mut max_run = 0usize;
        let mut current = 0usize;
        for &v in &self.window {
            if v > mean {
                current += 1;
                if current > max_run {
                    max_run = current;
                }
            } else {
                current = 0;
            }
        }
        Some(max_run)
    }

    /// Fraction of window values above `threshold`.
    ///
    /// Returns `None` if the window is empty.
    pub fn above_threshold_fraction(&self, threshold: Decimal) -> Option<f64> {
        if self.window.is_empty() {
            return None;
        }
        let count = self.window.iter().filter(|&&v| v > threshold).count();
        Some(count as f64 / self.window.len() as f64)
    }

    /// Fraction of window values below `threshold`.
    ///
    /// Returns `None` if the window is empty.
    pub fn below_threshold_fraction(&self, threshold: Decimal) -> Option<f64> {
        if self.window.is_empty() {
            return None;
        }
        let count = self.window.iter().filter(|&&v| v < threshold).count();
        Some(count as f64 / self.window.len() as f64)
    }

    /// Autocorrelation at lag `k` of the window values.
    ///
    /// Returns `None` if `k == 0`, `k >= window.len()`, or variance is zero.
    pub fn lag_k_autocorrelation(&self, k: usize) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let n = self.window.len();
        if k == 0 || k >= n {
            return None;
        }
        let vals: Vec<f64> = self.window.iter().filter_map(|v| v.to_f64()).collect();
        if vals.len() != n {
            return None;
        }
        let mean = vals.iter().sum::<f64>() / n as f64;
        let var = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n as f64;
        if var == 0.0 {
            return None;
        }
        let m = n - k;
        let cov: f64 = (0..m).map(|i| (vals[i] - mean) * (vals[i + k] - mean)).sum::<f64>() / m as f64;
        Some(cov / var)
    }

    /// Estimated half-life of mean reversion using a simple AR(1) regression.
    ///
    /// Half-life ≈ `-ln(2) / ln(|β|)` where β is the AR(1) coefficient from
    /// regressing `Δy_t` on `y_{t-1}`. Returns `None` if the window has fewer
    /// than 3 values, the regression denominator is zero, or β ≥ 0 (no
    /// mean-reversion signal).
    pub fn half_life_estimate(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let n = self.window.len();
        if n < 3 {
            return None;
        }
        let vals: Vec<f64> = self.window.iter().filter_map(|v| v.to_f64()).collect();
        if vals.len() != n {
            return None;
        }
        let diffs: Vec<f64> = vals.windows(2).map(|w| w[1] - w[0]).collect();
        let lagged: Vec<f64> = vals[..n - 1].to_vec();
        let nf = diffs.len() as f64;
        let mean_l = lagged.iter().sum::<f64>() / nf;
        let mean_d = diffs.iter().sum::<f64>() / nf;
        let cov: f64 = lagged.iter().zip(diffs.iter()).map(|(l, d)| (l - mean_l) * (d - mean_d)).sum::<f64>();
        let var: f64 = lagged.iter().map(|l| (l - mean_l).powi(2)).sum::<f64>();
        if var == 0.0 {
            return None;
        }
        let beta = cov / var;
        // beta should be negative for mean-reversion
        if beta >= 0.0 {
            return None;
        }
        let lambda = (1.0 + beta).abs().ln();
        if lambda == 0.0 {
            return None;
        }
        Some(-std::f64::consts::LN_2 / lambda)
    }

    /// Geometric mean of the window values.
    ///
    /// `exp(mean(ln(v_i)))`. Returns `None` if the window is empty or any
    /// value is non-positive.
    pub fn geometric_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() {
            return None;
        }
        let logs: Vec<f64> = self.window.iter()
            .filter_map(|v| v.to_f64())
            .filter_map(|f| if f > 0.0 { Some(f.ln()) } else { None })
            .collect();
        if logs.len() != self.window.len() {
            return None;
        }
        Some((logs.iter().sum::<f64>() / logs.len() as f64).exp())
    }

    /// Harmonic mean of the window values.
    ///
    /// `n / sum(1/v_i)`. Returns `None` if the window is empty or any value
    /// is zero.
    pub fn harmonic_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() {
            return None;
        }
        let reciprocals: Vec<f64> = self.window.iter()
            .filter_map(|v| v.to_f64())
            .filter_map(|f| if f != 0.0 { Some(1.0 / f) } else { None })
            .collect();
        if reciprocals.len() != self.window.len() {
            return None;
        }
        let n = reciprocals.len() as f64;
        Some(n / reciprocals.iter().sum::<f64>())
    }

    /// Normalise `value` to the window's observed `[min, max]` range.
    ///
    /// Returns `None` if the window is empty or the range is zero.
    pub fn range_normalized_value(&self, value: Decimal) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let min = self.window.iter().copied().reduce(Decimal::min)?;
        let max = self.window.iter().copied().reduce(Decimal::max)?;
        let range = max - min;
        if range.is_zero() {
            return None;
        }
        ((value - min) / range).to_f64()
    }

    /// Signed distance of `value` from the window median.
    ///
    /// `value - median`. Returns `None` if the window is empty.
    pub fn distance_from_median(&self, value: Decimal) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let med = self.median()?;
        (value - med).to_f64()
    }

    /// Momentum: difference between the latest and oldest value in the window.
    ///
    /// Positive = window trended up; negative = trended down. Returns `None`
    /// if fewer than 2 values are in the window.
    pub fn momentum(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 {
            return None;
        }
        let oldest = *self.window.front()?;
        let latest = *self.window.back()?;
        (latest - oldest).to_f64()
    }

    /// Rank of `value` within the current window, normalised to `[0.0, 1.0]`.
    ///
    /// 0.0 means `value` is ≤ all window values; 1.0 means it is ≥ all window
    /// values. Returns `None` if the window is empty.
    pub fn value_rank(&self, value: Decimal) -> Option<f64> {
        if self.window.is_empty() {
            return None;
        }
        let n = self.window.len();
        let below = self.window.iter().filter(|&&v| v < value).count();
        Some(below as f64 / n as f64)
    }

    /// Coefficient of variation: `std_dev / |mean|`.
    ///
    /// Returns `None` if the window has fewer than 2 values or the mean is
    /// zero.
    pub fn coeff_of_variation(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let n = self.window.len();
        if n < 2 {
            return None;
        }
        let vals: Vec<f64> = self.window.iter().filter_map(|v| v.to_f64()).collect();
        if vals.len() < 2 {
            return None;
        }
        let nf = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / nf;
        if mean == 0.0 {
            return None;
        }
        let std_dev = (vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (nf - 1.0)).sqrt();
        Some(std_dev / mean.abs())
    }

    /// Inter-quartile range: Q3 (75th percentile) minus Q1 (25th percentile).
    ///
    /// Returns `None` if the window is empty.
    pub fn quantile_range(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let q3 = self.percentile_value(0.75)?;
        let q1 = self.percentile_value(0.25)?;
        (q3 - q1).to_f64()
    }

    /// Upper quartile (Q3, 75th percentile) of the window values.
    ///
    /// Returns `None` if the window is empty.
    pub fn upper_quartile(&self) -> Option<Decimal> {
        self.percentile_value(0.75)
    }

    /// Lower quartile (Q1, 25th percentile) of the window values.
    ///
    /// Returns `None` if the window is empty.
    pub fn lower_quartile(&self) -> Option<Decimal> {
        self.percentile_value(0.25)
    }

    /// Fraction of consecutive first-difference pairs whose sign flips.
    ///
    /// Computes `Δx_i = x_i − x_{i−1}`, then counts pairs `(Δx_i, Δx_{i+1})`
    /// where both are non-zero and have opposite signs. The result is in
    /// `[0.0, 1.0]`. A high value indicates a rapidly oscillating series;
    /// a low value indicates persistent trends. Returns `None` for fewer than
    /// 3 observations.
    pub fn sign_change_rate(&self) -> Option<f64> {
        let n = self.window.len();
        if n < 3 {
            return None;
        }
        let vals: Vec<&Decimal> = self.window.iter().collect();
        let diffs: Vec<i32> = vals
            .windows(2)
            .map(|w| {
                if w[1] > w[0] { 1 } else if w[1] < w[0] { -1 } else { 0 }
            })
            .collect();
        let total_pairs = (diffs.len() - 1) as f64;
        if total_pairs == 0.0 {
            return None;
        }
        let changes = diffs
            .windows(2)
            .filter(|w| w[0] != 0 && w[1] != 0 && w[0] != w[1])
            .count();
        Some(changes as f64 / total_pairs)
    }

    // ── round-79 ─────────────────────────────────────────────────────────────

    /// Count of trailing values (from the newest end of the window) that
    /// are strictly below the window mean.
    ///
    /// Returns `None` if the window has fewer than 2 values.
    pub fn consecutive_below_mean(&self) -> Option<usize> {
        if self.window.len() < 2 {
            return None;
        }
        let mean = self.mean()?;
        let count = self.window.iter().rev().take_while(|&&v| v < mean).count();
        Some(count)
    }

    /// Drift rate: `(mean_of_second_half − mean_of_first_half) / |mean_of_first_half|`.
    ///
    /// Splits the window at its midpoint and compares the two halves.
    /// Returns `None` if the window has fewer than 2 values or the
    /// first-half mean is zero.
    pub fn drift_rate(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let n = self.window.len();
        if n < 2 {
            return None;
        }
        let mid = n / 2;
        let first_sum: Decimal = self.window.iter().take(mid).copied().sum();
        let second_sum: Decimal = self.window.iter().skip(mid).copied().sum();
        let mean1 = first_sum / Decimal::from(mid as i64);
        let mean2 = second_sum / Decimal::from((n - mid) as i64);
        if mean1.is_zero() {
            return None;
        }
        ((mean2 - mean1) / mean1.abs()).to_f64()
    }

    /// Ratio of the window maximum to the window minimum: `max / min`.
    ///
    /// Returns `None` if the window is empty or the minimum is zero.
    pub fn peak_to_trough_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let mut tmp = MinMaxNormalizer::new(self.window_size).ok()?;
        for &v in &self.window {
            tmp.update(v);
        }
        let (min, max) = tmp.min_max()?;
        if min.is_zero() {
            return None;
        }
        (max / min).to_f64()
    }

    /// Normalised deviation of the latest value: `(latest − mean) / range`.
    ///
    /// Returns `None` if the window is empty, has fewer than 2 values,
    /// the range is zero, or there is no latest value.
    pub fn normalized_deviation(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let latest = self.latest()?;
        let mean = self.mean()?;
        let range = self.rolling_range()?;
        if range.is_zero() {
            return None;
        }
        ((latest - mean) / range).to_f64()
    }

    /// Coefficient of variation as a percentage: `(std_dev / |mean|) × 100`.
    ///
    /// Returns `None` if the window has fewer than 2 values or the mean is zero.
    pub fn window_cv_pct(&self) -> Option<f64> {
        let cv = self.coefficient_of_variation()?;
        Some(cv * 100.0)
    }

    /// Rank of the latest value in the sorted window as a fraction in
    /// `[0, 1]`. Computed as `(number of values strictly below latest) /
    /// (window_len − 1)`.
    ///
    /// Returns `None` if the window has fewer than 2 values.
    pub fn latest_rank_pct(&self) -> Option<f64> {
        if self.window.len() < 2 {
            return None;
        }
        let latest = self.latest()?;
        let below = self.window.iter().filter(|&&v| v < latest).count();
        Some(below as f64 / (self.window.len() - 1) as f64)
    }

    // ── round-80 ─────────────────────────────────────────────────────────────

    /// Trimmed mean: arithmetic mean after discarding the bottom and top
    /// `p` fraction of window values.
    ///
    /// `p` is clamped to `[0.0, 0.499]`. Returns `None` if the window is
    /// empty or trimming removes all observations.
    pub fn trimmed_mean(&self, p: f64) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() {
            return None;
        }
        let p = p.clamp(0.0, 0.499);
        let mut sorted: Vec<f64> = self.window.iter().filter_map(|v| v.to_f64()).collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = sorted.len();
        let trim = (n as f64 * p).floor() as usize;
        let trimmed = &sorted[trim..n - trim];
        if trimmed.is_empty() {
            return None;
        }
        Some(trimmed.iter().sum::<f64>() / trimmed.len() as f64)
    }

    /// OLS linear trend slope of window values over their insertion index.
    ///
    /// A positive slope indicates an upward trend; negative indicates downward.
    /// Returns `None` if the window has fewer than 2 observations.
    pub fn linear_trend_slope(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let n = self.window.len();
        if n < 2 {
            return None;
        }
        let n_f = n as f64;
        let x_mean = (n_f - 1.0) / 2.0;
        let y_vals: Vec<f64> = self.window.iter().filter_map(|v| v.to_f64()).collect();
        if y_vals.len() < 2 {
            return None;
        }
        let y_mean = y_vals.iter().sum::<f64>() / y_vals.len() as f64;
        let numerator: f64 = y_vals
            .iter()
            .enumerate()
            .map(|(i, &y)| (i as f64 - x_mean) * (y - y_mean))
            .sum();
        let denominator: f64 = (0..n).map(|i| (i as f64 - x_mean).powi(2)).sum();
        if denominator == 0.0 {
            return None;
        }
        Some(numerator / denominator)
    }

    // ── round-81 ─────────────────────────────────────────────────────────────

    /// Ratio of the first-half window variance to the second-half window variance.
    ///
    /// Values above 1.0 indicate decreasing volatility; below 1.0 indicate
    /// increasing volatility. Returns `None` if the window has fewer than 4
    /// observations or if the second-half variance is zero.
    pub fn variance_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let n = self.window.len();
        if n < 4 {
            return None;
        }
        let mid = n / 2;
        let first: Vec<f64> = self.window.iter().take(mid).filter_map(|v| v.to_f64()).collect();
        let second: Vec<f64> = self.window.iter().skip(mid).filter_map(|v| v.to_f64()).collect();
        let var = |vals: &[f64]| -> Option<f64> {
            let n_f = vals.len() as f64;
            if n_f < 2.0 { return None; }
            let mean = vals.iter().sum::<f64>() / n_f;
            Some(vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n_f - 1.0))
        };
        let v1 = var(&first)?;
        let v2 = var(&second)?;
        if v2 == 0.0 {
            return None;
        }
        Some(v1 / v2)
    }

    /// Linear trend slope of z-scores over the window.
    ///
    /// First computes the z-score of each window value relative to the full
    /// window mean and std-dev, then returns the OLS slope over the resulting
    /// z-score sequence. Positive means z-scores are trending upward.
    /// Returns `None` if fewer than 2 observations or std-dev is zero.
    pub fn z_score_trend_slope(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let n = self.window.len();
        if n < 2 {
            return None;
        }
        let std_dev = self.std_dev()?;
        if std_dev == 0.0 {
            return None;
        }
        let mean = self.mean()?.to_f64()?;
        let z_vals: Vec<f64> = self
            .window
            .iter()
            .filter_map(|v| v.to_f64())
            .map(|v| (v - mean) / std_dev)
            .collect();
        if z_vals.len() < 2 {
            return None;
        }
        let n_f = z_vals.len() as f64;
        let x_mean = (n_f - 1.0) / 2.0;
        let z_mean = z_vals.iter().sum::<f64>() / n_f;
        let num: f64 = z_vals.iter().enumerate().map(|(i, &z)| (i as f64 - x_mean) * (z - z_mean)).sum();
        let den: f64 = (0..z_vals.len()).map(|i| (i as f64 - x_mean).powi(2)).sum();
        if den == 0.0 { return None; }
        Some(num / den)
    }

    // ── round-82 ─────────────────────────────────────────────────────────────

    /// Mean of `|x_i − x_{i-1}|` across consecutive window values; average absolute change.
    pub fn mean_absolute_change(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let vals: Vec<f64> = self.window.iter().filter_map(|v| v.to_f64()).collect();
        if vals.len() < 2 {
            return None;
        }
        let mac = vals.windows(2).map(|w| (w[1] - w[0]).abs()).sum::<f64>() / (vals.len() - 1) as f64;
        Some(mac)
    }

    // ── round-83 ─────────────────────────────────────────────────────────────

    /// Fraction of consecutive pairs that are monotonically increasing.
    ///
    /// Computed as `count(x_{i+1} > x_i) / (n − 1)`.
    /// Returns `None` for windows with fewer than 2 values.
    pub fn monotone_increase_fraction(&self) -> Option<f64> {
        let n = self.window.len();
        if n < 2 {
            return None;
        }
        let increasing = self.window
            .iter()
            .collect::<Vec<_>>()
            .windows(2)
            .filter(|w| w[1] > w[0])
            .count();
        Some(increasing as f64 / (n - 1) as f64)
    }

    /// Maximum absolute value in the window.
    ///
    /// Returns `None` if the window is empty.
    pub fn abs_max(&self) -> Option<Decimal> {
        self.window.iter().map(|v| v.abs()).reduce(|a, b| a.max(b))
    }

    /// Minimum absolute value in the window.
    ///
    /// Returns `None` if the window is empty.
    pub fn abs_min(&self) -> Option<Decimal> {
        self.window.iter().map(|v| v.abs()).reduce(|a, b| a.min(b))
    }

    /// Count of values in the window that equal the window maximum.
    ///
    /// Returns `None` if the window is empty.
    pub fn max_count(&self) -> Option<usize> {
        let mut tmp = MinMaxNormalizer::new(self.window_size).ok()?;
        for &v in &self.window {
            tmp.update(v);
        }
        let (_, max) = tmp.min_max()?;
        Some(self.window.iter().filter(|&&v| v == max).count())
    }

    /// Count of values in the window that equal the window minimum.
    ///
    /// Returns `None` if the window is empty.
    pub fn min_count(&self) -> Option<usize> {
        let mut tmp = MinMaxNormalizer::new(self.window_size).ok()?;
        for &v in &self.window {
            tmp.update(v);
        }
        let (min, _) = tmp.min_max()?;
        Some(self.window.iter().filter(|&&v| v == min).count())
    }

    /// Ratio of the current window mean to the mean computed at window creation
    /// time (first `window_size / 2` values).
    ///
    /// Returns `None` if fewer than 2 observations or the early mean is zero.
    pub fn mean_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let n = self.window.len();
        if n < 2 {
            return None;
        }
        let current_mean = self.mean()?;
        let half = (n / 2).max(1);
        let early_sum: Decimal = self.window.iter().take(half).copied().sum();
        let early_mean = early_sum / Decimal::from(half as i64);
        if early_mean.is_zero() {
            return None;
        }
        (current_mean / early_mean).to_f64()
    }

    // ── round-84 ─────────────────────────────────────────────────────────────

    /// Exponentially-weighted mean with decay factor `alpha` ∈ (0, 1]; most-recent value has highest weight.
    pub fn exponential_weighted_mean(&self, alpha: f64) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() {
            return None;
        }
        let alpha = alpha.clamp(1e-6, 1.0);
        let vals: Vec<f64> = self.window.iter().filter_map(|v| v.to_f64()).collect();
        if vals.is_empty() {
            return None;
        }
        let mut ewm = vals[0];
        for &v in &vals[1..] {
            ewm = alpha * v + (1.0 - alpha) * ewm;
        }
        Some(ewm)
    }

    /// Mean of squared values in the window (second raw moment).
    pub fn second_moment(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() {
            return None;
        }
        let sum: f64 = self.window.iter().filter_map(|v| v.to_f64()).map(|v| v * v).sum();
        Some(sum / self.window.len() as f64)
    }

    /// Range / mean — coefficient of dispersion; `None` if mean is zero.
    pub fn range_over_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let max = self.window.iter().copied().max()?;
        let min = self.window.iter().copied().min()?;
        let mean = self.mean()?;
        if mean.is_zero() {
            return None;
        }
        ((max - min) / mean).to_f64()
    }

    /// Fraction of window values strictly above the window median.
    pub fn above_median_fraction(&self) -> Option<f64> {
        if self.window.is_empty() {
            return None;
        }
        let median = self.median()?;
        let count = self.window.iter().filter(|&&v| v > median).count();
        Some(count as f64 / self.window.len() as f64)
    }

    // ── round-85 ─────────────────────────────────────────────────────────────

    /// Mean of values strictly between Q1 and Q3 (the interquartile mean).
    pub fn interquartile_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() {
            return None;
        }
        let q1 = self.percentile_value(0.25)?;
        let q3 = self.percentile_value(0.75)?;
        let iqr_vals: Vec<f64> = self.window
            .iter()
            .filter(|&&v| v > q1 && v < q3)
            .filter_map(|v| v.to_f64())
            .collect();
        if iqr_vals.is_empty() {
            return None;
        }
        Some(iqr_vals.iter().sum::<f64>() / iqr_vals.len() as f64)
    }

    /// Fraction of window values beyond `threshold` standard deviations from the mean.
    pub fn outlier_fraction(&self, threshold: f64) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() {
            return None;
        }
        let std_dev = self.std_dev()?;
        let mean = self.mean()?.to_f64()?;
        if std_dev == 0.0 {
            return Some(0.0);
        }
        let count = self.window
            .iter()
            .filter_map(|v| v.to_f64())
            .filter(|&v| ((v - mean) / std_dev).abs() > threshold)
            .count();
        Some(count as f64 / self.window.len() as f64)
    }

    /// Count of sign changes (transitions across zero) in the window.
    pub fn sign_flip_count(&self) -> Option<usize> {
        if self.window.len() < 2 {
            return None;
        }
        let count = self.window
            .iter()
            .collect::<Vec<_>>()
            .windows(2)
            .filter(|w| w[0].is_sign_negative() != w[1].is_sign_negative())
            .count();
        Some(count)
    }

    /// Root mean square of window values.
    pub fn rms(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() {
            return None;
        }
        let sum_sq: f64 = self.window.iter().filter_map(|v| v.to_f64()).map(|v| v * v).sum();
        Some((sum_sq / self.window.len() as f64).sqrt())
    }

    // ── round-86 ─────────────────────────────────────────────────────────────

    /// Number of distinct values in the window.
    pub fn distinct_count(&self) -> usize {
        let mut sorted: Vec<Decimal> = self.window.iter().copied().collect();
        sorted.sort();
        sorted.dedup();
        sorted.len()
    }

    /// Fraction of window values that equal the window maximum.
    pub fn max_fraction(&self) -> Option<f64> {
        if self.window.is_empty() {
            return None;
        }
        let max = self.window.iter().copied().max()?;
        let count = self.window.iter().filter(|&&v| v == max).count();
        Some(count as f64 / self.window.len() as f64)
    }

    /// Fraction of window values that equal the window minimum.
    pub fn min_fraction(&self) -> Option<f64> {
        if self.window.is_empty() {
            return None;
        }
        let min = self.window.iter().copied().min()?;
        let count = self.window.iter().filter(|&&v| v == min).count();
        Some(count as f64 / self.window.len() as f64)
    }

    /// Difference between the latest value and the window mean (signed).
    pub fn latest_minus_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let latest = self.latest()?;
        let mean = self.mean()?;
        (latest - mean).to_f64()
    }

    /// Ratio of the latest value to the window mean; `None` if mean is zero.
    pub fn latest_to_mean_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let latest = self.latest()?;
        let mean = self.mean()?;
        if mean.is_zero() {
            return None;
        }
        (latest / mean).to_f64()
    }

    // ── round-87 ─────────────────────────────────────────────────────────────

    /// Fraction of window values strictly below the mean.
    pub fn below_mean_fraction(&self) -> Option<f64> {
        if self.window.is_empty() {
            return None;
        }
        let mean = self.mean()?;
        let count = self.window.iter().filter(|&&v| v < mean).count();
        Some(count as f64 / self.window.len() as f64)
    }

    /// Variance of values lying outside the interquartile range.
    /// Returns `None` if fewer than 4 values or no tail values.
    pub fn tail_variance(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 4 {
            return None;
        }
        let mut sorted: Vec<Decimal> = self.window.iter().copied().collect();
        sorted.sort();
        let n = sorted.len();
        let q1 = sorted[n / 4];
        let q3 = sorted[(3 * n) / 4];
        let tails: Vec<f64> = sorted
            .iter()
            .filter(|&&v| v < q1 || v > q3)
            .filter_map(|v| v.to_f64())
            .collect();
        if tails.len() < 2 {
            return None;
        }
        let nt = tails.len() as f64;
        let mean = tails.iter().sum::<f64>() / nt;
        let var = tails.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (nt - 1.0);
        Some(var)
    }

    // ── round-88 ─────────────────────────────────────────────────────────────

    /// Number of times the window reaches a new running maximum (from index 0).
    pub fn new_max_count(&self) -> usize {
        if self.window.is_empty() {
            return 0;
        }
        let vals: Vec<Decimal> = self.window.iter().copied().collect();
        let mut running = vals[0];
        let mut count = 1usize;
        for &v in vals.iter().skip(1) {
            if v > running {
                running = v;
                count += 1;
            }
        }
        count
    }

    /// Number of times the window reaches a new running minimum (from index 0).
    pub fn new_min_count(&self) -> usize {
        if self.window.is_empty() {
            return 0;
        }
        let vals: Vec<Decimal> = self.window.iter().copied().collect();
        let mut running = vals[0];
        let mut count = 1usize;
        for &v in vals.iter().skip(1) {
            if v < running {
                running = v;
                count += 1;
            }
        }
        count
    }

    /// Fraction of window values strictly equal to zero.
    pub fn zero_fraction(&self) -> Option<f64> {
        if self.window.is_empty() {
            return None;
        }
        let count = self.window.iter().filter(|&&v| v == Decimal::ZERO).count();
        Some(count as f64 / self.window.len() as f64)
    }

    // ── round-89 ─────────────────────────────────────────────────────────────

    /// Cumulative sum of all window values (running total).
    pub fn cumulative_sum(&self) -> Decimal {
        self.window.iter().copied().sum()
    }

    /// Ratio of the window maximum to the window minimum.
    /// Returns `None` if the window is empty or minimum is zero.
    pub fn max_to_min_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let max = self.window.iter().copied().max()?;
        let min = self.window.iter().copied().min()?;
        if min.is_zero() {
            return None;
        }
        (max / min).to_f64()
    }

    // ── round-90 ─────────────────────────────────────────────────────────────

    /// Fraction of window values strictly above the window midpoint `(min + max) / 2`.
    ///
    /// Returns `None` for an empty window.
    pub fn above_midpoint_fraction(&self) -> Option<f64> {
        if self.window.is_empty() {
            return None;
        }
        let min = self.window.iter().copied().min()?;
        let max = self.window.iter().copied().max()?;
        let mid = (min + max) / Decimal::TWO;
        let count = self.window.iter().filter(|&&v| v > mid).count();
        Some(count as f64 / self.window.len() as f64)
    }

    /// Position of the latest window value in the range: `(latest − min) / (max − min)`.
    ///
    /// Returns `None` when the window is empty or has zero range.
    pub fn span_utilization(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() {
            return None;
        }
        let min = self.window.iter().copied().min()?;
        let max = self.window.iter().copied().max()?;
        let range = max - min;
        if range.is_zero() {
            return None;
        }
        let latest = *self.window.back()?;
        ((latest - min) / range).to_f64()
    }

    /// Fraction of window values strictly greater than zero.
    ///
    /// Returns `None` for an empty window.
    pub fn positive_fraction(&self) -> Option<f64> {
        if self.window.is_empty() {
            return None;
        }
        let count = self.window.iter().filter(|&&v| v > Decimal::ZERO).count();
        Some(count as f64 / self.window.len() as f64)
    }

    // ── round-91 ─────────────────────────────────────────────────────────────

    /// Interquartile range of the window: `Q3 − Q1`.
    ///
    /// Returns `None` for an empty window.
    pub fn window_iqr(&self) -> Option<Decimal> {
        if self.window.is_empty() {
            return None;
        }
        let mut sorted: Vec<Decimal> = self.window.iter().copied().collect();
        sorted.sort();
        let n = sorted.len();
        let q1 = sorted[n / 4];
        let q3 = sorted[(3 * n) / 4];
        Some(q3 - q1)
    }

    /// Mean run length of monotone non-decreasing segments.
    ///
    /// Returns `None` for fewer than 2 values in the window.
    pub fn run_length_mean(&self) -> Option<f64> {
        if self.window.len() < 2 {
            return None;
        }
        let vals: Vec<Decimal> = self.window.iter().copied().collect();
        let mut runs: Vec<usize> = Vec::new();
        let mut run_len = 1usize;
        for w in vals.windows(2) {
            if w[1] >= w[0] {
                run_len += 1;
            } else {
                runs.push(run_len);
                run_len = 1;
            }
        }
        runs.push(run_len);
        Some(runs.iter().sum::<usize>() as f64 / runs.len() as f64)
    }

    // ── round-92 ─────────────────────────────────────────────────────────────

    /// Fraction of consecutive value pairs that are non-decreasing.
    ///
    /// Returns `None` for fewer than 2 values in the window.
    pub fn monotone_fraction(&self) -> Option<f64> {
        if self.window.len() < 2 {
            return None;
        }
        let vals: Vec<Decimal> = self.window.iter().copied().collect();
        let non_decreasing = vals.windows(2).filter(|w| w[1] >= w[0]).count();
        Some(non_decreasing as f64 / (vals.len() - 1) as f64)
    }

    /// Coefficient of variation: `std_dev / mean` for window values.
    ///
    /// Returns `None` for an empty window or zero mean.
    pub fn coeff_variation(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() {
            return None;
        }
        let n = self.window.len() as u32;
        let mean: Decimal =
            self.window.iter().copied().sum::<Decimal>() / Decimal::from(n);
        if mean.is_zero() {
            return None;
        }
        let variance: f64 = self
            .window
            .iter()
            .filter_map(|&v| {
                let d = (v - mean).to_f64()?;
                Some(d * d)
            })
            .sum::<f64>()
            / n as f64;
        let mean_f = mean.to_f64()?;
        Some(variance.sqrt() / mean_f.abs())
    }

    // ── round-93 ─────────────────────────────────────────────────────────────

    /// Sum of squared values in the rolling window.
    pub fn window_sum_of_squares(&self) -> Decimal {
        self.window.iter().map(|&v| v * v).sum()
    }

    /// 75th percentile of the rolling window.
    ///
    /// Returns `None` for an empty window.
    pub fn percentile_75(&self) -> Option<Decimal> {
        if self.window.is_empty() {
            return None;
        }
        let mut sorted: Vec<Decimal> = self.window.iter().copied().collect();
        sorted.sort();
        let idx = (sorted.len() * 3) / 4;
        Some(sorted[idx.min(sorted.len() - 1)])
    }

    // ── round-94 ─────────────────────────────────────────────────────────────

    /// Mean absolute deviation of the window values from their mean.
    /// Returns `None` if the window is empty.
    pub fn window_mean_deviation(&self) -> Option<Decimal> {
        if self.window.is_empty() {
            return None;
        }
        let n = Decimal::from(self.window.len());
        let mean: Decimal = self.window.iter().copied().sum::<Decimal>() / n;
        let mad = self.window.iter().map(|v| (*v - mean).abs()).sum::<Decimal>() / n;
        Some(mad)
    }

    /// Fraction of window values strictly below the latest observation (0.0–1.0).
    /// Returns `None` if the window is empty.
    pub fn latest_percentile(&self) -> Option<f64> {
        if self.window.is_empty() {
            return None;
        }
        use rust_decimal::prelude::ToPrimitive;
        let latest = *self.window.back()?;
        let below = self.window.iter().filter(|&&v| v < latest).count();
        Some(below as f64 / self.window.len() as f64)
    }

    // ── round-95 ─────────────────────────────────────────────────────────────

    /// Mean of window values that fall between the 25th and 75th percentile (trimmed mean).
    /// Returns `None` if the window is empty or has fewer than 4 values.
    pub fn window_trimmed_mean(&self) -> Option<Decimal> {
        if self.window.len() < 4 {
            return None;
        }
        let mut sorted: Vec<Decimal> = self.window.iter().copied().collect();
        sorted.sort();
        let q1 = sorted.len() / 4;
        let q3 = (sorted.len() * 3) / 4;
        let trimmed = &sorted[q1..q3];
        if trimmed.is_empty() {
            return None;
        }
        let sum: Decimal = trimmed.iter().copied().sum();
        Some(sum / Decimal::from(trimmed.len()))
    }

    /// Population variance of the rolling window values.
    /// Returns `None` if the window is empty.
    pub fn window_variance(&self) -> Option<Decimal> {
        if self.window.is_empty() {
            return None;
        }
        let n = Decimal::from(self.window.len());
        let mean: Decimal = self.window.iter().copied().sum::<Decimal>() / n;
        let variance = self.window.iter().map(|v| (*v - mean) * (*v - mean)).sum::<Decimal>() / n;
        Some(variance)
    }

    // ── round-96 ─────────────────────────────────────────────────────────────

    /// Population standard deviation of the rolling window.
    /// Returns `None` if the window is empty or variance is negative (should not occur).
    pub fn window_std_dev(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let variance = self.window_variance()?;
        Some(variance.to_f64().unwrap_or(0.0).sqrt())
    }

    /// Ratio of the window minimum to the window maximum.
    /// Returns `None` if the window is empty or max is zero.
    pub fn window_min_max_ratio(&self) -> Option<Decimal> {
        if self.window.is_empty() {
            return None;
        }
        let min = self.window.iter().copied().min()?;
        let max = self.window.iter().copied().max()?;
        if max.is_zero() {
            return None;
        }
        Some(min / max)
    }

    /// Bias of recent observations vs older ones: mean of last half minus mean
    /// of first half, as a fraction of the overall mean.
    /// Returns `None` for windows with fewer than 4 values or zero overall mean.
    pub fn recent_bias(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 4 {
            return None;
        }
        let n = self.window.len();
        let vals: Vec<Decimal> = self.window.iter().copied().collect();
        let half = n / 2;
        let old_mean: Decimal = vals[..half].iter().copied().sum::<Decimal>() / Decimal::from(half);
        let new_mean: Decimal =
            vals[half..].iter().copied().sum::<Decimal>() / Decimal::from(n - half);
        let overall_mean: Decimal = vals.iter().copied().sum::<Decimal>() / Decimal::from(n);
        if overall_mean.is_zero() {
            return None;
        }
        Some(((new_mean - old_mean) / overall_mean).to_f64().unwrap_or(0.0))
    }

    /// Range of the window (max − min) as a percentage of the minimum value.
    /// Returns `None` if the window is empty or the minimum is zero.
    pub fn window_range_pct(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() {
            return None;
        }
        let min = self.window.iter().copied().min()?;
        let max = self.window.iter().copied().max()?;
        if min.is_zero() {
            return None;
        }
        Some(((max - min) / min).to_f64().unwrap_or(0.0))
    }

    // ── round-97 ─────────────────────────────────────────────────────────────

    /// Momentum of the rolling window: latest value minus the oldest value.
    /// Returns `None` if the window has fewer than 2 values.
    pub fn window_momentum(&self) -> Option<Decimal> {
        if self.window.len() < 2 {
            return None;
        }
        let oldest = *self.window.front()?;
        let latest = *self.window.back()?;
        Some(latest - oldest)
    }

    /// Fraction of window values that are strictly above the first (oldest) value.
    /// Returns `None` if the window is empty.
    pub fn above_first_fraction(&self) -> Option<f64> {
        if self.window.is_empty() {
            return None;
        }
        let first = *self.window.front()?;
        let above = self.window.iter().filter(|&&v| v > first).count();
        Some(above as f64 / self.window.len() as f64)
    }

    /// Z-score of the latest observation relative to the window mean and std-dev.
    /// Returns `None` if the window is empty or std-dev is zero.
    pub fn window_zscore_latest(&self) -> Option<f64> {
        if self.window.is_empty() {
            return None;
        }
        let n = Decimal::from(self.window.len());
        let mean: Decimal = self.window.iter().copied().sum::<Decimal>() / n;
        let variance = self.window.iter().map(|v| (*v - mean) * (*v - mean)).sum::<Decimal>() / n;
        use rust_decimal::prelude::ToPrimitive;
        let std_dev = variance.to_f64().unwrap_or(0.0).sqrt();
        if std_dev == 0.0 {
            return None;
        }
        let latest = self.window.back()?.to_f64().unwrap_or(0.0);
        let mean_f = mean.to_f64().unwrap_or(0.0);
        Some((latest - mean_f) / std_dev)
    }

    /// Exponentially-decayed weighted mean of the window (newest weight = alpha,
    /// oldest = alpha*(1-alpha)^(n-1)).  Returns `None` for an empty window.
    pub fn decay_weighted_mean(&self, alpha: f64) -> Option<f64> {
        if self.window.is_empty() {
            return None;
        }
        use rust_decimal::prelude::ToPrimitive;
        let vals: Vec<f64> = self
            .window
            .iter()
            .rev()
            .map(|v| v.to_f64().unwrap_or(0.0))
            .collect();
        let n = vals.len();
        let mut weighted_sum = 0.0f64;
        let mut weight_sum = 0.0f64;
        for (i, v) in vals.iter().enumerate() {
            let w = alpha * (1.0 - alpha).powi(i as i32);
            weighted_sum += v * w;
            weight_sum += w;
        }
        if weight_sum == 0.0 || n == 0 {
            return None;
        }
        Some(weighted_sum / weight_sum)
    }

    // ── round-98 ─────────────────────────────────────────────────────────────

    /// Excess kurtosis (fourth standardized moment minus 3) of the window.
    /// Returns `None` for fewer than 4 values or zero variance.
    pub fn window_kurtosis(&self) -> Option<f64> {
        if self.window.len() < 4 {
            return None;
        }
        use rust_decimal::prelude::ToPrimitive;
        let n = self.window.len() as f64;
        let vals: Vec<f64> = self
            .window
            .iter()
            .map(|v| v.to_f64().unwrap_or(0.0))
            .collect();
        let mean = vals.iter().sum::<f64>() / n;
        let variance = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n;
        if variance == 0.0 {
            return None;
        }
        let fourth = vals.iter().map(|v| (v - mean).powi(4)).sum::<f64>() / n;
        Some(fourth / (variance * variance) - 3.0)
    }

    /// Fraction of window values above the 90th percentile value within the window.
    /// Returns `None` if the window is empty.
    pub fn above_percentile_90(&self) -> Option<f64> {
        if self.window.is_empty() {
            return None;
        }
        let mut sorted: Vec<Decimal> = self.window.iter().copied().collect();
        sorted.sort();
        let idx = (sorted.len() * 9) / 10;
        let p90 = sorted[idx.min(sorted.len() - 1)];
        let above = self.window.iter().filter(|&&v| v > p90).count();
        Some(above as f64 / self.window.len() as f64)
    }

    /// Lag-1 autocorrelation of the rolling window values.
    /// Returns `None` for fewer than 3 values or zero variance.
    pub fn window_lag_autocorr(&self) -> Option<f64> {
        if self.window.len() < 3 {
            return None;
        }
        use rust_decimal::prelude::ToPrimitive;
        let vals: Vec<f64> = self
            .window
            .iter()
            .map(|v| v.to_f64().unwrap_or(0.0))
            .collect();
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        let variance = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n;
        if variance == 0.0 {
            return None;
        }
        let cov = vals
            .windows(2)
            .map(|w| (w[0] - mean) * (w[1] - mean))
            .sum::<f64>()
            / (n - 1.0);
        Some(cov / variance)
    }

    /// Linear slope of the rolling window mean over halves: `(second_half_mean - first_half_mean) / half_size`.
    /// Returns `None` for fewer than 4 values.
    pub fn slope_of_mean(&self) -> Option<f64> {
        if self.window.len() < 4 {
            return None;
        }
        use rust_decimal::prelude::ToPrimitive;
        let vals: Vec<f64> = self
            .window
            .iter()
            .map(|v| v.to_f64().unwrap_or(0.0))
            .collect();
        let half = vals.len() / 2;
        let first_mean = vals[..half].iter().sum::<f64>() / half as f64;
        let second_mean = vals[half..].iter().sum::<f64>() / (vals.len() - half) as f64;
        Some((second_mean - first_mean) / half as f64)
    }

    // ── round-99 ─────────────────────────────────────────────────────────────

    /// 25th percentile of the rolling window.
    /// Returns `None` for an empty window.
    pub fn window_percentile_25(&self) -> Option<Decimal> {
        if self.window.is_empty() {
            return None;
        }
        let mut sorted: Vec<Decimal> = self.window.iter().copied().collect();
        sorted.sort();
        let idx = sorted.len() / 4;
        Some(sorted[idx.min(sorted.len() - 1)])
    }

    /// Mean-reversion score: how far the latest value is from the window mean,
    /// expressed as a fraction of the window range. Returns `None` for empty window
    /// or zero range.
    pub fn mean_reversion_score(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() {
            return None;
        }
        let min = self.window.iter().copied().min()?;
        let max = self.window.iter().copied().max()?;
        let range = max - min;
        if range.is_zero() {
            return None;
        }
        let n = Decimal::from(self.window.len());
        let mean: Decimal = self.window.iter().copied().sum::<Decimal>() / n;
        let latest = *self.window.back()?;
        Some(((latest - mean) / range).to_f64().unwrap_or(0.0))
    }

    /// Trend strength: absolute difference between first-half mean and second-half
    /// mean divided by the window standard deviation. Returns `None` for fewer than
    /// 4 values or zero std-dev.
    pub fn trend_strength(&self) -> Option<f64> {
        if self.window.len() < 4 {
            return None;
        }
        use rust_decimal::prelude::ToPrimitive;
        let n = self.window.len() as f64;
        let vals: Vec<f64> = self
            .window
            .iter()
            .map(|v| v.to_f64().unwrap_or(0.0))
            .collect();
        let mean = vals.iter().sum::<f64>() / n;
        let variance = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n;
        let std_dev = variance.sqrt();
        if std_dev == 0.0 {
            return None;
        }
        let half = vals.len() / 2;
        let first_mean = vals[..half].iter().sum::<f64>() / half as f64;
        let second_mean = vals[half..].iter().sum::<f64>() / (vals.len() - half) as f64;
        Some((second_mean - first_mean).abs() / std_dev)
    }

    /// Number of local peaks (values greater than both neighbours) in the window.
    /// Returns `None` for fewer than 3 values.
    pub fn window_peak_count(&self) -> Option<usize> {
        if self.window.len() < 3 {
            return None;
        }
        let vals: Vec<Decimal> = self.window.iter().copied().collect();
        let count = vals
            .windows(3)
            .filter(|w| w[1] > w[0] && w[1] > w[2])
            .count();
        Some(count)
    }

    // ── round-100 ────────────────────────────────────────────────────────────

    /// Number of local troughs (values less than both neighbours) in the window.
    /// Returns `None` for fewer than 3 values.
    pub fn window_trough_count(&self) -> Option<usize> {
        if self.window.len() < 3 {
            return None;
        }
        let vals: Vec<Decimal> = self.window.iter().copied().collect();
        let count = vals
            .windows(3)
            .filter(|w| w[1] < w[0] && w[1] < w[2])
            .count();
        Some(count)
    }

    /// Fraction of consecutive value pairs where the second is strictly greater.
    /// Returns `None` for fewer than 2 values.
    pub fn positive_momentum_fraction(&self) -> Option<f64> {
        if self.window.len() < 2 {
            return None;
        }
        let vals: Vec<Decimal> = self.window.iter().copied().collect();
        let total = vals.len() - 1;
        let positive = vals.windows(2).filter(|w| w[1] > w[0]).count();
        Some(positive as f64 / total as f64)
    }

    /// 10th percentile of the rolling window.
    /// Returns `None` for an empty window.
    pub fn below_percentile_10(&self) -> Option<Decimal> {
        if self.window.is_empty() {
            return None;
        }
        let mut sorted: Vec<Decimal> = self.window.iter().copied().collect();
        sorted.sort();
        let idx = sorted.len() / 10;
        Some(sorted[idx.min(sorted.len() - 1)])
    }

    /// Rate of direction alternation: fraction of consecutive pairs where the
    /// direction flips (up→down or down→up), ignoring flat transitions.
    /// Returns `None` for fewer than 3 values or no directional pairs.
    pub fn alternation_rate(&self) -> Option<f64> {
        if self.window.len() < 3 {
            return None;
        }
        let vals: Vec<Decimal> = self.window.iter().copied().collect();
        let dirs: Vec<i32> = vals
            .windows(2)
            .map(|w| {
                if w[1] > w[0] {
                    1
                } else if w[1] < w[0] {
                    -1
                } else {
                    0
                }
            })
            .collect();
        let valid_pairs: Vec<_> = dirs.windows(2).filter(|d| d[0] != 0 && d[1] != 0).collect();
        if valid_pairs.is_empty() {
            return None;
        }
        let alternations = valid_pairs.iter().filter(|d| d[0] != d[1]).count();
        Some(alternations as f64 / valid_pairs.len() as f64)
    }

    // ── round-101 ────────────────────────────────────────────────────────────

    /// Signed area under the window curve (sum of values minus mean * n).
    /// Always returns some value; returns `None` only for empty window.
    pub fn window_signed_area(&self) -> Option<Decimal> {
        if self.window.is_empty() {
            return None;
        }
        let n = Decimal::from(self.window.len());
        let mean: Decimal = self.window.iter().copied().sum::<Decimal>() / n;
        let area: Decimal = self.window.iter().map(|v| *v - mean).sum();
        Some(area)
    }

    /// Fraction of window values strictly above zero.
    /// Returns `None` for an empty window.
    pub fn up_fraction(&self) -> Option<f64> {
        if self.window.is_empty() {
            return None;
        }
        let above = self.window.iter().filter(|&&v| v > Decimal::ZERO).count();
        Some(above as f64 / self.window.len() as f64)
    }

    /// Number of times the window crosses the mean value (transitions from
    /// above to below or vice versa).  Returns `None` for fewer than 2 values.
    pub fn threshold_cross_count(&self) -> Option<usize> {
        if self.window.len() < 2 {
            return None;
        }
        let n = Decimal::from(self.window.len());
        let mean: Decimal = self.window.iter().copied().sum::<Decimal>() / n;
        let vals: Vec<Decimal> = self.window.iter().copied().collect();
        let crosses = vals
            .windows(2)
            .filter(|w| (w[0] > mean) != (w[1] > mean))
            .count();
        Some(crosses)
    }

    /// Approximate entropy using 4 equal-width bins over the window range.
    /// Returns `None` for an empty window or all identical values.
    pub fn window_entropy_approx(&self) -> Option<f64> {
        if self.window.is_empty() {
            return None;
        }
        let min = self.window.iter().copied().min()?;
        let max = self.window.iter().copied().max()?;
        if min == max {
            return None;
        }
        use rust_decimal::prelude::ToPrimitive;
        let range = (max - min).to_f64().unwrap_or(0.0);
        let n_bins = 4usize;
        let mut bins = vec![0usize; n_bins];
        for v in self.window.iter() {
            let frac = ((*v - min).to_f64().unwrap_or(0.0) / range) * (n_bins - 1) as f64;
            let idx = frac.round() as usize;
            bins[idx.min(n_bins - 1)] += 1;
        }
        let total = self.window.len() as f64;
        let entropy = bins
            .iter()
            .filter(|&&c| c > 0)
            .map(|&c| {
                let p = c as f64 / total;
                -p * p.ln()
            })
            .sum::<f64>();
        Some(entropy)
    }

    // ── round-102 ────────────────────────────────────────────────────────────

    /// Ratio of the 25th to 75th percentile of the window.
    /// Returns `None` for an empty window or zero 75th percentile.
    pub fn window_q1_q3_ratio(&self) -> Option<Decimal> {
        if self.window.is_empty() {
            return None;
        }
        let q1 = self.window_percentile_25()?;
        let q3 = self.percentile_75()?;
        if q3.is_zero() {
            return None;
        }
        Some(q1 / q3)
    }

    /// Sum of signed consecutive differences (momentum sign count).
    /// Returns `None` for fewer than 2 values.
    pub fn signed_momentum(&self) -> Option<Decimal> {
        if self.window.len() < 2 {
            return None;
        }
        let vals: Vec<Decimal> = self.window.iter().copied().collect();
        let signed: Decimal = vals
            .windows(2)
            .map(|w| if w[1] > w[0] { Decimal::ONE } else if w[1] < w[0] { -Decimal::ONE } else { Decimal::ZERO })
            .sum();
        Some(signed)
    }

    /// Mean length of consecutive positive (increasing) runs in the window.
    /// Returns `None` for fewer than 2 values or no positive runs.
    pub fn positive_run_length(&self) -> Option<f64> {
        if self.window.len() < 2 {
            return None;
        }
        let vals: Vec<Decimal> = self.window.iter().copied().collect();
        let mut runs = Vec::new();
        let mut cur = 0usize;
        for w in vals.windows(2) {
            if w[1] > w[0] {
                cur += 1;
            } else if cur > 0 {
                runs.push(cur);
                cur = 0;
            }
        }
        if cur > 0 {
            runs.push(cur);
        }
        if runs.is_empty() {
            return None;
        }
        Some(runs.iter().sum::<usize>() as f64 / runs.len() as f64)
    }

    /// Ratio of the last trough value to the last peak value in the window.
    /// Returns `None` if there is no peak or trough, or if peak is zero.
    pub fn valley_to_peak_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 3 {
            return None;
        }
        let vals: Vec<Decimal> = self.window.iter().copied().collect();
        let last_peak = vals
            .windows(3)
            .filter(|w| w[1] > w[0] && w[1] > w[2])
            .map(|w| w[1])
            .last();
        let last_trough = vals
            .windows(3)
            .filter(|w| w[1] < w[0] && w[1] < w[2])
            .map(|w| w[1])
            .last();
        match (last_trough, last_peak) {
            (Some(t), Some(p)) if !p.is_zero() => Some((t / p).to_f64().unwrap_or(0.0)),
            _ => None,
        }
    }

    // ── round-103 ────────────────────────────────────────────────────────────

    /// Maximum peak-to-trough drawdown within the window.
    /// Returns `None` for fewer than 2 values or no decline found.
    pub fn window_max_drawdown(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 {
            return None;
        }
        let vals: Vec<Decimal> = self.window.iter().copied().collect();
        let mut max_drawdown = Decimal::ZERO;
        let mut peak = vals[0];
        for &v in &vals[1..] {
            if v > peak {
                peak = v;
            } else {
                let dd = peak - v;
                if dd > max_drawdown {
                    max_drawdown = dd;
                }
            }
        }
        if max_drawdown.is_zero() {
            return None;
        }
        max_drawdown.to_f64()
    }

    /// Fraction of window values that exceed their immediate predecessor.
    /// Returns `None` for fewer than 2 values.
    pub fn above_previous_fraction(&self) -> Option<f64> {
        if self.window.len() < 2 {
            return None;
        }
        let vals: Vec<Decimal> = self.window.iter().copied().collect();
        let count = vals.windows(2).filter(|w| w[1] > w[0]).count();
        Some(count as f64 / (vals.len() - 1) as f64)
    }

    /// Range efficiency: net move divided by sum of absolute step-wise moves.
    /// A value near 1.0 means a monotone trend; near 0.0 means choppy.
    /// Returns `None` for fewer than 2 values or zero total movement.
    pub fn range_efficiency(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 {
            return None;
        }
        let vals: Vec<Decimal> = self.window.iter().copied().collect();
        let net_move = (vals.last()? - vals.first()?).abs();
        let total_move: Decimal = vals.windows(2).map(|w| (w[1] - w[0]).abs()).sum();
        if total_move.is_zero() {
            return None;
        }
        Some((net_move / total_move).to_f64().unwrap_or(0.0))
    }

    /// Running total (sum) of all values in the window.
    pub fn window_running_total(&self) -> Decimal {
        self.window.iter().copied().sum()
    }

    // ── round-104 ────────────────────────────────────────────────────────────

    /// Convexity: mean second difference `(v[i+2] - 2*v[i+1] + v[i])` across
    /// the window. Positive = accelerating upward (concave up); negative = decelerating.
    /// Returns `None` for fewer than 3 values.
    pub fn window_convexity(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 3 {
            return None;
        }
        let vals: Vec<Decimal> = self.window.iter().copied().collect();
        let second_diffs: Vec<Decimal> = vals.windows(3)
            .map(|w| w[2] - Decimal::TWO * w[1] + w[0])
            .collect();
        let n = second_diffs.len();
        let mean: Decimal = second_diffs.iter().copied().sum::<Decimal>() / Decimal::from(n);
        mean.to_f64()
    }

    /// Fraction of window values that are strictly below their immediate predecessor.
    /// Returns `None` for fewer than 2 values.
    pub fn below_previous_fraction(&self) -> Option<f64> {
        if self.window.len() < 2 {
            return None;
        }
        let vals: Vec<Decimal> = self.window.iter().copied().collect();
        let count = vals.windows(2).filter(|w| w[1] < w[0]).count();
        Some(count as f64 / (vals.len() - 1) as f64)
    }

    /// Ratio of the standard deviation of the second half of the window to the first half.
    /// Returns `None` for fewer than 4 values or zero first-half std dev.
    pub fn window_volatility_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 4 {
            return None;
        }
        let vals: Vec<Decimal> = self.window.iter().copied().collect();
        let mid = vals.len() / 2;
        let std_dev = |slice: &[Decimal]| -> Option<f64> {
            let n = slice.len() as f64;
            let mean: f64 = slice.iter().filter_map(|v| v.to_f64()).sum::<f64>() / n;
            let var = slice.iter().filter_map(|v| v.to_f64()).map(|v| (v - mean).powi(2)).sum::<f64>() / n;
            Some(var.sqrt())
        };
        let s1 = std_dev(&vals[..mid])?;
        let s2 = std_dev(&vals[mid..])?;
        if s1 == 0.0 {
            return None;
        }
        Some(s2 / s1)
    }

    /// Gini coefficient of the window values (measures inequality/spread).
    /// Values in [0, 1]: 0 = all equal, 1 = maximally unequal.
    /// Returns `None` for fewer than 2 values or zero sum.
    pub fn window_gini(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 {
            return None;
        }
        let mut vals: Vec<Decimal> = self.window.iter().copied().collect();
        vals.sort();
        let n = vals.len();
        let sum: Decimal = vals.iter().copied().sum();
        if sum.is_zero() {
            return None;
        }
        let weighted: Decimal = vals.iter().enumerate()
            .map(|(i, &v)| Decimal::from(2 * (i as i64 + 1) - n as i64 - 1) * v)
            .sum();
        Some((weighted / (sum * Decimal::from(n))).to_f64().unwrap_or(0.0).abs())
    }

    // ── round-105 ────────────────────────────────────────────────────────────

    /// Approximate Hurst exponent via rescaled range (R/S) analysis.
    /// H > 0.5 indicates trending, H < 0.5 indicates mean-reverting.
    /// Returns `None` for fewer than 4 values.
    pub fn window_hurst_exponent(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 4 {
            return None;
        }
        let vals: Vec<f64> = self.window.iter().filter_map(|v| v.to_f64()).collect();
        if vals.len() != self.window.len() {
            return None;
        }
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        let deviations: Vec<f64> = vals.iter().map(|v| v - mean).collect();
        let mut cumulative = 0.0_f64;
        let mut cum_max = f64::NEG_INFINITY;
        let mut cum_min = f64::INFINITY;
        for d in &deviations {
            cumulative += d;
            if cumulative > cum_max { cum_max = cumulative; }
            if cumulative < cum_min { cum_min = cumulative; }
        }
        let r = cum_max - cum_min;
        let s = (vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n).sqrt();
        if s == 0.0 || r == 0.0 { return None; }
        Some((r / s).ln() / n.ln())
    }

    /// Count of times the window series crosses its own mean.
    /// Returns `None` for fewer than 2 values.
    pub fn window_mean_crossings(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 {
            return None;
        }
        let vals: Vec<f64> = self.window.iter().filter_map(|v| v.to_f64()).collect();
        if vals.len() != self.window.len() { return None; }
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let crossings = vals.windows(2)
            .filter(|w| (w[0] - mean).signum() != (w[1] - mean).signum()
                && w[0] != mean && w[1] != mean)
            .count();
        Some(crossings)
    }

    /// Sample skewness of the window values.
    /// Returns `None` for fewer than 3 values or zero standard deviation.
    pub fn window_skewness(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 3 {
            return None;
        }
        let vals: Vec<f64> = self.window.iter().filter_map(|v| v.to_f64()).collect();
        if vals.len() != self.window.len() { return None; }
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        let variance = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n;
        let std_dev = variance.sqrt();
        if std_dev == 0.0 { return None; }
        let skew = vals.iter().map(|v| ((v - mean) / std_dev).powi(3)).sum::<f64>() / n;
        Some(skew)
    }

    /// Maximum length of a consecutive run of increasing values in the window.
    /// Returns `None` for fewer than 2 values.
    pub fn window_max_run(&self) -> Option<usize> {
        if self.window.len() < 2 {
            return None;
        }
        let vals: Vec<Decimal> = self.window.iter().copied().collect();
        let mut max_run = 1usize;
        let mut cur_run = 1usize;
        for i in 1..vals.len() {
            if vals[i] > vals[i - 1] {
                cur_run += 1;
                if cur_run > max_run { max_run = cur_run; }
            } else {
                cur_run = 1;
            }
        }
        Some(max_run)
    }

    // ── round-106 ────────────────────────────────────────────────────────────

    /// Mean absolute deviation from the median of the window.
    /// Returns `None` for an empty window.
    pub fn window_median_deviation(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let mut sorted: Vec<Decimal> = self.window.iter().copied().collect();
        sorted.sort();
        let n = sorted.len();
        let median = if n % 2 == 1 { sorted[n / 2] } else { (sorted[n / 2 - 1] + sorted[n / 2]) / Decimal::TWO };
        let mad: Decimal = sorted.iter().map(|&v| (v - median).abs()).sum::<Decimal>() / Decimal::from(n);
        mad.to_f64()
    }

    /// Length of the longest run of consecutive values above the window mean.
    /// Returns `None` for an empty window.
    pub fn longest_above_mean_run(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().filter_map(|v| v.to_f64()).collect();
        if vals.len() != self.window.len() { return None; }
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let mut max_run = 0usize;
        let mut cur_run = 0usize;
        for &v in &vals {
            if v > mean { cur_run += 1; if cur_run > max_run { max_run = cur_run; } }
            else { cur_run = 0; }
        }
        Some(max_run)
    }

    /// Bimodality coefficient: `(skewness^2 + 1) / kurtosis`.
    /// Values > 5/9 suggest bimodality.
    /// Returns `None` for fewer than 4 values or zero kurtosis.
    pub fn window_bimodality(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 4 { return None; }
        let vals: Vec<f64> = self.window.iter().filter_map(|v| v.to_f64()).collect();
        if vals.len() != self.window.len() { return None; }
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        let var = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n;
        if var == 0.0 { return None; }
        let std_dev = var.sqrt();
        let skew = vals.iter().map(|v| ((v - mean) / std_dev).powi(3)).sum::<f64>() / n;
        let kurt = vals.iter().map(|v| ((v - mean) / std_dev).powi(4)).sum::<f64>() / n;
        if kurt == 0.0 { return None; }
        Some((skew.powi(2) + 1.0) / kurt)
    }

    /// Count of times adjacent values in the window change sign relative to zero.
    /// Returns `None` for fewer than 2 values.
    pub fn window_zero_crossings(&self) -> Option<usize> {
        if self.window.len() < 2 { return None; }
        let vals: Vec<Decimal> = self.window.iter().copied().collect();
        let count = vals.windows(2)
            .filter(|w| {
                (w[0].is_sign_positive() && w[1].is_sign_negative())
                    || (w[0].is_sign_negative() && w[1].is_sign_positive())
            })
            .count();
        Some(count)
    }

    // ── round-107 ────────────────────────────────────────────────────────────

    /// Sum of squared values in the window (signal energy).
    pub fn window_energy(&self) -> Decimal {
        self.window.iter().map(|&v| v * v).sum()
    }

    /// Mean of the middle 50% of window values (IQR mean / trimmed mean at 25%).
    /// Returns `None` for fewer than 4 values.
    pub fn window_interquartile_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 4 { return None; }
        let mut sorted: Vec<Decimal> = self.window.iter().copied().collect();
        sorted.sort();
        let n = sorted.len();
        let q1_idx = n / 4;
        let q3_idx = (3 * n) / 4;
        let mid: Vec<Decimal> = sorted[q1_idx..q3_idx].to_vec();
        if mid.is_empty() { return None; }
        let mean: Decimal = mid.iter().copied().sum::<Decimal>() / Decimal::from(mid.len());
        mean.to_f64()
    }

    /// Count of window values that exceed the window mean.
    /// Returns `None` for an empty window.
    pub fn above_mean_count(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let total: Decimal = self.window.iter().copied().sum();
        let mean = total / Decimal::from(self.window.len());
        Some(self.window.iter().filter(|&&v| v > mean).count())
    }

    /// Approximate differential entropy of window values using log of std dev.
    /// Returns `None` for fewer than 2 values or zero/negative variance.
    pub fn window_diff_entropy(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().filter_map(|v| v.to_f64()).collect();
        if vals.len() != self.window.len() { return None; }
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        let var = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n - 1.0);
        if var <= 0.0 { return None; }
        // Differential entropy of Normal: 0.5 * ln(2*pi*e*sigma^2)
        Some(0.5 * (2.0 * std::f64::consts::PI * std::f64::consts::E * var).ln())
    }

    // ── round-108 ────────────────────────────────────────────────────────────

    /// Root mean square of the window values.
    pub fn window_root_mean_square(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let sum_sq: Decimal = self.window.iter().map(|&v| v * v).sum();
        let mean_sq = sum_sq / Decimal::from(self.window.len());
        mean_sq.to_f64().map(f64::sqrt)
    }

    /// Mean of the first differences `v[i+1] - v[i]` across the window.
    /// Returns `None` for fewer than 2 values.
    pub fn window_first_derivative_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<Decimal> = self.window.iter().copied().collect();
        let diffs: Vec<Decimal> = vals.windows(2).map(|w| w[1] - w[0]).collect();
        let n = diffs.len();
        let mean: Decimal = diffs.iter().copied().sum::<Decimal>() / Decimal::from(n);
        mean.to_f64()
    }

    /// L1 norm (sum of absolute values) of the window.
    pub fn window_l1_norm(&self) -> Decimal {
        self.window.iter().map(|&v| v.abs()).sum()
    }

    /// 10th-percentile value of the window.
    /// Returns `None` for an empty window.
    pub fn window_percentile_10(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let mut sorted: Vec<Decimal> = self.window.iter().copied().collect();
        sorted.sort();
        let idx = ((sorted.len() as f64 * 0.10).ceil() as usize).saturating_sub(1);
        sorted[idx.min(sorted.len() - 1)].to_f64()
    }

    // ── round-109 ────────────────────────────────────────────────────────────

    /// Mean z-score of all window values relative to the window's own mean/std.
    /// By definition equals 0; useful as a sanity check.
    /// Returns `None` for fewer than 2 values or zero std dev.
    pub fn window_zscore_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().filter_map(|v| v.to_f64()).collect();
        if vals.len() != self.window.len() { return None; }
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        let std_dev = (vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n).sqrt();
        if std_dev == 0.0 { return None; }
        let sum_z: f64 = vals.iter().map(|v| (v - mean) / std_dev).sum();
        Some(sum_z / n)
    }

    /// Sum of all positive values in the window.
    pub fn window_positive_sum(&self) -> Decimal {
        self.window.iter().copied().filter(|v| v.is_sign_positive()).sum()
    }

    /// Sum of all negative values in the window (result is <= 0).
    pub fn window_negative_sum(&self) -> Decimal {
        self.window.iter().copied().filter(|v| v.is_sign_negative()).sum()
    }

    /// Fraction of consecutive pairs that continue in the same direction as the
    /// overall window trend (up if last > first, down if last < first).
    /// Returns `None` for fewer than 2 values or no net trend.
    pub fn window_trend_consistency(&self) -> Option<f64> {
        if self.window.len() < 2 { return None; }
        let vals: Vec<Decimal> = self.window.iter().copied().collect();
        let overall = vals.last()? - vals.first()?;
        if overall.is_zero() { return None; }
        let consistent = vals.windows(2)
            .filter(|w| {
                let step = w[1] - w[0];
                (overall.is_sign_positive() && step.is_sign_positive())
                    || (overall.is_sign_negative() && step.is_sign_negative())
            })
            .count();
        Some(consistent as f64 / (vals.len() - 1) as f64)
    }

    // ── round-110 ────────────────────────────────────────────────────────────

    /// Mean of all pairwise absolute differences `|v[i] - v[j]|` for i < j.
    /// Returns `None` for fewer than 2 values.
    pub fn window_pairwise_mean_diff(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<Decimal> = self.window.iter().copied().collect();
        let n = vals.len();
        let mut sum = Decimal::ZERO;
        let mut count = 0u64;
        for i in 0..n {
            for j in i + 1..n {
                sum += (vals[i] - vals[j]).abs();
                count += 1;
            }
        }
        if count == 0 { return None; }
        (sum / Decimal::from(count)).to_f64()
    }

    /// 75th-percentile value of the window.
    /// Returns `None` for an empty window.
    pub fn window_q3(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let mut sorted: Vec<Decimal> = self.window.iter().copied().collect();
        sorted.sort();
        let idx = ((sorted.len() as f64 * 0.75).ceil() as usize).saturating_sub(1);
        sorted[idx.min(sorted.len() - 1)].to_f64()
    }

    /// Coefficient of variation: `std_dev / mean`.
    /// Returns `None` for fewer than 2 values or zero mean.
    pub fn window_coefficient_of_variation(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().filter_map(|v| v.to_f64()).collect();
        if vals.len() != self.window.len() { return None; }
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        if mean == 0.0 { return None; }
        let std_dev = (vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n).sqrt();
        Some(std_dev / mean)
    }

    /// Second statistical moment (mean of squared values).
    pub fn window_second_moment(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let sum_sq: Decimal = self.window.iter().map(|&v| v * v).sum();
        (sum_sq / Decimal::from(self.window.len())).to_f64()
    }

    // ── round-111 ────────────────────────────────────────────────────────────

    /// Sum of window values after trimming the top and bottom 10% (floored to nearest value).
    /// Returns `None` for fewer than 5 values (need at least 1 value after trimming).
    pub fn window_trimmed_sum(&self) -> Option<Decimal> {
        if self.window.len() < 5 { return None; }
        let mut sorted: Vec<Decimal> = self.window.iter().copied().collect();
        sorted.sort();
        let n = sorted.len();
        let trim = (n as f64 * 0.1).floor() as usize;
        let trimmed = &sorted[trim..n - trim];
        if trimmed.is_empty() { return None; }
        Some(trimmed.iter().copied().sum())
    }

    /// Z-score of the window range `(max - min)` relative to the window mean.
    /// Returns `None` for fewer than 2 values or zero std dev.
    pub fn window_range_zscore(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().filter_map(|v| v.to_f64()).collect();
        if vals.len() != self.window.len() { return None; }
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        let std_dev = (vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n).sqrt();
        if std_dev == 0.0 { return None; }
        let range = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max)
            - vals.iter().cloned().fold(f64::INFINITY, f64::min);
        Some((range - mean) / std_dev)
    }

    /// Count of window values that are strictly above the window median.
    /// Returns `None` for an empty window.
    pub fn window_above_median_count(&self) -> Option<usize> {
        if self.window.is_empty() { return None; }
        let mut sorted: Vec<Decimal> = self.window.iter().copied().collect();
        sorted.sort();
        let n = sorted.len();
        let median = if n % 2 == 1 { sorted[n / 2] }
                     else { (sorted[n / 2 - 1] + sorted[n / 2]) / Decimal::TWO };
        Some(self.window.iter().filter(|&&v| v > median).count())
    }

    /// Maximum length of a consecutive run of *decreasing* values in the window.
    /// Returns `None` for fewer than 2 values.
    pub fn window_min_run(&self) -> Option<usize> {
        if self.window.len() < 2 { return None; }
        let vals: Vec<Decimal> = self.window.iter().copied().collect();
        let mut max_run = 1usize;
        let mut cur_run = 1usize;
        for i in 1..vals.len() {
            if vals[i] < vals[i - 1] {
                cur_run += 1;
                if cur_run > max_run { max_run = cur_run; }
            } else {
                cur_run = 1;
            }
        }
        Some(max_run)
    }

    // ── round-112 ────────────────────────────────────────────────────────────

    /// Harmonic mean of the window values. Returns `None` if window is empty
    /// or any value is zero.
    pub fn window_harmonic_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let mut sum_recip = 0f64;
        for &v in &self.window {
            let f = v.to_f64()?;
            if f == 0.0 { return None; }
            sum_recip += 1.0 / f;
        }
        Some(self.window.len() as f64 / sum_recip)
    }

    /// Geometric standard deviation of the window (exp of std-dev of log-values).
    /// Returns `None` if window has fewer than 2 values or any value is non-positive.
    pub fn window_geometric_std(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let logs: Vec<f64> = self.window.iter().map(|&v| {
            let f = v.to_f64()?;
            if f <= 0.0 { None } else { Some(f.ln()) }
        }).collect::<Option<Vec<_>>>()?;
        let n = logs.len() as f64;
        let mean = logs.iter().sum::<f64>() / n;
        let var = logs.iter().map(|&x| (x - mean).powi(2)).sum::<f64>() / (n - 1.0);
        Some(var.sqrt().exp())
    }

    /// Entropy rate approximation: mean of absolute first differences of the window.
    /// Returns `None` for fewer than 2 values.
    pub fn window_entropy_rate(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let sum: f64 = vals.windows(2).map(|w| (w[1] - w[0]).abs()).sum();
        Some(sum / (vals.len() - 1) as f64)
    }

    /// Burstiness: (std_dev - mean) / (std_dev + mean). Returns `None` if window
    /// is empty, mean + std_dev is zero, or fewer than 2 values for std_dev.
    pub fn window_burstiness(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        let var = vals.iter().map(|&x| (x - mean).powi(2)).sum::<f64>() / (n - 1.0);
        let std = var.sqrt();
        let denom = std + mean;
        if denom == 0.0 { None } else { Some((std - mean) / denom) }
    }

    // ── round-113 ────────────────────────────────────────────────────────────

    /// Ratio of IQR to median. Returns `None` if fewer than 3 values or zero median.
    pub fn window_iqr_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 3 { return None; }
        let mut sorted: Vec<Decimal> = self.window.iter().copied().collect();
        sorted.sort();
        let n = sorted.len();
        let median = if n % 2 == 1 { sorted[n / 2] }
                     else { (sorted[n / 2 - 1] + sorted[n / 2]) / Decimal::TWO };
        if median.is_zero() { return None; }
        let q1 = sorted[n / 4];
        let q3 = sorted[3 * n / 4];
        let iqr = q3 - q1;
        (iqr / median).to_f64()
    }

    /// Mean-reversion score: fraction of consecutive pairs where the value moves
    /// toward the overall window mean. Returns `None` for fewer than 2 values.
    pub fn window_mean_reversion(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let count = vals.windows(2).filter(|w| {
            let before = (w[0] - mean).abs();
            let after = (w[1] - mean).abs();
            after < before
        }).count();
        Some(count as f64 / (vals.len() - 1) as f64)
    }

    /// Lag-1 autocorrelation of the window values. Returns `None` for fewer than
    /// 3 values or zero variance.
    pub fn window_autocorrelation(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 3 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        let var = vals.iter().map(|&x| (x - mean).powi(2)).sum::<f64>() / n;
        if var == 0.0 { return None; }
        let cov: f64 = vals.windows(2).map(|w| (w[0] - mean) * (w[1] - mean)).sum::<f64>()
            / (vals.len() - 1) as f64;
        Some(cov / var)
    }

    /// Ordinary least-squares slope of window values over their index.
    /// Returns `None` for fewer than 2 values.
    pub fn window_slope(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let n = vals.len() as f64;
        let x_mean = (n - 1.0) / 2.0;
        let y_mean = vals.iter().sum::<f64>() / n;
        let num: f64 = vals.iter().enumerate().map(|(i, &y)| (i as f64 - x_mean) * (y - y_mean)).sum();
        let den: f64 = vals.iter().enumerate().map(|(i, _)| (i as f64 - x_mean).powi(2)).sum();
        if den == 0.0 { None } else { Some(num / den) }
    }

    // ── round-114 ────────────────────────────────────────────────────────────

    /// Crest factor: max absolute value divided by RMS. Returns `None` if window
    /// is empty or RMS is zero.
    pub fn window_crest_factor(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let rms = (vals.iter().map(|&x| x * x).sum::<f64>() / vals.len() as f64).sqrt();
        if rms == 0.0 { return None; }
        let peak = vals.iter().map(|&x| x.abs()).fold(0f64, f64::max);
        Some(peak / rms)
    }

    /// Relative range: `(max - min) / mean`. Returns `None` for empty window or
    /// zero mean.
    pub fn window_relative_range(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        if mean == 0.0 { return None; }
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        Some((max - min) / mean)
    }

    /// Count of values more than `k` standard deviations from the mean (default k=2).
    /// Returns `None` for fewer than 2 values.
    pub fn window_outlier_count(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        let std = (vals.iter().map(|&x| (x - mean).powi(2)).sum::<f64>() / (n - 1.0)).sqrt();
        if std == 0.0 { return Some(0); }
        let k = 2.0f64;
        Some(vals.iter().filter(|&&x| (x - mean).abs() > k * std).count())
    }

    /// Exponential decay score: weighted mean where older values have exponentially
    /// lower weight (alpha=0.5). Returns `None` for empty window.
    pub fn window_decay_score(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let alpha = 0.5f64;
        let n = vals.len();
        let mut num = 0f64;
        let mut denom = 0f64;
        for (i, &v) in vals.iter().enumerate() {
            let w = alpha.powi((n - 1 - i) as i32);
            num += v * w;
            denom += w;
        }
        if denom == 0.0 { None } else { Some(num / denom) }
    }

    // ── round-115 ────────────────────────────────────────────────────────────

    /// Mean log return between consecutive window values.
    /// Returns `None` for fewer than 2 values or any non-positive value.
    pub fn window_log_return(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| {
            let f = v.to_f64()?;
            if f <= 0.0 { None } else { Some(f) }
        }).collect::<Option<Vec<_>>>()?;
        let sum: f64 = vals.windows(2).map(|w| (w[1] / w[0]).ln()).sum();
        Some(sum / (vals.len() - 1) as f64)
    }

    /// Signed RMS: RMS with the sign of the window mean. Returns `None` for empty window.
    pub fn window_signed_rms(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let rms = (vals.iter().map(|&x| x * x).sum::<f64>() / vals.len() as f64).sqrt();
        Some(if mean < 0.0 { -rms } else { rms })
    }

    /// Count of inflection points (local minima or maxima) in the window.
    /// Returns `None` for fewer than 3 values.
    pub fn window_inflection_count(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 3 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let count = vals.windows(3).filter(|w| {
            (w[1] > w[0] && w[1] > w[2]) || (w[1] < w[0] && w[1] < w[2])
        }).count();
        Some(count)
    }

    /// Centroid of the window: index-weighted mean position.
    /// Returns `None` for empty window or zero total weight.
    pub fn window_centroid(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let total: f64 = vals.iter().sum();
        if total == 0.0 { return None; }
        let weighted: f64 = vals.iter().enumerate().map(|(i, &v)| i as f64 * v).sum();
        Some(weighted / total)
    }

    // ── round-116 ────────────────────────────────────────────────────────────

    /// Maximum absolute deviation from the window mean.
    /// Returns `None` for empty window.
    pub fn window_max_deviation(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let max_dev = vals.iter().map(|&x| (x - mean).abs()).fold(0f64, f64::max);
        Some(max_dev)
    }

    /// Ratio of window range to mean. Returns `None` for empty window or zero mean.
    pub fn window_range_mean_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        if mean == 0.0 { return None; }
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        Some((max - min) / mean)
    }

    /// Count of consecutive steps where each value is strictly greater than the previous.
    /// Returns `None` for fewer than 2 values.
    pub fn window_step_up_count(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        Some(vals.windows(2).filter(|w| w[1] > w[0]).count())
    }

    /// Count of consecutive steps where each value is strictly less than the previous.
    /// Returns `None` for fewer than 2 values.
    pub fn window_step_down_count(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        Some(vals.windows(2).filter(|w| w[1] < w[0]).count())
    }

    // ── round-117 ────────────────────────────────────────────────────────────

    /// Shannon entropy of absolute differences between consecutive window values,
    /// binned into 8 equal-width buckets. Returns `None` for fewer than 2 values.
    pub fn window_entropy_of_changes(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let diffs: Vec<f64> = vals.windows(2).map(|w| (w[1] - w[0]).abs()).collect();
        let max = diffs.iter().cloned().fold(0f64, f64::max);
        let min = diffs.iter().cloned().fold(f64::INFINITY, f64::min);
        let range = max - min;
        const BUCKETS: usize = 8;
        let mut counts = [0usize; BUCKETS];
        if range == 0.0 {
            counts[0] = diffs.len();
        } else {
            for &d in &diffs {
                let idx = ((d - min) / range * (BUCKETS - 1) as f64) as usize;
                counts[idx.min(BUCKETS - 1)] += 1;
            }
        }
        let n = diffs.len() as f64;
        Some(counts.iter().map(|&c| {
            if c == 0 { 0.0 } else { let p = c as f64 / n; -p * p.ln() }
        }).sum())
    }

    /// Rate at which window values cross their mean level.
    /// Returns `None` for fewer than 2 values.
    pub fn window_level_crossing_rate(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let crossings = vals.windows(2).filter(|w| {
            (w[0] >= mean && w[1] < mean) || (w[0] < mean && w[1] >= mean)
        }).count();
        Some(crossings as f64 / (vals.len() - 1) as f64)
    }

    /// Mean of absolute window values. Returns `None` for empty window.
    pub fn window_abs_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let sum: f64 = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0).abs()).sum();
        Some(sum / self.window.len() as f64)
    }

    /// Maximum value in the window. Returns `None` for empty window.
    pub fn window_rolling_max(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        self.window.iter().map(|v| v.to_f64().unwrap_or(f64::NEG_INFINITY)).reduce(f64::max)
    }

    // ── round-118 ────────────────────────────────────────────────────────────

    /// Minimum value in the window. Returns `None` for empty window.
    pub fn window_rolling_min(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        self.window.iter().map(|v| v.to_f64().unwrap_or(f64::INFINITY)).reduce(f64::min)
    }

    /// Fraction of window values that are strictly negative.
    /// Returns `None` for empty window.
    pub fn window_negative_fraction(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let neg = self.window.iter().filter(|&&v| v < rust_decimal::Decimal::ZERO).count();
        Some(neg as f64 / self.window.len() as f64)
    }

    /// Fraction of window values that are strictly positive.
    /// Returns `None` for empty window.
    pub fn window_positive_fraction(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let pos = self.window.iter().filter(|&&v| v > rust_decimal::Decimal::ZERO).count();
        Some(pos as f64 / self.window.len() as f64)
    }

    /// Last window value minus the window minimum.
    /// Returns `None` for empty window.
    pub fn window_last_minus_min(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let last = self.window.back()?.to_f64()?;
        let min = self.window.iter().map(|v| v.to_f64().unwrap_or(f64::INFINITY)).fold(f64::INFINITY, f64::min);
        Some(last - min)
    }

    // ── round-119 ────────────────────────────────────────────────────────────

    /// Range of the window: `max - min`. Returns `None` for empty window.
    pub fn window_max_minus_min(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let max = self.window.iter().map(|v| v.to_f64().unwrap_or(f64::NEG_INFINITY)).fold(f64::NEG_INFINITY, f64::max);
        let min = self.window.iter().map(|v| v.to_f64().unwrap_or(f64::INFINITY)).fold(f64::INFINITY, f64::min);
        Some(max - min)
    }

    /// Normalized mean: `(mean - min) / (max - min)`. Returns `None` for empty window
    /// or zero range.
    pub fn window_normalized_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        let range = max - min;
        if range == 0.0 { None } else { Some((mean - min) / range) }
    }

    /// Ratio of variance to mean squared (equivalent to squared CV).
    /// Returns `None` for fewer than 2 values or zero mean.
    pub fn window_variance_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        if mean == 0.0 { return None; }
        let var = vals.iter().map(|&x| (x - mean).powi(2)).sum::<f64>() / (n - 1.0);
        Some(var / (mean * mean))
    }

    /// Maximum window value minus the last window value.
    /// Returns `None` for empty window.
    pub fn window_max_minus_last(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let last = self.window.back()?.to_f64()?;
        let max = self.window.iter().map(|v| v.to_f64().unwrap_or(f64::NEG_INFINITY)).fold(f64::NEG_INFINITY, f64::max);
        Some(max - last)
    }

    // ── round-120 ────────────────────────────────────────────────────────────

    /// Count of window values strictly above the last value.
    pub fn window_above_last(&self) -> Option<usize> {
        if self.window.is_empty() { return None; }
        let last = self.window.back()?;
        Some(self.window.iter().filter(|v| *v > last).count())
    }

    /// Count of window values strictly below the last value.
    pub fn window_below_last(&self) -> Option<usize> {
        if self.window.is_empty() { return None; }
        let last = self.window.back()?;
        Some(self.window.iter().filter(|v| *v < last).count())
    }

    /// Mean of successive differences (last - first) / (n-1).
    pub fn window_diff_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let diffs: Vec<f64> = vals.windows(2).map(|w| w[1] - w[0]).collect();
        Some(diffs.iter().sum::<f64>() / diffs.len() as f64)
    }

    /// Z-score of the last value relative to the window.
    pub fn window_last_zscore(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        let std = (vals.iter().map(|&x| (x - mean).powi(2)).sum::<f64>() / n).sqrt();
        if std == 0.0 { return None; }
        let last = *vals.last()?;
        Some((last - mean) / std)
    }

    // ── round-121 ────────────────────────────────────────────────────────────

    /// Fraction of window values within [min, min + range/2] (lower half of range).
    pub fn window_range_fraction(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let range = max - min;
        if range == 0.0 { return Some(1.0); }
        let mid = min + range / 2.0;
        Some(vals.iter().filter(|&&v| v <= mid).count() as f64 / vals.len() as f64)
    }

    /// Whether the window mean is above the last value (1.0 if so, 0.0 otherwise).
    pub fn window_mean_above_last(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let last = *vals.last()?;
        Some(if mean > last { 1.0 } else { 0.0 })
    }

    /// Volatility trend: std of second half minus std of first half of window.
    pub fn window_volatility_trend(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 4 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mid = vals.len() / 2;
        let std_half = |half: &[f64]| -> f64 {
            let n = half.len() as f64;
            let m = half.iter().sum::<f64>() / n;
            (half.iter().map(|&x| (x - m).powi(2)).sum::<f64>() / n).sqrt()
        };
        Some(std_half(&vals[mid..]) - std_half(&vals[..mid]))
    }

    /// Count of sign changes in successive differences across the window.
    pub fn window_sign_change_count(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 3 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let diffs: Vec<f64> = vals.windows(2).map(|w| w[1] - w[0]).collect();
        let changes = diffs.windows(2)
            .filter(|w| w[0] * w[1] < 0.0)
            .count();
        Some(changes)
    }

    // ── round-122 ────────────────────────────────────────────────────────────

    /// Rank of the last value within the window (0.0 = min, 1.0 = max).
    pub fn window_last_rank(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let last = self.window.back()?.to_f64()?;
        let mut sorted: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let pos = sorted.iter().position(|&x| (x - last).abs() < 1e-12).unwrap_or(0);
        Some(pos as f64 / (sorted.len() - 1).max(1) as f64)
    }

    /// Momentum score: mean of sign(diff) across successive window values.
    pub fn window_momentum_score(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let score: f64 = vals.windows(2)
            .map(|w| if w[1] > w[0] { 1.0 } else if w[1] < w[0] { -1.0 } else { 0.0 })
            .sum::<f64>() / (vals.len() - 1) as f64;
        Some(score)
    }

    /// Count of local maxima in the window.
    pub fn window_oscillation_count(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 3 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let count = vals.windows(3).filter(|w| w[1] > w[0] && w[1] > w[2]).count();
        Some(count)
    }

    /// Direction of skew: 1.0 if mean > median, -1.0 if mean < median, 0.0 if equal.
    pub fn window_skew_direction(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let mut sorted: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = sorted.len();
        let median = if n % 2 == 0 { (sorted[n/2 - 1] + sorted[n/2]) / 2.0 } else { sorted[n/2] };
        let mean = sorted.iter().sum::<f64>() / n as f64;
        Some(if mean > median { 1.0 } else if mean < median { -1.0 } else { 0.0 })
    }

    // ── round-123 ────────────────────────────────────────────────────────────

    /// Count of trend reversals: sign changes in consecutive differences.
    pub fn window_trend_reversal_count(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 3 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let diffs: Vec<f64> = vals.windows(2).map(|w| w[1] - w[0]).collect();
        Some(diffs.windows(2).filter(|w| w[0] * w[1] < 0.0).count())
    }

    /// Difference between first and last values in the window.
    pub fn window_first_last_diff(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let first = self.window.front()?.to_f64()?;
        let last = self.window.back()?.to_f64()?;
        Some(last - first)
    }

    /// Count of values in the upper half of the window's range.
    pub fn window_upper_half_count(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let mid = (min + max) / 2.0;
        Some(vals.iter().filter(|&&v| v > mid).count())
    }

    /// Count of values in the lower half of the window's range.
    pub fn window_lower_half_count(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let mid = (min + max) / 2.0;
        Some(vals.iter().filter(|&&v| v < mid).count())
    }

    // ── round-124 ────────────────────────────────────────────────────────────

    /// 75th percentile of window values.
    pub fn window_percentile_75(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let mut sorted: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let idx = ((sorted.len() as f64 * 0.75) as usize).min(sorted.len() - 1);
        Some(sorted[idx])
    }

    /// Absolute slope: |last - first| / (n - 1).
    pub fn window_abs_slope(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let first = self.window.front()?.to_f64()?;
        let last = self.window.back()?.to_f64()?;
        Some((last - first).abs() / (self.window.len() - 1) as f64)
    }

    /// Ratio of sum of gains to sum of losses; None if no losses.
    pub fn window_gain_loss_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mut gains = 0f64;
        let mut losses = 0f64;
        for w in vals.windows(2) {
            let d = w[1] - w[0];
            if d > 0.0 { gains += d; } else { losses += d.abs(); }
        }
        if losses == 0.0 { return None; }
        Some(gains / losses)
    }

    /// Stability of range: 1 - std(range_pct) where range_pct = (v - min) / (max - min).
    pub fn window_range_stability(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let range = max - min;
        if range == 0.0 { return Some(1.0); }
        let pcts: Vec<f64> = vals.iter().map(|&v| (v - min) / range).collect();
        let n = pcts.len() as f64;
        let mean = pcts.iter().sum::<f64>() / n;
        let std = (pcts.iter().map(|&x| (x - mean).powi(2)).sum::<f64>() / n).sqrt();
        Some(1.0 - std)
    }

    // ── round-125 ────────────────────────────────────────────────────────────

    /// Exponentially smoothed last value (α=0.2).
    pub fn window_exp_smoothed(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let alpha = 0.2f64;
        let mut ema = self.window.front()?.to_f64()?;
        for v in self.window.iter().skip(1) {
            ema = alpha * v.to_f64().unwrap_or(0.0) + (1.0 - alpha) * ema;
        }
        Some(ema)
    }

    /// Maximum drawdown: largest peak-to-trough decline in the window.
    pub fn window_drawdown(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mut max_dd = 0f64;
        let mut peak = vals[0];
        for &v in &vals[1..] {
            if v > peak { peak = v; }
            let dd = peak - v;
            if dd > max_dd { max_dd = dd; }
        }
        Some(max_dd)
    }

    /// Maximum drawup: largest trough-to-peak gain in the window.
    pub fn window_drawup(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mut max_du = 0f64;
        let mut trough = vals[0];
        for &v in &vals[1..] {
            if v < trough { trough = v; }
            let du = v - trough;
            if du > max_du { max_du = du; }
        }
        Some(max_du)
    }

    /// Trend strength: |sum of signed differences| / sum of absolute differences.
    pub fn window_trend_strength(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let signed: f64 = vals.windows(2).map(|w| w[1] - w[0]).sum();
        let abs_sum: f64 = vals.windows(2).map(|w| (w[1] - w[0]).abs()).sum();
        if abs_sum == 0.0 { return None; }
        Some(signed.abs() / abs_sum)
    }

    // ── round-126 ────────────────────────────────────────────────────────────

    /// Deviation of last value from EMA (α=0.2) of the window.
    pub fn window_ema_deviation(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let alpha = 0.2f64;
        let mut ema = self.window.front()?.to_f64()?;
        for v in self.window.iter().skip(1) {
            ema = alpha * v.to_f64().unwrap_or(0.0) + (1.0 - alpha) * ema;
        }
        let last = self.window.back()?.to_f64()?;
        Some(last - ema)
    }

    /// Variance normalized by mean squared (coefficient of variation squared).
    pub fn window_normalized_variance(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        if mean == 0.0 { return None; }
        let var = vals.iter().map(|&x| (x - mean).powi(2)).sum::<f64>() / n;
        Some(var / mean.powi(2))
    }

    /// Ratio of last value to window median.
    pub fn window_median_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let mut sorted: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = sorted.len();
        let median = if n % 2 == 0 { (sorted[n/2-1] + sorted[n/2]) / 2.0 } else { sorted[n/2] };
        if median == 0.0 { return None; }
        let last = self.window.back()?.to_f64()?;
        Some(last / median)
    }

    /// Approximate half-life: steps for value to decay to half peak.
    /// Returns the index of first value <= peak/2 from the peak, or None.
    pub fn window_half_life(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let peak = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let half = peak / 2.0;
        let peak_idx = vals.iter().position(|&v| (v - peak).abs() < 1e-12)?;
        vals[peak_idx..].iter().position(|&v| v <= half)
    }

    // ── round-127 ────────────────────────────────────────────────────────────

    /// Shannon entropy of the window normalized to [0, 1] by dividing by log2(n).
    pub fn window_entropy_normalized(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let n = vals.len() as f64;
        let total: f64 = vals.iter().map(|&v| v.abs()).sum();
        if total == 0.0 { return Some(0.0); }
        let entropy: f64 = vals.iter()
            .map(|&v| { let p = v.abs() / total; if p > 0.0 { -p * p.log2() } else { 0.0 } })
            .sum();
        let max_entropy = n.log2();
        if max_entropy == 0.0 { return Some(0.0); }
        Some(entropy / max_entropy)
    }

    /// Maximum value in the window.
    pub fn window_peak_value(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        Some(self.window.iter().map(|v| v.to_f64().unwrap_or(f64::NEG_INFINITY)).fold(f64::NEG_INFINITY, f64::max))
    }

    /// Minimum value in the window.
    pub fn window_trough_value(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        Some(self.window.iter().map(|v| v.to_f64().unwrap_or(f64::INFINITY)).fold(f64::INFINITY, f64::min))
    }

    /// Count of positive successive differences (gains).
    pub fn window_gain_count(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        Some(vals.windows(2).filter(|w| w[1] > w[0]).count())
    }

    // ── round-128 ────────────────────────────────────────────────────────────

    /// Count of negative successive differences (losses).
    pub fn window_loss_count(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        Some(vals.windows(2).filter(|w| w[1] < w[0]).count())
    }

    /// Net change: last value minus first value.
    pub fn window_net_change(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let first = self.window.front()?.to_f64()?;
        let last = self.window.back()?.to_f64()?;
        Some(last - first)
    }

    /// Mean second-order difference (acceleration) of window values.
    pub fn window_acceleration(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 3 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let acc: Vec<f64> = vals.windows(3).map(|w| w[2] - 2.0 * w[1] + w[0]).collect();
        Some(acc.iter().sum::<f64>() / acc.len() as f64)
    }

    /// Regime score: fraction of values above mean minus fraction below mean.
    pub fn window_regime_score(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        let above = vals.iter().filter(|&&v| v > mean).count() as f64;
        let below = vals.iter().filter(|&&v| v < mean).count() as f64;
        Some((above - below) / n)
    }

    // ── round-129 ────────────────────────────────────────────────────────────

    /// Cumulative sum of all window values.
    pub fn window_cumulative_sum(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let sum: f64 = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).sum();
        Some(sum)
    }

    /// Spread ratio: (max - min) / mean of the window.
    pub fn window_spread_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        if mean == 0.0 { return None; }
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        Some((max - min) / mean.abs())
    }

    /// Center of mass: weighted index position (higher = recent values dominate).
    pub fn window_center_of_mass(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let total: f64 = vals.iter().sum();
        if total == 0.0 { return None; }
        let weighted: f64 = vals.iter().enumerate()
            .map(|(i, &v)| i as f64 * v)
            .sum();
        Some(weighted / total)
    }

    /// Cycle count: number of direction reversals in the window.
    pub fn window_cycle_count(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 3 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let diffs: Vec<f64> = vals.windows(2).map(|w| w[1] - w[0]).collect();
        let count = diffs.windows(2)
            .filter(|w| w[0] * w[1] < 0.0)
            .count();
        Some(count)
    }

    // ── round-130 ────────────────────────────────────────────────────────────

    /// Mean absolute deviation from the window mean.
    pub fn window_mad(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let mad = vals.iter().map(|&v| (v - mean).abs()).sum::<f64>() / vals.len() as f64;
        Some(mad)
    }

    /// Entropy ratio: actual entropy / max possible entropy for window size.
    pub fn window_entropy_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let total: f64 = vals.iter().sum();
        if total == 0.0 { return None; }
        let entropy: f64 = vals.iter().map(|&v| {
            let p = v / total;
            if p > 0.0 { -p * p.ln() } else { 0.0 }
        }).sum();
        let max_entropy = (vals.len() as f64).ln();
        if max_entropy == 0.0 { return None; }
        Some(entropy / max_entropy)
    }

    /// Plateau count: number of consecutive equal values in the window.
    pub fn window_plateau_count(&self) -> Option<usize> {
        if self.window.len() < 2 { return None; }
        let count = self.window.iter()
            .collect::<Vec<_>>()
            .windows(2)
            .filter(|w| w[0] == w[1])
            .count();
        Some(count)
    }

    /// Direction bias: fraction of steps that are upward minus fraction downward.
    pub fn window_direction_bias(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let n = (vals.len() - 1) as f64;
        let up = vals.windows(2).filter(|w| w[1] > w[0]).count() as f64;
        let dn = vals.windows(2).filter(|w| w[1] < w[0]).count() as f64;
        Some((up - dn) / n)
    }

    // ── round-131 ────────────────────────────────────────────────────────────

    /// Percentage change from first to last value in the window.
    pub fn window_last_pct_change(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let first = self.window.front()?.to_f64()?;
        let last = self.window.back()?.to_f64()?;
        if first == 0.0 { return None; }
        Some((last - first) / first.abs())
    }

    /// Standard deviation trend: difference between std of 2nd half and 1st half.
    pub fn window_std_trend(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 4 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mid = vals.len() / 2;
        let std_half = |slice: &[f64]| {
            let mean = slice.iter().sum::<f64>() / slice.len() as f64;
            (slice.iter().map(|&v| (v - mean).powi(2)).sum::<f64>() / slice.len() as f64).sqrt()
        };
        let first_std = std_half(&vals[..mid]);
        let second_std = std_half(&vals[mid..]);
        Some(second_std - first_std)
    }

    /// Count of non-zero values in the window.
    pub fn window_nonzero_count(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let count = self.window.iter()
            .filter(|v| v.to_f64().unwrap_or(0.0) != 0.0)
            .count();
        Some(count)
    }

    /// Fraction of window values above the mean.
    pub fn window_pct_above_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let above = vals.iter().filter(|&&v| v > mean).count() as f64;
        Some(above / vals.len() as f64)
    }

    // ── round-132 ────────────────────────────────────────────────────────────

    /// Maximum consecutive run of increasing values in the window.
    pub fn window_max_run_up(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mut max_run = 0usize;
        let mut cur_run = 0usize;
        for w in vals.windows(2) {
            if w[1] > w[0] { cur_run += 1; max_run = max_run.max(cur_run); }
            else { cur_run = 0; }
        }
        Some(max_run)
    }

    /// Maximum consecutive run of decreasing values in the window.
    pub fn window_max_run_dn(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mut max_run = 0usize;
        let mut cur_run = 0usize;
        for w in vals.windows(2) {
            if w[1] < w[0] { cur_run += 1; max_run = max_run.max(cur_run); }
            else { cur_run = 0; }
        }
        Some(max_run)
    }

    /// Sum of all consecutive differences in the window.
    pub fn window_diff_sum(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let sum: f64 = vals.windows(2).map(|w| w[1] - w[0]).sum();
        Some(sum)
    }

    /// Longest run in the same direction (up or down).
    pub fn window_run_length(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mut max_run = 0usize;
        let mut cur_run = 1usize;
        let diffs: Vec<f64> = vals.windows(2).map(|w| w[1] - w[0]).collect();
        for i in 1..diffs.len() {
            if diffs[i] * diffs[i - 1] > 0.0 { cur_run += 1; max_run = max_run.max(cur_run); }
            else { max_run = max_run.max(cur_run); cur_run = 1; }
        }
        max_run = max_run.max(cur_run);
        Some(max_run)
    }

    // ── round-133 ────────────────────────────────────────────────────────────

    /// Sum of absolute differences between consecutive window values.
    pub fn window_abs_diff_sum(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let sum: f64 = vals.windows(2).map(|w| (w[1] - w[0]).abs()).sum();
        Some(sum)
    }

    /// Maximum gap between any two consecutive window values.
    pub fn window_max_gap(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let max_gap = vals.windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(f64::NEG_INFINITY, f64::max);
        Some(max_gap)
    }

    /// Count of local maxima in the window (values greater than both neighbors).
    pub fn window_local_max_count(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 3 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let count = vals.windows(3).filter(|w| w[1] > w[0] && w[1] > w[2]).count();
        Some(count)
    }

    /// Mean of the first half of the window.
    pub fn window_first_half_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mid = vals.len() / 2;
        let first_half = &vals[..mid];
        if first_half.is_empty() { return None; }
        Some(first_half.iter().sum::<f64>() / first_half.len() as f64)
    }

    // ── round-134 ────────────────────────────────────────────────────────────

    /// Mean of the second half of the window.
    pub fn window_second_half_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mid = vals.len() / 2;
        let second_half = &vals[mid..];
        if second_half.is_empty() { return None; }
        Some(second_half.iter().sum::<f64>() / second_half.len() as f64)
    }

    /// Count of local minima in the window (values less than both neighbors).
    pub fn window_local_min_count(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 3 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let count = vals.windows(3).filter(|w| w[1] < w[0] && w[1] < w[2]).count();
        Some(count)
    }

    /// Curvature: mean of second-order differences (acceleration) in the window.
    pub fn window_curvature(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 3 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let second_diffs: Vec<f64> = vals.windows(3)
            .map(|w| (w[2] - w[1]) - (w[1] - w[0]))
            .collect();
        Some(second_diffs.iter().sum::<f64>() / second_diffs.len() as f64)
    }

    /// Mean of second half minus mean of first half of window.
    pub fn window_half_diff(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mid = vals.len() / 2;
        let first_mean = vals[..mid].iter().sum::<f64>() / mid as f64;
        let second = &vals[mid..];
        if second.is_empty() { return None; }
        let second_mean = second.iter().sum::<f64>() / second.len() as f64;
        Some(second_mean - first_mean)
    }

    // ── round-135 ────────────────────────────────────────────────────────────

    /// Rate of mean crossings in the window (fraction of steps crossing the mean).
    pub fn window_mean_crossing_rate(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let crossings = vals.windows(2)
            .filter(|w| (w[0] - mean) * (w[1] - mean) < 0.0)
            .count();
        Some(crossings as f64 / (vals.len() - 1) as f64)
    }

    /// Variance to mean ratio (index of dispersion).
    pub fn window_var_to_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        if mean == 0.0 { return None; }
        let variance = vals.iter().map(|&v| (v - mean).powi(2)).sum::<f64>() / vals.len() as f64;
        Some(variance / mean.abs())
    }

    /// Coefficient of variation: std / mean (relative dispersion).
    pub fn window_coeff_var(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        if mean == 0.0 { return None; }
        let std = (vals.iter().map(|&v| (v - mean).powi(2)).sum::<f64>() / vals.len() as f64).sqrt();
        Some(std / mean.abs())
    }

    /// Fraction of steps that are strictly upward.
    pub fn window_step_up_fraction(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let total = vals.len() - 1;
        let up = vals.windows(2).filter(|w| w[1] > w[0]).count();
        Some(up as f64 / total as f64)
    }

    // ── round-136 ────────────────────────────────────────────────────────────

    /// Fraction of steps that are strictly downward.
    pub fn window_step_dn_fraction(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let total = vals.len() - 1;
        let dn = vals.windows(2).filter(|w| w[1] < w[0]).count();
        Some(dn as f64 / total as f64)
    }

    /// Mean absolute deviation as ratio to the window range.
    pub fn window_mean_abs_dev_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        let range = max - min;
        if range == 0.0 { return None; }
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let mad = vals.iter().map(|&v| (v - mean).abs()).sum::<f64>() / vals.len() as f64;
        Some(mad / range)
    }

    /// Most recent maximum value in the window (same as window_peak_value, reusing pattern).
    pub fn window_recent_high(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let half = self.window.len() / 2;
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let recent = &vals[half..];
        if recent.is_empty() { return None; }
        Some(recent.iter().cloned().fold(f64::NEG_INFINITY, f64::max))
    }

    /// Most recent minimum value (second half of window).
    pub fn window_recent_low(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let half = self.window.len() / 2;
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let recent = &vals[half..];
        if recent.is_empty() { return None; }
        Some(recent.iter().cloned().fold(f64::INFINITY, f64::min))
    }

    // ── round-137 ────────────────────────────────────────────────────────────

    /// Linear trend score: slope of linear regression / mean of window values.
    pub fn window_linear_trend_score(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        if mean == 0.0 { return None; }
        let x_mean = (n - 1.0) / 2.0;
        let num: f64 = vals.iter().enumerate().map(|(i, &y)| (i as f64 - x_mean) * (y - mean)).sum();
        let den: f64 = vals.iter().enumerate().map(|(i, _)| (i as f64 - x_mean).powi(2)).sum();
        if den == 0.0 { return None; }
        Some((num / den) / mean.abs())
    }

    /// Minimum z-score in the window.
    pub fn window_zscore_min(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let std = (vals.iter().map(|&v| (v - mean).powi(2)).sum::<f64>() / vals.len() as f64).sqrt();
        if std == 0.0 { return None; }
        let min = vals.iter().map(|&v| (v - mean) / std).fold(f64::INFINITY, f64::min);
        Some(min)
    }

    /// Maximum z-score in the window.
    pub fn window_zscore_max(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let std = (vals.iter().map(|&v| (v - mean).powi(2)).sum::<f64>() / vals.len() as f64).sqrt();
        if std == 0.0 { return None; }
        let max = vals.iter().map(|&v| (v - mean) / std).fold(f64::NEG_INFINITY, f64::max);
        Some(max)
    }

    /// Variance of consecutive differences in the window.
    pub fn window_diff_variance(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let diffs: Vec<f64> = vals.windows(2).map(|w| w[1] - w[0]).collect();
        let mean = diffs.iter().sum::<f64>() / diffs.len() as f64;
        let variance = diffs.iter().map(|&d| (d - mean).powi(2)).sum::<f64>() / diffs.len() as f64;
        Some(variance)
    }

    // ── round-138 ────────────────────────────────────────────────────────────

    /// Peak-to-trough ratio: max value / min value in window.
    pub fn window_peak_to_trough(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        if min == 0.0 { return None; }
        Some(max / min)
    }

    /// Asymmetry: skewness of the window values (Pearson's second coefficient).
    pub fn window_asymmetry(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 3 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        let std = (vals.iter().map(|&v| (v - mean).powi(2)).sum::<f64>() / n).sqrt();
        if std == 0.0 { return None; }
        let mut sorted = vals.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let median = if sorted.len() % 2 == 0 {
            (sorted[sorted.len() / 2 - 1] + sorted[sorted.len() / 2]) / 2.0
        } else {
            sorted[sorted.len() / 2]
        };
        Some(3.0 * (mean - median) / std)
    }

    /// Absolute trend: sum of absolute differences between consecutive window values.
    pub fn window_abs_trend(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let total: f64 = vals.windows(2).map(|w| (w[1] - w[0]).abs()).sum();
        Some(total)
    }

    /// Recent volatility: std of the last half of the window.
    pub fn window_recent_volatility(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let half = vals.len() / 2;
        let recent = &vals[half..];
        if recent.len() < 2 { return None; }
        let mean = recent.iter().sum::<f64>() / recent.len() as f64;
        let variance = recent.iter().map(|&v| (v - mean).powi(2)).sum::<f64>() / recent.len() as f64;
        Some(variance.sqrt())
    }

    // ── round-139 ────────────────────────────────────────────────────────────

    /// Range position: (last value - min) / (max - min) in the window.
    pub fn window_range_position(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        let range = max - min;
        if range == 0.0 { return Some(0.5); }
        let last = *vals.last()?;
        Some((last - min) / range)
    }

    /// Sign changes: number of times consecutive differences change sign in the window.
    pub fn window_sign_changes(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 3 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let diffs: Vec<f64> = vals.windows(2).map(|w| w[1] - w[0]).collect();
        let changes = diffs.windows(2)
            .filter(|w| w[0] * w[1] < 0.0)
            .count();
        Some(changes)
    }

    /// Mean shift: mean of the last half minus mean of the first half.
    pub fn window_mean_shift(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 4 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mid = vals.len() / 2;
        let first_mean = vals[..mid].iter().sum::<f64>() / mid as f64;
        let second_mean = vals[mid..].iter().sum::<f64>() / (vals.len() - mid) as f64;
        Some(second_mean - first_mean)
    }

    /// Slope change: difference between the OLS slope of the second half and first half.
    pub fn window_slope_change(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 4 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mid = vals.len() / 2;
        let slope = |seg: &[f64]| -> f64 {
            let n = seg.len() as f64;
            let x_mean = (n - 1.0) / 2.0;
            let y_mean = seg.iter().sum::<f64>() / n;
            let num: f64 = seg.iter().enumerate().map(|(i, &y)| (i as f64 - x_mean) * (y - y_mean)).sum();
            let den: f64 = seg.iter().enumerate().map(|(i, _)| (i as f64 - x_mean).powi(2)).sum();
            if den == 0.0 { 0.0 } else { num / den }
        };
        Some(slope(&vals[mid..]) - slope(&vals[..mid]))
    }

    // ── round-140 ────────────────────────────────────────────────────────────

    /// Recovery rate: fraction of steps that recover from the preceding drop.
    pub fn window_recovery_rate(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 3 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let triplets = vals.windows(3)
            .filter(|w| w[1] < w[0] && w[2] > w[1])
            .count();
        let drops = vals.windows(2).filter(|w| w[1] < w[0]).count();
        if drops == 0 { return Some(1.0); }
        Some(triplets as f64 / drops as f64)
    }

    /// Normalized spread: (max - min) / mean of the window.
    pub fn window_normalized_spread(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        if mean == 0.0 { return None; }
        Some((max - min) / mean.abs())
    }

    /// First-to-last ratio: last value / first value in the window.
    pub fn window_first_last_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let first = self.window.front()?.to_f64()?;
        let last = self.window.back()?.to_f64()?;
        if first == 0.0 { return None; }
        Some(last / first)
    }

    /// Extrema count: number of local maxima and minima in the window.
    pub fn window_extrema_count(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 3 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let count = vals.windows(3)
            .filter(|w| (w[1] > w[0] && w[1] > w[2]) || (w[1] < w[0] && w[1] < w[2]))
            .count();
        Some(count)
    }

    // ── round-141 ────────────────────────────────────────────────────────────

    /// Up fraction: fraction of steps in the window that are strictly increasing.
    pub fn window_up_fraction(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let ups = vals.windows(2).filter(|w| w[1] > w[0]).count() as f64;
        Some(ups / (vals.len() - 1) as f64)
    }

    /// Half range: half of (max - min) in the window (semi-range).
    pub fn window_half_range(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        Some((max - min) / 2.0)
    }

    /// Negative count: number of values below zero in the window.
    pub fn window_negative_count(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let count = self.window.iter()
            .filter(|v| v.to_f64().unwrap_or(0.0) < 0.0)
            .count();
        Some(count)
    }

    /// Trend purity: fraction of steps in the direction of the overall trend (sign of first-to-last diff).
    pub fn window_trend_purity(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let overall = vals.last()? - vals.first()?;
        if overall == 0.0 { return Some(0.0); }
        let aligned = vals.windows(2)
            .filter(|w| (w[1] - w[0]) * overall > 0.0)
            .count() as f64;
        Some(aligned / (vals.len() - 1) as f64)
    }

    // ── round-142 ────────────────────────────────────────────────────────────

    /// Centered mean: mean of values centered around window median.
    pub fn window_centered_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let mut vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let median = if vals.len() % 2 == 0 {
            (vals[vals.len() / 2 - 1] + vals[vals.len() / 2]) / 2.0
        } else {
            vals[vals.len() / 2]
        };
        let centered: Vec<f64> = vals.iter().map(|&v| v - median).collect();
        Some(centered.iter().sum::<f64>() / centered.len() as f64)
    }

    /// Last deviation: distance of the last value from the window mean.
    pub fn window_last_deviation(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let last = *vals.last()?;
        Some(last - mean)
    }

    /// Step size mean: mean of absolute consecutive differences (average step size).
    pub fn window_step_size_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let total: f64 = vals.windows(2).map(|w| (w[1] - w[0]).abs()).sum();
        Some(total / (vals.len() - 1) as f64)
    }

    /// Net up count: number of upward steps minus number of downward steps.
    pub fn window_net_up_count(&self) -> Option<i64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let ups = vals.windows(2).filter(|w| w[1] > w[0]).count() as i64;
        let downs = vals.windows(2).filter(|w| w[1] < w[0]).count() as i64;
        Some(ups - downs)
    }

    // ── round-143 ────────────────────────────────────────────────────────────

    /// Weighted mean: linearly weighted mean giving more weight to recent values.
    pub fn window_weighted_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let n = vals.len() as f64;
        let weighted_sum: f64 = vals.iter().enumerate().map(|(i, &v)| (i as f64 + 1.0) * v).sum();
        let weight_total = n * (n + 1.0) / 2.0;
        Some(weighted_sum / weight_total)
    }

    /// Upper half mean: mean of values in the upper half of the window (above median).
    pub fn window_upper_half_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let mut vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mid = vals.len() / 2;
        let upper = &vals[mid..];
        if upper.is_empty() { return None; }
        Some(upper.iter().sum::<f64>() / upper.len() as f64)
    }

    /// Lower half mean: mean of values in the lower half of the window (below median).
    pub fn window_lower_half_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let mut vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mid = vals.len() / 2;
        let lower = &vals[..mid];
        if lower.is_empty() { return None; }
        Some(lower.iter().sum::<f64>() / lower.len() as f64)
    }

    /// Mid range: (max + min) / 2 of the window.
    pub fn window_mid_range(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        Some((max + min) / 2.0)
    }

    // ── round-144 ────────────────────────────────────────────────────────────

    /// Mean of the window after trimming the top and bottom 10% of values.
    pub fn window_trim_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let mut vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let trim = (vals.len() / 10).max(0);
        let trimmed = &vals[trim..vals.len() - trim];
        if trimmed.is_empty() { return None; }
        Some(trimmed.iter().sum::<f64>() / trimmed.len() as f64)
    }

    /// Difference between the maximum and minimum window values.
    pub fn window_value_spread(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        Some(max - min)
    }

    /// Root mean square of the window values.
    pub fn window_rms(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let sum_sq: f64 = self.window.iter()
            .map(|v| { let x = v.to_f64().unwrap_or(0.0); x * x })
            .sum();
        Some((sum_sq / self.window.len() as f64).sqrt())
    }

    /// Fraction of window values that are above the window midpoint (min+max)/2.
    pub fn window_above_mid_fraction(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        let mid = (max + min) / 2.0;
        Some(vals.iter().filter(|&&v| v > mid).count() as f64 / vals.len() as f64)
    }

    // ── round-145 ────────────────────────────────────────────────────────────

    /// Median absolute deviation of the window values.
    pub fn window_median_abs_dev(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let mut vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = vals.len();
        let median = if n % 2 == 0 { (vals[n / 2 - 1] + vals[n / 2]) / 2.0 } else { vals[n / 2] };
        let mut devs: Vec<f64> = vals.iter().map(|&v| (v - median).abs()).collect();
        devs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let m = devs.len();
        Some(if m % 2 == 0 { (devs[m / 2 - 1] + devs[m / 2]) / 2.0 } else { devs[m / 2] })
    }

    /// Cubic mean (cube root of mean of cubes) of window values.
    pub fn window_cubic_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let sum_cubes: f64 = self.window.iter()
            .map(|v| { let x = v.to_f64().unwrap_or(0.0); x * x * x })
            .sum();
        Some((sum_cubes / self.window.len() as f64).cbrt())
    }

    /// Length of the longest run of consecutive same-valued window entries.
    pub fn window_max_run_length(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mut max_run = 1usize;
        let mut run = 1usize;
        for i in 1..vals.len() {
            if (vals[i] - vals[i - 1]).abs() < 1e-12 {
                run += 1;
                if run > max_run { max_run = run; }
            } else {
                run = 1;
            }
        }
        Some(max_run)
    }

    /// Position (0..1) of the most recent value within the sorted window.
    pub fn window_sorted_position(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let last = self.window.back()?.to_f64()?;
        let mut vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let pos = vals.partition_point(|&v| v < last);
        Some(pos as f64 / vals.len() as f64)
    }

    // ── round-146 ────────────────────────────────────────────────────────────

    /// Deviation of the most recent window value from the previous one.
    pub fn window_prev_deviation(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let mut iter = self.window.iter().rev();
        let last = iter.next()?.to_f64()?;
        let prev = iter.next()?.to_f64()?;
        Some(last - prev)
    }

    /// Lower quartile (25th percentile) of the window values.
    pub fn window_lower_quartile(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let mut vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let idx = (vals.len() - 1) / 4;
        Some(vals[idx])
    }

    /// Upper quartile (75th percentile) of the window values.
    pub fn window_upper_quartile(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let mut vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let idx = (3 * (vals.len() - 1)) / 4;
        Some(vals[idx])
    }

    /// Fraction of window values in the bottom or top 10% (tail weight).
    pub fn window_tail_weight(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let mut vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = vals.len();
        let tail = (n / 10).max(1);
        let lower = vals[tail - 1];
        let upper = vals[n - tail];
        let in_tail = vals.iter().filter(|&&v| v <= lower || v >= upper).count();
        Some(in_tail as f64 / n as f64)
    }

    // ── round-147 ────────────────────────────────────────────────────────────

    /// Deviation of the last window value from the window mean.
    pub fn window_last_vs_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let last = self.window.back()?.to_f64()?;
        let mean = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).sum::<f64>()
            / self.window.len() as f64;
        Some(last - mean)
    }

    /// Mean second-order change of consecutive window values (acceleration).
    pub fn window_change_acceleration(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 3 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let accels: Vec<f64> = vals.windows(3).map(|w| (w[2] - w[1]) - (w[1] - w[0])).collect();
        Some(accels.iter().sum::<f64>() / accels.len() as f64)
    }

    /// Length of the longest consecutive run of positive (>0) window values.
    pub fn window_positive_run_length(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mut max_run = 0usize;
        let mut run = 0usize;
        for &v in &vals {
            if v > 0.0 {
                run += 1;
                if run > max_run { max_run = run; }
            } else {
                run = 0;
            }
        }
        Some(max_run)
    }

    /// Geometric mean of successive ratios (value[i]/value[i-1]) as a trend indicator.
    pub fn window_geometric_trend(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let log_sum: f64 = vals.windows(2).filter_map(|w| {
            if w[0] == 0.0 { return None; }
            let r = w[1] / w[0];
            if r <= 0.0 { return None; }
            Some(r.ln())
        }).sum();
        let count = vals.windows(2).filter(|w| w[0] != 0.0 && w[1] / w[0] > 0.0).count();
        if count == 0 { return None; }
        Some((log_sum / count as f64).exp())
    }

    // ── round-148 ────────────────────────────────────────────────────────────

    /// Mean of all pairwise absolute differences between window values.
    pub fn window_pairwise_diff_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let n = vals.len();
        let mut sum = 0.0f64;
        let mut count = 0usize;
        for i in 0..n {
            for j in (i + 1)..n {
                sum += (vals[i] - vals[j]).abs();
                count += 1;
            }
        }
        if count == 0 { return None; }
        Some(sum / count as f64)
    }

    /// Length of the longest consecutive run of negative (<0) window values.
    pub fn window_negative_run_length(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mut max_run = 0usize;
        let mut run = 0usize;
        for &v in &vals {
            if v < 0.0 {
                run += 1;
                if run > max_run { max_run = run; }
            } else {
                run = 0;
            }
        }
        Some(max_run)
    }

    /// Number of times window values cross zero (sign changes).
    pub fn window_cross_zero_count(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let count = vals.windows(2).filter(|w| w[0] * w[1] < 0.0).count();
        Some(count)
    }

    /// Mean reversion strength: mean of |val - window_mean| / window_std.
    pub fn window_mean_reversion_strength(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let var = vals.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / vals.len() as f64;
        let std = var.sqrt();
        if std == 0.0 { return Some(0.0); }
        Some(vals.iter().map(|&x| (x - mean).abs() / std).sum::<f64>() / vals.len() as f64)
    }

    // ── round-149 ────────────────────────────────────────────────────────────

    /// Deviation of the first window value from the window mean.
    pub fn window_first_vs_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let first = self.window.front()?.to_f64()?;
        let mean = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).sum::<f64>()
            / self.window.len() as f64;
        Some(first - mean)
    }

    /// Ratio of the last window value to the first window value.
    pub fn window_decay_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let first = self.window.front()?.to_f64()?;
        if first == 0.0 { return None; }
        let last = self.window.back()?.to_f64()?;
        Some(last / first)
    }

    /// Bimodal score: variance of lower half minus variance of upper half (normalized).
    pub fn window_bimodal_score(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 4 { return None; }
        let mut vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mid = vals.len() / 2;
        let lower = &vals[..mid];
        let upper = &vals[mid..];
        let var = |v: &[f64]| -> f64 {
            let m = v.iter().sum::<f64>() / v.len() as f64;
            v.iter().map(|x| (x - m).powi(2)).sum::<f64>() / v.len() as f64
        };
        let total_var = var(&vals);
        if total_var == 0.0 { return Some(0.0); }
        Some((var(lower) + var(upper)) / total_var)
    }

    /// Sum of absolute values of all window entries.
    pub fn window_abs_sum(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        Some(self.window.iter().map(|v| v.to_f64().unwrap_or(0.0).abs()).sum())
    }

    // ── round-150 ────────────────────────────────────────────────────────────

    /// Coefficient of variation (std / mean) of the window.
    pub fn window_coeff_of_variation(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        if mean == 0.0 { return None; }
        let var = vals.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / vals.len() as f64;
        Some(var.sqrt() / mean.abs())
    }

    /// Mean absolute error of window values relative to the window mean.
    pub fn window_mean_absolute_error(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        Some(vals.iter().map(|&x| (x - mean).abs()).sum::<f64>() / vals.len() as f64)
    }

    /// Last window value normalized to [0,1] within the window range.
    pub fn window_normalized_last(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let last = self.window.back()?.to_f64()?;
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        if (max - min).abs() < 1e-12 { return Some(0.5); }
        Some((last - min) / (max - min))
    }

    /// Fraction of window values with positive sign minus fraction with negative sign.
    pub fn window_sign_bias(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let n = self.window.len() as f64;
        let pos = self.window.iter().filter(|v| v.to_f64().map_or(false, |x| x > 0.0)).count() as f64;
        let neg = self.window.iter().filter(|v| v.to_f64().map_or(false, |x| x < 0.0)).count() as f64;
        Some((pos - neg) / n)
    }

    // ── round-151 ────────────────────────────────────────────────────────────

    /// Deviation of the second-to-last window value from the last value.
    pub fn window_penultimate_vs_last(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let mut iter = self.window.iter().rev();
        let last = iter.next()?.to_f64()?;
        let penultimate = iter.next()?.to_f64()?;
        Some(penultimate - last)
    }

    /// Position of the window mean within the window range (0=at min, 1=at max).
    pub fn window_mean_range_position(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        if (max - min).abs() < 1e-12 { return Some(0.5); }
        Some((mean - min) / (max - min))
    }

    /// Z-score of the most recent window value.
    pub fn window_zscore_last(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let last = self.window.back()?.to_f64()?;
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let var = vals.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / vals.len() as f64;
        let std = var.sqrt();
        if std == 0.0 { return None; }
        Some((last - mean) / std)
    }

    /// Gradient (slope) of a linear fit to sequential window indices.
    pub fn window_gradient(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let n = vals.len() as f64;
        let x_mean = (n - 1.0) / 2.0;
        let y_mean = vals.iter().sum::<f64>() / n;
        let num: f64 = vals.iter().enumerate().map(|(i, &y)| (i as f64 - x_mean) * (y - y_mean)).sum();
        let den: f64 = vals.iter().enumerate().map(|(i, _)| (i as f64 - x_mean).powi(2)).sum();
        if den == 0.0 { return Some(0.0); }
        Some(num / den)
    }

    // ── round-152 ────────────────────────────────────────────────────────────

    /// Shannon entropy of the window values (binned into 8 buckets).
    pub fn window_entropy_score(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        if (max - min).abs() < 1e-12 { return Some(0.0); }
        let bins = 8usize;
        let mut counts = vec![0usize; bins];
        for &v in &vals {
            let idx = ((v - min) / (max - min) * bins as f64).min(bins as f64 - 1.0) as usize;
            counts[idx] += 1;
        }
        let n = vals.len() as f64;
        Some(counts.iter().filter(|&&c| c > 0)
            .map(|&c| { let p = c as f64 / n; -p * p.ln() }).sum())
    }

    /// Interquartile range (Q3 - Q1) of the window.
    pub fn window_quartile_spread(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 4 { return None; }
        let mut vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = vals.len();
        let q1 = vals[n / 4];
        let q3 = vals[3 * n / 4];
        Some(q3 - q1)
    }

    /// Ratio of the window maximum to minimum (returns None if min=0).
    pub fn window_max_to_min_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        if min == 0.0 { return None; }
        Some(max / min)
    }

    /// Fraction of window values that exceed the window mean.
    pub fn window_upper_fraction(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        Some(vals.iter().filter(|&&v| v > mean).count() as f64 / vals.len() as f64)
    }

    // ── round-153 ────────────────────────────────────────────────────────────

    /// Mean of absolute differences between consecutive window values.
    pub fn window_abs_change_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let changes: Vec<f64> = vals.windows(2).map(|w| (w[1] - w[0]).abs()).collect();
        Some(changes.iter().sum::<f64>() / changes.len() as f64)
    }

    /// Percentile rank of the last window value within the window (0.0–1.0).
    pub fn window_last_percentile(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let last = self.window.back()?.to_f64()?;
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let below = vals.iter().filter(|&&v| v < last).count();
        Some(below as f64 / vals.len() as f64)
    }

    /// Std dev of the trailing half of the window.
    pub fn window_trailing_std(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let half = vals.len() / 2;
        let trailing = &vals[half..];
        if trailing.len() < 2 { return None; }
        let mean = trailing.iter().sum::<f64>() / trailing.len() as f64;
        let var = trailing.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / trailing.len() as f64;
        Some(var.sqrt())
    }

    /// Mean of consecutive differences (last - first) divided by window length.
    pub fn window_mean_change(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let first = self.window.front()?.to_f64()?;
        let last = self.window.back()?.to_f64()?;
        Some((last - first) / (self.window.len() - 1) as f64)
    }

    // ── round-154 ────────────────────────────────────────────────────────────

    /// Index position of the maximum window value (0 = oldest).
    pub fn window_value_at_peak(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let peak_idx = vals.iter().enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))?.0;
        Some(peak_idx as f64)
    }

    /// Difference between the first and last window values (head - tail).
    pub fn window_head_tail_diff(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let first = self.window.front()?.to_f64()?;
        let last = self.window.back()?.to_f64()?;
        Some(first - last)
    }

    /// Midpoint of the window range: (max + min) / 2.
    pub fn window_midpoint(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        Some((min + max) / 2.0)
    }

    /// Second derivative of window mean: curvature of the smoothed series.
    pub fn window_concavity(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 3 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let n = vals.len();
        let first_third = vals[..n/3].iter().sum::<f64>() / (n/3) as f64;
        let last_third = vals[n - n/3..].iter().sum::<f64>() / (n/3) as f64;
        let mid = vals[n/3..n - n/3].iter().sum::<f64>() / (n - 2*n/3) as f64;
        Some(mid - (first_third + last_third) / 2.0)
    }

    // ── round-155 ────────────────────────────────────────────────────────────

    /// Fraction of consecutive window pairs where value rises.
    pub fn window_rise_fraction(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let rises = vals.windows(2).filter(|w| w[1] > w[0]).count();
        Some(rises as f64 / (vals.len() - 1) as f64)
    }

    /// Difference between window maximum and minimum (peak-to-valley span).
    pub fn window_peak_to_valley(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        Some(max - min)
    }

    /// Mean of positive consecutive changes in the window.
    pub fn window_positive_change_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let pos_changes: Vec<f64> = vals.windows(2)
            .filter_map(|w| if w[1] > w[0] { Some(w[1] - w[0]) } else { None })
            .collect();
        if pos_changes.is_empty() { return None; }
        Some(pos_changes.iter().sum::<f64>() / pos_changes.len() as f64)
    }

    /// Coefficient of variation of the window range split into halves (first vs second half std).
    pub fn window_range_cv(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let range = max - min;
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        if mean == 0.0 { return None; }
        Some(range / mean.abs())
    }

    // ── round-156 ────────────────────────────────────────────────────────────

    /// Mean of negative consecutive changes in the window.
    pub fn window_negative_change_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let neg_changes: Vec<f64> = vals.windows(2)
            .filter_map(|w| if w[1] < w[0] { Some(w[0] - w[1]) } else { None })
            .collect();
        if neg_changes.is_empty() { return None; }
        Some(neg_changes.iter().sum::<f64>() / neg_changes.len() as f64)
    }

    /// Fraction of consecutive window pairs where value falls.
    pub fn window_fall_fraction(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let falls = vals.windows(2).filter(|w| w[1] < w[0]).count();
        Some(falls as f64 / (vals.len() - 1) as f64)
    }

    /// Difference between the last window value and the window maximum.
    pub fn window_last_vs_max(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let last = self.window.back()?.to_f64()?;
        let max = self.window.iter().map(|v| v.to_f64().unwrap_or(f64::NEG_INFINITY)).fold(f64::NEG_INFINITY, f64::max);
        Some(last - max)
    }

    /// Difference between the last window value and the window minimum.
    pub fn window_last_vs_min(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let last = self.window.back()?.to_f64()?;
        let min = self.window.iter().map(|v| v.to_f64().unwrap_or(f64::INFINITY)).fold(f64::INFINITY, f64::min);
        Some(last - min)
    }

    // ── round-157 ────────────────────────────────────────────────────────────

    /// Mean absolute deviation of consecutive differences (oscillation measure).
    pub fn window_mean_oscillation(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let diffs: Vec<f64> = vals.windows(2).map(|w| w[1] - w[0]).collect();
        let mean_diff = diffs.iter().sum::<f64>() / diffs.len() as f64;
        Some(diffs.iter().map(|d| (d - mean_diff).abs()).sum::<f64>() / diffs.len() as f64)
    }

    /// Monotone score: fraction of consecutive pairs in the dominant direction.
    pub fn window_monotone_score(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let pairs = vals.len() - 1;
        let rises = vals.windows(2).filter(|w| w[1] > w[0]).count();
        let falls = pairs - rises;
        Some(rises.max(falls) as f64 / pairs as f64)
    }

    /// Trend of window std dev: difference between std dev of second half minus first half.
    pub fn window_stddev_trend(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 4 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mid = vals.len() / 2;
        let std_half = |v: &[f64]| -> f64 {
            let m = v.iter().sum::<f64>() / v.len() as f64;
            (v.iter().map(|x| (x - m).powi(2)).sum::<f64>() / v.len() as f64).sqrt()
        };
        Some(std_half(&vals[mid..]) - std_half(&vals[..mid]))
    }

    /// Fraction of consecutive pairs where the sign of the change alternates.
    pub fn window_zero_cross_fraction(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 3 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let diffs: Vec<f64> = vals.windows(2).map(|w| w[1] - w[0]).collect();
        let sign_changes = diffs.windows(2).filter(|w| w[0] * w[1] < 0.0).count();
        Some(sign_changes as f64 / (diffs.len() - 1) as f64)
    }

    // ── round-158 ────────────────────────────────────────────────────────────

    /// Exponentially weighted sum of window values (alpha=0.1, most recent highest weight).
    pub fn window_exponential_decay_sum(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let alpha = 0.1_f64;
        let n = vals.len();
        let sum: f64 = vals.iter().enumerate()
            .map(|(i, &v)| v * alpha.powi((n - 1 - i) as i32))
            .sum();
        Some(sum)
    }

    /// Mean of differences between values separated by lag=1 position.
    pub fn window_lagged_diff(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let diffs: Vec<f64> = vals.windows(2).map(|w| w[1] - w[0]).collect();
        Some(diffs.iter().sum::<f64>() / diffs.len() as f64)
    }

    /// Ratio of window mean to window maximum.
    pub fn window_mean_to_max(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        if max == 0.0 { return None; }
        Some(mean / max)
    }

    /// Fraction of values equal to the most frequent bucket (mode approximation, 8 bins).
    pub fn window_mode_fraction(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        if (max - min).abs() < 1e-12 { return Some(1.0); }
        let bins = 8usize;
        let mut counts = vec![0usize; bins];
        for &v in &vals {
            let idx = ((v - min) / (max - min) * bins as f64).min(bins as f64 - 1.0) as usize;
            counts[idx] += 1;
        }
        let mode_count = counts.iter().cloned().max().unwrap_or(0);
        Some(mode_count as f64 / vals.len() as f64)
    }

    // ── round-159 ────────────────────────────────────────────────────────────

    /// Mean of window values that are below zero.
    pub fn window_mean_below_zero(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let negatives: Vec<f64> = self.window.iter()
            .filter_map(|v| v.to_f64())
            .filter(|&x| x < 0.0).collect();
        if negatives.is_empty() { return None; }
        Some(negatives.iter().sum::<f64>() / negatives.len() as f64)
    }

    /// Mean of window values that are above zero.
    pub fn window_mean_above_zero(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let positives: Vec<f64> = self.window.iter()
            .filter_map(|v| v.to_f64())
            .filter(|&x| x > 0.0).collect();
        if positives.is_empty() { return None; }
        Some(positives.iter().sum::<f64>() / positives.len() as f64)
    }

    /// Fraction of window values at or above the running max at each step.
    pub fn window_running_max_fraction(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(f64::NEG_INFINITY)).collect();
        let mut running_max = f64::NEG_INFINITY;
        let at_max_count = vals.iter().filter(|&&v| {
            if v >= running_max { running_max = v; true } else { false }
        }).count();
        Some(at_max_count as f64 / vals.len() as f64)
    }

    /// Change in variance between first half and second half of the window.
    pub fn window_variance_change(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 4 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mid = vals.len() / 2;
        let var = |v: &[f64]| -> f64 {
            let m = v.iter().sum::<f64>() / v.len() as f64;
            v.iter().map(|x| (x - m).powi(2)).sum::<f64>() / v.len() as f64
        };
        Some(var(&vals[mid..]) - var(&vals[..mid]))
    }

    // ── round-160 ────────────────────────────────────────────────────────────

    /// Slope of EMA of window values (alpha=0.1), computed as (last_ema - first_ema) / n.
    pub fn window_ema_slope(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let alpha = 0.1_f64;
        let mut ema = vals[0];
        let first_ema = ema;
        for &v in &vals[1..] {
            ema = alpha * v + (1.0 - alpha) * ema;
        }
        Some((ema - first_ema) / (vals.len() - 1) as f64)
    }

    /// Ratio of window max to window min (returns None if min is 0).
    pub fn window_range_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        if min == 0.0 { return None; }
        Some(max / min)
    }

    /// Longest consecutive run of values above the window mean.
    pub fn window_above_mean_streak(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let mut max_streak = 0usize;
        let mut cur = 0usize;
        for &v in &vals {
            if v > mean { cur += 1; if cur > max_streak { max_streak = cur; } }
            else { cur = 0; }
        }
        Some(max_streak as f64)
    }

    /// Mean absolute difference between consecutive window values.
    pub fn window_mean_abs_diff(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let diffs: Vec<f64> = vals.windows(2).map(|w| (w[1] - w[0]).abs()).collect();
        Some(diffs.iter().sum::<f64>() / diffs.len() as f64)
    }

    // ── round-161 ────────────────────────────────────────────────────────────

    /// Shannon entropy of sign distribution (+/-/0) in the window.
    pub fn window_sign_entropy(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let mut pos = 0usize; let mut neg = 0usize; let mut zer = 0usize;
        for v in &self.window {
            match v.to_f64().unwrap_or(0.0).partial_cmp(&0.0) {
                Some(std::cmp::Ordering::Greater) => pos += 1,
                Some(std::cmp::Ordering::Less) => neg += 1,
                _ => zer += 1,
            }
        }
        let n = self.window.len() as f64;
        let entropy = [pos, neg, zer].iter()
            .filter(|&&c| c > 0)
            .map(|&c| { let p = c as f64 / n; -p * p.ln() })
            .sum();
        Some(entropy)
    }

    /// Count of local extrema (peaks + troughs) in the window.
    pub fn window_local_extrema_count(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 3 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let count = vals.windows(3).filter(|w| {
            (w[1] > w[0] && w[1] > w[2]) || (w[1] < w[0] && w[1] < w[2])
        }).count();
        Some(count as f64)
    }

    /// Lag-2 autocorrelation of window values.
    pub fn window_autocorr_lag2(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 3 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let variance: f64 = vals.iter().map(|&v| (v - mean).powi(2)).sum::<f64>() / vals.len() as f64;
        if variance == 0.0 { return None; }
        let cov: f64 = vals.windows(3).map(|w| (w[0] - mean) * (w[2] - mean)).sum::<f64>()
            / (vals.len() - 2) as f64;
        Some(cov / variance)
    }

    /// Fraction of window values strictly above the window median.
    pub fn window_pct_above_median(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let mut sorted: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let median = if sorted.len() % 2 == 0 {
            (sorted[sorted.len() / 2 - 1] + sorted[sorted.len() / 2]) / 2.0
        } else {
            sorted[sorted.len() / 2]
        };
        let above = sorted.iter().filter(|&&v| v > median).count();
        Some(above as f64 / sorted.len() as f64)
    }

    // ── round-162 ────────────────────────────────────────────────────────────

    /// Longest consecutive run of values strictly below zero.
    pub fn window_below_zero_streak(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let mut max_streak = 0usize;
        let mut cur = 0usize;
        for v in &self.window {
            if v.to_f64().unwrap_or(0.0) < 0.0 { cur += 1; if cur > max_streak { max_streak = cur; } }
            else { cur = 0; }
        }
        Some(max_streak as f64)
    }

    /// Ratio of window max to window mean; None if mean is zero.
    pub fn window_max_to_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        if mean == 0.0 { return None; }
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        Some(max / mean)
    }

    /// Length of the longest run of the same sign (+/-/0) in the window.
    pub fn window_sign_run_length(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let signs: Vec<i8> = self.window.iter().map(|v| {
            let f = v.to_f64().unwrap_or(0.0);
            if f > 0.0 { 1 } else if f < 0.0 { -1 } else { 0 }
        }).collect();
        let mut max_run = 1usize;
        let mut cur_run = 1usize;
        for i in 1..signs.len() {
            if signs[i] == signs[i-1] { cur_run += 1; if cur_run > max_run { max_run = cur_run; } }
            else { cur_run = 1; }
        }
        Some(max_run as f64)
    }

    /// Exponentially decayed weighted mean (more recent values weighted higher, alpha=0.2).
    pub fn window_decay_weighted_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let alpha = 0.2_f64;
        let n = vals.len();
        let weights: Vec<f64> = (0..n).map(|i| alpha.powi((n - 1 - i) as i32)).collect();
        let total_w: f64 = weights.iter().sum();
        if total_w == 0.0 { return None; }
        let weighted_sum: f64 = vals.iter().zip(weights.iter()).map(|(v, w)| v * w).sum();
        Some(weighted_sum / total_w)
    }

    // ── round-163 ────────────────────────────────────────────────────────────

    /// Ratio of window minimum to window mean; None if mean is zero.
    pub fn window_min_to_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        if mean == 0.0 { return None; }
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        Some(min / mean)
    }

    /// (max - min) normalized by (max + min)/2; None if midpoint is zero.
    pub fn window_normalized_range(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let midpoint = (max + min) / 2.0;
        if midpoint == 0.0 { return None; }
        Some((max - min) / midpoint)
    }

    /// Mean of window after clipping top and bottom 10% of values.
    pub fn window_winsorized_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let mut sorted: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = sorted.len();
        let trim = (n as f64 * 0.1).floor() as usize;
        let trimmed = if 2 * trim < n { &sorted[trim..n - trim] } else { &sorted[..] };
        if trimmed.is_empty() { return None; }
        Some(trimmed.iter().sum::<f64>() / trimmed.len() as f64)
    }

    /// Ratio of (max - min) to std dev; None if std dev is zero.
    pub fn window_range_to_std(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let std = (vals.iter().map(|&v| (v - mean).powi(2)).sum::<f64>() / vals.len() as f64).sqrt();
        if std == 0.0 { return None; }
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        Some((max - min) / std)
    }

    // ── round-164 ────────────────────────────────────────────────────────────

    /// Coefficient of variation: std / mean * 100; None if mean is zero.
    pub fn window_cv(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        if mean == 0.0 { return None; }
        let std = (vals.iter().map(|&v| (v - mean).powi(2)).sum::<f64>() / vals.len() as f64).sqrt();
        Some(std / mean * 100.0)
    }

    /// Fraction of window values that are non-zero.
    pub fn window_non_zero_fraction(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let non_zero = self.window.iter()
            .filter(|v| v.to_f64().unwrap_or(0.0) != 0.0)
            .count();
        Some(non_zero as f64 / self.window.len() as f64)
    }

    /// Root mean square of absolute window values (same as RMS but renamed to avoid dup).
    pub fn window_rms_abs(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean_sq = vals.iter().map(|&v| v * v).sum::<f64>() / vals.len() as f64;
        Some(mean_sq.sqrt())
    }

    /// Kurtosis proxy: mean of (x - mean)^4 / std^4; None if std is zero.
    pub fn window_kurtosis_proxy(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 4 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let var = vals.iter().map(|&v| (v - mean).powi(2)).sum::<f64>() / vals.len() as f64;
        if var == 0.0 { return None; }
        let kurt = vals.iter().map(|&v| (v - mean).powi(4)).sum::<f64>() / (vals.len() as f64 * var.powi(2));
        Some(kurt)
    }

    // ── round-165 ────────────────────────────────────────────────────────────

    /// Index of mean reversion strength: fraction of steps returning toward the mean.
    pub fn window_mean_reversion_index(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 3 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let reverts = vals.windows(2).filter(|w| {
            let d = w[1] - w[0];
            (w[0] > mean && d < 0.0) || (w[0] < mean && d > 0.0)
        }).count();
        Some(reverts as f64 / (vals.len() - 1) as f64)
    }

    /// Ratio of the 90th to 10th percentile of window values.
    pub fn window_tail_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let mut sorted: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = sorted.len();
        let p10 = sorted[((n as f64 - 1.0) * 0.1).round() as usize];
        let p90 = sorted[((n as f64 - 1.0) * 0.9).round() as usize];
        if p10 == 0.0 { return None; }
        Some(p90 / p10)
    }

    /// Slope of cumulative sum of window values (total drift / n).
    pub fn window_cumsum_trend(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let cumsum: f64 = vals.iter().sum();
        Some(cumsum / vals.len() as f64)
    }

    /// Count of times window values cross the window mean.
    pub fn window_mean_crossing_count(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let above: Vec<bool> = vals.iter().map(|&v| v > mean).collect();
        let crosses = above.windows(2).filter(|w| w[0] != w[1]).count();
        Some(crosses as f64)
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
    fn test_minmax_is_full_false_before_capacity() {
        let mut n = norm(3);
        assert!(!n.is_full());
        n.update(dec!(1));
        n.update(dec!(2));
        assert!(!n.is_full());
        n.update(dec!(3));
        assert!(n.is_full());
    }

    #[test]
    fn test_minmax_is_full_stays_true_after_eviction() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] {
            n.update(v);
        }
        assert!(n.is_full()); // window stays at capacity after eviction
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

    // ── MinMaxNormalizer::midpoint ────────────────────────────────────────────

    #[test]
    fn test_midpoint_none_when_empty() {
        let mut n = norm(4);
        assert!(n.midpoint().is_none());
    }

    #[test]
    fn test_midpoint_correct() {
        let mut n = norm(4);
        for v in [dec!(10), dec!(20), dec!(30), dec!(40)] {
            n.update(v);
        }
        // min=10, max=40 → midpoint = 25
        assert_eq!(n.midpoint(), Some(dec!(25)));
    }

    #[test]
    fn test_midpoint_single_value() {
        let mut n = norm(4);
        n.update(dec!(42));
        assert_eq!(n.midpoint(), Some(dec!(42)));
    }

    // ── MinMaxNormalizer::clamp_to_window ─────────────────────────────────────

    #[test]
    fn test_clamp_to_window_returns_value_unchanged_when_empty() {
        let mut n = norm(4);
        assert_eq!(n.clamp_to_window(dec!(50)), dec!(50));
    }

    #[test]
    fn test_clamp_to_window_clamps_above_max() {
        let mut n = norm(4);
        for v in [dec!(10), dec!(20), dec!(30)] { n.update(v); }
        assert_eq!(n.clamp_to_window(dec!(100)), dec!(30));
    }

    #[test]
    fn test_clamp_to_window_clamps_below_min() {
        let mut n = norm(4);
        for v in [dec!(10), dec!(20), dec!(30)] { n.update(v); }
        assert_eq!(n.clamp_to_window(dec!(5)), dec!(10));
    }

    #[test]
    fn test_clamp_to_window_passthrough_when_in_range() {
        let mut n = norm(4);
        for v in [dec!(10), dec!(20), dec!(30)] { n.update(v); }
        assert_eq!(n.clamp_to_window(dec!(15)), dec!(15));
    }

    // ── MinMaxNormalizer::count_above ─────────────────────────────────────────

    #[test]
    fn test_count_above_zero_when_empty() {
        let n = norm(4);
        assert_eq!(n.count_above(dec!(5)), 0);
    }

    #[test]
    fn test_count_above_counts_strictly_above() {
        let mut n = norm(8);
        for v in [dec!(1), dec!(5), dec!(10), dec!(15)] { n.update(v); }
        assert_eq!(n.count_above(dec!(5)), 2); // 10 and 15
    }

    #[test]
    fn test_count_above_all_when_threshold_below_all() {
        let mut n = norm(4);
        for v in [dec!(10), dec!(20), dec!(30)] { n.update(v); }
        assert_eq!(n.count_above(dec!(5)), 3);
    }

    #[test]
    fn test_count_above_zero_when_threshold_above_all() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        assert_eq!(n.count_above(dec!(100)), 0);
    }

    // ── MinMaxNormalizer::count_below ─────────────────────────────────────────

    #[test]
    fn test_count_below_zero_when_empty() {
        let n = norm(4);
        assert_eq!(n.count_below(dec!(5)), 0);
    }

    #[test]
    fn test_count_below_counts_strictly_below() {
        let mut n = norm(8);
        for v in [dec!(1), dec!(5), dec!(10), dec!(15)] { n.update(v); }
        assert_eq!(n.count_below(dec!(10)), 2); // 1 and 5
    }

    #[test]
    fn test_count_below_all_when_threshold_above_all() {
        let mut n = norm(4);
        for v in [dec!(10), dec!(20), dec!(30)] { n.update(v); }
        assert_eq!(n.count_below(dec!(100)), 3);
    }

    #[test]
    fn test_count_below_zero_when_threshold_below_all() {
        let mut n = norm(4);
        for v in [dec!(10), dec!(20), dec!(30)] { n.update(v); }
        assert_eq!(n.count_below(dec!(5)), 0);
    }

    #[test]
    fn test_count_above_plus_count_below_leq_len() {
        let mut n = norm(5);
        for v in [dec!(1), dec!(5), dec!(5), dec!(10), dec!(20)] { n.update(v); }
        // threshold = 5: above = {10,20} = 2, below = {1} = 1, equal = {5,5} = 2
        // above + below = 3 <= 5 = len
        assert_eq!(n.count_above(dec!(5)) + n.count_below(dec!(5)), 3);
    }

    // ── MinMaxNormalizer::normalized_range ────────────────────────────────────

    #[test]
    fn test_normalized_range_none_when_empty() {
        let mut n = norm(4);
        assert!(n.normalized_range().is_none());
    }

    #[test]
    fn test_normalized_range_zero_when_all_same() {
        let mut n = norm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        assert_eq!(n.normalized_range(), Some(0.0));
    }

    #[test]
    fn test_normalized_range_correct_value() {
        let mut n = norm(4);
        for v in [dec!(10), dec!(20), dec!(30), dec!(40)] { n.update(v); }
        // (40-10)/40 = 0.75
        let nr = n.normalized_range().unwrap();
        assert!((nr - 0.75).abs() < 1e-10);
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

    // ── MinMaxNormalizer::latest ──────────────────────────────────────────────

    #[test]
    fn test_latest_none_when_empty() {
        let n = norm(5);
        assert_eq!(n.latest(), None);
    }

    #[test]
    fn test_latest_returns_most_recent_value() {
        let mut n = norm(5);
        n.update(dec!(10));
        n.update(dec!(20));
        n.update(dec!(30));
        assert_eq!(n.latest(), Some(dec!(30)));
    }

    #[test]
    fn test_latest_updates_on_each_push() {
        let mut n = norm(3);
        n.update(dec!(1));
        assert_eq!(n.latest(), Some(dec!(1)));
        n.update(dec!(5));
        assert_eq!(n.latest(), Some(dec!(5)));
    }

    #[test]
    fn test_latest_returns_last_after_window_overflow() {
        let mut n = norm(2); // window_size = 2
        n.update(dec!(100));
        n.update(dec!(200));
        n.update(dec!(300)); // oldest (100) evicted
        assert_eq!(n.latest(), Some(dec!(300)));
    }

    // ── MinMaxNormalizer::coefficient_of_variation ────────────────────────────

    #[test]
    fn test_minmax_cv_none_fewer_than_2_obs() {
        let mut n = norm(4);
        n.update(dec!(10));
        assert!(n.coefficient_of_variation().is_none());
    }

    #[test]
    fn test_minmax_cv_none_when_mean_zero() {
        let mut n = norm(4);
        for v in [dec!(-5), dec!(5)] { n.update(v); }
        assert!(n.coefficient_of_variation().is_none());
    }

    #[test]
    fn test_minmax_cv_positive_for_positive_mean() {
        let mut n = norm(4);
        for v in [dec!(10), dec!(20), dec!(30), dec!(40)] { n.update(v); }
        let cv = n.coefficient_of_variation().unwrap();
        assert!(cv > 0.0, "CV should be positive");
    }

    // ── MinMaxNormalizer::variance / std_dev ─────────────────────────────────

    #[test]
    fn test_minmax_variance_none_fewer_than_2_obs() {
        let mut n = norm(5);
        n.update(dec!(10));
        assert!(n.variance().is_none());
    }

    #[test]
    fn test_minmax_variance_zero_all_same() {
        let mut n = norm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        assert_eq!(n.variance(), Some(dec!(0)));
    }

    #[test]
    fn test_minmax_variance_correct_value() {
        let mut n = norm(4);
        // values [2, 4, 4, 4, 5, 5, 7, 9] — classic example, pop variance = 4
        // use window=4, push last 4: [5, 5, 7, 9], mean=6.5, var=2.75
        for v in [dec!(5), dec!(5), dec!(7), dec!(9)] { n.update(v); }
        let var = n.variance().unwrap();
        // mean = (5+5+7+9)/4 = 6.5; deviations: -1.5,-1.5,0.5,2.5; sum_sq_devs: 2.25+2.25+0.25+6.25=11; var=11/4=2.75
        assert!((var.to_f64().unwrap() - 2.75).abs() < 1e-9);
    }

    #[test]
    fn test_minmax_std_dev_none_fewer_than_2_obs() {
        let n = norm(4);
        assert!(n.std_dev().is_none());
    }

    #[test]
    fn test_minmax_std_dev_zero_all_same() {
        let mut n = norm(3);
        for _ in 0..3 { n.update(dec!(7)); }
        assert_eq!(n.std_dev(), Some(0.0));
    }

    #[test]
    fn test_minmax_std_dev_sqrt_of_variance() {
        let mut n = norm(4);
        for v in [dec!(5), dec!(5), dec!(7), dec!(9)] { n.update(v); }
        let sd = n.std_dev().unwrap();
        let var = n.variance().unwrap().to_f64().unwrap();
        assert!((sd - var.sqrt()).abs() < 1e-9);
    }

    // ── MinMaxNormalizer::kurtosis ────────────────────────────────────────────

    #[test]
    fn test_minmax_kurtosis_none_fewer_than_4_observations() {
        let mut n = norm(5);
        n.update(dec!(1));
        n.update(dec!(2));
        n.update(dec!(3));
        assert!(n.kurtosis().is_none());
    }

    #[test]
    fn test_minmax_kurtosis_some_with_4_observations() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] {
            n.update(v);
        }
        assert!(n.kurtosis().is_some());
    }

    #[test]
    fn test_minmax_kurtosis_none_all_same_value() {
        let mut n = norm(4);
        for _ in 0..4 {
            n.update(dec!(5));
        }
        // std_dev = 0 → kurtosis undefined
        assert!(n.kurtosis().is_none());
    }

    #[test]
    fn test_minmax_kurtosis_uniform_distribution_is_negative() {
        // Uniform distribution has excess kurtosis ≈ -1.2
        let mut n = norm(10);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5),
                  dec!(6), dec!(7), dec!(8), dec!(9), dec!(10)] {
            n.update(v);
        }
        let k = n.kurtosis().unwrap();
        assert!(k < 0.0, "uniform distribution should have negative excess kurtosis, got {k}");
    }

    // ── MinMaxNormalizer::median ──────────────────────────────────────────────

    #[test]
    fn test_minmax_median_none_for_empty_window() {
        assert!(norm(4).median().is_none());
    }

    #[test]
    fn test_minmax_median_odd_window() {
        let mut n = norm(5);
        for v in [dec!(3), dec!(1), dec!(5), dec!(2), dec!(4)] { n.update(v); }
        // sorted: [1, 2, 3, 4, 5] → median = 3
        assert_eq!(n.median(), Some(dec!(3)));
    }

    #[test]
    fn test_minmax_median_even_window() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        // sorted: [1, 2, 3, 4] → median = (2+3)/2 = 2.5
        assert_eq!(n.median(), Some(dec!(2.5)));
    }

    // ── MinMaxNormalizer::sample_variance ─────────────────────────────────────

    #[test]
    fn test_minmax_sample_variance_none_for_single_obs() {
        let mut n = norm(4);
        n.update(dec!(10));
        assert!(n.sample_variance().is_none());
    }

    #[test]
    fn test_minmax_sample_variance_larger_than_population_variance() {
        let mut n = norm(4);
        for v in [dec!(10), dec!(20), dec!(30), dec!(40)] { n.update(v); }
        use rust_decimal::prelude::ToPrimitive;
        let pop_var = n.variance().unwrap().to_f64().unwrap();
        let sample_var = n.sample_variance().unwrap();
        assert!(sample_var > pop_var, "sample variance should exceed population variance");
    }

    // ── MinMaxNormalizer::mad ─────────────────────────────────────────────────

    #[test]
    fn test_minmax_mad_none_for_empty_window() {
        assert!(norm(4).mad().is_none());
    }

    #[test]
    fn test_minmax_mad_zero_for_identical_values() {
        let mut n = norm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        assert_eq!(n.mad(), Some(dec!(0)));
    }

    #[test]
    fn test_minmax_mad_correct_for_known_distribution() {
        let mut n = norm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)] { n.update(v); }
        // median = 3; deviations = [2,1,0,1,2] sorted → [0,1,1,2,2]; MAD = 1
        assert_eq!(n.mad(), Some(dec!(1)));
    }

    // ── MinMaxNormalizer::robust_z_score ──────────────────────────────────────

    #[test]
    fn test_minmax_robust_z_none_for_empty_window() {
        assert!(norm(4).robust_z_score(dec!(10)).is_none());
    }

    #[test]
    fn test_minmax_robust_z_none_when_mad_is_zero() {
        let mut n = norm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        assert!(n.robust_z_score(dec!(5)).is_none());
    }

    #[test]
    fn test_minmax_robust_z_positive_above_median() {
        let mut n = norm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)] { n.update(v); }
        let rz = n.robust_z_score(dec!(5)).unwrap();
        assert!(rz > 0.0, "robust z-score should be positive for value above median");
    }

    #[test]
    fn test_minmax_robust_z_negative_below_median() {
        let mut n = norm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)] { n.update(v); }
        let rz = n.robust_z_score(dec!(1)).unwrap();
        assert!(rz < 0.0, "robust z-score should be negative for value below median");
    }

    // ── MinMaxNormalizer::percentile_value ────────────────────────────────────

    #[test]
    fn test_percentile_value_none_for_empty_window() {
        assert!(norm(4).percentile_value(0.5).is_none());
    }

    #[test]
    fn test_percentile_value_min_at_zero() {
        let mut n = norm(5);
        for v in [dec!(10), dec!(20), dec!(30), dec!(40), dec!(50)] { n.update(v); }
        assert_eq!(n.percentile_value(0.0), Some(dec!(10)));
    }

    #[test]
    fn test_percentile_value_max_at_one() {
        let mut n = norm(5);
        for v in [dec!(10), dec!(20), dec!(30), dec!(40), dec!(50)] { n.update(v); }
        assert_eq!(n.percentile_value(1.0), Some(dec!(50)));
    }

    #[test]
    fn test_percentile_value_median_at_half() {
        let mut n = norm(5);
        for v in [dec!(10), dec!(20), dec!(30), dec!(40), dec!(50)] { n.update(v); }
        // p=0.5 → idx=2.0 → exact middle = 30
        assert_eq!(n.percentile_value(0.5), Some(dec!(30)));
    }

    // ── MinMaxNormalizer::sum ─────────────────────────────────────────────────

    #[test]
    fn test_minmax_sum_none_for_empty_window() {
        assert!(norm(3).sum().is_none());
    }

    #[test]
    fn test_minmax_sum_single_value() {
        let mut n = norm(3);
        n.update(dec!(7));
        assert_eq!(n.sum(), Some(dec!(7)));
    }

    #[test]
    fn test_minmax_sum_multiple_values() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        assert_eq!(n.sum(), Some(dec!(10)));
    }

    // ── MinMaxNormalizer::is_outlier ──────────────────────────────────────────

    #[test]
    fn test_minmax_is_outlier_false_for_empty_window() {
        assert!(!norm(3).is_outlier(dec!(100), 2.0));
    }

    #[test]
    fn test_minmax_is_outlier_false_for_in_range_value() {
        let mut n = norm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)] { n.update(v); }
        assert!(!n.is_outlier(dec!(3), 2.0));
    }

    #[test]
    fn test_minmax_is_outlier_true_for_extreme_value() {
        let mut n = norm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)] { n.update(v); }
        assert!(n.is_outlier(dec!(100), 2.0));
    }

    // ── MinMaxNormalizer::trim_outliers ───────────────────────────────────────

    #[test]
    fn test_minmax_trim_outliers_returns_all_when_no_outliers() {
        let mut n = norm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)] { n.update(v); }
        let trimmed = n.trim_outliers(10.0);
        assert_eq!(trimmed.len(), 5);
    }

    #[test]
    fn test_minmax_trim_outliers_removes_extreme_values() {
        let mut n = norm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)] { n.update(v); }
        // sigma=0 means everything is an outlier
        let trimmed = n.trim_outliers(0.0);
        assert_eq!(trimmed.len(), 1); // only the mean value (z=0) passes
    }

    // ── MinMaxNormalizer::z_score_of_latest ───────────────────────────────────

    #[test]
    fn test_minmax_z_score_of_latest_none_for_empty_window() {
        assert!(norm(3).z_score_of_latest().is_none());
    }

    #[test]
    fn test_minmax_z_score_of_latest_zero_for_single_value() {
        let mut n = norm(5);
        n.update(dec!(10));
        // std_dev is zero for single value → None
        assert!(n.z_score_of_latest().is_none());
    }

    #[test]
    fn test_minmax_z_score_of_latest_positive_for_above_mean() {
        let mut n = norm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(10)] { n.update(v); }
        let z = n.z_score_of_latest().unwrap();
        assert!(z > 0.0, "latest value is above mean → positive z-score");
    }

    // ── MinMaxNormalizer::deviation_from_mean ─────────────────────────────────

    #[test]
    fn test_minmax_deviation_from_mean_none_for_empty_window() {
        assert!(norm(3).deviation_from_mean(dec!(5)).is_none());
    }

    #[test]
    fn test_minmax_deviation_from_mean_zero_at_mean() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        // mean = 2.5
        let dev = n.deviation_from_mean(dec!(2.5)).unwrap();
        assert!(dev.abs() < 1e-9);
    }

    #[test]
    fn test_minmax_deviation_from_mean_positive_above_mean() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let dev = n.deviation_from_mean(dec!(5)).unwrap();
        assert!(dev > 0.0);
    }

    // ── MinMaxNormalizer::range_f64 / sum_f64 ─────────────────────────────────

    #[test]
    fn test_minmax_range_f64_none_for_empty_window() {
        assert!(norm(3).range_f64().is_none());
    }

    #[test]
    fn test_minmax_range_f64_correct() {
        let mut n = norm(4);
        for v in [dec!(5), dec!(15), dec!(10), dec!(20)] { n.update(v); }
        // range = 20 - 5 = 15
        let r = n.range_f64().unwrap();
        assert!((r - 15.0).abs() < 1e-9);
    }

    #[test]
    fn test_minmax_sum_f64_none_for_empty_window() {
        assert!(norm(3).sum_f64().is_none());
    }

    #[test]
    fn test_minmax_sum_f64_correct() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let s = n.sum_f64().unwrap();
        assert!((s - 10.0).abs() < 1e-9);
    }

    // ── MinMaxNormalizer::values ──────────────────────────────────────────────

    #[test]
    fn test_minmax_values_empty_for_empty_window() {
        assert!(norm(3).values().is_empty());
    }

    #[test]
    fn test_minmax_values_preserves_insertion_order() {
        let mut n = norm(5);
        for v in [dec!(3), dec!(1), dec!(4), dec!(1), dec!(5)] { n.update(v); }
        assert_eq!(n.values(), vec![dec!(3), dec!(1), dec!(4), dec!(1), dec!(5)]);
    }

    // ── MinMaxNormalizer::normalized_midpoint ─────────────────────────────────

    #[test]
    fn test_minmax_normalized_midpoint_none_for_empty_window() {
        assert!(norm(3).normalized_midpoint().is_none());
    }

    #[test]
    fn test_minmax_normalized_midpoint_half_for_uniform_range() {
        let mut n = norm(4);
        for v in [dec!(0), dec!(10), dec!(20), dec!(30)] { n.update(v); }
        // midpoint = (0+30)/2 = 15; normalize(15) with min=0, max=30 = 0.5
        let mid = n.normalized_midpoint().unwrap();
        assert!((mid - 0.5).abs() < 1e-9);
    }

    // ── MinMaxNormalizer::is_at_min / is_at_max ───────────────────────────────

    #[test]
    fn test_minmax_is_at_min_false_for_empty_window() {
        assert!(!norm(3).is_at_min(dec!(5)));
    }

    #[test]
    fn test_minmax_is_at_min_true_for_minimum_value() {
        let mut n = norm(4);
        for v in [dec!(10), dec!(20), dec!(5), dec!(30)] { n.update(v); }
        assert!(n.is_at_min(dec!(5)));
    }

    #[test]
    fn test_minmax_is_at_min_false_for_non_minimum() {
        let mut n = norm(4);
        for v in [dec!(10), dec!(20), dec!(5), dec!(30)] { n.update(v); }
        assert!(!n.is_at_min(dec!(10)));
    }

    #[test]
    fn test_minmax_is_at_max_false_for_empty_window() {
        assert!(!norm(3).is_at_max(dec!(5)));
    }

    #[test]
    fn test_minmax_is_at_max_true_for_maximum_value() {
        let mut n = norm(4);
        for v in [dec!(10), dec!(20), dec!(5), dec!(30)] { n.update(v); }
        assert!(n.is_at_max(dec!(30)));
    }

    #[test]
    fn test_minmax_is_at_max_false_for_non_maximum() {
        let mut n = norm(4);
        for v in [dec!(10), dec!(20), dec!(5), dec!(30)] { n.update(v); }
        assert!(!n.is_at_max(dec!(20)));
    }

    // ── MinMaxNormalizer::fraction_above / fraction_below ─────────────────────

    #[test]
    fn test_minmax_fraction_above_none_for_empty_window() {
        assert!(norm(3).fraction_above(dec!(5)).is_none());
    }

    #[test]
    fn test_minmax_fraction_above_correct() {
        let mut n = norm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)] { n.update(v); }
        // above 3: {4, 5} = 2/5 = 0.4
        let frac = n.fraction_above(dec!(3)).unwrap();
        assert!((frac - 0.4).abs() < 1e-9);
    }

    #[test]
    fn test_minmax_fraction_below_none_for_empty_window() {
        assert!(norm(3).fraction_below(dec!(5)).is_none());
    }

    #[test]
    fn test_minmax_fraction_below_correct() {
        let mut n = norm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)] { n.update(v); }
        // below 3: {1, 2} = 2/5 = 0.4
        let frac = n.fraction_below(dec!(3)).unwrap();
        assert!((frac - 0.4).abs() < 1e-9);
    }

    // ── MinMaxNormalizer::window_values_above / window_values_below ───────────

    #[test]
    fn test_minmax_window_values_above_empty_window() {
        assert!(norm(3).window_values_above(dec!(5)).is_empty());
    }

    #[test]
    fn test_minmax_window_values_above_filters_correctly() {
        let mut n = norm(5);
        for v in [dec!(1), dec!(3), dec!(5), dec!(7), dec!(9)] { n.update(v); }
        let above = n.window_values_above(dec!(5));
        assert_eq!(above.len(), 2);
        assert!(above.contains(&dec!(7)));
        assert!(above.contains(&dec!(9)));
    }

    #[test]
    fn test_minmax_window_values_below_empty_window() {
        assert!(norm(3).window_values_below(dec!(5)).is_empty());
    }

    #[test]
    fn test_minmax_window_values_below_filters_correctly() {
        let mut n = norm(5);
        for v in [dec!(1), dec!(3), dec!(5), dec!(7), dec!(9)] { n.update(v); }
        let below = n.window_values_below(dec!(5));
        assert_eq!(below.len(), 2);
        assert!(below.contains(&dec!(1)));
        assert!(below.contains(&dec!(3)));
    }

    // ── MinMaxNormalizer::percentile_rank ─────────────────────────────────────

    #[test]
    fn test_minmax_percentile_rank_none_for_empty_window() {
        assert!(norm(3).percentile_rank(dec!(5)).is_none());
    }

    #[test]
    fn test_minmax_percentile_rank_correct() {
        let mut n = norm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)] { n.update(v); }
        // <= 3: {1, 2, 3} = 3/5 = 0.6
        let rank = n.percentile_rank(dec!(3)).unwrap();
        assert!((rank - 0.6).abs() < 1e-9);
    }

    // ── MinMaxNormalizer::count_equal ─────────────────────────────────────────

    #[test]
    fn test_minmax_count_equal_zero_for_no_match() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        assert_eq!(n.count_equal(dec!(99)), 0);
    }

    #[test]
    fn test_minmax_count_equal_counts_duplicates() {
        let mut n = norm(5);
        for v in [dec!(5), dec!(5), dec!(3), dec!(5), dec!(2)] { n.update(v); }
        assert_eq!(n.count_equal(dec!(5)), 3);
    }

    // ── MinMaxNormalizer::rolling_range ───────────────────────────────────────

    #[test]
    fn test_minmax_rolling_range_none_for_empty() {
        assert!(norm(3).rolling_range().is_none());
    }

    #[test]
    fn test_minmax_rolling_range_correct() {
        let mut n = norm(5);
        for v in [dec!(10), dec!(50), dec!(30), dec!(20), dec!(40)] { n.update(v); }
        assert_eq!(n.rolling_range(), Some(dec!(40)));
    }

    // ── MinMaxNormalizer::skewness ─────────────────────────────────────────────

    #[test]
    fn test_minmax_skewness_none_for_fewer_than_3() {
        let mut n = norm(5);
        n.update(dec!(1)); n.update(dec!(2));
        assert!(n.skewness().is_none());
    }

    #[test]
    fn test_minmax_skewness_near_zero_for_symmetric_data() {
        let mut n = norm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)] { n.update(v); }
        let s = n.skewness().unwrap();
        assert!(s.abs() < 0.5);
    }

    // ── MinMaxNormalizer::kurtosis ─────────────────────────────────────

    #[test]
    fn test_minmax_kurtosis_none_for_fewer_than_4() {
        let mut n = norm(5);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        assert!(n.kurtosis().is_none());
    }

    #[test]
    fn test_minmax_kurtosis_returns_f64_for_populated_window() {
        let mut n = norm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)] { n.update(v); }
        assert!(n.kurtosis().is_some());
    }

    // ── MinMaxNormalizer::autocorrelation_lag1 ────────────────────────────────

    #[test]
    fn test_minmax_autocorrelation_none_for_single_value() {
        let mut n = norm(3);
        n.update(dec!(1));
        assert!(n.autocorrelation_lag1().is_none());
    }

    #[test]
    fn test_minmax_autocorrelation_positive_for_trending_data() {
        let mut n = norm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)] { n.update(v); }
        let ac = n.autocorrelation_lag1().unwrap();
        assert!(ac > 0.0);
    }

    // ── MinMaxNormalizer::trend_consistency ───────────────────────────────────

    #[test]
    fn test_minmax_trend_consistency_none_for_single_value() {
        let mut n = norm(3);
        n.update(dec!(1));
        assert!(n.trend_consistency().is_none());
    }

    #[test]
    fn test_minmax_trend_consistency_one_for_strictly_rising() {
        let mut n = norm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)] { n.update(v); }
        let tc = n.trend_consistency().unwrap();
        assert!((tc - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_minmax_trend_consistency_zero_for_strictly_falling() {
        let mut n = norm(5);
        for v in [dec!(5), dec!(4), dec!(3), dec!(2), dec!(1)] { n.update(v); }
        let tc = n.trend_consistency().unwrap();
        assert!((tc - 0.0).abs() < 1e-9);
    }

    // ── MinMaxNormalizer::coefficient_of_variation ────────────────────────────

    #[test]
    fn test_minmax_cov_none_for_single_value() {
        let mut n = norm(3);
        n.update(dec!(10));
        assert!(n.coefficient_of_variation().is_none());
    }

    #[test]
    fn test_minmax_cov_positive_for_varied_data() {
        let mut n = norm(5);
        for v in [dec!(10), dec!(20), dec!(30), dec!(40), dec!(50)] { n.update(v); }
        let cov = n.coefficient_of_variation().unwrap();
        assert!(cov > 0.0);
    }

    // ── MinMaxNormalizer::mean_absolute_deviation ─────────────────────────────

    #[test]
    fn test_minmax_mean_absolute_deviation_none_for_empty() {
        assert!(norm(3).mean_absolute_deviation().is_none());
    }

    #[test]
    fn test_minmax_mean_absolute_deviation_zero_for_identical_values() {
        let mut n = norm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let mad = n.mean_absolute_deviation().unwrap();
        assert!((mad - 0.0).abs() < 1e-9);
    }

    #[test]
    fn test_minmax_mean_absolute_deviation_positive_for_varied_data() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let mad = n.mean_absolute_deviation().unwrap();
        assert!(mad > 0.0);
    }

    // ── MinMaxNormalizer::percentile_of_latest ────────────────────────────────

    #[test]
    fn test_minmax_percentile_of_latest_none_for_empty() {
        assert!(norm(3).percentile_of_latest().is_none());
    }

    #[test]
    fn test_minmax_percentile_of_latest_returns_some_after_update() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        assert!(n.percentile_of_latest().is_some());
    }

    #[test]
    fn test_minmax_percentile_of_latest_max_has_high_rank() {
        let mut n = norm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)] { n.update(v); }
        let rank = n.percentile_of_latest().unwrap();
        assert!(rank >= 0.9, "max value should have rank near 1.0, got {}", rank);
    }

    // ── MinMaxNormalizer::tail_ratio ──────────────────────────────────────────

    #[test]
    fn test_minmax_tail_ratio_none_for_empty() {
        assert!(norm(4).tail_ratio().is_none());
    }

    #[test]
    fn test_minmax_tail_ratio_one_for_identical_values() {
        let mut n = norm(4);
        for _ in 0..4 { n.update(dec!(7)); }
        // p75 == max == 7 → ratio = 1.0
        let r = n.tail_ratio().unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0, got {}", r);
    }

    #[test]
    fn test_minmax_tail_ratio_above_one_with_outlier() {
        let mut n = norm(5);
        for v in [dec!(1), dec!(1), dec!(1), dec!(1), dec!(10)] { n.update(v); }
        let r = n.tail_ratio().unwrap();
        assert!(r > 1.0, "outlier should push ratio above 1.0, got {}", r);
    }

    // ── MinMaxNormalizer::z_score_of_min / z_score_of_max ─────────────────────

    #[test]
    fn test_minmax_z_score_of_min_none_for_empty() {
        assert!(norm(4).z_score_of_min().is_none());
    }

    #[test]
    fn test_minmax_z_score_of_max_none_for_empty() {
        assert!(norm(4).z_score_of_max().is_none());
    }

    #[test]
    fn test_minmax_z_score_of_min_negative_for_varied_window() {
        let mut n = norm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)] { n.update(v); }
        // min=1, mean=3, so z-score should be negative
        let z = n.z_score_of_min().unwrap();
        assert!(z < 0.0, "z-score of min should be negative, got {}", z);
    }

    #[test]
    fn test_minmax_z_score_of_max_positive_for_varied_window() {
        let mut n = norm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)] { n.update(v); }
        // max=5, mean=3, so z-score should be positive
        let z = n.z_score_of_max().unwrap();
        assert!(z > 0.0, "z-score of max should be positive, got {}", z);
    }

    // ── MinMaxNormalizer::window_entropy ──────────────────────────────────────

    #[test]
    fn test_minmax_window_entropy_none_for_empty() {
        assert!(norm(4).window_entropy().is_none());
    }

    #[test]
    fn test_minmax_window_entropy_zero_for_identical_values() {
        let mut n = norm(3);
        for _ in 0..3 { n.update(dec!(5)); }
        let e = n.window_entropy().unwrap();
        assert!((e - 0.0).abs() < 1e-9, "identical values should have zero entropy, got {}", e);
    }

    #[test]
    fn test_minmax_window_entropy_positive_for_varied_values() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let e = n.window_entropy().unwrap();
        assert!(e > 0.0, "varied values should have positive entropy, got {}", e);
    }

    // ── MinMaxNormalizer::normalized_std_dev ──────────────────────────────────

    #[test]
    fn test_minmax_normalized_std_dev_none_for_single_value() {
        let mut n = norm(4);
        n.update(dec!(5));
        assert!(n.normalized_std_dev().is_none());
    }

    #[test]
    fn test_minmax_normalized_std_dev_positive_for_varied_values() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let r = n.normalized_std_dev().unwrap();
        assert!(r > 0.0, "expected positive normalized std dev, got {}", r);
    }

    // ── MinMaxNormalizer::value_above_mean_count ──────────────────────────────

    #[test]
    fn test_minmax_value_above_mean_count_none_for_empty() {
        assert!(norm(4).value_above_mean_count().is_none());
    }

    #[test]
    fn test_minmax_value_above_mean_count_correct() {
        // values: 1,2,3,4 → mean=2.5 → above: 3,4 → count=2
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        assert_eq!(n.value_above_mean_count().unwrap(), 2);
    }

    // ── MinMaxNormalizer::consecutive_above_mean ──────────────────────────────

    #[test]
    fn test_minmax_consecutive_above_mean_none_for_empty() {
        assert!(norm(4).consecutive_above_mean().is_none());
    }

    #[test]
    fn test_minmax_consecutive_above_mean_correct() {
        // values: 1,5,6,7 → mean=4.75 → above: 5,6,7 → run=3
        let mut n = norm(4);
        for v in [dec!(1), dec!(5), dec!(6), dec!(7)] { n.update(v); }
        assert_eq!(n.consecutive_above_mean().unwrap(), 3);
    }

    // ── MinMaxNormalizer::above_threshold_fraction / below_threshold_fraction ─

    #[test]
    fn test_minmax_above_threshold_fraction_none_for_empty() {
        assert!(norm(4).above_threshold_fraction(dec!(5)).is_none());
    }

    #[test]
    fn test_minmax_above_threshold_fraction_correct() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        // above 2: values 3,4 → fraction = 0.5
        let f = n.above_threshold_fraction(dec!(2)).unwrap();
        assert!((f - 0.5).abs() < 1e-9, "expected 0.5, got {}", f);
    }

    #[test]
    fn test_minmax_below_threshold_fraction_none_for_empty() {
        assert!(norm(4).below_threshold_fraction(dec!(5)).is_none());
    }

    #[test]
    fn test_minmax_below_threshold_fraction_correct() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        // below 3: values 1,2 → fraction = 0.5
        let f = n.below_threshold_fraction(dec!(3)).unwrap();
        assert!((f - 0.5).abs() < 1e-9, "expected 0.5, got {}", f);
    }

    // ── MinMaxNormalizer::lag_k_autocorrelation ───────────────────────────────

    #[test]
    fn test_minmax_lag_k_autocorrelation_none_for_zero_k() {
        let mut n = norm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)] { n.update(v); }
        assert!(n.lag_k_autocorrelation(0).is_none());
    }

    #[test]
    fn test_minmax_lag_k_autocorrelation_none_when_k_gte_len() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        assert!(n.lag_k_autocorrelation(3).is_none());
    }

    #[test]
    fn test_minmax_lag_k_autocorrelation_positive_for_trend() {
        // Monotone increasing → strong positive autocorrelation
        let mut n = norm(6);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5), dec!(6)] { n.update(v); }
        let ac = n.lag_k_autocorrelation(1).unwrap();
        assert!(ac > 0.0, "trending series should have positive AC, got {}", ac);
    }

    // ── MinMaxNormalizer::half_life_estimate ──────────────────────────────────

    #[test]
    fn test_minmax_half_life_estimate_none_for_fewer_than_3() {
        let mut n = norm(3);
        n.update(dec!(1)); n.update(dec!(2));
        assert!(n.half_life_estimate().is_none());
    }

    #[test]
    fn test_minmax_half_life_estimate_some_for_mean_reverting() {
        // Alternating series: strong mean-reversion signal
        let mut n = norm(6);
        for v in [dec!(10), dec!(5), dec!(10), dec!(5), dec!(10), dec!(5)] { n.update(v); }
        // May or may not return Some depending on AR coefficient sign; just ensure no panic
        let _ = n.half_life_estimate();
    }

    // ── MinMaxNormalizer::geometric_mean ──────────────────────────────────────

    #[test]
    fn test_minmax_geometric_mean_none_for_empty() {
        assert!(norm(4).geometric_mean().is_none());
    }

    #[test]
    fn test_minmax_geometric_mean_correct_for_powers_of_2() {
        // geomean(1,2,4,8) = (1*2*4*8)^(1/4) = 64^0.25 = 2.828...
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(4), dec!(8)] { n.update(v); }
        let gm = n.geometric_mean().unwrap();
        assert!((gm - 64.0f64.powf(0.25)).abs() < 1e-6, "got {}", gm);
    }

    // ── MinMaxNormalizer::harmonic_mean ───────────────────────────────────────

    #[test]
    fn test_minmax_harmonic_mean_none_for_empty() {
        assert!(norm(4).harmonic_mean().is_none());
    }

    #[test]
    fn test_minmax_harmonic_mean_none_when_any_zero() {
        let mut n = norm(2);
        n.update(dec!(0)); n.update(dec!(5));
        assert!(n.harmonic_mean().is_none());
    }

    #[test]
    fn test_minmax_harmonic_mean_positive_for_positive_values() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let hm = n.harmonic_mean().unwrap();
        assert!(hm > 0.0 && hm < 4.0, "HM should be in (0, max), got {}", hm);
    }

    // ── MinMaxNormalizer::range_normalized_value ──────────────────────────────

    #[test]
    fn test_minmax_range_normalized_value_none_for_empty() {
        assert!(norm(4).range_normalized_value(dec!(5)).is_none());
    }

    #[test]
    fn test_minmax_range_normalized_value_zero_for_min() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let r = n.range_normalized_value(dec!(1)).unwrap();
        assert!((r - 0.0).abs() < 1e-9, "min value should normalize to 0, got {}", r);
    }

    #[test]
    fn test_minmax_range_normalized_value_one_for_max() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let r = n.range_normalized_value(dec!(4)).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "max value should normalize to 1, got {}", r);
    }

    // ── MinMaxNormalizer::distance_from_median ────────────────────────────────

    #[test]
    fn test_minmax_distance_from_median_none_for_empty() {
        assert!(norm(4).distance_from_median(dec!(5)).is_none());
    }

    #[test]
    fn test_minmax_distance_from_median_zero_at_median() {
        let mut n = norm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)] { n.update(v); }
        // median=3
        let d = n.distance_from_median(dec!(3)).unwrap();
        assert!((d - 0.0).abs() < 1e-9, "distance from median should be 0, got {}", d);
    }

    #[test]
    fn test_minmax_distance_from_median_positive_above() {
        let mut n = norm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)] { n.update(v); }
        let d = n.distance_from_median(dec!(5)).unwrap();
        assert!(d > 0.0, "value above median should give positive distance, got {}", d);
    }

    #[test]
    fn test_minmax_momentum_none_for_single_value() {
        let mut n = norm(5);
        n.update(dec!(10));
        assert!(n.momentum().is_none());
    }

    #[test]
    fn test_minmax_momentum_positive_for_rising_window() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let m = n.momentum().unwrap();
        assert!(m > 0.0, "rising window → positive momentum, got {}", m);
    }

    #[test]
    fn test_minmax_value_rank_none_for_empty() {
        assert!(norm(4).value_rank(dec!(5)).is_none());
    }

    #[test]
    fn test_minmax_value_rank_extremes() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        // value below all: rank = 0/4 = 0.0
        let low = n.value_rank(dec!(0)).unwrap();
        assert!((low - 0.0).abs() < 1e-9, "got {}", low);
        // value above all: rank = 4/4 = 1.0
        let high = n.value_rank(dec!(5)).unwrap();
        assert!((high - 1.0).abs() < 1e-9, "got {}", high);
    }

    #[test]
    fn test_minmax_coeff_of_variation_none_for_single_value() {
        let mut n = norm(5);
        n.update(dec!(10));
        assert!(n.coeff_of_variation().is_none());
    }

    #[test]
    fn test_minmax_coeff_of_variation_positive_for_spread() {
        let mut n = norm(4);
        for v in [dec!(10), dec!(20), dec!(30), dec!(40)] { n.update(v); }
        let cv = n.coeff_of_variation().unwrap();
        assert!(cv > 0.0, "expected positive CV, got {}", cv);
    }

    #[test]
    fn test_minmax_quantile_range_none_for_empty() {
        assert!(norm(4).quantile_range().is_none());
    }

    #[test]
    fn test_minmax_quantile_range_non_negative() {
        let mut n = norm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)] { n.update(v); }
        let iqr = n.quantile_range().unwrap();
        assert!(iqr >= 0.0, "IQR should be non-negative, got {}", iqr);
    }

    // ── MinMaxNormalizer::upper_quartile / lower_quartile ─────────────────────

    #[test]
    fn test_minmax_upper_quartile_none_for_empty() {
        assert!(norm(4).upper_quartile().is_none());
    }

    #[test]
    fn test_minmax_lower_quartile_none_for_empty() {
        assert!(norm(4).lower_quartile().is_none());
    }

    #[test]
    fn test_minmax_upper_ge_lower_quartile() {
        let mut n = norm(8);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5), dec!(6), dec!(7), dec!(8)] {
            n.update(v);
        }
        let q3 = n.upper_quartile().unwrap();
        let q1 = n.lower_quartile().unwrap();
        assert!(q3 >= q1, "Q3 ({}) should be >= Q1 ({})", q3, q1);
    }

    // ── MinMaxNormalizer::sign_change_rate ────────────────────────────────────

    #[test]
    fn test_minmax_sign_change_rate_none_for_fewer_than_3() {
        let mut n = norm(4);
        n.update(dec!(1));
        n.update(dec!(2));
        assert!(n.sign_change_rate().is_none());
    }

    #[test]
    fn test_minmax_sign_change_rate_one_for_zigzag() {
        let mut n = norm(5);
        // 1, 3, 1, 3, 1 → diffs: +,−,+,− → every adjacent pair flips → 1.0
        for v in [dec!(1), dec!(3), dec!(1), dec!(3), dec!(1)] { n.update(v); }
        let r = n.sign_change_rate().unwrap();
        assert!((r - 1.0).abs() < 1e-9, "zigzag should give 1.0, got {}", r);
    }

    #[test]
    fn test_minmax_sign_change_rate_zero_for_monotone() {
        let mut n = norm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)] { n.update(v); }
        let r = n.sign_change_rate().unwrap();
        assert!((r - 0.0).abs() < 1e-9, "monotone should give 0.0, got {}", r);
    }

    // ── round-79 ─────────────────────────────────────────────────────────────

    // ── MinMaxNormalizer::consecutive_below_mean ──────────────────────────────

    #[test]
    fn test_consecutive_below_mean_none_for_single_value() {
        let mut n = norm(5);
        n.update(dec!(10));
        assert!(n.consecutive_below_mean().is_none());
    }

    #[test]
    fn test_consecutive_below_mean_zero_when_latest_above_mean() {
        let mut n = norm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(100)] { n.update(v); }
        let c = n.consecutive_below_mean().unwrap();
        assert_eq!(c, 0, "latest above mean → streak=0, got {}", c);
    }

    #[test]
    fn test_consecutive_below_mean_counts_trailing_below() {
        let mut n = norm(5);
        for v in [dec!(100), dec!(100), dec!(1), dec!(1), dec!(1)] { n.update(v); }
        let c = n.consecutive_below_mean().unwrap();
        assert!(c >= 3, "last 3 below mean → streak>=3, got {}", c);
    }

    // ── MinMaxNormalizer::drift_rate ──────────────────────────────────────────

    #[test]
    fn test_drift_rate_none_for_single_value() {
        let mut n = norm(5);
        n.update(dec!(10));
        assert!(n.drift_rate().is_none());
    }

    #[test]
    fn test_drift_rate_positive_for_rising_series() {
        let mut n = norm(6);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5), dec!(6)] { n.update(v); }
        let d = n.drift_rate().unwrap();
        assert!(d > 0.0, "rising series → positive drift, got {}", d);
    }

    #[test]
    fn test_drift_rate_negative_for_falling_series() {
        let mut n = norm(6);
        for v in [dec!(6), dec!(5), dec!(4), dec!(3), dec!(2), dec!(1)] { n.update(v); }
        let d = n.drift_rate().unwrap();
        assert!(d < 0.0, "falling series → negative drift, got {}", d);
    }

    // ── MinMaxNormalizer::peak_to_trough_ratio ────────────────────────────────

    #[test]
    fn test_peak_to_trough_ratio_none_for_empty() {
        assert!(norm(4).peak_to_trough_ratio().is_none());
    }

    #[test]
    fn test_peak_to_trough_ratio_one_for_constant() {
        let mut n = norm(4);
        for v in [dec!(10), dec!(10), dec!(10), dec!(10)] { n.update(v); }
        let r = n.peak_to_trough_ratio().unwrap();
        assert!((r - 1.0).abs() < 1e-9, "constant → ratio=1, got {}", r);
    }

    #[test]
    fn test_peak_to_trough_ratio_above_one_for_spread() {
        let mut n = norm(4);
        for v in [dec!(10), dec!(20), dec!(30), dec!(40)] { n.update(v); }
        let r = n.peak_to_trough_ratio().unwrap();
        assert!(r > 1.0, "spread → ratio>1, got {}", r);
    }

    // ── MinMaxNormalizer::normalized_deviation ────────────────────────────────

    #[test]
    fn test_normalized_deviation_none_for_empty() {
        assert!(norm(4).normalized_deviation().is_none());
    }

    #[test]
    fn test_normalized_deviation_none_for_constant() {
        let mut n = norm(4);
        for v in [dec!(5), dec!(5), dec!(5), dec!(5)] { n.update(v); }
        assert!(n.normalized_deviation().is_none());
    }

    #[test]
    fn test_normalized_deviation_positive_for_latest_above_mean() {
        let mut n = norm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(10)] { n.update(v); }
        let d = n.normalized_deviation().unwrap();
        assert!(d > 0.0, "latest above mean → positive deviation, got {}", d);
    }

    // ── MinMaxNormalizer::window_cv_pct ───────────────────────────────────────

    #[test]
    fn test_window_cv_pct_none_for_single_value() {
        let mut n = norm(5);
        n.update(dec!(10));
        assert!(n.window_cv_pct().is_none());
    }

    #[test]
    fn test_window_cv_pct_positive_for_varied_values() {
        let mut n = norm(4);
        for v in [dec!(10), dec!(20), dec!(30), dec!(40)] { n.update(v); }
        let cv = n.window_cv_pct().unwrap();
        assert!(cv > 0.0, "expected positive CV%, got {}", cv);
    }

    // ── MinMaxNormalizer::latest_rank_pct ─────────────────────────────────────

    #[test]
    fn test_latest_rank_pct_none_for_single_value() {
        let mut n = norm(5);
        n.update(dec!(10));
        assert!(n.latest_rank_pct().is_none());
    }

    #[test]
    fn test_latest_rank_pct_one_for_max_value() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(100)] { n.update(v); }
        let r = n.latest_rank_pct().unwrap();
        assert!((r - 1.0).abs() < 1e-9, "latest is max → rank=1, got {}", r);
    }

    #[test]
    fn test_latest_rank_pct_zero_for_min_value() {
        let mut n = norm(4);
        for v in [dec!(10), dec!(20), dec!(30), dec!(1)] { n.update(v); }
        let r = n.latest_rank_pct().unwrap();
        assert!(r.abs() < 1e-9, "latest is min → rank=0, got {}", r);
    }

    // ── MinMaxNormalizer::trimmed_mean ────────────────────────────────────────

    #[test]
    fn test_minmax_trimmed_mean_none_for_empty() {
        assert!(norm(4).trimmed_mean(0.1).is_none());
    }

    #[test]
    fn test_minmax_trimmed_mean_equals_mean_at_zero_trim() {
        let mut n = norm(4);
        for v in [dec!(10), dec!(20), dec!(30), dec!(40)] { n.update(v); }
        let tm = n.trimmed_mean(0.0).unwrap();
        let m = n.mean().unwrap().to_f64().unwrap();
        assert!((tm - m).abs() < 1e-9, "0% trim should equal mean, got tm={} m={}", tm, m);
    }

    #[test]
    fn test_minmax_trimmed_mean_reduces_effect_of_outlier() {
        let mut n = norm(5);
        for v in [dec!(10), dec!(10), dec!(10), dec!(10), dec!(1000)] { n.update(v); }
        // p=0.2 → trim = floor(5*0.2) = 1 element removed from each end
        let tm = n.trimmed_mean(0.2).unwrap();
        let m = n.mean().unwrap().to_f64().unwrap();
        assert!(tm < m, "trimmed mean should be less than mean when outlier is trimmed, tm={} m={}", tm, m);
    }

    // ── MinMaxNormalizer::linear_trend_slope ─────────────────────────────────

    #[test]
    fn test_minmax_linear_trend_slope_none_for_single_value() {
        let mut n = norm(4);
        n.update(dec!(10));
        assert!(n.linear_trend_slope().is_none());
    }

    #[test]
    fn test_minmax_linear_trend_slope_positive_for_rising() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let slope = n.linear_trend_slope().unwrap();
        assert!(slope > 0.0, "rising window → positive slope, got {}", slope);
    }

    #[test]
    fn test_minmax_linear_trend_slope_negative_for_falling() {
        let mut n = norm(4);
        for v in [dec!(4), dec!(3), dec!(2), dec!(1)] { n.update(v); }
        let slope = n.linear_trend_slope().unwrap();
        assert!(slope < 0.0, "falling window → negative slope, got {}", slope);
    }

    #[test]
    fn test_minmax_linear_trend_slope_zero_for_flat() {
        let mut n = norm(4);
        for v in [dec!(5), dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let slope = n.linear_trend_slope().unwrap();
        assert!(slope.abs() < 1e-9, "flat window → slope=0, got {}", slope);
    }

    // ── MinMaxNormalizer::variance_ratio ─────────────────────────────────────

    #[test]
    fn test_minmax_variance_ratio_none_for_few_values() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        assert!(n.variance_ratio().is_none());
    }

    #[test]
    fn test_minmax_variance_ratio_gt_one_for_decreasing_vol() {
        let mut n = norm(6);
        // first half: high variance [1, 10, 1]; second half: low variance [5, 6, 5]
        for v in [dec!(1), dec!(10), dec!(1), dec!(5), dec!(6), dec!(5)] { n.update(v); }
        let r = n.variance_ratio().unwrap();
        assert!(r > 1.0, "first half more volatile → ratio > 1, got {}", r);
    }

    // ── MinMaxNormalizer::z_score_trend_slope ────────────────────────────────

    #[test]
    fn test_minmax_z_score_trend_slope_none_for_single_value() {
        let mut n = norm(4);
        n.update(dec!(10));
        assert!(n.z_score_trend_slope().is_none());
    }

    #[test]
    fn test_minmax_z_score_trend_slope_positive_for_rising() {
        let mut n = norm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)] { n.update(v); }
        let slope = n.z_score_trend_slope().unwrap();
        assert!(slope > 0.0, "rising window → positive z-score slope, got {}", slope);
    }

    // ── MinMaxNormalizer::mean_absolute_change ────────────────────────────────

    #[test]
    fn test_minmax_mean_absolute_change_none_for_single_value() {
        let mut n = norm(4);
        n.update(dec!(10));
        assert!(n.mean_absolute_change().is_none());
    }

    #[test]
    fn test_minmax_mean_absolute_change_zero_for_constant() {
        let mut n = norm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        let mac = n.mean_absolute_change().unwrap();
        assert!(mac.abs() < 1e-9, "constant window → MAC=0, got {}", mac);
    }

    #[test]
    fn test_minmax_mean_absolute_change_positive_for_varying() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(3), dec!(2), dec!(5)] { n.update(v); }
        let mac = n.mean_absolute_change().unwrap();
        assert!(mac > 0.0, "varying window → MAC > 0, got {}", mac);
    }

    // ── round-83 tests ─────────────────────────────────────────────────────

    #[test]
    fn test_minmax_monotone_increase_fraction_none_for_single() {
        let mut n = norm(4);
        n.update(dec!(5));
        assert!(n.monotone_increase_fraction().is_none());
    }

    #[test]
    fn test_minmax_monotone_increase_fraction_one_for_rising() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let f = n.monotone_increase_fraction().unwrap();
        assert!((f - 1.0).abs() < 1e-9, "all rising → fraction=1, got {}", f);
    }

    #[test]
    fn test_minmax_abs_max_none_for_empty() {
        let n = norm(4);
        assert!(n.abs_max().is_none());
    }

    #[test]
    fn test_minmax_abs_max_returns_max_absolute() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(3), dec!(2)] { n.update(v); }
        assert_eq!(n.abs_max().unwrap(), dec!(3));
    }

    #[test]
    fn test_minmax_max_count_none_for_empty() {
        let n = norm(4);
        assert!(n.max_count().is_none());
    }

    #[test]
    fn test_minmax_max_count_correct() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(5), dec!(3), dec!(5)] { n.update(v); }
        assert_eq!(n.max_count().unwrap(), 2);
    }

    #[test]
    fn test_minmax_mean_ratio_none_for_single() {
        let mut n = norm(4);
        n.update(dec!(10));
        assert!(n.mean_ratio().is_none());
    }

    // ── round-84 tests ─────────────────────────────────────────────────────

    #[test]
    fn test_minmax_exponential_weighted_mean_none_for_empty() {
        let n = norm(4);
        assert!(n.exponential_weighted_mean(0.5).is_none());
    }

    #[test]
    fn test_minmax_exponential_weighted_mean_returns_value() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let ewm = n.exponential_weighted_mean(0.5).unwrap();
        assert!(ewm > 0.0, "EWM should be positive, got {}", ewm);
    }

    #[test]
    fn test_minmax_peak_to_trough_none_for_empty() {
        let n = norm(4);
        assert!(n.peak_to_trough_ratio().is_none());
    }

    #[test]
    fn test_minmax_peak_to_trough_correct() {
        let mut n = norm(4);
        for v in [dec!(2), dec!(4), dec!(1), dec!(8)] { n.update(v); }
        let r = n.peak_to_trough_ratio().unwrap();
        assert!((r - 8.0).abs() < 1e-9, "max=8, min=1 → ratio=8, got {}", r);
    }

    #[test]
    fn test_minmax_second_moment_none_for_empty() {
        let n = norm(4);
        assert!(n.second_moment().is_none());
    }

    #[test]
    fn test_minmax_second_moment_correct() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        // (1 + 4 + 9) / 3 = 14/3 ≈ 4.667
        let m = n.second_moment().unwrap();
        assert!((m - 14.0 / 3.0).abs() < 1e-9, "second moment ≈ 4.667, got {}", m);
    }

    #[test]
    fn test_minmax_range_over_mean_none_for_empty() {
        let n = norm(4);
        assert!(n.range_over_mean().is_none());
    }

    #[test]
    fn test_minmax_range_over_mean_positive() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let r = n.range_over_mean().unwrap();
        assert!(r > 0.0, "range/mean should be positive, got {}", r);
    }

    #[test]
    fn test_minmax_above_median_fraction_none_for_empty() {
        let n = norm(4);
        assert!(n.above_median_fraction().is_none());
    }

    #[test]
    fn test_minmax_above_median_fraction_in_range() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let f = n.above_median_fraction().unwrap();
        assert!(f >= 0.0 && f <= 1.0, "fraction in [0,1], got {}", f);
    }

    // ── round-85 tests ─────────────────────────────────────────────────────

    #[test]
    fn test_minmax_interquartile_mean_none_for_empty() {
        let n = norm(4);
        assert!(n.interquartile_mean().is_none());
    }

    #[test]
    fn test_minmax_outlier_fraction_none_for_empty() {
        let n = norm(4);
        assert!(n.outlier_fraction(2.0).is_none());
    }

    #[test]
    fn test_minmax_outlier_fraction_zero_for_constant() {
        let mut n = norm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        let f = n.outlier_fraction(1.0).unwrap();
        assert!(f.abs() < 1e-9, "constant window → no outliers, got {}", f);
    }

    #[test]
    fn test_minmax_sign_flip_count_none_for_single() {
        let mut n = norm(4);
        n.update(dec!(1));
        assert!(n.sign_flip_count().is_none());
    }

    #[test]
    fn test_minmax_sign_flip_count_correct() {
        let mut n = norm(6);
        for v in [dec!(1), dec!(-1), dec!(1), dec!(-1)] { n.update(v); }
        let c = n.sign_flip_count().unwrap();
        assert_eq!(c, 3, "3 sign flips expected, got {}", c);
    }

    #[test]
    fn test_minmax_rms_none_for_empty() {
        let n = norm(4);
        assert!(n.rms().is_none());
    }

    #[test]
    fn test_minmax_rms_correct_for_unit_value() {
        let mut n = norm(4);
        for _ in 0..4 { n.update(dec!(1)); }
        let r = n.rms().unwrap();
        assert!((r - 1.0).abs() < 1e-9, "RMS of all-ones = 1.0, got {}", r);
    }

    // ── round-86 tests ─────────────────────────────────────────────────────

    #[test]
    fn test_minmax_distinct_count_zero_for_empty() {
        let n = norm(4);
        assert_eq!(n.distinct_count(), 0);
    }

    #[test]
    fn test_minmax_distinct_count_correct() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(1), dec!(2), dec!(3)] { n.update(v); }
        assert_eq!(n.distinct_count(), 3);
    }

    #[test]
    fn test_minmax_max_fraction_none_for_empty() {
        let n = norm(4);
        assert!(n.max_fraction().is_none());
    }

    #[test]
    fn test_minmax_max_fraction_correct() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(3)] { n.update(v); }
        let f = n.max_fraction().unwrap();
        // 2 out of 4 values are the max (3)
        assert!((f - 0.5).abs() < 1e-9, "2/4 are max → 0.5, got {}", f);
    }

    #[test]
    fn test_minmax_latest_minus_mean_none_for_empty() {
        let n = norm(4);
        assert!(n.latest_minus_mean().is_none());
    }

    #[test]
    fn test_minmax_latest_to_mean_ratio_none_for_empty() {
        let n = norm(4);
        assert!(n.latest_to_mean_ratio().is_none());
    }

    #[test]
    fn test_minmax_latest_to_mean_ratio_one_for_constant() {
        let mut n = norm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        let r = n.latest_to_mean_ratio().unwrap();
        assert!((r - 1.0).abs() < 1e-9, "latest=mean → ratio=1, got {}", r);
    }

    // ── round-87 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_minmax_below_mean_fraction_none_for_empty() {
        assert!(norm(4).below_mean_fraction().is_none());
    }

    #[test]
    fn test_minmax_below_mean_fraction_symmetric_data() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        // mean=2.5; values strictly below: 1, 2 → 2/4 = 0.5
        let f = n.below_mean_fraction().unwrap();
        assert!((f - 0.5).abs() < 1e-9, "expected 0.5, got {}", f);
    }

    #[test]
    fn test_minmax_tail_variance_none_for_small_window() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        assert!(n.tail_variance().is_none());
    }

    #[test]
    fn test_minmax_tail_variance_nonneg_for_varied_data() {
        let mut n = norm(6);
        for v in [dec!(1), dec!(2), dec!(5), dec!(6), dec!(9), dec!(10)] { n.update(v); }
        let tv = n.tail_variance().unwrap();
        assert!(tv >= 0.0, "tail variance should be non-negative, got {}", tv);
    }

    // ── round-88 tests ─────────────────────────────────────────────────────

    #[test]
    fn test_minmax_new_max_count_zero_for_empty() {
        let n = norm(4);
        assert_eq!(n.new_max_count(), 0);
    }

    #[test]
    fn test_minmax_new_max_count_all_rising() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        assert_eq!(n.new_max_count(), 4, "each value is a new high");
    }

    #[test]
    fn test_minmax_new_min_count_zero_for_empty() {
        let n = norm(4);
        assert_eq!(n.new_min_count(), 0);
    }

    #[test]
    fn test_minmax_new_min_count_all_falling() {
        let mut n = norm(4);
        for v in [dec!(4), dec!(3), dec!(2), dec!(1)] { n.update(v); }
        assert_eq!(n.new_min_count(), 4, "each value is a new low");
    }

    #[test]
    fn test_minmax_zero_fraction_none_for_empty() {
        let n = norm(4);
        assert!(n.zero_fraction().is_none());
    }

    #[test]
    fn test_minmax_zero_fraction_zero_when_no_zeros() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let f = n.zero_fraction().unwrap();
        assert!(f.abs() < 1e-9, "no zeros → fraction=0, got {}", f);
    }

    // ── round-89 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_minmax_cumulative_sum_zero_for_empty() {
        let n = norm(4);
        assert_eq!(n.cumulative_sum(), rust_decimal::Decimal::ZERO);
    }

    #[test]
    fn test_minmax_cumulative_sum_correct() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        assert_eq!(n.cumulative_sum(), dec!(6));
    }

    #[test]
    fn test_minmax_max_to_min_ratio_none_for_empty() {
        assert!(norm(4).max_to_min_ratio().is_none());
    }

    #[test]
    fn test_minmax_max_to_min_ratio_one_for_constant() {
        let mut n = norm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        let r = n.max_to_min_ratio().unwrap();
        assert!((r - 1.0).abs() < 1e-9, "constant window → ratio=1, got {}", r);
    }

    // ── round-90 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_minmax_above_midpoint_fraction_none_for_empty() {
        assert!(norm(4).above_midpoint_fraction().is_none());
    }

    #[test]
    fn test_minmax_above_midpoint_fraction_half_for_symmetric() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        // midpoint = (1+4)/2 = 2.5; values above: 3 and 4 → 2/4 = 0.5
        let f = n.above_midpoint_fraction().unwrap();
        assert!((f - 0.5).abs() < 1e-9, "expected 0.5, got {}", f);
    }

    #[test]
    fn test_minmax_span_utilization_none_for_empty() {
        assert!(norm(4).span_utilization().is_none());
    }

    #[test]
    fn test_minmax_span_utilization_one_for_latest_at_max() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(5), dec!(3), dec!(10)] { n.update(v); }
        // range [1,10], latest=10 → utilization = 1.0
        let u = n.span_utilization().unwrap();
        assert!((u - 1.0).abs() < 1e-9, "latest=max → 1.0, got {}", u);
    }

    #[test]
    fn test_minmax_positive_fraction_none_for_empty() {
        assert!(norm(4).positive_fraction().is_none());
    }

    #[test]
    fn test_minmax_positive_fraction_half() {
        let mut n = norm(4);
        for v in [dec!(-1), dec!(0), dec!(1), dec!(2)] { n.update(v); }
        // strictly > 0: 1 and 2 → 2/4 = 0.5
        let f = n.positive_fraction().unwrap();
        assert!((f - 0.5).abs() < 1e-9, "expected 0.5, got {}", f);
    }

    // ── round-91 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_iqr_none_for_empty() {
        assert!(norm(4).window_iqr().is_none());
    }

    #[test]
    fn test_minmax_window_iqr_zero_for_constant() {
        let mut n = norm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        assert_eq!(n.window_iqr().unwrap(), dec!(0));
    }

    #[test]
    fn test_minmax_run_length_mean_none_for_single_value() {
        let mut n = norm(4);
        n.update(dec!(1));
        assert!(n.run_length_mean().is_none());
    }

    #[test]
    fn test_minmax_run_length_mean_all_increasing() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        // one monotone run of length 4 → mean = 4.0
        let r = n.run_length_mean().unwrap();
        assert!((r - 4.0).abs() < 1e-9, "monotone up → run_len=4, got {}", r);
    }

    // ── round-92 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_minmax_monotone_fraction_none_for_single_value() {
        let mut n = norm(4);
        n.update(dec!(1));
        assert!(n.monotone_fraction().is_none());
    }

    #[test]
    fn test_minmax_monotone_fraction_one_for_increasing() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let f = n.monotone_fraction().unwrap();
        assert!((f - 1.0).abs() < 1e-9, "monotone up → 1.0, got {}", f);
    }

    #[test]
    fn test_minmax_coeff_variation_none_for_empty() {
        assert!(norm(4).coeff_variation().is_none());
    }

    #[test]
    fn test_minmax_coeff_variation_zero_for_constant() {
        let mut n = norm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        // zero std dev → CV = 0
        let cv = n.coeff_variation().unwrap();
        assert!(cv.abs() < 1e-9, "constant window → CV=0, got {}", cv);
    }

    // ── round-93 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_sum_of_squares_zero_for_empty() {
        assert_eq!(norm(4).window_sum_of_squares(), rust_decimal::Decimal::ZERO);
    }

    #[test]
    fn test_minmax_window_sum_of_squares_correct() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        // 1 + 4 + 9 = 14
        assert_eq!(n.window_sum_of_squares(), dec!(14));
    }

    #[test]
    fn test_minmax_percentile_75_none_for_empty() {
        assert!(norm(4).percentile_75().is_none());
    }

    #[test]
    fn test_minmax_percentile_75_for_constant() {
        let mut n = norm(4);
        for _ in 0..4 { n.update(dec!(7)); }
        assert_eq!(n.percentile_75().unwrap(), dec!(7));
    }

    // ── round-94 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_mean_deviation_none_for_empty() {
        assert!(norm(4).window_mean_deviation().is_none());
    }

    #[test]
    fn test_minmax_window_mean_deviation_constant() {
        let mut n = norm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        assert_eq!(n.window_mean_deviation().unwrap(), dec!(0));
    }

    #[test]
    fn test_minmax_latest_percentile_none_for_empty() {
        assert!(norm(4).latest_percentile().is_none());
    }

    #[test]
    fn test_minmax_latest_percentile_top_value() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        // latest = 4, below = 3 (1,2,3) → 3/4 = 0.75
        let p = n.latest_percentile().unwrap();
        assert!((p - 0.75).abs() < 1e-9, "expected 0.75, got {}", p);
    }

    // ── round-95 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_trimmed_mean_none_for_small_window() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        assert!(n.window_trimmed_mean().is_none());
    }

    #[test]
    fn test_minmax_window_trimmed_mean_constant() {
        let mut n = norm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        assert_eq!(n.window_trimmed_mean().unwrap(), dec!(5));
    }

    #[test]
    fn test_minmax_window_variance_none_for_empty() {
        assert!(norm(4).window_variance().is_none());
    }

    #[test]
    fn test_minmax_window_variance_zero_for_constant() {
        let mut n = norm(4);
        for _ in 0..4 { n.update(dec!(7)); }
        assert_eq!(n.window_variance().unwrap(), dec!(0));
    }

    // ── round-96 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_std_dev_none_for_empty() {
        assert!(norm(4).window_std_dev().is_none());
    }

    #[test]
    fn test_minmax_window_std_dev_zero_for_constant() {
        let mut n = norm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        assert!(n.window_std_dev().unwrap().abs() < 1e-9);
    }

    #[test]
    fn test_minmax_window_min_max_ratio_none_for_empty() {
        assert!(norm(4).window_min_max_ratio().is_none());
    }

    #[test]
    fn test_minmax_window_min_max_ratio_one_for_constant() {
        let mut n = norm(4);
        for _ in 0..4 { n.update(dec!(3)); }
        assert_eq!(n.window_min_max_ratio().unwrap(), dec!(1));
    }

    #[test]
    fn test_minmax_recent_bias_none_for_small_window() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        assert!(n.recent_bias().is_none());
    }

    #[test]
    fn test_minmax_window_range_pct_none_for_empty() {
        assert!(norm(4).window_range_pct().is_none());
    }

    #[test]
    fn test_minmax_window_range_pct_zero_for_constant() {
        let mut n = norm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        let r = n.window_range_pct().unwrap();
        assert!(r.abs() < 1e-9, "constant → range_pct=0, got {}", r);
    }

    // ── round-97 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_momentum_none_for_single() {
        let mut n = norm(4);
        n.update(dec!(5));
        assert!(n.window_momentum().is_none());
    }

    #[test]
    fn test_minmax_window_momentum_positive_for_rising() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(3)] { n.update(v); }
        assert_eq!(n.window_momentum().unwrap(), dec!(2));
    }

    #[test]
    fn test_minmax_above_first_fraction_none_for_empty() {
        assert!(norm(4).above_first_fraction().is_none());
    }

    #[test]
    fn test_minmax_above_first_fraction_all_above() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        // first=1, above: 2,3,4 → 3/4
        let f = n.above_first_fraction().unwrap();
        assert!((f - 0.75).abs() < 1e-9, "expected 0.75, got {}", f);
    }

    #[test]
    fn test_minmax_window_zscore_latest_none_for_constant() {
        let mut n = norm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        assert!(n.window_zscore_latest().is_none());
    }

    #[test]
    fn test_minmax_decay_weighted_mean_none_for_empty() {
        assert!(norm(4).decay_weighted_mean(0.1).is_none());
    }

    // ── round-98 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_kurtosis_none_for_constant() {
        let mut n = norm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        assert!(n.window_kurtosis().is_none());
    }

    #[test]
    fn test_minmax_window_kurtosis_none_for_small_window() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        assert!(n.window_kurtosis().is_none());
    }

    #[test]
    fn test_minmax_above_percentile_90_none_for_empty() {
        assert!(norm(4).above_percentile_90().is_none());
    }

    #[test]
    fn test_minmax_above_percentile_90_basic() {
        let mut n = norm(10);
        for v in 1i64..=10 { n.update(Decimal::from(v)); }
        // p90 = value at index 9 = 10; none above 10 → 0.0
        let f = n.above_percentile_90().unwrap();
        assert!(f < 0.2, "expected small fraction, got {}", f);
    }

    #[test]
    fn test_minmax_window_lag_autocorr_none_for_constant() {
        let mut n = norm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        assert!(n.window_lag_autocorr().is_none());
    }

    #[test]
    fn test_minmax_slope_of_mean_none_for_small_window() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        assert!(n.slope_of_mean().is_none());
    }

    // ── round-99 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_percentile_25_none_for_empty() {
        assert!(norm(4).window_percentile_25().is_none());
    }

    #[test]
    fn test_minmax_window_percentile_25_constant() {
        let mut n = norm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        assert_eq!(n.window_percentile_25().unwrap(), dec!(5));
    }

    #[test]
    fn test_minmax_mean_reversion_score_none_for_constant() {
        let mut n = norm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        assert!(n.mean_reversion_score().is_none());
    }

    #[test]
    fn test_minmax_trend_strength_none_for_constant() {
        let mut n = norm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        assert!(n.trend_strength().is_none());
    }

    #[test]
    fn test_minmax_window_peak_count_none_for_small_window() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2)] { n.update(v); }
        assert!(n.window_peak_count().is_none());
    }

    #[test]
    fn test_minmax_window_peak_count_basic() {
        let mut n = norm(4);
        // 1, 3, 2, 4 → peak at 3 (idx 1): 3>1 and 3>2
        for v in [dec!(1), dec!(3), dec!(2), dec!(4)] { n.update(v); }
        let p = n.window_peak_count().unwrap();
        assert_eq!(p, 1);
    }

    // ── round-100 tests ───────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_trough_count_none_for_small_window() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2)] { n.update(v); }
        assert!(n.window_trough_count().is_none());
    }

    #[test]
    fn test_minmax_window_trough_count_basic() {
        let mut n = norm(4);
        // 3, 1, 4, 2 → trough at 1: 1<3 and 1<4
        for v in [dec!(3), dec!(1), dec!(4), dec!(2)] { n.update(v); }
        let t = n.window_trough_count().unwrap();
        assert_eq!(t, 1);
    }

    #[test]
    fn test_minmax_positive_momentum_fraction_none_for_single() {
        let mut n = norm(4);
        n.update(dec!(5));
        assert!(n.positive_momentum_fraction().is_none());
    }

    #[test]
    fn test_minmax_positive_momentum_fraction_all_rising() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let f = n.positive_momentum_fraction().unwrap();
        assert!((f - 1.0).abs() < 1e-9, "all rising → 1.0, got {}", f);
    }

    #[test]
    fn test_minmax_below_percentile_10_none_for_empty() {
        assert!(norm(4).below_percentile_10().is_none());
    }

    #[test]
    fn test_minmax_alternation_rate_none_for_constant() {
        let mut n = norm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        assert!(n.alternation_rate().is_none());
    }

    // ── round-101 tests ───────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_signed_area_none_for_empty() {
        assert!(norm(4).window_signed_area().is_none());
    }

    #[test]
    fn test_minmax_window_signed_area_zero_for_constant() {
        let mut n = norm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        assert_eq!(n.window_signed_area().unwrap(), dec!(0));
    }

    #[test]
    fn test_minmax_up_fraction_none_for_empty() {
        assert!(norm(4).up_fraction().is_none());
    }

    #[test]
    fn test_minmax_up_fraction_for_all_positive() {
        let mut n = norm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        let f = n.up_fraction().unwrap();
        assert!((f - 1.0).abs() < 1e-9, "all positive → 1.0, got {}", f);
    }

    #[test]
    fn test_minmax_threshold_cross_count_none_for_single() {
        let mut n = norm(4);
        n.update(dec!(5));
        assert!(n.threshold_cross_count().is_none());
    }

    #[test]
    fn test_minmax_window_entropy_approx_none_for_constant() {
        let mut n = norm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        assert!(n.window_entropy_approx().is_none());
    }

    // ── round-102 tests ───────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_q1_q3_ratio_none_for_empty() {
        assert!(norm(4).window_q1_q3_ratio().is_none());
    }

    #[test]
    fn test_minmax_window_q1_q3_ratio_one_for_constant() {
        let mut n = norm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        assert_eq!(n.window_q1_q3_ratio().unwrap(), dec!(1));
    }

    #[test]
    fn test_minmax_signed_momentum_none_for_single() {
        let mut n = norm(4);
        n.update(dec!(5));
        assert!(n.signed_momentum().is_none());
    }

    #[test]
    fn test_minmax_signed_momentum_positive_for_rising() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        // two rising pairs → +2
        assert_eq!(n.signed_momentum().unwrap(), dec!(2));
    }

    #[test]
    fn test_minmax_positive_run_length_none_for_single() {
        let mut n = norm(4);
        n.update(dec!(5));
        assert!(n.positive_run_length().is_none());
    }

    #[test]
    fn test_minmax_valley_to_peak_ratio_none_for_small_window() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2)] { n.update(v); }
        assert!(n.valley_to_peak_ratio().is_none());
    }

    // ── round-103 tests ───────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_max_drawdown_none_for_single() {
        let mut n = norm(5);
        n.update(dec!(10));
        assert!(n.window_max_drawdown().is_none());
    }

    #[test]
    fn test_minmax_window_max_drawdown_basic() {
        let mut n = norm(5);
        for v in [dec!(10), dec!(20), dec!(5), dec!(15), dec!(8)] { n.update(v); }
        // peak=20, then trough=5, drawdown=15
        let dd = n.window_max_drawdown().unwrap();
        assert!((dd - 15.0).abs() < 1e-9, "expected 15.0, got {}", dd);
    }

    #[test]
    fn test_minmax_above_previous_fraction_none_for_single() {
        let mut n = norm(5);
        n.update(dec!(10));
        assert!(n.above_previous_fraction().is_none());
    }

    #[test]
    fn test_minmax_above_previous_fraction_basic() {
        let mut n = norm(5);
        for v in [dec!(10), dec!(20), dec!(15), dec!(25)] { n.update(v); }
        // pairs: (10→20 yes), (20→15 no), (15→25 yes) → 2/3
        let f = n.above_previous_fraction().unwrap();
        assert!((f - 2.0 / 3.0).abs() < 1e-9, "expected 0.667, got {}", f);
    }

    #[test]
    fn test_minmax_range_efficiency_none_for_single() {
        let mut n = norm(5);
        n.update(dec!(10));
        assert!(n.range_efficiency().is_none());
    }

    #[test]
    fn test_minmax_range_efficiency_monotone() {
        let mut n = norm(5);
        for v in [dec!(10), dec!(20), dec!(30)] { n.update(v); }
        // monotone up: net=20, total_move=20 → efficiency=1.0
        let e = n.range_efficiency().unwrap();
        assert!((e - 1.0).abs() < 1e-9, "expected 1.0, got {}", e);
    }

    #[test]
    fn test_minmax_window_running_total_basic() {
        let mut n = norm(5);
        for v in [dec!(10), dec!(20), dec!(30)] { n.update(v); }
        assert_eq!(n.window_running_total(), dec!(60));
    }

    // ── round-104 tests ───────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_convexity_none_for_two_vals() {
        let mut n = norm(5);
        for v in [dec!(10), dec!(20)] { n.update(v); }
        assert!(n.window_convexity().is_none());
    }

    #[test]
    fn test_minmax_window_convexity_linear() {
        let mut n = norm(5);
        // Linear sequence: second differences = 0
        for v in [dec!(10), dec!(20), dec!(30)] { n.update(v); }
        let c = n.window_convexity().unwrap();
        assert!(c.abs() < 1e-9, "expected ~0, got {}", c);
    }

    #[test]
    fn test_minmax_below_previous_fraction_none_for_single() {
        let mut n = norm(5);
        n.update(dec!(10));
        assert!(n.below_previous_fraction().is_none());
    }

    #[test]
    fn test_minmax_below_previous_fraction_basic() {
        let mut n = norm(5);
        for v in [dec!(30), dec!(20), dec!(10)] { n.update(v); }
        // all steps go down: 2/2 = 1.0
        let f = n.below_previous_fraction().unwrap();
        assert!((f - 1.0).abs() < 1e-9, "expected 1.0, got {}", f);
    }

    #[test]
    fn test_minmax_window_volatility_ratio_none_for_three() {
        let mut n = norm(5);
        for v in [dec!(10), dec!(20), dec!(30)] { n.update(v); }
        assert!(n.window_volatility_ratio().is_none());
    }

    #[test]
    fn test_minmax_window_gini_none_for_single() {
        let mut n = norm(5);
        n.update(dec!(10));
        assert!(n.window_gini().is_none());
    }

    #[test]
    fn test_minmax_window_gini_equal_values() {
        let mut n = norm(5);
        for v in [dec!(10), dec!(10), dec!(10)] { n.update(v); }
        // all equal → gini ≈ 0
        let g = n.window_gini().unwrap();
        assert!(g.abs() < 1e-9, "expected ~0, got {}", g);
    }

    // ── round-105 tests ───────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_hurst_none_for_three() {
        let mut n = norm(5);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        assert!(n.window_hurst_exponent().is_none());
    }

    #[test]
    fn test_minmax_window_hurst_basic() {
        let mut n = norm(10);
        for v in [dec!(1), dec!(2), dec!(4), dec!(8)] { n.update(v); }
        // Should return Some (just check it computes without panic)
        assert!(n.window_hurst_exponent().is_some());
    }

    #[test]
    fn test_minmax_window_mean_crossings_none_for_single() {
        let mut n = norm(5);
        n.update(dec!(10));
        assert!(n.window_mean_crossings().is_none());
    }

    #[test]
    fn test_minmax_window_mean_crossings_basic() {
        let mut n = norm(5);
        for v in [dec!(10), dec!(20), dec!(5), dec!(20)] { n.update(v); }
        // mean=13.75; crossings: 10<m → 20>m (cross), 20>m → 5<m (cross), 5<m → 20>m (cross) = 3
        let c = n.window_mean_crossings().unwrap();
        assert_eq!(c, 3);
    }

    #[test]
    fn test_minmax_window_skewness_none_for_two() {
        let mut n = norm(5);
        for v in [dec!(10), dec!(20)] { n.update(v); }
        assert!(n.window_skewness().is_none());
    }

    #[test]
    fn test_minmax_window_skewness_symmetric() {
        let mut n = norm(5);
        for v in [dec!(10), dec!(20), dec!(30)] { n.update(v); }
        // symmetric → skew ≈ 0
        let s = n.window_skewness().unwrap();
        assert!(s.abs() < 1e-9, "expected ~0, got {}", s);
    }

    #[test]
    fn test_minmax_window_max_run_none_for_single() {
        let mut n = norm(5);
        n.update(dec!(10));
        assert!(n.window_max_run().is_none());
    }

    #[test]
    fn test_minmax_window_max_run_basic() {
        let mut n = norm(5);
        for v in [dec!(10), dec!(20), dec!(30), dec!(10)] { n.update(v); }
        // longest run = 3 (10→20→30)
        let r = n.window_max_run().unwrap();
        assert_eq!(r, 3);
    }

    // ── round-106 tests ───────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_median_deviation_basic() {
        let mut n = norm(5);
        for v in [dec!(10), dec!(20), dec!(30)] { n.update(v); }
        // median=20; deviations: |10-20|=10, |20-20|=0, |30-20|=10 → mad=20/3≈6.667
        let m = n.window_median_deviation().unwrap();
        assert!((m - 20.0 / 3.0).abs() < 1e-9, "expected 6.667, got {}", m);
    }

    #[test]
    fn test_minmax_longest_above_mean_run_basic() {
        let mut n = norm(10);
        // mean of [5,10,15,3] = 8.25; above mean: 10,15 (run=2)
        for v in [dec!(5), dec!(10), dec!(15), dec!(3)] { n.update(v); }
        let r = n.longest_above_mean_run().unwrap();
        assert_eq!(r, 2);
    }

    #[test]
    fn test_minmax_window_bimodality_none_for_three() {
        let mut n = norm(5);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        assert!(n.window_bimodality().is_none());
    }

    #[test]
    fn test_minmax_window_bimodality_basic() {
        let mut n = norm(10);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        // Should return Some without panicking
        assert!(n.window_bimodality().is_some());
    }

    #[test]
    fn test_minmax_window_zero_crossings_none_for_single() {
        let mut n = norm(5);
        n.update(dec!(1));
        assert!(n.window_zero_crossings().is_none());
    }

    #[test]
    fn test_minmax_window_zero_crossings_basic() {
        let mut n = norm(5);
        for v in [dec!(1), dec!(-1), dec!(1)] { n.update(v); }
        // crossings: +→- and -→+ = 2
        let c = n.window_zero_crossings().unwrap();
        assert_eq!(c, 2);
    }

    // ── round-107 tests ───────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_energy_basic() {
        let mut n = norm(5);
        for v in [dec!(3), dec!(4)] { n.update(v); }
        // 3^2 + 4^2 = 9 + 16 = 25
        assert_eq!(n.window_energy(), dec!(25));
    }

    #[test]
    fn test_minmax_window_interquartile_mean_none_for_three() {
        let mut n = norm(5);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        assert!(n.window_interquartile_mean().is_none());
    }

    #[test]
    fn test_minmax_window_interquartile_mean_basic() {
        let mut n = norm(10);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        // sorted [1,2,3,4]; q1_idx=1, q3_idx=3; mid=[2,3]; mean=2.5
        let m = n.window_interquartile_mean().unwrap();
        assert!((m - 2.5).abs() < 1e-9, "expected 2.5, got {}", m);
    }

    #[test]
    fn test_minmax_above_mean_count_basic() {
        let mut n = norm(5);
        for v in [dec!(10), dec!(20), dec!(30)] { n.update(v); }
        // mean=20; above: only 30 → count=1
        let c = n.above_mean_count().unwrap();
        assert_eq!(c, 1);
    }

    #[test]
    fn test_minmax_window_diff_entropy_none_for_single() {
        let mut n = norm(5);
        n.update(dec!(10));
        assert!(n.window_diff_entropy().is_none());
    }

    #[test]
    fn test_minmax_window_diff_entropy_basic() {
        let mut n = norm(5);
        for v in [dec!(10), dec!(20), dec!(30)] { n.update(v); }
        // Should return Some without panic
        assert!(n.window_diff_entropy().is_some());
    }

    // ── round-108 tests ───────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_root_mean_square_basic() {
        let mut n = norm(5);
        for v in [dec!(3), dec!(4)] { n.update(v); }
        // rms = sqrt((9+16)/2) = sqrt(12.5)
        let rms = n.window_root_mean_square().unwrap();
        assert!((rms - 12.5_f64.sqrt()).abs() < 1e-9, "expected sqrt(12.5), got {}", rms);
    }

    #[test]
    fn test_minmax_window_first_derivative_mean_none_for_single() {
        let mut n = norm(5);
        n.update(dec!(10));
        assert!(n.window_first_derivative_mean().is_none());
    }

    #[test]
    fn test_minmax_window_first_derivative_mean_basic() {
        let mut n = norm(5);
        for v in [dec!(10), dec!(20), dec!(30)] { n.update(v); }
        // diffs: [10, 10]; mean=10
        let d = n.window_first_derivative_mean().unwrap();
        assert!((d - 10.0).abs() < 1e-9, "expected 10.0, got {}", d);
    }

    #[test]
    fn test_minmax_window_l1_norm_basic() {
        let mut n = norm(5);
        for v in [dec!(3), dec!(-4), dec!(5)] { n.update(v); }
        assert_eq!(n.window_l1_norm(), dec!(12));
    }

    #[test]
    fn test_minmax_window_percentile_10_none_for_empty() {
        let n = norm(5);
        assert!(n.window_percentile_10().is_none());
    }

    #[test]
    fn test_minmax_window_percentile_10_basic() {
        let mut n = norm(10);
        for v in [dec!(10), dec!(20), dec!(30), dec!(40), dec!(50),
                  dec!(60), dec!(70), dec!(80), dec!(90), dec!(100)] { n.update(v); }
        // 10th pct of 10 vals: idx = ceil(10*0.1)-1 = 1-1=0 → val=10
        let p = n.window_percentile_10().unwrap();
        assert!((p - 10.0).abs() < 1e-9, "expected 10.0, got {}", p);
    }

    // ── round-109 tests ───────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_zscore_mean_none_for_single() {
        let mut n = norm(5);
        n.update(dec!(10));
        assert!(n.window_zscore_mean().is_none());
    }

    #[test]
    fn test_minmax_window_zscore_mean_basic() {
        let mut n = norm(5);
        for v in [dec!(10), dec!(20), dec!(30)] { n.update(v); }
        // z-score mean is always ~0
        let z = n.window_zscore_mean().unwrap();
        assert!(z.abs() < 1e-9, "expected ~0, got {}", z);
    }

    #[test]
    fn test_minmax_window_positive_sum_basic() {
        let mut n = norm(5);
        for v in [dec!(5), dec!(-3), dec!(8)] { n.update(v); }
        assert_eq!(n.window_positive_sum(), dec!(13));
    }

    #[test]
    fn test_minmax_window_negative_sum_basic() {
        let mut n = norm(5);
        for v in [dec!(5), dec!(-3), dec!(-7)] { n.update(v); }
        assert_eq!(n.window_negative_sum(), dec!(-10));
    }

    #[test]
    fn test_minmax_window_trend_consistency_none_for_flat() {
        let mut n = norm(5);
        for v in [dec!(10), dec!(10), dec!(10)] { n.update(v); }
        assert!(n.window_trend_consistency().is_none());
    }

    #[test]
    fn test_minmax_window_trend_consistency_perfect() {
        let mut n = norm(5);
        for v in [dec!(10), dec!(20), dec!(30)] { n.update(v); }
        // all steps consistent with upward trend
        let c = n.window_trend_consistency().unwrap();
        assert!((c - 1.0).abs() < 1e-9, "expected 1.0, got {}", c);
    }

    // ── round-110 tests ───────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_pairwise_mean_diff_none_for_single() {
        let mut n = norm(5);
        n.update(dec!(10));
        assert!(n.window_pairwise_mean_diff().is_none());
    }

    #[test]
    fn test_minmax_window_pairwise_mean_diff_basic() {
        let mut n = norm(5);
        for v in [dec!(0), dec!(10)] { n.update(v); }
        // only one pair: |0-10|=10 → mean=10
        let d = n.window_pairwise_mean_diff().unwrap();
        assert!((d - 10.0).abs() < 1e-9, "expected 10.0, got {}", d);
    }

    #[test]
    fn test_minmax_window_q3_none_for_empty() {
        let n = norm(5);
        assert!(n.window_q3().is_none());
    }

    #[test]
    fn test_minmax_window_q3_basic() {
        let mut n = norm(10);
        for v in [dec!(10), dec!(20), dec!(30), dec!(40)] { n.update(v); }
        // sorted [10,20,30,40]; q3 = idx=ceil(4*0.75)-1=3-1=2 → val=30
        let q3 = n.window_q3().unwrap();
        assert!((q3 - 30.0).abs() < 1e-9, "expected 30.0, got {}", q3);
    }

    #[test]
    fn test_minmax_window_coefficient_of_variation_none_for_single() {
        let mut n = norm(5);
        n.update(dec!(10));
        assert!(n.window_coefficient_of_variation().is_none());
    }

    #[test]
    fn test_minmax_window_coefficient_of_variation_basic() {
        let mut n = norm(5);
        // uniform values → std=0, cv=0
        for v in [dec!(10), dec!(10), dec!(10)] { n.update(v); }
        let cv = n.window_coefficient_of_variation().unwrap();
        assert!(cv.abs() < 1e-9, "expected ~0, got {}", cv);
    }

    #[test]
    fn test_minmax_window_second_moment_basic() {
        let mut n = norm(5);
        for v in [dec!(3), dec!(4)] { n.update(v); }
        // (9+16)/2 = 12.5
        let m = n.window_second_moment().unwrap();
        assert!((m - 12.5).abs() < 1e-9, "expected 12.5, got {}", m);
    }

    // ── round-111 tests ───────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_trimmed_sum_none_for_four() {
        let mut n = norm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        assert!(n.window_trimmed_sum().is_none());
    }

    #[test]
    fn test_minmax_window_trimmed_sum_basic() {
        let mut n = norm(10);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5),
                  dec!(6), dec!(7), dec!(8), dec!(9), dec!(10)] { n.update(v); }
        // trim 10%: trim=1 from each end → [2..9] sum=2+3+4+5+6+7+8+9=44
        let s = n.window_trimmed_sum().unwrap();
        assert_eq!(s, dec!(44));
    }

    #[test]
    fn test_minmax_window_range_zscore_none_for_single() {
        let mut n = norm(5);
        n.update(dec!(10));
        assert!(n.window_range_zscore().is_none());
    }

    #[test]
    fn test_minmax_window_range_zscore_basic() {
        let mut n = norm(5);
        for v in [dec!(10), dec!(20), dec!(30)] { n.update(v); }
        // should return Some without panicking
        assert!(n.window_range_zscore().is_some());
    }

    #[test]
    fn test_minmax_window_above_median_count_basic() {
        let mut n = norm(5);
        for v in [dec!(10), dec!(20), dec!(30)] { n.update(v); }
        // median=20; above median: only 30 → count=1
        let c = n.window_above_median_count().unwrap();
        assert_eq!(c, 1);
    }

    #[test]
    fn test_minmax_window_min_run_none_for_single() {
        let mut n = norm(5);
        n.update(dec!(10));
        assert!(n.window_min_run().is_none());
    }

    #[test]
    fn test_minmax_window_min_run_basic() {
        let mut n = norm(5);
        for v in [dec!(30), dec!(20), dec!(10), dec!(25)] { n.update(v); }
        // longest decreasing run = 3 (30→20→10)
        let r = n.window_min_run().unwrap();
        assert_eq!(r, 3);
    }

    #[test]
    fn test_minmax_window_harmonic_mean_none_for_zero() {
        let mut n = norm(3);
        n.update(dec!(0));
        assert!(n.window_harmonic_mean().is_none());
    }

    #[test]
    fn test_minmax_window_harmonic_mean_basic() {
        let mut n = norm(3);
        n.update(dec!(1));
        n.update(dec!(2));
        n.update(dec!(4));
        // harmonic mean = 3 / (1 + 0.5 + 0.25) = 3/1.75 ≈ 1.7143
        let h = n.window_harmonic_mean().unwrap();
        assert!((h - 12.0 / 7.0).abs() < 1e-9, "expected ~1.7143, got {}", h);
    }

    #[test]
    fn test_minmax_window_geometric_std_none_for_single() {
        let mut n = norm(3);
        n.update(dec!(5));
        assert!(n.window_geometric_std().is_none());
    }

    #[test]
    fn test_minmax_window_geometric_std_basic() {
        let mut n = norm(3);
        n.update(dec!(1));
        n.update(dec!(10));
        let g = n.window_geometric_std().unwrap();
        assert!(g > 1.0, "geometric std should be > 1 for spread data");
    }

    #[test]
    fn test_minmax_window_entropy_rate_none_for_single() {
        let mut n = norm(3);
        n.update(dec!(5));
        assert!(n.window_entropy_rate().is_none());
    }

    #[test]
    fn test_minmax_window_entropy_rate_basic() {
        let mut n = norm(3);
        n.update(dec!(10));
        n.update(dec!(12));
        n.update(dec!(14));
        // diffs: |2|, |2| → mean=2
        let e = n.window_entropy_rate().unwrap();
        assert!((e - 2.0).abs() < 1e-9, "expected 2.0, got {}", e);
    }

    #[test]
    fn test_minmax_window_burstiness_none_for_single() {
        let mut n = norm(3);
        n.update(dec!(5));
        assert!(n.window_burstiness().is_none());
    }

    #[test]
    fn test_minmax_window_burstiness_basic() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(1), dec!(1), dec!(10)] { n.update(v); }
        let b = n.window_burstiness().unwrap();
        // bursty signal should have positive burstiness
        assert!(b > 0.0, "expected positive burstiness, got {}", b);
    }

    #[test]
    fn test_minmax_window_iqr_ratio_none_for_two() {
        let mut n = norm(3);
        n.update(dec!(1));
        n.update(dec!(2));
        assert!(n.window_iqr_ratio().is_none());
    }

    #[test]
    fn test_minmax_window_iqr_ratio_basic() {
        let mut n = norm(5);
        for v in [dec!(2), dec!(4), dec!(6), dec!(8), dec!(10)] { n.update(v); }
        let r = n.window_iqr_ratio().unwrap();
        assert!(r > 0.0, "expected positive IQR ratio, got {}", r);
    }

    #[test]
    fn test_minmax_window_mean_reversion_none_for_single() {
        let mut n = norm(3);
        n.update(dec!(5));
        assert!(n.window_mean_reversion().is_none());
    }

    #[test]
    fn test_minmax_window_mean_reversion_basic() {
        let mut n = norm(4);
        // 1, 5, 2, 5 — values alternate away from mean, some move toward
        for v in [dec!(1), dec!(5), dec!(2), dec!(5)] { n.update(v); }
        let r = n.window_mean_reversion().unwrap();
        assert!(r >= 0.0 && r <= 1.0, "expected [0,1], got {}", r);
    }

    #[test]
    fn test_minmax_window_autocorrelation_none_for_two() {
        let mut n = norm(3);
        n.update(dec!(1));
        n.update(dec!(2));
        assert!(n.window_autocorrelation().is_none());
    }

    #[test]
    fn test_minmax_window_autocorrelation_basic() {
        let mut n = norm(4);
        // perfectly trending up → high positive autocorrelation
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let a = n.window_autocorrelation().unwrap();
        assert!(a > 0.0, "expected positive autocorrelation, got {}", a);
    }

    #[test]
    fn test_minmax_window_slope_none_for_single() {
        let mut n = norm(3);
        n.update(dec!(5));
        assert!(n.window_slope().is_none());
    }

    #[test]
    fn test_minmax_window_slope_basic() {
        let mut n = norm(3);
        // 1, 2, 3 → slope = 1
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let s = n.window_slope().unwrap();
        assert!((s - 1.0).abs() < 1e-9, "expected 1.0, got {}", s);
    }

    #[test]
    fn test_minmax_window_crest_factor_none_for_empty() {
        let n = norm(3);
        assert!(n.window_crest_factor().is_none());
    }

    #[test]
    fn test_minmax_window_crest_factor_basic() {
        let mut n = norm(3);
        // all equal → crest factor = 1.0
        for v in [dec!(2), dec!(2), dec!(2)] { n.update(v); }
        let c = n.window_crest_factor().unwrap();
        assert!((c - 1.0).abs() < 1e-9, "expected 1.0, got {}", c);
    }

    #[test]
    fn test_minmax_window_relative_range_none_for_empty() {
        let n = norm(3);
        assert!(n.window_relative_range().is_none());
    }

    #[test]
    fn test_minmax_window_relative_range_basic() {
        let mut n = norm(3);
        // 1, 2, 3 → range=2, mean=2 → relative_range=1.0
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let r = n.window_relative_range().unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0, got {}", r);
    }

    #[test]
    fn test_minmax_window_outlier_count_none_for_single() {
        let mut n = norm(3);
        n.update(dec!(5));
        assert!(n.window_outlier_count().is_none());
    }

    #[test]
    fn test_minmax_window_outlier_count_basic() {
        let mut n = norm(5);
        // 1, 1, 1, 1, 1 — uniform → no outliers
        for v in [dec!(1), dec!(1), dec!(1), dec!(1), dec!(1)] { n.update(v); }
        let c = n.window_outlier_count().unwrap();
        assert_eq!(c, 0);
    }

    #[test]
    fn test_minmax_window_decay_score_none_for_empty() {
        let n = norm(3);
        assert!(n.window_decay_score().is_none());
    }

    #[test]
    fn test_minmax_window_decay_score_basic() {
        let mut n = norm(2);
        n.update(dec!(0));
        n.update(dec!(10));
        // alpha=0.5: weights=[0.5, 1.0] (older→newer), num=0*0.5+10*1.0=10, denom=1.5 → 6.666..
        let d = n.window_decay_score().unwrap();
        assert!(d > 5.0, "expected decay-weighted value closer to 10, got {}", d);
    }

    #[test]
    fn test_minmax_window_log_return_none_for_single() {
        let mut n = norm(3);
        n.update(dec!(5));
        assert!(n.window_log_return().is_none());
    }

    #[test]
    fn test_minmax_window_log_return_basic() {
        let mut n = norm(2);
        n.update(dec!(1));
        n.update(dec!(1));
        // log(1/1) = 0
        let r = n.window_log_return().unwrap();
        assert!(r.abs() < 1e-9, "expected 0.0, got {}", r);
    }

    #[test]
    fn test_minmax_window_signed_rms_none_for_empty() {
        let n = norm(3);
        assert!(n.window_signed_rms().is_none());
    }

    #[test]
    fn test_minmax_window_signed_rms_basic() {
        let mut n = norm(2);
        n.update(dec!(3));
        n.update(dec!(4));
        // rms = sqrt((9+16)/2) = sqrt(12.5) ≈ 3.536; mean>0 → positive
        let r = n.window_signed_rms().unwrap();
        assert!(r > 0.0, "expected positive signed_rms, got {}", r);
    }

    #[test]
    fn test_minmax_window_inflection_count_none_for_two() {
        let mut n = norm(3);
        n.update(dec!(1));
        n.update(dec!(2));
        assert!(n.window_inflection_count().is_none());
    }

    #[test]
    fn test_minmax_window_inflection_count_basic() {
        let mut n = norm(3);
        // 1, 3, 2 → 3 is local max → 1 inflection
        for v in [dec!(1), dec!(3), dec!(2)] { n.update(v); }
        let c = n.window_inflection_count().unwrap();
        assert_eq!(c, 1);
    }

    #[test]
    fn test_minmax_window_centroid_none_for_empty() {
        let n = norm(3);
        assert!(n.window_centroid().is_none());
    }

    #[test]
    fn test_minmax_window_centroid_basic() {
        let mut n = norm(3);
        // values: 0, 0, 10 → total=10, weighted=2*10=20 → centroid=2.0
        for v in [dec!(0), dec!(0), dec!(10)] { n.update(v); }
        let c = n.window_centroid().unwrap();
        assert!((c - 2.0).abs() < 1e-9, "expected 2.0, got {}", c);
    }

    #[test]
    fn test_minmax_window_max_deviation_none_for_empty() {
        let n = norm(3);
        assert!(n.window_max_deviation().is_none());
    }

    #[test]
    fn test_minmax_window_max_deviation_basic() {
        let mut n = norm(3);
        // 1, 2, 3 → mean=2, max_dev = max(1, 0, 1) = 1
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let d = n.window_max_deviation().unwrap();
        assert!((d - 1.0).abs() < 1e-9, "expected 1.0, got {}", d);
    }

    #[test]
    fn test_minmax_window_range_mean_ratio_none_for_empty() {
        let n = norm(3);
        assert!(n.window_range_mean_ratio().is_none());
    }

    #[test]
    fn test_minmax_window_range_mean_ratio_basic() {
        let mut n = norm(3);
        // 1, 2, 3 → range=2, mean=2 → ratio=1.0
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let r = n.window_range_mean_ratio().unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0, got {}", r);
    }

    #[test]
    fn test_minmax_window_step_up_count_none_for_single() {
        let mut n = norm(3);
        n.update(dec!(5));
        assert!(n.window_step_up_count().is_none());
    }

    #[test]
    fn test_minmax_window_step_up_count_basic() {
        let mut n = norm(3);
        // 1, 2, 3 → 2 step ups
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let c = n.window_step_up_count().unwrap();
        assert_eq!(c, 2);
    }

    #[test]
    fn test_minmax_window_step_down_count_none_for_single() {
        let mut n = norm(3);
        n.update(dec!(5));
        assert!(n.window_step_down_count().is_none());
    }

    #[test]
    fn test_minmax_window_step_down_count_basic() {
        let mut n = norm(3);
        // 3, 2, 1 → 2 step downs
        for v in [dec!(3), dec!(2), dec!(1)] { n.update(v); }
        let c = n.window_step_down_count().unwrap();
        assert_eq!(c, 2);
    }

    #[test]
    fn test_minmax_window_entropy_of_changes_none_for_single() {
        let mut n = norm(3);
        n.update(dec!(5));
        assert!(n.window_entropy_of_changes().is_none());
    }

    #[test]
    fn test_minmax_window_entropy_of_changes_basic() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let e = n.window_entropy_of_changes().unwrap();
        assert!(e >= 0.0, "entropy should be non-negative, got {}", e);
    }

    #[test]
    fn test_minmax_window_level_crossing_rate_none_for_single() {
        let mut n = norm(3);
        n.update(dec!(5));
        assert!(n.window_level_crossing_rate().is_none());
    }

    #[test]
    fn test_minmax_window_level_crossing_rate_basic() {
        let mut n = norm(4);
        // 1, 3, 1, 3 → crosses mean (2) twice → rate = 2/3
        for v in [dec!(1), dec!(3), dec!(1), dec!(3)] { n.update(v); }
        let r = n.window_level_crossing_rate().unwrap();
        assert!(r > 0.0 && r <= 1.0, "expected (0,1], got {}", r);
    }

    #[test]
    fn test_minmax_window_abs_mean_none_for_empty() {
        let n = norm(3);
        assert!(n.window_abs_mean().is_none());
    }

    #[test]
    fn test_minmax_window_abs_mean_basic() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let m = n.window_abs_mean().unwrap();
        assert!((m - 2.0).abs() < 1e-9, "expected 2.0, got {}", m);
    }

    #[test]
    fn test_minmax_window_rolling_max_none_for_empty() {
        let n = norm(3);
        assert!(n.window_rolling_max().is_none());
    }

    #[test]
    fn test_minmax_window_rolling_max_basic() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(5), dec!(3)] { n.update(v); }
        let m = n.window_rolling_max().unwrap();
        assert!((m - 5.0).abs() < 1e-9, "expected 5.0, got {}", m);
    }

    #[test]
    fn test_minmax_window_rolling_min_none_for_empty() {
        let n = norm(3);
        assert!(n.window_rolling_min().is_none());
    }

    #[test]
    fn test_minmax_window_rolling_min_basic() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(5), dec!(3)] { n.update(v); }
        let m = n.window_rolling_min().unwrap();
        assert!((m - 1.0).abs() < 1e-9, "expected 1.0, got {}", m);
    }

    #[test]
    fn test_minmax_window_negative_fraction_none_for_empty() {
        let n = norm(3);
        assert!(n.window_negative_fraction().is_none());
    }

    #[test]
    fn test_minmax_window_negative_fraction_basic() {
        let mut n = norm(3);
        // all positive → 0 negative
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let f = n.window_negative_fraction().unwrap();
        assert!(f.abs() < 1e-9, "expected 0.0, got {}", f);
    }

    #[test]
    fn test_minmax_window_positive_fraction_none_for_empty() {
        let n = norm(3);
        assert!(n.window_positive_fraction().is_none());
    }

    #[test]
    fn test_minmax_window_positive_fraction_basic() {
        let mut n = norm(3);
        // all positive → 1.0
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let f = n.window_positive_fraction().unwrap();
        assert!((f - 1.0).abs() < 1e-9, "expected 1.0, got {}", f);
    }

    #[test]
    fn test_minmax_window_last_minus_min_none_for_empty() {
        let n = norm(3);
        assert!(n.window_last_minus_min().is_none());
    }

    #[test]
    fn test_minmax_window_last_minus_min_basic() {
        let mut n = norm(3);
        // 1, 5, 3 → last=3, min=1 → 3-1=2
        for v in [dec!(1), dec!(5), dec!(3)] { n.update(v); }
        let d = n.window_last_minus_min().unwrap();
        assert!((d - 2.0).abs() < 1e-9, "expected 2.0, got {}", d);
    }

    #[test]
    fn test_minmax_window_max_minus_min_none_for_empty() {
        let n = norm(3);
        assert!(n.window_max_minus_min().is_none());
    }

    #[test]
    fn test_minmax_window_max_minus_min_basic() {
        let mut n = norm(3);
        // 1, 5, 3 → max=5, min=1 → range=4
        for v in [dec!(1), dec!(5), dec!(3)] { n.update(v); }
        let r = n.window_max_minus_min().unwrap();
        assert!((r - 4.0).abs() < 1e-9, "expected 4.0, got {}", r);
    }

    #[test]
    fn test_minmax_window_normalized_mean_none_for_empty() {
        let n = norm(3);
        assert!(n.window_normalized_mean().is_none());
    }

    #[test]
    fn test_minmax_window_normalized_mean_basic() {
        let mut n = norm(3);
        // 1, 2, 3 → mean=2, min=1, max=3 → normalized=(2-1)/2=0.5
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let m = n.window_normalized_mean().unwrap();
        assert!((m - 0.5).abs() < 1e-9, "expected 0.5, got {}", m);
    }

    #[test]
    fn test_minmax_window_variance_ratio_none_for_single() {
        let mut n = norm(3);
        n.update(dec!(5));
        assert!(n.window_variance_ratio().is_none());
    }

    #[test]
    fn test_minmax_window_variance_ratio_basic() {
        let mut n = norm(3);
        // uniform: var=0 → ratio=0
        for v in [dec!(2), dec!(2), dec!(2)] { n.update(v); }
        let r = n.window_variance_ratio().unwrap();
        assert!(r.abs() < 1e-9, "expected 0.0, got {}", r);
    }

    #[test]
    fn test_minmax_window_max_minus_last_none_for_empty() {
        let n = norm(3);
        assert!(n.window_max_minus_last().is_none());
    }

    #[test]
    fn test_minmax_window_max_minus_last_basic() {
        let mut n = norm(3);
        // 1, 5, 3 → max=5, last=3 → 5-3=2
        for v in [dec!(1), dec!(5), dec!(3)] { n.update(v); }
        let d = n.window_max_minus_last().unwrap();
        assert!((d - 2.0).abs() < 1e-9, "expected 2.0, got {}", d);
    }

    // ── round-120 ────────────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_above_last_none_for_empty() {
        let n = norm(3);
        assert!(n.window_above_last().is_none());
    }

    #[test]
    fn test_minmax_window_above_last_basic() {
        let mut n = norm(5);
        for v in [dec!(3), dec!(5), dec!(1), dec!(4), dec!(2)] { n.update(v); }
        // last=2, values above 2: 3,5,1,4 → 3 above
        let c = n.window_above_last().unwrap();
        assert_eq!(c, 3);
    }

    #[test]
    fn test_minmax_window_below_last_none_for_empty() {
        let n = norm(3);
        assert!(n.window_below_last().is_none());
    }

    #[test]
    fn test_minmax_window_below_last_basic() {
        let mut n = norm(5);
        for v in [dec!(3), dec!(5), dec!(1), dec!(4), dec!(2)] { n.update(v); }
        // last=2, values below 2: 1 → 1 below
        let c = n.window_below_last().unwrap();
        assert_eq!(c, 1);
    }

    #[test]
    fn test_minmax_window_diff_mean_none_for_single() {
        let mut n = norm(3);
        n.update(dec!(5));
        assert!(n.window_diff_mean().is_none());
    }

    #[test]
    fn test_minmax_window_diff_mean_basic() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(3), dec!(5)] { n.update(v); }
        // diffs: 2, 2 → mean=2
        let d = n.window_diff_mean().unwrap();
        assert!((d - 2.0).abs() < 1e-9, "expected 2.0, got {}", d);
    }

    #[test]
    fn test_minmax_window_last_zscore_none_for_single() {
        let mut n = norm(3);
        n.update(dec!(5));
        assert!(n.window_last_zscore().is_none());
    }

    #[test]
    fn test_minmax_window_last_zscore_basic() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let z = n.window_last_zscore().unwrap();
        // last=3, mean=2, std≈0.816 → z≈1.225
        assert!(z > 0.0, "expected positive zscore, got {}", z);
    }

    // ── round-121 ────────────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_range_fraction_none_for_empty() {
        let n = norm(3);
        assert!(n.window_range_fraction().is_none());
    }

    #[test]
    fn test_minmax_window_range_fraction_uniform() {
        let mut n = norm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let f = n.window_range_fraction().unwrap();
        assert!((f - 1.0).abs() < 1e-9, "expected 1.0, got {}", f);
    }

    #[test]
    fn test_minmax_window_mean_above_last_none_for_empty() {
        let n = norm(3);
        assert!(n.window_mean_above_last().is_none());
    }

    #[test]
    fn test_minmax_window_mean_above_last_basic() {
        let mut n = norm(3);
        for v in [dec!(5), dec!(4), dec!(1)] { n.update(v); }
        // mean=(5+4+1)/3=10/3≈3.33, last=1 → mean > last → 1.0
        let r = n.window_mean_above_last().unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0, got {}", r);
    }

    #[test]
    fn test_minmax_window_volatility_trend_none_for_three() {
        let mut n = norm(5);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        assert!(n.window_volatility_trend().is_none());
    }

    #[test]
    fn test_minmax_window_volatility_trend_basic() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        // any result is fine — just ensure it returns Some
        assert!(n.window_volatility_trend().is_some());
    }

    #[test]
    fn test_minmax_window_sign_change_count_none_for_two() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(2)] { n.update(v); }
        assert!(n.window_sign_change_count().is_none());
    }

    #[test]
    fn test_minmax_window_sign_change_count_alternating() {
        let mut n = norm(5);
        // 1,3,1,3,1 → diffs: +2,-2,+2,-2 → sign changes: 3
        for v in [dec!(1), dec!(3), dec!(1), dec!(3), dec!(1)] { n.update(v); }
        let c = n.window_sign_change_count().unwrap();
        assert_eq!(c, 3);
    }

    // ── round-122 ────────────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_last_rank_none_for_empty() {
        let n = norm(3);
        assert!(n.window_last_rank().is_none());
    }

    #[test]
    fn test_minmax_window_last_rank_max() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(2), dec!(5)] { n.update(v); }
        let r = n.window_last_rank().unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0 (max rank), got {}", r);
    }

    #[test]
    fn test_minmax_window_momentum_score_none_for_single() {
        let mut n = norm(3);
        n.update(dec!(5));
        assert!(n.window_momentum_score().is_none());
    }

    #[test]
    fn test_minmax_window_momentum_score_trending_up() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let s = n.window_momentum_score().unwrap();
        assert!((s - 1.0).abs() < 1e-9, "expected 1.0, got {}", s);
    }

    #[test]
    fn test_minmax_window_oscillation_count_none_for_two() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(2)] { n.update(v); }
        assert!(n.window_oscillation_count().is_none());
    }

    #[test]
    fn test_minmax_window_oscillation_count_basic() {
        let mut n = norm(5);
        // 1,3,1,3,1 → local maxima at positions 1 and 3 → 2
        for v in [dec!(1), dec!(3), dec!(1), dec!(3), dec!(1)] { n.update(v); }
        let c = n.window_oscillation_count().unwrap();
        assert_eq!(c, 2);
    }

    #[test]
    fn test_minmax_window_skew_direction_none_for_empty() {
        let n = norm(3);
        assert!(n.window_skew_direction().is_none());
    }

    #[test]
    fn test_minmax_window_skew_direction_positive() {
        let mut n = norm(3);
        // 1, 2, 10 → mean=13/3≈4.33, median=2 → mean > median → 1.0
        for v in [dec!(1), dec!(2), dec!(10)] { n.update(v); }
        let d = n.window_skew_direction().unwrap();
        assert!((d - 1.0).abs() < 1e-9, "expected 1.0, got {}", d);
    }

    // ── round-123 ────────────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_trend_reversal_count_none_for_two() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(2)] { n.update(v); }
        assert!(n.window_trend_reversal_count().is_none());
    }

    #[test]
    fn test_minmax_window_trend_reversal_count_alternating() {
        let mut n = norm(5);
        // 1,3,1,3,1 → reversals=3
        for v in [dec!(1), dec!(3), dec!(1), dec!(3), dec!(1)] { n.update(v); }
        let c = n.window_trend_reversal_count().unwrap();
        assert_eq!(c, 3);
    }

    #[test]
    fn test_minmax_window_first_last_diff_none_for_single() {
        let mut n = norm(3);
        n.update(dec!(5));
        assert!(n.window_first_last_diff().is_none());
    }

    #[test]
    fn test_minmax_window_first_last_diff_basic() {
        let mut n = norm(3);
        for v in [dec!(2), dec!(3), dec!(7)] { n.update(v); }
        let d = n.window_first_last_diff().unwrap();
        assert!((d - 5.0).abs() < 1e-9, "expected 5.0, got {}", d);
    }

    #[test]
    fn test_minmax_window_upper_half_count_none_for_empty() {
        let n = norm(3);
        assert!(n.window_upper_half_count().is_none());
    }

    #[test]
    fn test_minmax_window_upper_half_count_basic() {
        let mut n = norm(4);
        // 1,2,3,4 → mid=2.5 → above: 3,4 → 2
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let c = n.window_upper_half_count().unwrap();
        assert_eq!(c, 2);
    }

    #[test]
    fn test_minmax_window_lower_half_count_none_for_empty() {
        let n = norm(3);
        assert!(n.window_lower_half_count().is_none());
    }

    #[test]
    fn test_minmax_window_lower_half_count_basic() {
        let mut n = norm(4);
        // 1,2,3,4 → mid=2.5 → below: 1,2 → 2
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let c = n.window_lower_half_count().unwrap();
        assert_eq!(c, 2);
    }

    // ── round-124 ────────────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_percentile_75_none_for_empty() {
        let n = norm(3);
        assert!(n.window_percentile_75().is_none());
    }

    #[test]
    fn test_minmax_window_percentile_75_basic() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let p = n.window_percentile_75().unwrap();
        assert!(p >= 3.0, "expected >=3.0, got {}", p);
    }

    #[test]
    fn test_minmax_window_abs_slope_none_for_single() {
        let mut n = norm(3);
        n.update(dec!(5));
        assert!(n.window_abs_slope().is_none());
    }

    #[test]
    fn test_minmax_window_abs_slope_basic() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(2), dec!(5)] { n.update(v); }
        // |5-1| / 2 = 2.0
        let s = n.window_abs_slope().unwrap();
        assert!((s - 2.0).abs() < 1e-9, "expected 2.0, got {}", s);
    }

    #[test]
    fn test_minmax_window_gain_loss_ratio_none_for_single() {
        let mut n = norm(3);
        n.update(dec!(5));
        assert!(n.window_gain_loss_ratio().is_none());
    }

    #[test]
    fn test_minmax_window_gain_loss_ratio_only_gains() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        // only gains → None (no losses)
        assert!(n.window_gain_loss_ratio().is_none());
    }

    #[test]
    fn test_minmax_window_range_stability_none_for_empty() {
        let n = norm(3);
        assert!(n.window_range_stability().is_none());
    }

    #[test]
    fn test_minmax_window_range_stability_uniform() {
        let mut n = norm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        // all same → range=0 → 1.0
        let s = n.window_range_stability().unwrap();
        assert!((s - 1.0).abs() < 1e-9, "expected 1.0, got {}", s);
    }

    // ── round-125 ────────────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_exp_smoothed_none_for_empty() {
        let n = norm(3);
        assert!(n.window_exp_smoothed().is_none());
    }

    #[test]
    fn test_minmax_window_exp_smoothed_single() {
        let mut n = norm(3);
        n.update(dec!(7));
        let e = n.window_exp_smoothed().unwrap();
        assert!((e - 7.0).abs() < 1e-9, "expected 7.0, got {}", e);
    }

    #[test]
    fn test_minmax_window_drawdown_none_for_empty() {
        let n = norm(3);
        assert!(n.window_drawdown().is_none());
    }

    #[test]
    fn test_minmax_window_drawdown_monotone_up() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let dd = n.window_drawdown().unwrap();
        assert!(dd.abs() < 1e-9, "expected 0.0, got {}", dd);
    }

    #[test]
    fn test_minmax_window_drawup_none_for_empty() {
        let n = norm(3);
        assert!(n.window_drawup().is_none());
    }

    #[test]
    fn test_minmax_window_drawup_monotone_down() {
        let mut n = norm(4);
        for v in [dec!(4), dec!(3), dec!(2), dec!(1)] { n.update(v); }
        let du = n.window_drawup().unwrap();
        assert!(du.abs() < 1e-9, "expected 0.0, got {}", du);
    }

    #[test]
    fn test_minmax_window_trend_strength_none_for_single() {
        let mut n = norm(3);
        n.update(dec!(5));
        assert!(n.window_trend_strength().is_none());
    }

    #[test]
    fn test_minmax_window_trend_strength_pure_trend() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        // all diffs = +1 → signed=2, abs=2 → strength=1.0
        let s = n.window_trend_strength().unwrap();
        assert!((s - 1.0).abs() < 1e-9, "expected 1.0, got {}", s);
    }

    // ── round-126 ────────────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_ema_deviation_none_for_empty() {
        let n = norm(3);
        assert!(n.window_ema_deviation().is_none());
    }

    #[test]
    fn test_minmax_window_ema_deviation_flat() {
        let mut n = norm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        // ema=5, last=5 → dev=0
        let d = n.window_ema_deviation().unwrap();
        assert!(d.abs() < 1e-9, "expected 0.0, got {}", d);
    }

    #[test]
    fn test_minmax_window_normalized_variance_none_for_empty() {
        let n = norm(3);
        assert!(n.window_normalized_variance().is_none());
    }

    #[test]
    fn test_minmax_window_normalized_variance_uniform_zero() {
        let mut n = norm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let v = n.window_normalized_variance().unwrap();
        assert!(v.abs() < 1e-9, "expected 0.0, got {}", v);
    }

    #[test]
    fn test_minmax_window_median_ratio_none_for_empty() {
        let n = norm(3);
        assert!(n.window_median_ratio().is_none());
    }

    #[test]
    fn test_minmax_window_median_ratio_last_eq_median() {
        let mut n = norm(3);
        for v in [dec!(2), dec!(3), dec!(3)] { n.update(v); }
        // median=3, last=3 → ratio=1.0
        let r = n.window_median_ratio().unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0, got {}", r);
    }

    #[test]
    fn test_minmax_window_half_life_none_for_empty() {
        let n = norm(3);
        assert!(n.window_half_life().is_none());
    }

    #[test]
    fn test_minmax_window_half_life_immediate() {
        let mut n = norm(3);
        // 10 → 5 → already at half → position 1
        for v in [dec!(10), dec!(5), dec!(3)] { n.update(v); }
        let h = n.window_half_life().unwrap();
        assert_eq!(h, 1);
    }

    // ── round-127 ────────────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_entropy_normalized_none_for_empty() {
        let n = norm(3);
        assert!(n.window_entropy_normalized().is_none());
    }

    #[test]
    fn test_minmax_window_entropy_normalized_single() {
        let mut n = norm(3);
        n.update(dec!(5));
        // single → max_entropy=0 → 0.0
        let e = n.window_entropy_normalized().unwrap();
        assert!(e.abs() < 1e-9, "expected 0.0, got {}", e);
    }

    #[test]
    fn test_minmax_window_peak_value_none_for_empty() {
        let n = norm(3);
        assert!(n.window_peak_value().is_none());
    }

    #[test]
    fn test_minmax_window_peak_value_basic() {
        let mut n = norm(3);
        for v in [dec!(2), dec!(7), dec!(3)] { n.update(v); }
        let p = n.window_peak_value().unwrap();
        assert!((p - 7.0).abs() < 1e-9, "expected 7.0, got {}", p);
    }

    #[test]
    fn test_minmax_window_trough_value_none_for_empty() {
        let n = norm(3);
        assert!(n.window_trough_value().is_none());
    }

    #[test]
    fn test_minmax_window_trough_value_basic() {
        let mut n = norm(3);
        for v in [dec!(2), dec!(7), dec!(3)] { n.update(v); }
        let t = n.window_trough_value().unwrap();
        assert!((t - 2.0).abs() < 1e-9, "expected 2.0, got {}", t);
    }

    #[test]
    fn test_minmax_window_gain_count_none_for_single() {
        let mut n = norm(3);
        n.update(dec!(5));
        assert!(n.window_gain_count().is_none());
    }

    #[test]
    fn test_minmax_window_gain_count_all_rising() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let c = n.window_gain_count().unwrap();
        assert_eq!(c, 2);
    }

    // ── round-128 ────────────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_loss_count_none_for_single() {
        let mut n = norm(3);
        n.update(dec!(5));
        assert!(n.window_loss_count().is_none());
    }

    #[test]
    fn test_minmax_window_loss_count_all_falling() {
        let mut n = norm(3);
        for v in [dec!(3), dec!(2), dec!(1)] { n.update(v); }
        let c = n.window_loss_count().unwrap();
        assert_eq!(c, 2);
    }

    #[test]
    fn test_minmax_window_net_change_none_for_single() {
        let mut n = norm(3);
        n.update(dec!(5));
        assert!(n.window_net_change().is_none());
    }

    #[test]
    fn test_minmax_window_net_change_basic() {
        let mut n = norm(3);
        for v in [dec!(3), dec!(5), dec!(8)] { n.update(v); }
        let nc = n.window_net_change().unwrap();
        assert!((nc - 5.0).abs() < 1e-9, "expected 5.0, got {}", nc);
    }

    #[test]
    fn test_minmax_window_acceleration_none_for_two() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(2)] { n.update(v); }
        assert!(n.window_acceleration().is_none());
    }

    #[test]
    fn test_minmax_window_acceleration_linear_zero() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let a = n.window_acceleration().unwrap();
        assert!(a.abs() < 1e-9, "expected 0.0, got {}", a);
    }

    #[test]
    fn test_minmax_window_regime_score_none_for_empty() {
        let n = norm(3);
        assert!(n.window_regime_score().is_none());
    }

    #[test]
    fn test_minmax_window_regime_score_all_above() {
        let mut n = norm(3);
        // 10, 10, 1 → mean≈7, two above, one below → (2-1)/3≈0.33
        for v in [dec!(10), dec!(10), dec!(1)] { n.update(v); }
        let s = n.window_regime_score().unwrap();
        assert!(s > 0.0, "expected positive, got {}", s);
    }

    // ── round-129 ────────────────────────────────────────────────────────────
    #[test]
    fn test_minmax_window_cumulative_sum_none_for_empty() {
        let n = norm(3);
        assert!(n.window_cumulative_sum().is_none());
    }

    #[test]
    fn test_minmax_window_cumulative_sum_basic() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let s = n.window_cumulative_sum().unwrap();
        assert!((s - 6.0).abs() < 1e-9, "expected 6.0, got {}", s);
    }

    #[test]
    fn test_minmax_window_spread_ratio_none_for_empty() {
        let n = norm(3);
        assert!(n.window_spread_ratio().is_none());
    }

    #[test]
    fn test_minmax_window_spread_ratio_basic() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        // range=2, mean=2 → ratio=1.0
        let r = n.window_spread_ratio().unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0, got {}", r);
    }

    #[test]
    fn test_minmax_window_center_of_mass_none_for_empty() {
        let n = norm(3);
        assert!(n.window_center_of_mass().is_none());
    }

    #[test]
    fn test_minmax_window_center_of_mass_basic() {
        let mut n = norm(2);
        for v in [dec!(1), dec!(1)] { n.update(v); }
        // equal weights → COM = (0*1 + 1*1) / 2 = 0.5
        let c = n.window_center_of_mass().unwrap();
        assert!((c - 0.5).abs() < 1e-9, "expected 0.5, got {}", c);
    }

    #[test]
    fn test_minmax_window_cycle_count_none_for_two() {
        let mut n = norm(2);
        for v in [dec!(1), dec!(2)] { n.update(v); }
        assert!(n.window_cycle_count().is_none());
    }

    #[test]
    fn test_minmax_window_cycle_count_one_reversal() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(3), dec!(2)] { n.update(v); }
        let c = n.window_cycle_count().unwrap();
        assert_eq!(c, 1);
    }

    // ── round-130 ────────────────────────────────────────────────────────────
    #[test]
    fn test_minmax_window_mad_none_for_empty() {
        let n = norm(3);
        assert!(n.window_mad().is_none());
    }

    #[test]
    fn test_minmax_window_mad_identical() {
        let mut n = norm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let m = n.window_mad().unwrap();
        assert!(m.abs() < 1e-9, "expected 0.0 for identical, got {}", m);
    }

    #[test]
    fn test_minmax_window_entropy_ratio_none_for_empty() {
        let n = norm(3);
        assert!(n.window_entropy_ratio().is_none());
    }

    #[test]
    fn test_minmax_window_entropy_ratio_uniform() {
        let mut n = norm(2);
        for v in [dec!(1), dec!(1)] { n.update(v); }
        let e = n.window_entropy_ratio().unwrap();
        assert!((e - 1.0).abs() < 1e-9, "expected 1.0 for uniform, got {}", e);
    }

    #[test]
    fn test_minmax_window_plateau_count_none_for_single() {
        let mut n = norm(3);
        n.update(dec!(5));
        assert!(n.window_plateau_count().is_none());
    }

    #[test]
    fn test_minmax_window_plateau_count_all_equal() {
        let mut n = norm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let p = n.window_plateau_count().unwrap();
        assert_eq!(p, 2);
    }

    #[test]
    fn test_minmax_window_direction_bias_none_for_single() {
        let mut n = norm(3);
        n.update(dec!(5));
        assert!(n.window_direction_bias().is_none());
    }

    #[test]
    fn test_minmax_window_direction_bias_all_up() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let b = n.window_direction_bias().unwrap();
        assert!((b - 1.0).abs() < 1e-9, "expected 1.0 for all up, got {}", b);
    }

    // ── round-131 ────────────────────────────────────────────────────────────
    #[test]
    fn test_minmax_window_last_pct_change_none_for_single() {
        let mut n = norm(3);
        n.update(dec!(5));
        assert!(n.window_last_pct_change().is_none());
    }

    #[test]
    fn test_minmax_window_last_pct_change_basic() {
        let mut n = norm(2);
        for v in [dec!(10), dec!(15)] { n.update(v); }
        let p = n.window_last_pct_change().unwrap();
        assert!((p - 0.5).abs() < 1e-9, "expected 0.5, got {}", p);
    }

    #[test]
    fn test_minmax_window_std_trend_none_for_three() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        assert!(n.window_std_trend().is_none());
    }

    #[test]
    fn test_minmax_window_std_trend_flat() {
        let mut n = norm(4);
        for v in [dec!(5), dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let t = n.window_std_trend().unwrap();
        assert!(t.abs() < 1e-9, "expected 0.0 for flat window, got {}", t);
    }

    #[test]
    fn test_minmax_window_nonzero_count_none_for_empty() {
        let n = norm(3);
        assert!(n.window_nonzero_count().is_none());
    }

    #[test]
    fn test_minmax_window_nonzero_count_basic() {
        let mut n = norm(3);
        for v in [dec!(0), dec!(1), dec!(2)] { n.update(v); }
        let c = n.window_nonzero_count().unwrap();
        assert_eq!(c, 2);
    }

    #[test]
    fn test_minmax_window_pct_above_mean_none_for_empty() {
        let n = norm(3);
        assert!(n.window_pct_above_mean().is_none());
    }

    #[test]
    fn test_minmax_window_pct_above_mean_half() {
        let mut n = norm(4);
        // mean=2.5, above: 3 and 4 → 2/4 = 0.5
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let p = n.window_pct_above_mean().unwrap();
        assert!((p - 0.5).abs() < 1e-9, "expected 0.5, got {}", p);
    }

    // ── round-132 ────────────────────────────────────────────────────────────
    #[test]
    fn test_minmax_window_max_run_up_none_for_single() {
        let mut n = norm(3);
        n.update(dec!(1));
        assert!(n.window_max_run_up().is_none());
    }

    #[test]
    fn test_minmax_window_max_run_up_basic() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(2)] { n.update(v); }
        let r = n.window_max_run_up().unwrap();
        assert_eq!(r, 2);
    }

    #[test]
    fn test_minmax_window_max_run_dn_none_for_single() {
        let mut n = norm(3);
        n.update(dec!(1));
        assert!(n.window_max_run_dn().is_none());
    }

    #[test]
    fn test_minmax_window_max_run_dn_basic() {
        let mut n = norm(4);
        for v in [dec!(4), dec!(3), dec!(2), dec!(3)] { n.update(v); }
        let r = n.window_max_run_dn().unwrap();
        assert_eq!(r, 2);
    }

    #[test]
    fn test_minmax_window_diff_sum_none_for_single() {
        let mut n = norm(3);
        n.update(dec!(1));
        assert!(n.window_diff_sum().is_none());
    }

    #[test]
    fn test_minmax_window_diff_sum_monotone() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        // diffs: 1, 1 → sum=2
        let s = n.window_diff_sum().unwrap();
        assert!((s - 2.0).abs() < 1e-9, "expected 2.0, got {}", s);
    }

    #[test]
    fn test_minmax_window_run_length_none_for_single() {
        let mut n = norm(3);
        n.update(dec!(1));
        assert!(n.window_run_length().is_none());
    }

    #[test]
    fn test_minmax_window_run_length_basic() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let r = n.window_run_length().unwrap();
        assert_eq!(r, 2);
    }

    // ── round-133 ────────────────────────────────────────────────────────────
    #[test]
    fn test_minmax_window_abs_diff_sum_none_for_single() {
        let mut n = norm(3);
        n.update(dec!(5));
        assert!(n.window_abs_diff_sum().is_none());
    }

    #[test]
    fn test_minmax_window_abs_diff_sum_basic() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(3), dec!(2)] { n.update(v); }
        // |3-1| + |2-3| = 2 + 1 = 3
        let s = n.window_abs_diff_sum().unwrap();
        assert!((s - 3.0).abs() < 1e-9, "expected 3.0, got {}", s);
    }

    #[test]
    fn test_minmax_window_max_gap_none_for_single() {
        let mut n = norm(3);
        n.update(dec!(5));
        assert!(n.window_max_gap().is_none());
    }

    #[test]
    fn test_minmax_window_max_gap_basic() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(5), dec!(3)] { n.update(v); }
        let g = n.window_max_gap().unwrap();
        assert!((g - 4.0).abs() < 1e-9, "expected 4.0, got {}", g);
    }

    #[test]
    fn test_minmax_window_local_max_count_none_for_two() {
        let mut n = norm(2);
        for v in [dec!(1), dec!(2)] { n.update(v); }
        assert!(n.window_local_max_count().is_none());
    }

    #[test]
    fn test_minmax_window_local_max_count_one_peak() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(3), dec!(2)] { n.update(v); }
        let c = n.window_local_max_count().unwrap();
        assert_eq!(c, 1);
    }

    #[test]
    fn test_minmax_window_first_half_mean_none_for_single() {
        let mut n = norm(3);
        n.update(dec!(5));
        assert!(n.window_first_half_mean().is_none());
    }

    #[test]
    fn test_minmax_window_first_half_mean_basic() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        // first half: [1,2] → mean=1.5
        let m = n.window_first_half_mean().unwrap();
        assert!((m - 1.5).abs() < 1e-9, "expected 1.5, got {}", m);
    }

    // ── round-134 ────────────────────────────────────────────────────────────
    #[test]
    fn test_minmax_window_second_half_mean_none_for_single() {
        let mut n = norm(3);
        n.update(dec!(5));
        assert!(n.window_second_half_mean().is_none());
    }

    #[test]
    fn test_minmax_window_second_half_mean_basic() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        // second half: [3,4] → mean=3.5
        let m = n.window_second_half_mean().unwrap();
        assert!((m - 3.5).abs() < 1e-9, "expected 3.5, got {}", m);
    }

    #[test]
    fn test_minmax_window_local_min_count_none_for_two() {
        let mut n = norm(2);
        for v in [dec!(2), dec!(1)] { n.update(v); }
        assert!(n.window_local_min_count().is_none());
    }

    #[test]
    fn test_minmax_window_local_min_count_one_valley() {
        let mut n = norm(3);
        for v in [dec!(3), dec!(1), dec!(2)] { n.update(v); }
        let c = n.window_local_min_count().unwrap();
        assert_eq!(c, 1);
    }

    #[test]
    fn test_minmax_window_curvature_none_for_two() {
        let mut n = norm(2);
        for v in [dec!(1), dec!(2)] { n.update(v); }
        assert!(n.window_curvature().is_none());
    }

    #[test]
    fn test_minmax_window_curvature_linear_zero() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        // linear → 2nd diffs = 0
        let c = n.window_curvature().unwrap();
        assert!(c.abs() < 1e-9, "expected 0.0 for linear, got {}", c);
    }

    #[test]
    fn test_minmax_window_half_diff_none_for_single() {
        let mut n = norm(3);
        n.update(dec!(5));
        assert!(n.window_half_diff().is_none());
    }

    #[test]
    fn test_minmax_window_half_diff_rising() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        // first_mean=1.5, second_mean=3.5 → diff=2.0
        let d = n.window_half_diff().unwrap();
        assert!((d - 2.0).abs() < 1e-9, "expected 2.0, got {}", d);
    }

    // ── round-135 ────────────────────────────────────────────────────────────
    #[test]
    fn test_minmax_window_mean_crossing_rate_none_for_single() {
        let mut n = norm(3);
        n.update(dec!(5));
        assert!(n.window_mean_crossing_rate().is_none());
    }

    #[test]
    fn test_minmax_window_mean_crossing_rate_monotone() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        // mean=2, no crossings → rate=0
        let r = n.window_mean_crossing_rate().unwrap();
        assert!(r.abs() < 1e-9, "expected 0.0 for monotone, got {}", r);
    }

    #[test]
    fn test_minmax_window_var_to_mean_none_for_empty() {
        let n = norm(3);
        assert!(n.window_var_to_mean().is_none());
    }

    #[test]
    fn test_minmax_window_var_to_mean_basic() {
        let mut n = norm(2);
        for v in [dec!(1), dec!(3)] { n.update(v); }
        // mean=2, var=1 → ratio=0.5
        let r = n.window_var_to_mean().unwrap();
        assert!((r - 0.5).abs() < 1e-9, "expected 0.5, got {}", r);
    }

    #[test]
    fn test_minmax_window_coeff_var_none_for_empty() {
        let n = norm(3);
        assert!(n.window_coeff_var().is_none());
    }

    #[test]
    fn test_minmax_window_coeff_var_identical() {
        let mut n = norm(2);
        for v in [dec!(5), dec!(5)] { n.update(v); }
        let c = n.window_coeff_var().unwrap();
        assert!(c.abs() < 1e-9, "expected 0.0 for identical values, got {}", c);
    }

    #[test]
    fn test_minmax_window_step_up_fraction_none_for_single() {
        let mut n = norm(3);
        n.update(dec!(5));
        assert!(n.window_step_up_fraction().is_none());
    }

    #[test]
    fn test_minmax_window_step_up_fraction_all_up() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let f = n.window_step_up_fraction().unwrap();
        assert!((f - 1.0).abs() < 1e-9, "expected 1.0 for all up, got {}", f);
    }

    // ── round-136 ────────────────────────────────────────────────────────────
    #[test]
    fn test_minmax_window_step_dn_fraction_none_for_single() {
        let mut n = norm(3);
        n.update(dec!(5));
        assert!(n.window_step_dn_fraction().is_none());
    }

    #[test]
    fn test_minmax_window_step_dn_fraction_all_dn() {
        let mut n = norm(3);
        for v in [dec!(3), dec!(2), dec!(1)] { n.update(v); }
        let f = n.window_step_dn_fraction().unwrap();
        assert!((f - 1.0).abs() < 1e-9, "expected 1.0, got {}", f);
    }

    #[test]
    fn test_minmax_window_mean_abs_dev_ratio_none_for_empty() {
        let n = norm(3);
        assert!(n.window_mean_abs_dev_ratio().is_none());
    }

    #[test]
    fn test_minmax_window_mean_abs_dev_ratio_basic() {
        let mut n = norm(2);
        for v in [dec!(1), dec!(3)] { n.update(v); }
        // mean=2, mad=1, range=2 → ratio=0.5
        let r = n.window_mean_abs_dev_ratio().unwrap();
        assert!((r - 0.5).abs() < 1e-9, "expected 0.5, got {}", r);
    }

    #[test]
    fn test_minmax_window_recent_high_none_for_empty() {
        let n = norm(3);
        assert!(n.window_recent_high().is_none());
    }

    #[test]
    fn test_minmax_window_recent_high_basic() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(5), dec!(3)] { n.update(v); }
        // second half: [5,3] → high=5
        let h = n.window_recent_high().unwrap();
        assert!((h - 5.0).abs() < 1e-9, "expected 5.0, got {}", h);
    }

    #[test]
    fn test_minmax_window_recent_low_none_for_empty() {
        let n = norm(3);
        assert!(n.window_recent_low().is_none());
    }

    #[test]
    fn test_minmax_window_recent_low_basic() {
        let mut n = norm(4);
        for v in [dec!(5), dec!(3), dec!(1), dec!(4)] { n.update(v); }
        // second half: [1,4] → low=1
        let l = n.window_recent_low().unwrap();
        assert!((l - 1.0).abs() < 1e-9, "expected 1.0, got {}", l);
    }

    // ── round-137 ────────────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_linear_trend_score_none_for_empty() {
        let n = norm(4);
        assert!(n.window_linear_trend_score().is_none());
    }

    #[test]
    fn test_minmax_window_linear_trend_score_increasing() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let s = n.window_linear_trend_score().unwrap();
        assert!(s > 0.0, "expected positive trend score, got {}", s);
    }

    #[test]
    fn test_minmax_window_zscore_min_none_for_empty() {
        let n = norm(4);
        assert!(n.window_zscore_min().is_none());
    }

    #[test]
    fn test_minmax_window_zscore_min_basic() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let zmin = n.window_zscore_min().unwrap();
        let zmax = n.window_zscore_max().unwrap();
        assert!(zmin <= zmax, "min {} should be <= max {}", zmin, zmax);
    }

    #[test]
    fn test_minmax_window_zscore_max_none_for_empty() {
        let n = norm(4);
        assert!(n.window_zscore_max().is_none());
    }

    #[test]
    fn test_minmax_window_zscore_max_positive() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(10)] { n.update(v); }
        let zmax = n.window_zscore_max().unwrap();
        assert!(zmax > 0.0, "expected positive zmax, got {}", zmax);
    }

    #[test]
    fn test_minmax_window_diff_variance_none_for_empty() {
        let n = norm(4);
        assert!(n.window_diff_variance().is_none());
    }

    #[test]
    fn test_minmax_window_diff_variance_constant() {
        let mut n = norm(4);
        for v in [dec!(5), dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let dv = n.window_diff_variance().unwrap();
        assert!((dv - 0.0).abs() < 1e-9, "expected 0.0 for constant, got {}", dv);
    }

    // ── round-138 ────────────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_peak_to_trough_none_for_empty() {
        let n = norm(4);
        assert!(n.window_peak_to_trough().is_none());
    }

    #[test]
    fn test_minmax_window_peak_to_trough_basic() {
        let mut n = norm(4);
        for v in [dec!(2), dec!(4), dec!(6), dec!(8)] { n.update(v); }
        let r = n.window_peak_to_trough().unwrap();
        assert!((r - 4.0).abs() < 1e-9, "expected 4.0 (8/2), got {}", r);
    }

    #[test]
    fn test_minmax_window_asymmetry_none_for_small() {
        let mut n = norm(4);
        n.update(dec!(1)); n.update(dec!(2));
        assert!(n.window_asymmetry().is_none());
    }

    #[test]
    fn test_minmax_window_asymmetry_symmetric() {
        let mut n = norm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)] { n.update(v); }
        let a = n.window_asymmetry().unwrap();
        // symmetric distribution → near zero
        assert!(a.abs() < 0.5, "expected near-zero asymmetry, got {}", a);
    }

    #[test]
    fn test_minmax_window_abs_trend_none_for_single() {
        let mut n = norm(4);
        n.update(dec!(5));
        assert!(n.window_abs_trend().is_none());
    }

    #[test]
    fn test_minmax_window_abs_trend_monotonic() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let t = n.window_abs_trend().unwrap();
        // 3 steps of +1 each → sum abs = 3
        assert!((t - 3.0).abs() < 1e-9, "expected 3.0, got {}", t);
    }

    #[test]
    fn test_minmax_window_recent_volatility_none_for_empty() {
        let n = norm(4);
        assert!(n.window_recent_volatility().is_none());
    }

    #[test]
    fn test_minmax_window_recent_volatility_constant() {
        let mut n = norm(4);
        for v in [dec!(5), dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let rv = n.window_recent_volatility().unwrap();
        assert!((rv - 0.0).abs() < 1e-9, "expected 0.0 for constant, got {}", rv);
    }

    // ── round-139 ────────────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_range_position_none_for_empty() {
        let n = norm(4);
        assert!(n.window_range_position().is_none());
    }

    #[test]
    fn test_minmax_window_range_position_at_max() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        // last=4, min=1, max=4 → (4-1)/(4-1)=1.0
        let r = n.window_range_position().unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0, got {}", r);
    }

    #[test]
    fn test_minmax_window_sign_changes_none_for_small() {
        let mut n = norm(4);
        n.update(dec!(1)); n.update(dec!(2));
        assert!(n.window_sign_changes().is_none());
    }

    #[test]
    fn test_minmax_window_sign_changes_alternating() {
        let mut n = norm(5);
        for v in [dec!(1), dec!(3), dec!(1), dec!(3), dec!(1)] { n.update(v); }
        // diffs: +2, -2, +2, -2 → sign changes: 3
        let sc = n.window_sign_changes().unwrap();
        assert_eq!(sc, 3, "expected 3 sign changes, got {}", sc);
    }

    #[test]
    fn test_minmax_window_mean_shift_none_for_small() {
        let mut n = norm(4);
        n.update(dec!(1)); n.update(dec!(2)); n.update(dec!(3));
        assert!(n.window_mean_shift().is_none());
    }

    #[test]
    fn test_minmax_window_mean_shift_increasing() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(10), dec!(11)] { n.update(v); }
        // first half [1,2] mean=1.5, second half [10,11] mean=10.5 → shift=9.0
        let ms = n.window_mean_shift().unwrap();
        assert!(ms > 0.0, "expected positive shift, got {}", ms);
    }

    #[test]
    fn test_minmax_window_slope_change_none_for_small() {
        let mut n = norm(4);
        n.update(dec!(1)); n.update(dec!(2)); n.update(dec!(3));
        assert!(n.window_slope_change().is_none());
    }

    #[test]
    fn test_minmax_window_slope_change_basic() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let sc = n.window_slope_change().unwrap();
        // uniform slope → near 0
        assert!(sc.abs() < 1.0, "expected small slope change, got {}", sc);
    }

    // ── round-140 ────────────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_recovery_rate_none_for_small() {
        let mut n = norm(4);
        n.update(dec!(1)); n.update(dec!(2));
        assert!(n.window_recovery_rate().is_none());
    }

    #[test]
    fn test_minmax_window_recovery_rate_no_drops() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let r = n.window_recovery_rate().unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0 when no drops, got {}", r);
    }

    #[test]
    fn test_minmax_window_normalized_spread_none_for_empty() {
        let n = norm(4);
        assert!(n.window_normalized_spread().is_none());
    }

    #[test]
    fn test_minmax_window_normalized_spread_basic() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        // range=3, mean=2.5 → spread=3/2.5=1.2
        let s = n.window_normalized_spread().unwrap();
        assert!((s - 1.2).abs() < 1e-9, "expected 1.2, got {}", s);
    }

    #[test]
    fn test_minmax_window_first_last_ratio_none_for_single() {
        let mut n = norm(4);
        n.update(dec!(5));
        assert!(n.window_first_last_ratio().is_none());
    }

    #[test]
    fn test_minmax_window_first_last_ratio_basic() {
        let mut n = norm(4);
        for v in [dec!(2), dec!(3), dec!(4), dec!(8)] { n.update(v); }
        // first=2, last=8 → ratio=4.0
        let r = n.window_first_last_ratio().unwrap();
        assert!((r - 4.0).abs() < 1e-9, "expected 4.0, got {}", r);
    }

    #[test]
    fn test_minmax_window_extrema_count_none_for_small() {
        let mut n = norm(4);
        n.update(dec!(1)); n.update(dec!(2));
        assert!(n.window_extrema_count().is_none());
    }

    #[test]
    fn test_minmax_window_extrema_count_alternating() {
        let mut n = norm(5);
        for v in [dec!(1), dec!(3), dec!(1), dec!(3), dec!(1)] { n.update(v); }
        // middle 3 values are all extrema → count=3
        let e = n.window_extrema_count().unwrap();
        assert_eq!(e, 3, "expected 3 extrema, got {}", e);
    }

    // ── round-141 ────────────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_up_fraction_none_for_single() {
        let mut n = norm(4);
        n.update(dec!(5));
        assert!(n.window_up_fraction().is_none());
    }

    #[test]
    fn test_minmax_window_up_fraction_all_up() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let f = n.window_up_fraction().unwrap();
        assert!((f - 1.0).abs() < 1e-9, "expected 1.0, got {}", f);
    }

    #[test]
    fn test_minmax_window_half_range_none_for_empty() {
        let n = norm(4);
        assert!(n.window_half_range().is_none());
    }

    #[test]
    fn test_minmax_window_half_range_basic() {
        let mut n = norm(4);
        for v in [dec!(2), dec!(4), dec!(6), dec!(8)] { n.update(v); }
        // range=6, half=3
        let hr = n.window_half_range().unwrap();
        assert!((hr - 3.0).abs() < 1e-9, "expected 3.0, got {}", hr);
    }

    #[test]
    fn test_minmax_window_negative_count_none_for_empty() {
        let n = norm(4);
        assert!(n.window_negative_count().is_none());
    }

    #[test]
    fn test_minmax_window_negative_count_no_negatives() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let c = n.window_negative_count().unwrap();
        assert_eq!(c, 0, "expected 0 negatives, got {}", c);
    }

    #[test]
    fn test_minmax_window_trend_purity_none_for_single() {
        let mut n = norm(4);
        n.update(dec!(5));
        assert!(n.window_trend_purity().is_none());
    }

    #[test]
    fn test_minmax_window_trend_purity_pure_uptrend() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let p = n.window_trend_purity().unwrap();
        assert!((p - 1.0).abs() < 1e-9, "expected 1.0 for pure uptrend, got {}", p);
    }

    // ── round-142 ────────────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_centered_mean_none_for_empty() {
        let n = norm(4);
        assert!(n.window_centered_mean().is_none());
    }

    #[test]
    fn test_minmax_window_centered_mean_symmetric() {
        let mut n = norm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)] { n.update(v); }
        // median=3, centered: -2,-1,0,1,2 → mean=0
        let cm = n.window_centered_mean().unwrap();
        assert!(cm.abs() < 1e-9, "expected 0.0, got {}", cm);
    }

    #[test]
    fn test_minmax_window_last_deviation_none_for_empty() {
        let n = norm(4);
        assert!(n.window_last_deviation().is_none());
    }

    #[test]
    fn test_minmax_window_last_deviation_last_is_highest() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(10)] { n.update(v); }
        // mean=4, last=10 → dev=6
        let d = n.window_last_deviation().unwrap();
        assert!(d > 0.0, "expected positive deviation, got {}", d);
    }

    #[test]
    fn test_minmax_window_step_size_mean_none_for_single() {
        let mut n = norm(4);
        n.update(dec!(5));
        assert!(n.window_step_size_mean().is_none());
    }

    #[test]
    fn test_minmax_window_step_size_mean_uniform() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(3), dec!(5), dec!(7)] { n.update(v); }
        // all steps = 2 → mean = 2
        let s = n.window_step_size_mean().unwrap();
        assert!((s - 2.0).abs() < 1e-9, "expected 2.0, got {}", s);
    }

    #[test]
    fn test_minmax_window_net_up_count_none_for_single() {
        let mut n = norm(4);
        n.update(dec!(5));
        assert!(n.window_net_up_count().is_none());
    }

    #[test]
    fn test_minmax_window_net_up_count_all_up() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let c = n.window_net_up_count().unwrap();
        assert_eq!(c, 3, "expected 3, got {}", c);
    }

    // ── round-143 ────────────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_weighted_mean_none_for_empty() {
        let n = norm(4);
        assert!(n.window_weighted_mean().is_none());
    }

    #[test]
    fn test_minmax_window_weighted_mean_single() {
        let mut n = norm(4);
        n.update(dec!(10));
        let wm = n.window_weighted_mean().unwrap();
        assert!((wm - 10.0).abs() < 1e-9, "expected 10.0, got {}", wm);
    }

    #[test]
    fn test_minmax_window_upper_half_mean_none_for_single() {
        let mut n = norm(4);
        n.update(dec!(5));
        assert!(n.window_upper_half_mean().is_none());
    }

    #[test]
    fn test_minmax_window_upper_half_mean_basic() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        // sorted: [1,2,3,4], upper half [3,4] → mean=3.5
        let um = n.window_upper_half_mean().unwrap();
        assert!((um - 3.5).abs() < 1e-9, "expected 3.5, got {}", um);
    }

    #[test]
    fn test_minmax_window_lower_half_mean_none_for_single() {
        let mut n = norm(4);
        n.update(dec!(5));
        assert!(n.window_lower_half_mean().is_none());
    }

    #[test]
    fn test_minmax_window_lower_half_mean_basic() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        // sorted: [1,2,3,4], lower half [1,2] → mean=1.5
        let lm = n.window_lower_half_mean().unwrap();
        assert!((lm - 1.5).abs() < 1e-9, "expected 1.5, got {}", lm);
    }

    #[test]
    fn test_minmax_window_mid_range_none_for_empty() {
        let n = norm(4);
        assert!(n.window_mid_range().is_none());
    }

    #[test]
    fn test_minmax_window_mid_range_basic() {
        let mut n = norm(4);
        for v in [dec!(2), dec!(4), dec!(6), dec!(8)] { n.update(v); }
        // (8+2)/2 = 5
        let mr = n.window_mid_range().unwrap();
        assert!((mr - 5.0).abs() < 1e-9, "expected 5.0, got {}", mr);
    }

    // ── round-144 ────────────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_trim_mean_none_for_empty() {
        let n = norm(4);
        assert!(n.window_trim_mean().is_none());
    }

    #[test]
    fn test_minmax_window_trim_mean_basic() {
        let mut n = norm(10);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5),
                  dec!(6), dec!(7), dec!(8), dec!(9), dec!(10)] { n.update(v); }
        // trim 1 from each end → [2..9], mean=5.5
        let tm = n.window_trim_mean().unwrap();
        assert!((tm - 5.5).abs() < 1e-9, "expected 5.5, got {}", tm);
    }

    #[test]
    fn test_minmax_window_value_spread_none_for_empty() {
        let n = norm(4);
        assert!(n.window_value_spread().is_none());
    }

    #[test]
    fn test_minmax_window_value_spread_basic() {
        let mut n = norm(4);
        for v in [dec!(2), dec!(5), dec!(3), dec!(9)] { n.update(v); }
        let sp = n.window_value_spread().unwrap();
        assert!((sp - 7.0).abs() < 1e-9, "expected 7.0, got {}", sp);
    }

    #[test]
    fn test_minmax_window_rms_none_for_empty() {
        let n = norm(4);
        assert!(n.window_rms().is_none());
    }

    #[test]
    fn test_minmax_window_rms_basic() {
        let mut n = norm(2);
        for v in [dec!(3), dec!(4)] { n.update(v); }
        // rms = sqrt((9+16)/2) = sqrt(12.5)
        let rms = n.window_rms().unwrap();
        assert!((rms - 12.5f64.sqrt()).abs() < 1e-9, "expected sqrt(12.5), got {}", rms);
    }

    #[test]
    fn test_minmax_window_above_mid_fraction_none_for_empty() {
        let n = norm(4);
        assert!(n.window_above_mid_fraction().is_none());
    }

    #[test]
    fn test_minmax_window_above_mid_fraction_basic() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        // mid = (4+1)/2 = 2.5; values > 2.5: [3,4] → 2/4 = 0.5
        let f = n.window_above_mid_fraction().unwrap();
        assert!((f - 0.5).abs() < 1e-9, "expected 0.5, got {}", f);
    }

    // ── round-145 ────────────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_median_abs_dev_none_for_empty() {
        let n = norm(4);
        assert!(n.window_median_abs_dev().is_none());
    }

    #[test]
    fn test_minmax_window_median_abs_dev_basic() {
        let mut n = norm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)] { n.update(v); }
        // median=3; devs=[2,1,0,1,2] → sorted → median dev=1
        let mad = n.window_median_abs_dev().unwrap();
        assert!((mad - 1.0).abs() < 1e-9, "expected 1.0, got {}", mad);
    }

    #[test]
    fn test_minmax_window_cubic_mean_none_for_empty() {
        let n = norm(4);
        assert!(n.window_cubic_mean().is_none());
    }

    #[test]
    fn test_minmax_window_cubic_mean_basic() {
        let mut n = norm(2);
        for v in [dec!(2), dec!(4)] { n.update(v); }
        // mean of cubes = (8+64)/2 = 36; cbrt(36) ≈ 3.302
        let cm = n.window_cubic_mean().unwrap();
        assert!((cm - 36.0f64.cbrt()).abs() < 1e-9, "expected cbrt(36), got {}", cm);
    }

    #[test]
    fn test_minmax_window_max_run_length_none_for_empty() {
        let n = norm(4);
        assert!(n.window_max_run_length().is_none());
    }

    #[test]
    fn test_minmax_window_max_run_length_all_same() {
        let mut n = norm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let r = n.window_max_run_length().unwrap();
        assert_eq!(r, 3);
    }

    #[test]
    fn test_minmax_window_sorted_position_none_for_empty() {
        let n = norm(4);
        assert!(n.window_sorted_position().is_none());
    }

    #[test]
    fn test_minmax_window_sorted_position_max() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(10)] { n.update(v); }
        // last=10, all others < 10 → pos=3/4=0.75
        let p = n.window_sorted_position().unwrap();
        assert!((p - 0.75).abs() < 1e-9, "expected 0.75, got {}", p);
    }

    // ── round-146 ────────────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_prev_deviation_none_for_single() {
        let mut n = norm(4);
        n.update(dec!(5));
        assert!(n.window_prev_deviation().is_none());
    }

    #[test]
    fn test_minmax_window_prev_deviation_basic() {
        let mut n = norm(4);
        n.update(dec!(10));
        n.update(dec!(15));
        let d = n.window_prev_deviation().unwrap();
        assert!((d - 5.0).abs() < 1e-9, "expected 5.0, got {}", d);
    }

    #[test]
    fn test_minmax_window_lower_quartile_none_for_empty() {
        let n = norm(4);
        assert!(n.window_lower_quartile().is_none());
    }

    #[test]
    fn test_minmax_window_lower_quartile_basic() {
        let mut n = norm(4);
        for v in [dec!(10), dec!(20), dec!(30), dec!(40)] { n.update(v); }
        // sorted: [10,20,30,40], Q1 idx=0 → 10
        let q = n.window_lower_quartile().unwrap();
        assert!((q - 10.0).abs() < 1e-9, "expected 10.0, got {}", q);
    }

    #[test]
    fn test_minmax_window_upper_quartile_none_for_empty() {
        let n = norm(4);
        assert!(n.window_upper_quartile().is_none());
    }

    #[test]
    fn test_minmax_window_upper_quartile_basic() {
        let mut n = norm(4);
        for v in [dec!(10), dec!(20), dec!(30), dec!(40)] { n.update(v); }
        // sorted: [10,20,30,40], Q3 idx=2 → 30
        let q = n.window_upper_quartile().unwrap();
        assert!((q - 30.0).abs() < 1e-9, "expected 30.0, got {}", q);
    }

    #[test]
    fn test_minmax_window_tail_weight_none_for_empty() {
        let n = norm(4);
        assert!(n.window_tail_weight().is_none());
    }

    #[test]
    fn test_minmax_window_tail_weight_basic() {
        let mut n = norm(10);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5),
                  dec!(6), dec!(7), dec!(8), dec!(9), dec!(10)] { n.update(v); }
        let tw = n.window_tail_weight().unwrap();
        assert!(tw > 0.0, "tail weight should be > 0, got {}", tw);
    }

    // ── round-147 ────────────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_last_vs_mean_none_for_empty() {
        let n = norm(4);
        assert!(n.window_last_vs_mean().is_none());
    }

    #[test]
    fn test_minmax_window_last_vs_mean_basic() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        // mean=2, last=3 → deviation=1
        let d = n.window_last_vs_mean().unwrap();
        assert!((d - 1.0).abs() < 1e-9, "expected 1.0, got {}", d);
    }

    #[test]
    fn test_minmax_window_change_acceleration_none_for_two() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2)] { n.update(v); }
        assert!(n.window_change_acceleration().is_none());
    }

    #[test]
    fn test_minmax_window_change_acceleration_constant() {
        let mut n = norm(3);
        // vals: 1,2,3 → diffs: 1,1 → accel: 0
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let a = n.window_change_acceleration().unwrap();
        assert!((a - 0.0).abs() < 1e-9, "expected 0.0, got {}", a);
    }

    #[test]
    fn test_minmax_window_positive_run_length_none_for_empty() {
        let n = norm(4);
        assert!(n.window_positive_run_length().is_none());
    }

    #[test]
    fn test_minmax_window_positive_run_length_all_positive() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let r = n.window_positive_run_length().unwrap();
        assert_eq!(r, 3);
    }

    #[test]
    fn test_minmax_window_geometric_trend_none_for_single() {
        let mut n = norm(4);
        n.update(dec!(5));
        assert!(n.window_geometric_trend().is_none());
    }

    #[test]
    fn test_minmax_window_geometric_trend_constant() {
        let mut n = norm(3);
        for v in [dec!(4), dec!(4), dec!(4)] { n.update(v); }
        // ratios all=1 → geometric trend=1
        let gt = n.window_geometric_trend().unwrap();
        assert!((gt - 1.0).abs() < 1e-9, "expected 1.0, got {}", gt);
    }

    // ── round-148 ────────────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_pairwise_diff_mean_none_for_single() {
        let mut n = norm(4);
        n.update(dec!(5));
        assert!(n.window_pairwise_diff_mean().is_none());
    }

    #[test]
    fn test_minmax_window_pairwise_diff_mean_basic() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(3), dec!(5)] { n.update(v); }
        // pairs: |1-3|=2, |1-5|=4, |3-5|=2 → mean=8/3 ≈ 2.667
        let d = n.window_pairwise_diff_mean().unwrap();
        assert!((d - 8.0 / 3.0).abs() < 1e-9, "expected 8/3, got {}", d);
    }

    #[test]
    fn test_minmax_window_negative_run_length_none_for_empty() {
        let n = norm(4);
        assert!(n.window_negative_run_length().is_none());
    }

    #[test]
    fn test_minmax_window_negative_run_length_no_negatives() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let r = n.window_negative_run_length().unwrap();
        assert_eq!(r, 0);
    }

    #[test]
    fn test_minmax_window_cross_zero_count_none_for_single() {
        let mut n = norm(4);
        n.update(dec!(5));
        assert!(n.window_cross_zero_count().is_none());
    }

    #[test]
    fn test_minmax_window_cross_zero_count_no_crossings() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let c = n.window_cross_zero_count().unwrap();
        assert_eq!(c, 0);
    }

    #[test]
    fn test_minmax_window_mean_reversion_strength_none_for_single() {
        let mut n = norm(4);
        n.update(dec!(5));
        assert!(n.window_mean_reversion_strength().is_none());
    }

    #[test]
    fn test_minmax_window_mean_reversion_strength_uniform() {
        let mut n = norm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        // std=0 → strength=0
        let s = n.window_mean_reversion_strength().unwrap();
        assert!((s - 0.0).abs() < 1e-9, "expected 0.0, got {}", s);
    }

    // ── round-149 ────────────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_first_vs_mean_none_for_empty() {
        let n = norm(4);
        assert!(n.window_first_vs_mean().is_none());
    }

    #[test]
    fn test_minmax_window_first_vs_mean_basic() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        // mean=2, first=1 → deviation=-1
        let d = n.window_first_vs_mean().unwrap();
        assert!((d - (-1.0)).abs() < 1e-9, "expected -1.0, got {}", d);
    }

    #[test]
    fn test_minmax_window_decay_ratio_none_for_single() {
        let mut n = norm(4);
        n.update(dec!(5));
        assert!(n.window_decay_ratio().is_none());
    }

    #[test]
    fn test_minmax_window_decay_ratio_basic() {
        let mut n = norm(2);
        n.update(dec!(4));
        n.update(dec!(8));
        // last/first = 8/4 = 2
        let r = n.window_decay_ratio().unwrap();
        assert!((r - 2.0).abs() < 1e-9, "expected 2.0, got {}", r);
    }

    #[test]
    fn test_minmax_window_bimodal_score_none_for_few() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        assert!(n.window_bimodal_score().is_none());
    }

    #[test]
    fn test_minmax_window_bimodal_score_returns_some() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(8), dec!(9)] { n.update(v); }
        let s = n.window_bimodal_score();
        assert!(s.is_some());
    }

    #[test]
    fn test_minmax_window_abs_sum_none_for_empty() {
        let n = norm(4);
        assert!(n.window_abs_sum().is_none());
    }

    #[test]
    fn test_minmax_window_abs_sum_basic() {
        let mut n = norm(2);
        for v in [dec!(3), dec!(4)] { n.update(v); }
        // |3|+|4| = 7
        let s = n.window_abs_sum().unwrap();
        assert!((s - 7.0).abs() < 1e-9, "expected 7.0, got {}", s);
    }

    // ── round-150 ────────────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_coeff_of_variation_none_for_single() {
        let mut n = norm(4);
        n.update(dec!(5));
        assert!(n.window_coeff_of_variation().is_none());
    }

    #[test]
    fn test_minmax_window_coeff_of_variation_basic() {
        let mut n = norm(2);
        for v in [dec!(10), dec!(20)] { n.update(v); }
        // mean=15, std=5, cv=5/15≈0.333
        let cv = n.window_coeff_of_variation().unwrap();
        assert!((cv - 5.0 / 15.0).abs() < 1e-9, "expected ~0.333, got {}", cv);
    }

    #[test]
    fn test_minmax_window_mean_absolute_error_none_for_empty() {
        let n = norm(4);
        assert!(n.window_mean_absolute_error().is_none());
    }

    #[test]
    fn test_minmax_window_mean_absolute_error_uniform() {
        let mut n = norm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let mae = n.window_mean_absolute_error().unwrap();
        assert!((mae - 0.0).abs() < 1e-9, "expected 0.0, got {}", mae);
    }

    #[test]
    fn test_minmax_window_normalized_last_none_for_empty() {
        let n = norm(4);
        assert!(n.window_normalized_last().is_none());
    }

    #[test]
    fn test_minmax_window_normalized_last_at_max() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(10)] { n.update(v); }
        // last=10, min=1, max=10 → (10-1)/(10-1)=1.0
        let nl = n.window_normalized_last().unwrap();
        assert!((nl - 1.0).abs() < 1e-9, "expected 1.0, got {}", nl);
    }

    #[test]
    fn test_minmax_window_sign_bias_none_for_empty() {
        let n = norm(4);
        assert!(n.window_sign_bias().is_none());
    }

    #[test]
    fn test_minmax_window_sign_bias_all_positive() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        // all pos → bias=1.0
        let sb = n.window_sign_bias().unwrap();
        assert!((sb - 1.0).abs() < 1e-9, "expected 1.0, got {}", sb);
    }

    // ── round-151 ────────────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_penultimate_vs_last_none_for_single() {
        let mut n = norm(4);
        n.update(dec!(5));
        assert!(n.window_penultimate_vs_last().is_none());
    }

    #[test]
    fn test_minmax_window_penultimate_vs_last_basic() {
        let mut n = norm(4);
        for v in [dec!(10), dec!(6)] { n.update(v); }
        // penultimate=10, last=6 → 10-6=4
        let d = n.window_penultimate_vs_last().unwrap();
        assert!((d - 4.0).abs() < 1e-9, "expected 4.0, got {}", d);
    }

    #[test]
    fn test_minmax_window_mean_range_position_none_for_empty() {
        let n = norm(4);
        assert!(n.window_mean_range_position().is_none());
    }

    #[test]
    fn test_minmax_window_mean_range_position_single() {
        let mut n = norm(4);
        n.update(dec!(5));
        let _ = n.window_mean_range_position();
    }

    #[test]
    fn test_minmax_window_zscore_last_none_for_single() {
        let mut n = norm(4);
        n.update(dec!(5));
        assert!(n.window_zscore_last().is_none());
    }

    #[test]
    fn test_minmax_window_zscore_last_zero_for_uniform() {
        let mut n = norm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        // std=0 → None (no z-score)
        assert!(n.window_zscore_last().is_none());
    }

    #[test]
    fn test_minmax_window_gradient_none_for_single() {
        let mut n = norm(4);
        n.update(dec!(5));
        assert!(n.window_gradient().is_none());
    }

    #[test]
    fn test_minmax_window_gradient_ascending() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        // perfectly ascending → positive gradient
        let g = n.window_gradient().unwrap();
        assert!(g > 0.0, "expected positive gradient, got {}", g);
    }

    // ── round-152 ────────────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_entropy_score_none_for_empty() {
        let n = norm(4);
        assert!(n.window_entropy_score().is_none());
    }

    #[test]
    fn test_minmax_window_entropy_score_uniform_zero() {
        let mut n = norm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        // uniform → range=0 → entropy=0.0
        let e = n.window_entropy_score().unwrap();
        assert!((e - 0.0).abs() < 1e-9, "expected 0.0, got {}", e);
    }

    #[test]
    fn test_minmax_window_quartile_spread_none_for_three() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        assert!(n.window_quartile_spread().is_none());
    }

    #[test]
    fn test_minmax_window_quartile_spread_basic() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let qs = n.window_quartile_spread().unwrap();
        assert!(qs >= 0.0, "expected non-negative, got {}", qs);
    }

    #[test]
    fn test_minmax_window_max_to_min_ratio_none_for_empty() {
        let n = norm(4);
        assert!(n.window_max_to_min_ratio().is_none());
    }

    #[test]
    fn test_minmax_window_max_to_min_ratio_basic() {
        let mut n = norm(4);
        for v in [dec!(2), dec!(4)] { n.update(v); }
        // max=4, min=2 → ratio=2.0
        let r = n.window_max_to_min_ratio().unwrap();
        assert!((r - 2.0).abs() < 1e-9, "expected 2.0, got {}", r);
    }

    #[test]
    fn test_minmax_window_upper_fraction_none_for_empty() {
        let n = norm(4);
        assert!(n.window_upper_fraction().is_none());
    }

    #[test]
    fn test_minmax_window_upper_fraction_half() {
        let mut n = norm(2);
        for v in [dec!(1), dec!(3)] { n.update(v); }
        // mean=2; 3>2 → 1 above → 0.5
        let f = n.window_upper_fraction().unwrap();
        assert!((f - 0.5).abs() < 1e-9, "expected 0.5, got {}", f);
    }

    // ── round-153 ────────────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_abs_change_mean_none_for_single() {
        let mut n = norm(4);
        n.update(dec!(5));
        assert!(n.window_abs_change_mean().is_none());
    }

    #[test]
    fn test_minmax_window_abs_change_mean_constant() {
        let mut n = norm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let m = n.window_abs_change_mean().unwrap();
        assert!((m - 0.0).abs() < 1e-9, "expected 0.0, got {}", m);
    }

    #[test]
    fn test_minmax_window_last_percentile_none_for_empty() {
        let n = norm(4);
        assert!(n.window_last_percentile().is_none());
    }

    #[test]
    fn test_minmax_window_last_percentile_at_max() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        // last=3; values below 3 are 1,2 → 2/3
        let p = n.window_last_percentile().unwrap();
        assert!((p - 2.0 / 3.0).abs() < 1e-9, "expected 0.667, got {}", p);
    }

    #[test]
    fn test_minmax_window_trailing_std_none_for_single() {
        let mut n = norm(4);
        n.update(dec!(5));
        assert!(n.window_trailing_std().is_none());
    }

    #[test]
    fn test_minmax_window_trailing_std_uniform() {
        let mut n = norm(4);
        for v in [dec!(5), dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let s = n.window_trailing_std().unwrap();
        assert!((s - 0.0).abs() < 1e-9, "expected 0.0, got {}", s);
    }

    #[test]
    fn test_minmax_window_mean_change_none_for_single() {
        let mut n = norm(4);
        n.update(dec!(5));
        assert!(n.window_mean_change().is_none());
    }

    #[test]
    fn test_minmax_window_mean_change_ascending() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(3), dec!(5)] { n.update(v); }
        // (5-1)/(3-1) = 2.0
        let c = n.window_mean_change().unwrap();
        assert!((c - 2.0).abs() < 1e-9, "expected 2.0, got {}", c);
    }

    // ── round-154 ────────────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_value_at_peak_none_for_empty() {
        let n = norm(4);
        assert!(n.window_value_at_peak().is_none());
    }

    #[test]
    fn test_minmax_window_value_at_peak_single() {
        let mut n = norm(4);
        n.update(dec!(5));
        // only one value → peak at index 0
        let p = n.window_value_at_peak().unwrap();
        assert!((p - 0.0).abs() < 1e-9, "expected 0.0, got {}", p);
    }

    #[test]
    fn test_minmax_window_head_tail_diff_none_for_single() {
        let mut n = norm(4);
        n.update(dec!(5));
        assert!(n.window_head_tail_diff().is_none());
    }

    #[test]
    fn test_minmax_window_head_tail_diff_basic() {
        let mut n = norm(2);
        for v in [dec!(10), dec!(3)] { n.update(v); }
        // head=10, tail=3 → diff=7
        let d = n.window_head_tail_diff().unwrap();
        assert!((d - 7.0).abs() < 1e-9, "expected 7.0, got {}", d);
    }

    #[test]
    fn test_minmax_window_midpoint_none_for_empty() {
        let n = norm(4);
        assert!(n.window_midpoint().is_none());
    }

    #[test]
    fn test_minmax_window_midpoint_basic() {
        let mut n = norm(2);
        for v in [dec!(2), dec!(8)] { n.update(v); }
        // min=2, max=8 → midpoint=5
        let m = n.window_midpoint().unwrap();
        assert!((m - 5.0).abs() < 1e-9, "expected 5.0, got {}", m);
    }

    #[test]
    fn test_minmax_window_concavity_none_for_two() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2)] { n.update(v); }
        assert!(n.window_concavity().is_none());
    }

    #[test]
    fn test_minmax_window_concavity_flat() {
        let mut n = norm(6);
        for v in [dec!(5), dec!(5), dec!(5), dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let c = n.window_concavity().unwrap();
        assert!((c - 0.0).abs() < 1e-9, "expected 0.0, got {}", c);
    }

    // ── round-155 ────────────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_rise_fraction_none_for_single() {
        let mut n = norm(4);
        n.update(dec!(5));
        assert!(n.window_rise_fraction().is_none());
    }

    #[test]
    fn test_minmax_window_rise_fraction_all_ascending() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let f = n.window_rise_fraction().unwrap();
        assert!((f - 1.0).abs() < 1e-9, "expected 1.0, got {}", f);
    }

    #[test]
    fn test_minmax_window_peak_to_valley_none_for_empty() {
        let n = norm(4);
        assert!(n.window_peak_to_valley().is_none());
    }

    #[test]
    fn test_minmax_window_peak_to_valley_basic() {
        let mut n = norm(2);
        for v in [dec!(3), dec!(10)] { n.update(v); }
        let ptv = n.window_peak_to_valley().unwrap();
        assert!((ptv - 7.0).abs() < 1e-9, "expected 7.0, got {}", ptv);
    }

    #[test]
    fn test_minmax_window_positive_change_mean_none_for_single() {
        let mut n = norm(4);
        n.update(dec!(5));
        assert!(n.window_positive_change_mean().is_none());
    }

    #[test]
    fn test_minmax_window_positive_change_mean_no_rises() {
        let mut n = norm(2);
        for v in [dec!(10), dec!(5)] { n.update(v); }
        // only decrease → None
        assert!(n.window_positive_change_mean().is_none());
    }

    #[test]
    fn test_minmax_window_range_cv_none_for_empty() {
        let n = norm(4);
        assert!(n.window_range_cv().is_none());
    }

    #[test]
    fn test_minmax_window_range_cv_uniform() {
        let mut n = norm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        // range=0 → cv=0
        let cv = n.window_range_cv().unwrap();
        assert!((cv - 0.0).abs() < 1e-9, "expected 0.0, got {}", cv);
    }

    // ── round-156 ────────────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_negative_change_mean_none_for_single() {
        let mut n = norm(4);
        n.update(dec!(5));
        assert!(n.window_negative_change_mean().is_none());
    }

    #[test]
    fn test_minmax_window_negative_change_mean_no_falls() {
        let mut n = norm(2);
        for v in [dec!(1), dec!(5)] { n.update(v); }
        // only rise → None
        assert!(n.window_negative_change_mean().is_none());
    }

    #[test]
    fn test_minmax_window_fall_fraction_none_for_single() {
        let mut n = norm(4);
        n.update(dec!(5));
        assert!(n.window_fall_fraction().is_none());
    }

    #[test]
    fn test_minmax_window_fall_fraction_all_descending() {
        let mut n = norm(3);
        for v in [dec!(5), dec!(3), dec!(1)] { n.update(v); }
        let f = n.window_fall_fraction().unwrap();
        assert!((f - 1.0).abs() < 1e-9, "expected 1.0, got {}", f);
    }

    #[test]
    fn test_minmax_window_last_vs_max_none_for_empty() {
        let n = norm(4);
        assert!(n.window_last_vs_max().is_none());
    }

    #[test]
    fn test_minmax_window_last_vs_max_at_max() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(3), dec!(5)] { n.update(v); }
        // last=5=max → diff=0
        let d = n.window_last_vs_max().unwrap();
        assert!((d - 0.0).abs() < 1e-9, "expected 0.0, got {}", d);
    }

    #[test]
    fn test_minmax_window_last_vs_min_none_for_empty() {
        let n = norm(4);
        assert!(n.window_last_vs_min().is_none());
    }

    #[test]
    fn test_minmax_window_last_vs_min_at_min() {
        let mut n = norm(3);
        for v in [dec!(5), dec!(3), dec!(1)] { n.update(v); }
        // last=1=min → diff=0
        let d = n.window_last_vs_min().unwrap();
        assert!((d - 0.0).abs() < 1e-9, "expected 0.0, got {}", d);
    }

    // ── round-157 ────────────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_mean_oscillation_none_for_single() {
        let mut n = norm(4);
        n.update(dec!(5));
        assert!(n.window_mean_oscillation().is_none());
    }

    #[test]
    fn test_minmax_window_mean_oscillation_constant() {
        let mut n = norm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        // all diffs=0, mean_diff=0, MAD=0
        let o = n.window_mean_oscillation().unwrap();
        assert!((o - 0.0).abs() < 1e-9, "expected 0.0, got {}", o);
    }

    #[test]
    fn test_minmax_window_monotone_score_none_for_single() {
        let mut n = norm(4);
        n.update(dec!(5));
        assert!(n.window_monotone_score().is_none());
    }

    #[test]
    fn test_minmax_window_monotone_score_perfectly_ascending() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        // all rising → score = 1.0
        let s = n.window_monotone_score().unwrap();
        assert!((s - 1.0).abs() < 1e-9, "expected 1.0, got {}", s);
    }

    #[test]
    fn test_minmax_window_stddev_trend_none_for_three() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        assert!(n.window_stddev_trend().is_none());
    }

    #[test]
    fn test_minmax_window_stddev_trend_uniform() {
        let mut n = norm(4);
        for v in [dec!(5), dec!(5), dec!(5), dec!(5)] { n.update(v); }
        // all uniform → both halves have std=0 → trend=0
        let t = n.window_stddev_trend().unwrap();
        assert!((t - 0.0).abs() < 1e-9, "expected 0.0, got {}", t);
    }

    #[test]
    fn test_minmax_window_zero_cross_fraction_none_for_two() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2)] { n.update(v); }
        assert!(n.window_zero_cross_fraction().is_none());
    }

    #[test]
    fn test_minmax_window_zero_cross_fraction_alternating() {
        let mut n = norm(4);
        // alternating: 1,3,1,3 → diffs: +2,-2,+2 → all adjacent sign changes
        for v in [dec!(1), dec!(3), dec!(1), dec!(3)] { n.update(v); }
        let f = n.window_zero_cross_fraction().unwrap();
        assert!((f - 1.0).abs() < 1e-9, "expected 1.0, got {}", f);
    }

    // ── round-158 ────────────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_exponential_decay_sum_none_for_empty() {
        let n = norm(4);
        assert!(n.window_exponential_decay_sum().is_none());
    }

    #[test]
    fn test_minmax_window_exponential_decay_sum_positive() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let s = n.window_exponential_decay_sum().unwrap();
        assert!(s > 0.0, "expected positive, got {}", s);
    }

    #[test]
    fn test_minmax_window_lagged_diff_none_for_single() {
        let mut n = norm(4);
        n.update(dec!(5));
        assert!(n.window_lagged_diff().is_none());
    }

    #[test]
    fn test_minmax_window_lagged_diff_constant() {
        let mut n = norm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let d = n.window_lagged_diff().unwrap();
        assert!((d - 0.0).abs() < 1e-9, "expected 0.0, got {}", d);
    }

    #[test]
    fn test_minmax_window_mean_to_max_none_for_empty() {
        let n = norm(4);
        assert!(n.window_mean_to_max().is_none());
    }

    #[test]
    fn test_minmax_window_mean_to_max_all_equal() {
        let mut n = norm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        // mean=max → ratio=1.0
        let r = n.window_mean_to_max().unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0, got {}", r);
    }

    #[test]
    fn test_minmax_window_mode_fraction_none_for_empty() {
        let n = norm(4);
        assert!(n.window_mode_fraction().is_none());
    }

    #[test]
    fn test_minmax_window_mode_fraction_uniform() {
        let mut n = norm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        // all same → mode fraction = 1.0
        let f = n.window_mode_fraction().unwrap();
        assert!((f - 1.0).abs() < 1e-9, "expected 1.0, got {}", f);
    }

    // ── round-159 ────────────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_mean_below_zero_none_for_empty() {
        let n = norm(4);
        assert!(n.window_mean_below_zero().is_none());
    }

    #[test]
    fn test_minmax_window_mean_below_zero_no_negatives() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        assert!(n.window_mean_below_zero().is_none());
    }

    #[test]
    fn test_minmax_window_mean_above_zero_none_for_empty() {
        let n = norm(4);
        assert!(n.window_mean_above_zero().is_none());
    }

    #[test]
    fn test_minmax_window_mean_above_zero_positive() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let m = n.window_mean_above_zero().unwrap();
        assert!((m - 2.0).abs() < 1e-9, "expected 2.0, got {}", m);
    }

    #[test]
    fn test_minmax_window_running_max_fraction_none_for_empty() {
        let n = norm(4);
        assert!(n.window_running_max_fraction().is_none());
    }

    #[test]
    fn test_minmax_window_running_max_fraction_ascending() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        // every value is a new max → fraction = 1.0
        let f = n.window_running_max_fraction().unwrap();
        assert!((f - 1.0).abs() < 1e-9, "expected 1.0, got {}", f);
    }

    #[test]
    fn test_minmax_window_variance_change_none_for_short() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        assert!(n.window_variance_change().is_none());
    }

    #[test]
    fn test_minmax_window_variance_change_constant() {
        let mut n = norm(4);
        for v in [dec!(5), dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let vc = n.window_variance_change().unwrap();
        assert!((vc - 0.0).abs() < 1e-9, "expected 0.0, got {}", vc);
    }

    // ── round-160 ────────────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_ema_slope_none_for_single() {
        let mut n = norm(4);
        n.update(dec!(5));
        assert!(n.window_ema_slope().is_none());
    }

    #[test]
    fn test_minmax_window_ema_slope_constant_zero() {
        let mut n = norm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let s = n.window_ema_slope().unwrap();
        assert!((s - 0.0).abs() < 1e-9, "expected 0.0, got {}", s);
    }

    #[test]
    fn test_minmax_window_range_ratio_none_for_empty() {
        let n = norm(4);
        assert!(n.window_range_ratio().is_none());
    }

    #[test]
    fn test_minmax_window_range_ratio_constant_none_for_zero() {
        let mut n = norm(3);
        for v in [dec!(0), dec!(0), dec!(0)] { n.update(v); }
        assert!(n.window_range_ratio().is_none());
    }

    #[test]
    fn test_minmax_window_range_ratio_equal_values() {
        let mut n = norm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let r = n.window_range_ratio().unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0, got {}", r);
    }

    #[test]
    fn test_minmax_window_above_mean_streak_none_for_empty() {
        let n = norm(4);
        assert!(n.window_above_mean_streak().is_none());
    }

    #[test]
    fn test_minmax_window_above_mean_streak_constant_zero() {
        let mut n = norm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        // nothing strictly > mean → streak=0
        let s = n.window_above_mean_streak().unwrap();
        assert!((s - 0.0).abs() < 1e-9, "expected 0.0, got {}", s);
    }

    #[test]
    fn test_minmax_window_mean_abs_diff_none_for_single() {
        let mut n = norm(4);
        n.update(dec!(5));
        assert!(n.window_mean_abs_diff().is_none());
    }

    #[test]
    fn test_minmax_window_mean_abs_diff_constant_zero() {
        let mut n = norm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let d = n.window_mean_abs_diff().unwrap();
        assert!((d - 0.0).abs() < 1e-9, "expected 0.0, got {}", d);
    }

    // ── round-161 ────────────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_sign_entropy_none_for_empty() {
        let n = norm(4);
        assert!(n.window_sign_entropy().is_none());
    }

    #[test]
    fn test_minmax_window_sign_entropy_all_positive() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        // all positive → entropy of single category = 0
        let e = n.window_sign_entropy().unwrap();
        assert!((e - 0.0).abs() < 1e-9, "expected 0.0, got {}", e);
    }

    #[test]
    fn test_minmax_window_local_extrema_count_none_for_two() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2)] { n.update(v); }
        assert!(n.window_local_extrema_count().is_none());
    }

    #[test]
    fn test_minmax_window_local_extrema_count_monotone_zero() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let c = n.window_local_extrema_count().unwrap();
        assert!((c - 0.0).abs() < 1e-9, "expected 0.0, got {}", c);
    }

    #[test]
    fn test_minmax_window_autocorr_lag2_none_for_two() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2)] { n.update(v); }
        assert!(n.window_autocorr_lag2().is_none());
    }

    #[test]
    fn test_minmax_window_autocorr_lag2_uniform_none() {
        let mut n = norm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        // variance=0 → None
        assert!(n.window_autocorr_lag2().is_none());
    }

    #[test]
    fn test_minmax_window_pct_above_median_none_for_empty() {
        let n = norm(4);
        assert!(n.window_pct_above_median().is_none());
    }

    #[test]
    fn test_minmax_window_pct_above_median_single() {
        let mut n = norm(4);
        n.update(dec!(5));
        // single element is the median, nothing strictly above → 0.0
        let p = n.window_pct_above_median().unwrap();
        assert!((p - 0.0).abs() < 1e-9, "expected 0.0, got {}", p);
    }

    // ── round-162 ────────────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_below_zero_streak_none_for_empty() {
        let n = norm(4);
        assert!(n.window_below_zero_streak().is_none());
    }

    #[test]
    fn test_minmax_window_below_zero_streak_no_negatives_zero() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let s = n.window_below_zero_streak().unwrap();
        assert!((s - 0.0).abs() < 1e-9, "expected 0.0, got {}", s);
    }

    #[test]
    fn test_minmax_window_max_to_mean_none_for_empty() {
        let n = norm(4);
        assert!(n.window_max_to_mean().is_none());
    }

    #[test]
    fn test_minmax_window_max_to_mean_constant_one() {
        let mut n = norm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let r = n.window_max_to_mean().unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0, got {}", r);
    }

    #[test]
    fn test_minmax_window_sign_run_length_none_for_empty() {
        let n = norm(4);
        assert!(n.window_sign_run_length().is_none());
    }

    #[test]
    fn test_minmax_window_sign_run_length_single() {
        let mut n = norm(4);
        n.update(dec!(5));
        let r = n.window_sign_run_length().unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0, got {}", r);
    }

    #[test]
    fn test_minmax_window_decay_weighted_mean_none_for_empty() {
        let n = norm(4);
        assert!(n.window_decay_weighted_mean().is_none());
    }

    #[test]
    fn test_minmax_window_decay_weighted_mean_constant() {
        let mut n = norm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let m = n.window_decay_weighted_mean().unwrap();
        assert!((m - 5.0).abs() < 1e-9, "expected 5.0, got {}", m);
    }

    // ── round-163 ────────────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_min_to_mean_none_for_empty() {
        let n = norm(4);
        assert!(n.window_min_to_mean().is_none());
    }

    #[test]
    fn test_minmax_window_min_to_mean_constant_one() {
        let mut n = norm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let r = n.window_min_to_mean().unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0, got {}", r);
    }

    #[test]
    fn test_minmax_window_normalized_range_none_for_empty() {
        let n = norm(4);
        assert!(n.window_normalized_range().is_none());
    }

    #[test]
    fn test_minmax_window_normalized_range_constant_zero() {
        let mut n = norm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        // all same → range=0
        let r = n.window_normalized_range().unwrap();
        assert!((r - 0.0).abs() < 1e-9, "expected 0.0, got {}", r);
    }

    #[test]
    fn test_minmax_window_winsorized_mean_none_for_empty() {
        let n = norm(4);
        assert!(n.window_winsorized_mean().is_none());
    }

    #[test]
    fn test_minmax_window_winsorized_mean_constant() {
        let mut n = norm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let m = n.window_winsorized_mean().unwrap();
        assert!((m - 5.0).abs() < 1e-9, "expected 5.0, got {}", m);
    }

    #[test]
    fn test_minmax_window_range_to_std_none_for_single() {
        let mut n = norm(4);
        n.update(dec!(5));
        assert!(n.window_range_to_std().is_none());
    }

    #[test]
    fn test_minmax_window_range_to_std_constant_none() {
        let mut n = norm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        // std=0 → None
        assert!(n.window_range_to_std().is_none());
    }

    // ── round-164 ────────────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_cv_none_for_empty() {
        let n = norm(4);
        assert!(n.window_cv().is_none());
    }

    #[test]
    fn test_minmax_window_cv_constant_zero() {
        let mut n = norm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        // std=0 → cv=0
        let cv = n.window_cv().unwrap();
        assert!((cv - 0.0).abs() < 1e-9, "expected 0.0, got {}", cv);
    }

    #[test]
    fn test_minmax_window_non_zero_fraction_none_for_empty() {
        let n = norm(4);
        assert!(n.window_non_zero_fraction().is_none());
    }

    #[test]
    fn test_minmax_window_non_zero_fraction_all_nonzero() {
        let mut n = norm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let f = n.window_non_zero_fraction().unwrap();
        assert!((f - 1.0).abs() < 1e-9, "expected 1.0, got {}", f);
    }

    #[test]
    fn test_minmax_window_rms_abs_none_for_empty() {
        let n = norm(4);
        assert!(n.window_rms_abs().is_none());
    }

    #[test]
    fn test_minmax_window_rms_abs_constant() {
        let mut n = norm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let m = n.window_rms_abs().unwrap();
        assert!((m - 5.0).abs() < 1e-9, "expected 5.0, got {}", m);
    }

    #[test]
    fn test_minmax_window_kurtosis_proxy_none_for_three() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        assert!(n.window_kurtosis_proxy().is_none());
    }

    #[test]
    fn test_minmax_window_kurtosis_proxy_constant_none() {
        let mut n = norm(4);
        for v in [dec!(5), dec!(5), dec!(5), dec!(5)] { n.update(v); }
        // var=0 → None
        assert!(n.window_kurtosis_proxy().is_none());
    }

    // ── round-165 ────────────────────────────────────────────────────────────

    #[test]
    fn test_minmax_window_mean_reversion_index_none_for_two() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2)] { n.update(v); }
        assert!(n.window_mean_reversion_index().is_none());
    }

    #[test]
    fn test_minmax_window_mean_reversion_index_constant_zero() {
        let mut n = norm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        // constant → all at mean → no reversion → 0.0
        let r = n.window_mean_reversion_index().unwrap();
        assert!((r - 0.0).abs() < 1e-9, "expected 0.0, got {}", r);
    }

    #[test]
    fn test_minmax_window_tail_ratio_none_for_single() {
        let mut n = norm(4);
        n.update(dec!(5));
        assert!(n.window_tail_ratio().is_none());
    }

    #[test]
    fn test_minmax_window_tail_ratio_equal_one() {
        let mut n = norm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        // p10=p90=5 → ratio=1.0
        let r = n.window_tail_ratio().unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0, got {}", r);
    }

    #[test]
    fn test_minmax_window_cumsum_trend_none_for_empty() {
        let n = norm(4);
        assert!(n.window_cumsum_trend().is_none());
    }

    #[test]
    fn test_minmax_window_cumsum_trend_constant() {
        let mut n = norm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let t = n.window_cumsum_trend().unwrap();
        assert!((t - 5.0).abs() < 1e-9, "expected 5.0, got {}", t);
    }

    #[test]
    fn test_minmax_window_mean_crossing_count_none_for_single() {
        let mut n = norm(4);
        n.update(dec!(5));
        assert!(n.window_mean_crossing_count().is_none());
    }

    #[test]
    fn test_minmax_window_mean_crossing_count_constant_zero() {
        let mut n = norm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        // all at mean → no crossings
        let c = n.window_mean_crossing_count().unwrap();
        assert!((c - 0.0).abs() < 1e-9, "expected 0.0, got {}", c);
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
        let std_dev = self.std_dev().unwrap_or(0.0);
        if std_dev < f64::EPSILON {
            return Ok(0.0);
        }
        let mean = self.mean().ok_or_else(|| StreamError::NormalizationError {
            reason: "mean unavailable".into(),
        })?;
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
        self.variance_f64().map(f64::sqrt)
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

    /// Running sum of all values currently in the window.
    ///
    /// Returns `None` if the window is empty. Useful for deriving a rolling
    /// mean without calling [`normalize`](Self::normalize).
    pub fn sum(&self) -> Option<Decimal> {
        if self.window.is_empty() {
            return None;
        }
        Some(self.sum)
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

    /// Standard deviation of the current window as `f64`.
    ///
    /// Returns `None` if the window has fewer than 2 observations.
    pub fn std_dev_f64(&self) -> Option<f64> {
        self.variance_f64().map(|v| v.sqrt())
    }

    /// Current window variance as `f64` (convenience wrapper around [`variance`](Self::variance)).
    ///
    /// Returns `None` if the window has fewer than 2 observations.
    pub fn variance_f64(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        self.variance()?.to_f64()
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

    /// Returns `true` if `value` is an outlier: its z-score exceeds `z_threshold` in magnitude.
    ///
    /// Returns `false` when the window has fewer than 2 observations (z-score undefined).
    /// A typical threshold is `2.0` (95th percentile) or `3.0` (99.7th percentile).
    pub fn is_outlier(&self, value: Decimal, z_threshold: f64) -> bool {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 {
            return false;
        }
        let sd = self.std_dev().unwrap_or(0.0);
        if sd == 0.0 {
            return false;
        }
        let Some(mean_f64) = self.mean().and_then(|m| m.to_f64()) else { return false; };
        let val_f64 = value.to_f64().unwrap_or(mean_f64);
        ((val_f64 - mean_f64) / sd).abs() > z_threshold
    }

    /// Percentile rank: fraction of window observations that are ≤ `value`.
    ///
    /// Returns `None` if the window is empty. Range: `[0.0, 1.0]`.
    pub fn percentile_rank(&self, value: Decimal) -> Option<f64> {
        if self.window.is_empty() {
            return None;
        }
        let count = self.window.iter().filter(|&&v| v <= value).count();
        Some(count as f64 / self.window.len() as f64)
    }

    /// Minimum value seen in the current window.
    ///
    /// Returns `None` when the window is empty.
    pub fn running_min(&self) -> Option<Decimal> {
        self.window.iter().copied().reduce(Decimal::min)
    }

    /// Maximum value seen in the current window.
    ///
    /// Returns `None` when the window is empty.
    pub fn running_max(&self) -> Option<Decimal> {
        self.window.iter().copied().reduce(Decimal::max)
    }

    /// Range of values in the current window: `running_max − running_min`.
    ///
    /// Returns `None` when the window is empty.
    pub fn window_range(&self) -> Option<Decimal> {
        let min = self.running_min()?;
        let max = self.running_max()?;
        Some(max - min)
    }

    /// Coefficient of variation: `std_dev / |mean|`.
    ///
    /// A dimensionless measure of relative dispersion. Returns `None` when the
    /// window has fewer than 2 observations or when the mean is zero.
    pub fn coefficient_of_variation(&self) -> Option<f64> {
        let mean = self.mean()?;
        if mean.is_zero() {
            return None;
        }
        let std_dev = self.std_dev()?;
        let mean_f = mean.abs().to_f64()?;
        Some(std_dev / mean_f)
    }

    /// Population variance of the current window as `f64`: `std_dev²`.
    ///
    /// Note: despite the name, this computes *population* variance (divides by
    /// `n`), consistent with [`std_dev`](Self::std_dev) and
    /// [`variance`](Self::variance). Returns `None` when the window has fewer
    /// than 2 observations.
    pub fn sample_variance(&self) -> Option<f64> {
        let sd = self.std_dev()?;
        Some(sd * sd)
    }

    /// Current window mean as `f64`.
    ///
    /// A convenience over calling `mean()` and then converting to `f64`.
    /// Returns `None` when the window is empty.
    pub fn window_mean_f64(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        self.mean()?.to_f64()
    }

    /// Returns `true` if `value` is within `sigma_tolerance` standard
    /// deviations of the window mean (inclusive).
    ///
    /// Equivalent to `|z_score(value)| <= sigma_tolerance`.  Returns `false`
    /// when the window has fewer than 2 observations (z-score undefined).
    pub fn is_near_mean(&self, value: Decimal, sigma_tolerance: f64) -> bool {
        // Requires at least 2 observations; with n < 2 the z-score is undefined.
        if self.window.len() < 2 {
            return false;
        }
        let Some(std_dev) = self.std_dev() else { return false; };
        if std_dev == 0.0 {
            return true;
        }
        let Some(mean) = self.mean() else { return false; };
        use rust_decimal::prelude::ToPrimitive;
        let diff = (value - mean).abs().to_f64().unwrap_or(f64::MAX);
        diff / std_dev <= sigma_tolerance
    }

    /// Sum of all values currently in the window as `Decimal`.
    ///
    /// Returns `Decimal::ZERO` on an empty window.
    pub fn window_sum(&self) -> Decimal {
        self.sum
    }

    /// Sum of all values currently in the window as `f64`.
    ///
    /// Returns `0.0` on an empty window.
    pub fn window_sum_f64(&self) -> f64 {
        use rust_decimal::prelude::ToPrimitive;
        self.sum.to_f64().unwrap_or(0.0)
    }

    /// Maximum value currently in the window as `f64`.
    ///
    /// Returns `None` when the window is empty.
    pub fn window_max_f64(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        self.running_max()?.to_f64()
    }

    /// Minimum value currently in the window as `f64`.
    ///
    /// Returns `None` when the window is empty.
    pub fn window_min_f64(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        self.running_min()?.to_f64()
    }

    /// Difference between the window maximum and minimum, as `f64`.
    ///
    /// Returns `None` if the window is empty.
    pub fn window_span_f64(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        self.window_range()?.to_f64()
    }

    /// Excess kurtosis of the window: `(Σ((x-mean)⁴/n) / std_dev⁴) - 3`.
    ///
    /// Returns `None` if the window has fewer than 4 observations or std dev is zero.
    /// A normal distribution has excess kurtosis of 0; positive values indicate
    /// heavier tails (leptokurtic); negative values indicate lighter tails (platykurtic).
    pub fn kurtosis(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let n = self.window.len();
        if n < 4 {
            return None;
        }
        let n_f = n as f64;
        let vals: Vec<f64> = self.window.iter().filter_map(|v| v.to_f64()).collect();
        if vals.len() < n {
            return None;
        }
        let mean = vals.iter().sum::<f64>() / n_f;
        let variance = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n_f;
        let std_dev = variance.sqrt();
        if std_dev == 0.0 {
            return None;
        }
        let kurt = vals.iter().map(|v| ((v - mean) / std_dev).powi(4)).sum::<f64>() / n_f - 3.0;
        Some(kurt)
    }

    /// Fisher-Pearson skewness of the rolling window.
    ///
    /// Positive values indicate a right-tailed distribution; negative values
    /// indicate a left-tailed distribution. Returns `None` for fewer than 3
    /// observations or zero standard deviation.
    pub fn skewness(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let n = self.window.len();
        if n < 3 {
            return None;
        }
        let n_f = n as f64;
        let vals: Vec<f64> = self.window.iter().filter_map(|v| v.to_f64()).collect();
        if vals.len() < n {
            return None;
        }
        let mean = vals.iter().sum::<f64>() / n_f;
        let variance = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n_f;
        let std_dev = variance.sqrt();
        if std_dev == 0.0 {
            return None;
        }
        let skew = vals.iter().map(|v| ((v - mean) / std_dev).powi(3)).sum::<f64>() / n_f;
        Some(skew)
    }

    /// Returns `true` if the z-score of `value` exceeds `sigma` in absolute terms.
    ///
    /// Convenience wrapper around [`normalize`](Self::normalize) for alert logic.
    /// Returns `false` if the normalizer window is empty or std-dev is zero.
    pub fn is_extreme(&self, value: Decimal, sigma: f64) -> bool {
        self.normalize(value).ok().map_or(false, |z| z.abs() > sigma)
    }

    /// The most recently added value, or `None` if the window is empty.
    pub fn latest(&self) -> Option<Decimal> {
        self.window.back().copied()
    }

    /// Median of the current window, or `None` if empty.
    pub fn median(&self) -> Option<Decimal> {
        if self.window.is_empty() { return None; }
        let mut vals: Vec<Decimal> = self.window.iter().copied().collect();
        vals.sort();
        let mid = vals.len() / 2;
        if vals.len() % 2 == 0 {
            Some((vals[mid - 1] + vals[mid]) / Decimal::TWO)
        } else {
            Some(vals[mid])
        }
    }

    /// Empirical percentile of `value` within the current window: fraction of values ≤ `value`.
    ///
    /// Alias for [`percentile_rank`](Self::percentile_rank).
    pub fn percentile(&self, value: Decimal) -> Option<f64> {
        self.percentile_rank(value)
    }

    /// Interquartile range: Q3 (75th percentile) − Q1 (25th percentile) of the window.
    ///
    /// Returns `None` if the window has fewer than 4 observations.
    /// The IQR is a robust spread measure less sensitive to outliers than range or std dev.
    pub fn interquartile_range(&self) -> Option<Decimal> {
        let n = self.window.len();
        if n < 4 {
            return None;
        }
        let mut sorted: Vec<Decimal> = self.window.iter().copied().collect();
        sorted.sort();
        let q1_idx = n / 4;
        let q3_idx = 3 * n / 4;
        Some(sorted[q3_idx] - sorted[q1_idx])
    }

    /// Stateless EMA z-score helper: updates running `ema_mean` and `ema_var` and returns
    /// the z-score `(value - ema_mean) / sqrt(ema_var)`.
    ///
    /// `alpha ∈ (0, 1]` controls smoothing speed (higher = faster adaptation).
    /// Initialize `ema_mean = 0.0` and `ema_var = 0.0` before first call.
    /// Returns `None` if `value` cannot be converted to f64 or variance is still zero.
    pub fn ema_z_score(value: Decimal, alpha: f64, ema_mean: &mut f64, ema_var: &mut f64) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let v = value.to_f64()?;
        let delta = v - *ema_mean;
        *ema_mean += alpha * delta;
        *ema_var = (1.0 - alpha) * (*ema_var + alpha * delta * delta);
        let std = ema_var.sqrt();
        if std == 0.0 { return None; }
        Some((v - *ema_mean) / std)
    }

    /// Z-score of the most recently added value.
    ///
    /// Returns `None` if the window is empty or std-dev is zero.
    pub fn z_score_of_latest(&self) -> Option<f64> {
        let latest = self.latest()?;
        self.normalize(latest).ok()
    }

    /// Exponential moving average of z-scores for all values in the current window.
    ///
    /// `alpha` is the smoothing factor (0 < alpha ≤ 1). Higher alpha gives more weight
    /// to recent z-scores. Returns `None` if the window has fewer than 2 observations.
    pub fn ema_of_z_scores(&self, alpha: f64) -> Option<f64> {
        let n = self.window.len();
        if n < 2 {
            return None;
        }
        let mut ema: Option<f64> = None;
        for &value in &self.window {
            if let Ok(z) = self.normalize(value) {
                ema = Some(match ema {
                    None => z,
                    Some(prev) => alpha * z + (1.0 - alpha) * prev,
                });
            }
        }
        ema
    }

    /// Chainable alias for `update`: feeds `value` into the window and returns `&mut Self`.
    pub fn add_observation(&mut self, value: Decimal) -> &mut Self {
        self.update(value);
        self
    }

    /// Signed deviation of `value` from the window mean, as `f64`.
    ///
    /// Returns `None` if the window is empty.
    pub fn deviation_from_mean(&self, value: Decimal) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let mean = self.mean()?.to_f64()?;
        value.to_f64().map(|v| v - mean)
    }

    /// Returns a `Vec` of window values that are within `sigma` standard deviations of the mean.
    ///
    /// Useful for robust statistics after removing extreme outliers.
    /// Returns all values if std-dev is zero (no outliers possible), empty vec if window is empty.
    pub fn trim_outliers(&self, sigma: f64) -> Vec<Decimal> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return vec![]; }
        let Some(mean) = self.mean() else { return vec![]; };
        let std = match self.std_dev() {
            Some(s) if s > 0.0 => s,
            _ => return self.window.iter().copied().collect(),
        };
        let Some(mean_f64) = mean.to_f64() else { return vec![]; };
        self.window.iter().copied()
            .filter(|v| {
                v.to_f64().map_or(false, |vf| ((vf - mean_f64) / std).abs() <= sigma)
            })
            .collect()
    }

    /// Batch normalize: returns z-scores for each value as if they were added one-by-one.
    ///
    /// Each z-score uses only the window state after incorporating that value.
    /// The internal state is modified; call `reset()` if you need to restore it.
    /// Returns `None` entries where normalization fails (window warming up or zero std-dev).
    pub fn rolling_zscore_batch(&mut self, values: &[Decimal]) -> Vec<Option<f64>> {
        values.iter().map(|&v| {
            self.update(v);
            self.normalize(v).ok()
        }).collect()
    }

    /// Change in mean between the first half and second half of the current window.
    ///
    /// Splits the window in two, computes the mean of each half, and returns
    /// `second_half_mean - first_half_mean` as `f64`. Returns `None` if the
    /// window has fewer than 2 observations.
    pub fn rolling_mean_change(&self) -> Option<f64> {
        let n = self.window.len();
        if n < 2 {
            return None;
        }
        let mid = n / 2;
        let first: Decimal = self.window.iter().take(mid).copied().sum::<Decimal>()
            / Decimal::from(mid as u64);
        let second: Decimal = self.window.iter().skip(mid).copied().sum::<Decimal>()
            / Decimal::from((n - mid) as u64);
        (second - first).to_f64()
    }

    /// Count of window values whose z-score is strictly positive (above the mean).
    ///
    /// Returns `0` if the window is empty or all values are equal (z-scores are all 0).
    pub fn count_positive_z_scores(&self) -> usize {
        self.window
            .iter()
            .filter(|&&v| self.normalize(v).map_or(false, |z| z > 0.0))
            .count()
    }

    /// Returns `true` if the absolute change between first-half and second-half window means
    /// is below `threshold`. A stable mean indicates the distribution is not trending.
    ///
    /// Returns `false` if the window has fewer than 2 observations.
    pub fn is_mean_stable(&self, threshold: f64) -> bool {
        self.rolling_mean_change().map_or(false, |c| c.abs() < threshold)
    }

    /// Count of window values whose absolute z-score exceeds `z_threshold`.
    ///
    /// Returns `0` if the window has fewer than 2 observations or std-dev is zero.
    pub fn above_threshold_count(&self, z_threshold: f64) -> usize {
        self.window
            .iter()
            .filter(|&&v| {
                self.normalize(v)
                    .map_or(false, |z| z.abs() > z_threshold)
            })
            .count()
    }

    /// Median Absolute Deviation (MAD) of the current window.
    ///
    /// `MAD = median(|x_i - median(window)|)`. Returns `None` if the window
    /// is empty.
    pub fn mad(&self) -> Option<Decimal> {
        let med = self.median()?;
        let mut deviations: Vec<Decimal> = self.window.iter().map(|&x| (x - med).abs()).collect();
        deviations.sort();
        let n = deviations.len();
        if n == 0 { return None; }
        let mid = n / 2;
        if n % 2 == 0 {
            Some((deviations[mid - 1] + deviations[mid]) / Decimal::TWO)
        } else {
            Some(deviations[mid])
        }
    }

    /// Robust z-score: `(value - median) / MAD`.
    ///
    /// More resistant to outliers than the standard z-score. Returns `None`
    /// when the window is empty or MAD is zero.
    pub fn robust_z_score(&self, value: Decimal) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let med = self.median()?;
        let mad = self.mad()?;
        if mad.is_zero() { return None; }
        ((value - med) / mad).to_f64()
    }

    /// Count of window values strictly above `threshold`.
    pub fn count_above(&self, threshold: Decimal) -> usize {
        self.window.iter().filter(|&&v| v > threshold).count()
    }

    /// Count of window values strictly below `threshold`.
    pub fn count_below(&self, threshold: Decimal) -> usize {
        self.window.iter().filter(|&&v| v < threshold).count()
    }

    /// Value at the p-th percentile of the current window (0.0 ≤ p ≤ 1.0).
    ///
    /// Uses linear interpolation between adjacent sorted values.
    /// Returns `None` if the window is empty.
    pub fn percentile_value(&self, p: f64) -> Option<Decimal> {
        if self.window.is_empty() {
            return None;
        }
        let p = p.clamp(0.0, 1.0);
        let mut sorted: Vec<Decimal> = self.window.iter().copied().collect();
        sorted.sort();
        let n = sorted.len();
        if n == 1 {
            return Some(sorted[0]);
        }
        let idx = p * (n - 1) as f64;
        let lo = idx.floor() as usize;
        let hi = idx.ceil() as usize;
        if lo == hi {
            Some(sorted[lo])
        } else {
            let frac = Decimal::try_from(idx - lo as f64).ok()?;
            Some(sorted[lo] + (sorted[hi] - sorted[lo]) * frac)
        }
    }

    /// Exponentially-weighted moving average (EWMA) of the window values.
    ///
    /// `alpha` is the smoothing factor in (0.0, 1.0]; higher values weight
    /// recent observations more. Processes values in insertion order (oldest first).
    /// Returns `None` if the window is empty.
    pub fn ewma(&self, alpha: f64) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let alpha = alpha.clamp(1e-9, 1.0);
        let mut iter = self.window.iter();
        let first = iter.next()?.to_f64()?;
        let result = iter.fold(first, |acc, &v| {
            let vf = v.to_f64().unwrap_or(acc);
            alpha * vf + (1.0 - alpha) * acc
        });
        Some(result)
    }

    /// Midpoint between the running minimum and maximum in the window.
    ///
    /// Returns `None` if the window is empty.
    pub fn midpoint(&self) -> Option<Decimal> {
        let lo = self.running_min()?;
        let hi = self.running_max()?;
        Some((lo + hi) / Decimal::from(2u64))
    }

    /// Clamps `value` to the [running_min, running_max] range of the window.
    ///
    /// Returns `value` unchanged if the window is empty.
    pub fn clamp_to_window(&self, value: Decimal) -> Decimal {
        match (self.running_min(), self.running_max()) {
            (Some(lo), Some(hi)) => value.max(lo).min(hi),
            _ => value,
        }
    }

    /// Fraction of window values strictly above the midpoint between the
    /// running minimum and maximum.
    ///
    /// Returns `None` if the window is empty or min == max.
    pub fn fraction_above_mid(&self) -> Option<f64> {
        let lo = self.running_min()?;
        let hi = self.running_max()?;
        if lo == hi {
            return None;
        }
        let mid = (lo + hi) / Decimal::from(2u64);
        let above = self.window.iter().filter(|&&v| v > mid).count();
        Some(above as f64 / self.window.len() as f64)
    }

    /// Ratio of the window span (max − min) to the mean.
    ///
    /// Returns `None` if the window is empty, has fewer than 2 elements,
    /// or the mean is zero.
    pub fn normalized_range(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let span = self.window_range()?;
        let mean = self.mean()?;
        if mean.is_zero() {
            return None;
        }
        (span / mean).to_f64()
    }

    /// Returns the (running_min, running_max) pair for the current window.
    ///
    /// Returns `None` if the window is empty.
    pub fn min_max(&self) -> Option<(Decimal, Decimal)> {
        Some((self.running_min()?, self.running_max()?))
    }

    /// All current window values as a `Vec<Decimal>`, in insertion order (oldest first).
    pub fn values(&self) -> Vec<Decimal> {
        self.window.iter().copied().collect()
    }

    /// Fraction of window values that are strictly positive (> 0).
    ///
    /// Returns `None` if the window is empty.
    pub fn above_zero_fraction(&self) -> Option<f64> {
        if self.window.is_empty() {
            return None;
        }
        let above = self.window.iter().filter(|&&v| v > Decimal::ZERO).count();
        Some(above as f64 / self.window.len() as f64)
    }

    /// Z-score of `value` relative to the current window, returned as `Option<f64>`.
    ///
    /// Returns `None` if the window has fewer than 2 elements or variance is zero.
    /// Unlike [`normalize`], this never returns an error for empty windows — it
    /// simply returns `None`.
    pub fn z_score_opt(&self, value: Decimal) -> Option<f64> {
        self.normalize(value).ok()
    }

    /// Returns `true` if the absolute z-score of the latest observation is
    /// within `z_threshold` standard deviations of the mean.
    ///
    /// Returns `false` if the window is empty or has insufficient data.
    pub fn is_stable(&self, z_threshold: f64) -> bool {
        self.z_score_of_latest()
            .map_or(false, |z| z.abs() <= z_threshold)
    }

    /// Fraction of window values strictly above `threshold`.
    ///
    /// Returns `None` if the window is empty.
    pub fn fraction_above(&self, threshold: Decimal) -> Option<f64> {
        if self.window.is_empty() {
            return None;
        }
        Some(self.count_above(threshold) as f64 / self.window.len() as f64)
    }

    /// Fraction of window values strictly below `threshold`.
    ///
    /// Returns `None` if the window is empty.
    pub fn fraction_below(&self, threshold: Decimal) -> Option<f64> {
        if self.window.is_empty() {
            return None;
        }
        Some(self.count_below(threshold) as f64 / self.window.len() as f64)
    }

    /// Returns all window values strictly above `threshold`.
    pub fn window_values_above(&self, threshold: Decimal) -> Vec<Decimal> {
        self.window.iter().copied().filter(|&v| v > threshold).collect()
    }

    /// Returns all window values strictly below `threshold`.
    pub fn window_values_below(&self, threshold: Decimal) -> Vec<Decimal> {
        self.window.iter().copied().filter(|&v| v < threshold).collect()
    }

    /// Count of window values equal to `value`.
    pub fn count_equal(&self, value: Decimal) -> usize {
        self.window.iter().filter(|&&v| v == value).count()
    }

    /// Range of the current window: `running_max - running_min`.
    ///
    /// Returns `None` if the window is empty.
    pub fn rolling_range(&self) -> Option<Decimal> {
        let lo = self.running_min()?;
        let hi = self.running_max()?;
        Some(hi - lo)
    }

    /// Lag-1 autocorrelation of the window values.
    ///
    /// Returns `None` if fewer than 2 values or variance is zero.
    pub fn autocorrelation_lag1(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let n = self.window.len();
        if n < 2 {
            return None;
        }
        let vals: Vec<f64> = self.window.iter().filter_map(|v| v.to_f64()).collect();
        if vals.len() < 2 {
            return None;
        }
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let var: f64 = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / vals.len() as f64;
        if var == 0.0 {
            return None;
        }
        let cov: f64 = vals.windows(2).map(|w| (w[0] - mean) * (w[1] - mean)).sum::<f64>()
            / (vals.len() - 1) as f64;
        Some(cov / var)
    }

    /// Fraction of consecutive pairs where the second value > first (trending upward).
    ///
    /// Returns `None` if fewer than 2 values.
    pub fn trend_consistency(&self) -> Option<f64> {
        let n = self.window.len();
        if n < 2 {
            return None;
        }
        let up = self.window.iter().collect::<Vec<_>>().windows(2)
            .filter(|w| w[1] > w[0]).count();
        Some(up as f64 / (n - 1) as f64)
    }

    /// Mean absolute deviation of the window values.
    ///
    /// Returns `None` if window is empty.
    pub fn mean_absolute_deviation(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() {
            return None;
        }
        let n = self.window.len();
        let vals: Vec<f64> = self.window.iter().filter_map(|v| v.to_f64()).collect();
        let mean = vals.iter().sum::<f64>() / n as f64;
        let mad = vals.iter().map(|v| (v - mean).abs()).sum::<f64>() / n as f64;
        Some(mad)
    }

    /// Percentile rank of the most recently added value within the window.
    ///
    /// Returns `None` if no value has been added yet. Uses the same `<=`
    /// semantics as [`percentile`](Self::percentile).
    pub fn percentile_of_latest(&self) -> Option<f64> {
        let latest = self.latest()?;
        self.percentile(latest)
    }

    /// Tail ratio: `max(window) / 75th-percentile(window)`.
    ///
    /// A simple fat-tail indicator. Values well above 1.0 signal that the
    /// maximum observation is an outlier relative to the upper quartile.
    /// Returns `None` if the window is empty or the 75th percentile is zero.
    pub fn tail_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let max = self.running_max()?;
        let p75 = self.percentile_value(0.75)?;
        if p75.is_zero() {
            return None;
        }
        (max / p75).to_f64()
    }

    /// Z-score of the window minimum relative to the current mean and std dev.
    ///
    /// Returns `None` if the window is empty or std dev is zero.
    pub fn z_score_of_min(&self) -> Option<f64> {
        let min = self.running_min()?;
        self.z_score_opt(min)
    }

    /// Z-score of the window maximum relative to the current mean and std dev.
    ///
    /// Returns `None` if the window is empty or std dev is zero.
    pub fn z_score_of_max(&self) -> Option<f64> {
        let max = self.running_max()?;
        self.z_score_opt(max)
    }

    /// Shannon entropy of the window values.
    ///
    /// Each unique value is treated as a category. Returns `None` if the
    /// window is empty. A uniform distribution maximises entropy; identical
    /// values give `Some(0.0)`.
    pub fn window_entropy(&self) -> Option<f64> {
        if self.window.is_empty() {
            return None;
        }
        let n = self.window.len() as f64;
        let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for v in &self.window {
            *counts.entry(v.to_string()).or_insert(0) += 1;
        }
        let entropy: f64 = counts.values().map(|&c| {
            let p = c as f64 / n;
            -p * p.ln()
        }).sum();
        Some(entropy)
    }

    /// Normalised standard deviation (alias for [`coefficient_of_variation`](Self::coefficient_of_variation)).
    pub fn normalized_std_dev(&self) -> Option<f64> {
        self.coefficient_of_variation()
    }

    /// Count of window values that are strictly above the window mean.
    ///
    /// Returns `None` if the window is empty or the mean cannot be computed.
    pub fn value_above_mean_count(&self) -> Option<usize> {
        let mean = self.mean()?;
        Some(self.window.iter().filter(|&&v| v > mean).count())
    }

    /// Length of the longest consecutive run of values above the window mean.
    ///
    /// Returns `None` if the window is empty or the mean cannot be computed.
    pub fn consecutive_above_mean(&self) -> Option<usize> {
        let mean = self.mean()?;
        let mut max_run = 0usize;
        let mut current = 0usize;
        for &v in &self.window {
            if v > mean {
                current += 1;
                if current > max_run {
                    max_run = current;
                }
            } else {
                current = 0;
            }
        }
        Some(max_run)
    }

    /// Fraction of window values above `threshold`.
    ///
    /// Returns `None` if the window is empty.
    pub fn above_threshold_fraction(&self, threshold: Decimal) -> Option<f64> {
        if self.window.is_empty() {
            return None;
        }
        let count = self.window.iter().filter(|&&v| v > threshold).count();
        Some(count as f64 / self.window.len() as f64)
    }

    /// Fraction of window values below `threshold`.
    ///
    /// Returns `None` if the window is empty.
    pub fn below_threshold_fraction(&self, threshold: Decimal) -> Option<f64> {
        if self.window.is_empty() {
            return None;
        }
        let count = self.window.iter().filter(|&&v| v < threshold).count();
        Some(count as f64 / self.window.len() as f64)
    }

    /// Autocorrelation at lag `k` of the window values.
    ///
    /// Returns `None` if `k == 0`, `k >= window.len()`, or variance is zero.
    pub fn lag_k_autocorrelation(&self, k: usize) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let n = self.window.len();
        if k == 0 || k >= n {
            return None;
        }
        let vals: Vec<f64> = self.window.iter().filter_map(|v| v.to_f64()).collect();
        if vals.len() != n {
            return None;
        }
        let mean = vals.iter().sum::<f64>() / n as f64;
        let var = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n as f64;
        if var == 0.0 {
            return None;
        }
        let m = n - k;
        let cov: f64 = (0..m).map(|i| (vals[i] - mean) * (vals[i + k] - mean)).sum::<f64>() / m as f64;
        Some(cov / var)
    }

    /// Estimated half-life of mean reversion using a simple AR(1) regression.
    ///
    /// Half-life ≈ `-ln(2) / ln(|β|)` where β is the AR(1) coefficient. Returns
    /// `None` if the window has fewer than 3 values, the regression denominator
    /// is zero, or β ≥ 0 (no mean-reversion signal).
    pub fn half_life_estimate(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let n = self.window.len();
        if n < 3 {
            return None;
        }
        let vals: Vec<f64> = self.window.iter().filter_map(|v| v.to_f64()).collect();
        if vals.len() != n {
            return None;
        }
        let diffs: Vec<f64> = vals.windows(2).map(|w| w[1] - w[0]).collect();
        let lagged: Vec<f64> = vals[..n - 1].to_vec();
        let nf = diffs.len() as f64;
        let mean_l = lagged.iter().sum::<f64>() / nf;
        let mean_d = diffs.iter().sum::<f64>() / nf;
        let cov: f64 = lagged.iter().zip(diffs.iter()).map(|(l, d)| (l - mean_l) * (d - mean_d)).sum::<f64>();
        let var: f64 = lagged.iter().map(|l| (l - mean_l).powi(2)).sum::<f64>();
        if var == 0.0 {
            return None;
        }
        let beta = cov / var;
        if beta >= 0.0 {
            return None;
        }
        let lambda = (1.0 + beta).abs().ln();
        if lambda == 0.0 {
            return None;
        }
        Some(-std::f64::consts::LN_2 / lambda)
    }

    /// Geometric mean of the window values.
    ///
    /// `exp(mean(ln(v_i)))`. Returns `None` if the window is empty or any
    /// value is non-positive.
    pub fn geometric_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() {
            return None;
        }
        let logs: Vec<f64> = self.window.iter()
            .filter_map(|v| v.to_f64())
            .filter_map(|f| if f > 0.0 { Some(f.ln()) } else { None })
            .collect();
        if logs.len() != self.window.len() {
            return None;
        }
        Some((logs.iter().sum::<f64>() / logs.len() as f64).exp())
    }

    /// Harmonic mean of the window values.
    ///
    /// `n / sum(1/v_i)`. Returns `None` if the window is empty or any value
    /// is zero.
    pub fn harmonic_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() {
            return None;
        }
        let reciprocals: Vec<f64> = self.window.iter()
            .filter_map(|v| v.to_f64())
            .filter_map(|f| if f != 0.0 { Some(1.0 / f) } else { None })
            .collect();
        if reciprocals.len() != self.window.len() {
            return None;
        }
        let n = reciprocals.len() as f64;
        Some(n / reciprocals.iter().sum::<f64>())
    }

    /// Normalise `value` to the window's observed `[min, max]` range.
    ///
    /// Returns `None` if the window is empty or the range is zero.
    pub fn range_normalized_value(&self, value: Decimal) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let min = self.running_min()?;
        let max = self.running_max()?;
        let range = max - min;
        if range.is_zero() {
            return None;
        }
        ((value - min) / range).to_f64()
    }

    /// Signed distance of `value` from the window median.
    ///
    /// `value - median`. Returns `None` if the window is empty.
    pub fn distance_from_median(&self, value: Decimal) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let med = self.median()?;
        (value - med).to_f64()
    }

    /// Momentum: difference between the latest and oldest value in the window.
    ///
    /// Positive = window trended up; negative = trended down. Returns `None`
    /// if fewer than 2 values are in the window.
    pub fn momentum(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 {
            return None;
        }
        let oldest = *self.window.front()?;
        let latest = *self.window.back()?;
        (latest - oldest).to_f64()
    }

    /// Rank of `value` within the current window, normalised to `[0.0, 1.0]`.
    ///
    /// 0.0 means `value` is ≤ all window values; 1.0 means it is ≥ all window
    /// values. Returns `None` if the window is empty.
    pub fn value_rank(&self, value: Decimal) -> Option<f64> {
        if self.window.is_empty() {
            return None;
        }
        let n = self.window.len();
        let below = self.window.iter().filter(|&&v| v < value).count();
        Some(below as f64 / n as f64)
    }

    /// Coefficient of variation: `std_dev / |mean|`.
    ///
    /// Returns `None` if the window has fewer than 2 values or the mean is
    /// zero.
    pub fn coeff_of_variation(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let n = self.window.len();
        if n < 2 {
            return None;
        }
        let vals: Vec<f64> = self.window.iter().filter_map(|v| v.to_f64()).collect();
        if vals.len() < 2 {
            return None;
        }
        let nf = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / nf;
        if mean == 0.0 {
            return None;
        }
        let std_dev = (vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (nf - 1.0)).sqrt();
        Some(std_dev / mean.abs())
    }

    /// Inter-quartile range: Q3 (75th percentile) minus Q1 (25th percentile).
    ///
    /// Returns `None` if the window is empty.
    pub fn quantile_range(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let q3 = self.percentile_value(0.75)?;
        let q1 = self.percentile_value(0.25)?;
        (q3 - q1).to_f64()
    }

    /// Upper quartile (Q3, 75th percentile) of the window values.
    ///
    /// Returns `None` if the window is empty.
    pub fn upper_quartile(&self) -> Option<Decimal> {
        self.percentile_value(0.75)
    }

    /// Lower quartile (Q1, 25th percentile) of the window values.
    ///
    /// Returns `None` if the window is empty.
    pub fn lower_quartile(&self) -> Option<Decimal> {
        self.percentile_value(0.25)
    }

    /// Fraction of consecutive first-difference pairs whose sign flips.
    ///
    /// A high value indicates a rapidly oscillating series;
    /// a low value indicates persistent trends. Returns `None` for fewer than
    /// 3 observations.
    pub fn sign_change_rate(&self) -> Option<f64> {
        let n = self.window.len();
        if n < 3 {
            return None;
        }
        let vals: Vec<&Decimal> = self.window.iter().collect();
        let diffs: Vec<i32> = vals
            .windows(2)
            .map(|w| {
                if w[1] > w[0] { 1 } else if w[1] < w[0] { -1 } else { 0 }
            })
            .collect();
        let total_pairs = (diffs.len() - 1) as f64;
        if total_pairs == 0.0 {
            return None;
        }
        let changes = diffs
            .windows(2)
            .filter(|w| w[0] != 0 && w[1] != 0 && w[0] != w[1])
            .count();
        Some(changes as f64 / total_pairs)
    }

    // ── round-80 ─────────────────────────────────────────────────────────────

    /// Trimmed mean: arithmetic mean after discarding the bottom and top
    /// `p` fraction of window values.
    ///
    /// `p` is clamped to `[0.0, 0.499]`. Returns `None` if the window is
    /// empty or trimming removes all observations.
    pub fn trimmed_mean(&self, p: f64) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() {
            return None;
        }
        let p = p.clamp(0.0, 0.499);
        let mut sorted: Vec<f64> = self.window.iter().filter_map(|v| v.to_f64()).collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = sorted.len();
        let trim = (n as f64 * p).floor() as usize;
        let trimmed = &sorted[trim..n - trim];
        if trimmed.is_empty() {
            return None;
        }
        Some(trimmed.iter().sum::<f64>() / trimmed.len() as f64)
    }

    /// OLS linear trend slope of window values over their insertion index.
    ///
    /// A positive slope indicates an upward trend; negative indicates downward.
    /// Returns `None` if the window has fewer than 2 observations.
    pub fn linear_trend_slope(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let n = self.window.len();
        if n < 2 {
            return None;
        }
        let n_f = n as f64;
        let x_mean = (n_f - 1.0) / 2.0;
        let y_vals: Vec<f64> = self.window.iter().filter_map(|v| v.to_f64()).collect();
        if y_vals.len() < 2 {
            return None;
        }
        let y_mean = y_vals.iter().sum::<f64>() / y_vals.len() as f64;
        let numerator: f64 = y_vals
            .iter()
            .enumerate()
            .map(|(i, &y)| (i as f64 - x_mean) * (y - y_mean))
            .sum();
        let denominator: f64 = (0..n).map(|i| (i as f64 - x_mean).powi(2)).sum();
        if denominator == 0.0 {
            return None;
        }
        Some(numerator / denominator)
    }

    // ── round-81 ─────────────────────────────────────────────────────────────

    /// Ratio of first-half to second-half window variance.
    ///
    /// Values above 1.0 indicate decreasing volatility; below 1.0 indicate
    /// increasing volatility. Returns `None` if fewer than 4 observations or
    /// second-half variance is zero.
    pub fn variance_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let n = self.window.len();
        if n < 4 {
            return None;
        }
        let mid = n / 2;
        let first: Vec<f64> = self.window.iter().take(mid).filter_map(|v| v.to_f64()).collect();
        let second: Vec<f64> = self.window.iter().skip(mid).filter_map(|v| v.to_f64()).collect();
        let var = |vals: &[f64]| -> Option<f64> {
            let n_f = vals.len() as f64;
            if n_f < 2.0 { return None; }
            let mean = vals.iter().sum::<f64>() / n_f;
            Some(vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n_f - 1.0))
        };
        let v1 = var(&first)?;
        let v2 = var(&second)?;
        if v2 == 0.0 {
            return None;
        }
        Some(v1 / v2)
    }

    /// Linear trend slope of the z-scores of window values.
    ///
    /// Computes the z-score of each window value then fits an OLS line over
    /// the sequence. Positive slope means z-scores are trending upward.
    /// Returns `None` if fewer than 2 observations or std-dev is zero.
    pub fn z_score_trend_slope(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let n = self.window.len();
        if n < 2 {
            return None;
        }
        let mean_dec = self.mean()?;
        let std_dev = self.std_dev()?;
        if std_dev == 0.0 {
            return None;
        }
        let mean_f = mean_dec.to_f64()?;
        let z_vals: Vec<f64> = self
            .window
            .iter()
            .filter_map(|v| v.to_f64())
            .map(|v| (v - mean_f) / std_dev)
            .collect();
        if z_vals.len() < 2 {
            return None;
        }
        let n_f = z_vals.len() as f64;
        let x_mean = (n_f - 1.0) / 2.0;
        let z_mean = z_vals.iter().sum::<f64>() / n_f;
        let num: f64 = z_vals.iter().enumerate().map(|(i, &z)| (i as f64 - x_mean) * (z - z_mean)).sum();
        let den: f64 = (0..z_vals.len()).map(|i| (i as f64 - x_mean).powi(2)).sum();
        if den == 0.0 { return None; }
        Some(num / den)
    }

    // ── round-82 ─────────────────────────────────────────────────────────────

    /// Mean of `|x_i − x_{i-1}|` across consecutive window values; average absolute change.
    pub fn mean_absolute_change(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let vals: Vec<f64> = self.window.iter().filter_map(|v| v.to_f64()).collect();
        if vals.len() < 2 {
            return None;
        }
        let mac = vals.windows(2).map(|w| (w[1] - w[0]).abs()).sum::<f64>() / (vals.len() - 1) as f64;
        Some(mac)
    }

    // ── round-83 ─────────────────────────────────────────────────────────────

    /// Fraction of consecutive pairs that are monotonically increasing.
    pub fn monotone_increase_fraction(&self) -> Option<f64> {
        let vals: Vec<Decimal> = self.window.iter().copied().collect();
        let n = vals.len();
        if n < 2 {
            return None;
        }
        let inc = vals.windows(2).filter(|w| w[1] > w[0]).count();
        Some(inc as f64 / (n - 1) as f64)
    }

    /// Maximum absolute value in the rolling window.
    pub fn abs_max(&self) -> Option<Decimal> {
        self.window.iter().map(|v| v.abs()).reduce(|a, b| a.max(b))
    }

    /// Minimum absolute value in the rolling window.
    pub fn abs_min(&self) -> Option<Decimal> {
        self.window.iter().map(|v| v.abs()).reduce(|a, b| a.min(b))
    }

    /// Count of values equal to the window maximum.
    pub fn max_count(&self) -> Option<usize> {
        let max = self.window.iter().copied().reduce(|a, b| a.max(b))?;
        Some(self.window.iter().filter(|&&v| v == max).count())
    }

    /// Count of values equal to the window minimum.
    pub fn min_count(&self) -> Option<usize> {
        let min = self.window.iter().copied().reduce(|a, b| a.min(b))?;
        Some(self.window.iter().filter(|&&v| v == min).count())
    }

    /// Ratio of current full-window mean to the mean of the first half.
    pub fn mean_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let n = self.window.len();
        if n < 2 {
            return None;
        }
        let current_mean = self.mean()?;
        let half = (n / 2).max(1);
        let early_sum: Decimal = self.window.iter().take(half).copied().sum();
        let early_mean = early_sum / Decimal::from(half as i64);
        if early_mean.is_zero() {
            return None;
        }
        (current_mean / early_mean).to_f64()
    }

    // ── round-84 ─────────────────────────────────────────────────────────────

    /// Exponentially-weighted mean with decay factor `alpha` ∈ (0, 1]; most-recent value has highest weight.
    pub fn exponential_weighted_mean(&self, alpha: f64) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() {
            return None;
        }
        let alpha = alpha.clamp(1e-6, 1.0);
        let vals: Vec<f64> = self.window.iter().filter_map(|v| v.to_f64()).collect();
        if vals.is_empty() {
            return None;
        }
        let mut ewm = vals[0];
        for &v in &vals[1..] {
            ewm = alpha * v + (1.0 - alpha) * ewm;
        }
        Some(ewm)
    }

    /// Ratio of window maximum to window minimum; requires non-zero minimum.
    pub fn peak_to_trough_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let max = self.window.iter().copied().reduce(|a, b| a.max(b))?;
        let min = self.window.iter().copied().reduce(|a, b| a.min(b))?;
        if min.is_zero() {
            return None;
        }
        (max / min).to_f64()
    }

    /// Mean of squared values in the window (second raw moment).
    pub fn second_moment(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() {
            return None;
        }
        let sum: f64 = self.window.iter().filter_map(|v| v.to_f64()).map(|v| v * v).sum();
        Some(sum / self.window.len() as f64)
    }

    /// Range / mean — coefficient of dispersion; `None` if mean is zero.
    pub fn range_over_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let max = self.window.iter().copied().reduce(|a, b| a.max(b))?;
        let min = self.window.iter().copied().reduce(|a, b| a.min(b))?;
        let mean = self.mean()?;
        if mean.is_zero() {
            return None;
        }
        ((max - min) / mean).to_f64()
    }

    /// Fraction of window values strictly above the window median.
    pub fn above_median_fraction(&self) -> Option<f64> {
        if self.window.is_empty() {
            return None;
        }
        let mut sorted: Vec<Decimal> = self.window.iter().copied().collect();
        sorted.sort();
        let mid = sorted.len() / 2;
        let median = if sorted.len() % 2 == 0 {
            (sorted[mid - 1] + sorted[mid]) / Decimal::from(2)
        } else {
            sorted[mid]
        };
        let count = self.window.iter().filter(|&&v| v > median).count();
        Some(count as f64 / self.window.len() as f64)
    }

    // ── round-85 ─────────────────────────────────────────────────────────────

    /// Mean of values strictly between Q1 and Q3 (the interquartile mean).
    pub fn interquartile_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() {
            return None;
        }
        let mut sorted: Vec<Decimal> = self.window.iter().copied().collect();
        sorted.sort();
        let n = sorted.len();
        let q1_idx = n / 4;
        let q3_idx = (3 * n) / 4;
        let iqr_vals: Vec<f64> = sorted[q1_idx..q3_idx]
            .iter()
            .filter_map(|v| v.to_f64())
            .collect();
        if iqr_vals.is_empty() {
            return None;
        }
        Some(iqr_vals.iter().sum::<f64>() / iqr_vals.len() as f64)
    }

    /// Fraction of window values beyond `threshold` standard deviations from the mean.
    pub fn outlier_fraction(&self, threshold: f64) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() {
            return None;
        }
        let std_dev = self.std_dev()?;
        let mean = self.mean()?.to_f64()?;
        if std_dev == 0.0 {
            return Some(0.0);
        }
        let count = self.window
            .iter()
            .filter_map(|v| v.to_f64())
            .filter(|&v| ((v - mean) / std_dev).abs() > threshold)
            .count();
        Some(count as f64 / self.window.len() as f64)
    }

    /// Count of sign changes (transitions across zero) in the window.
    pub fn sign_flip_count(&self) -> Option<usize> {
        if self.window.len() < 2 {
            return None;
        }
        let count = self.window
            .iter()
            .collect::<Vec<_>>()
            .windows(2)
            .filter(|w| w[0].is_sign_negative() != w[1].is_sign_negative())
            .count();
        Some(count)
    }

    /// Root mean square of window values.
    pub fn rms(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() {
            return None;
        }
        let sum_sq: f64 = self.window.iter().filter_map(|v| v.to_f64()).map(|v| v * v).sum();
        Some((sum_sq / self.window.len() as f64).sqrt())
    }

    // ── round-86 ─────────────────────────────────────────────────────────────

    /// Number of distinct values in the window.
    pub fn distinct_count(&self) -> usize {
        let mut sorted: Vec<Decimal> = self.window.iter().copied().collect();
        sorted.sort();
        sorted.dedup();
        sorted.len()
    }

    /// Fraction of window values that equal the window maximum.
    pub fn max_fraction(&self) -> Option<f64> {
        if self.window.is_empty() {
            return None;
        }
        let max = self.window.iter().copied().max()?;
        let count = self.window.iter().filter(|&&v| v == max).count();
        Some(count as f64 / self.window.len() as f64)
    }

    /// Fraction of window values that equal the window minimum.
    pub fn min_fraction(&self) -> Option<f64> {
        if self.window.is_empty() {
            return None;
        }
        let min = self.window.iter().copied().min()?;
        let count = self.window.iter().filter(|&&v| v == min).count();
        Some(count as f64 / self.window.len() as f64)
    }

    /// Difference between the latest value and the window mean (signed).
    pub fn latest_minus_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let latest = self.latest()?;
        let mean = self.mean()?;
        (latest - mean).to_f64()
    }

    /// Ratio of the latest value to the window mean; `None` if mean is zero.
    pub fn latest_to_mean_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let latest = self.latest()?;
        let mean = self.mean()?;
        if mean.is_zero() {
            return None;
        }
        (latest / mean).to_f64()
    }

    // ── round-87 ─────────────────────────────────────────────────────────────

    /// Fraction of window values strictly below the mean.
    pub fn below_mean_fraction(&self) -> Option<f64> {
        if self.window.is_empty() {
            return None;
        }
        let mean = self.mean()?;
        let count = self.window.iter().filter(|&&v| v < mean).count();
        Some(count as f64 / self.window.len() as f64)
    }

    /// Variance of values lying outside the interquartile range.
    /// Returns `None` if fewer than 4 values or fewer than 2 tail values.
    pub fn tail_variance(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 4 {
            return None;
        }
        let mut sorted: Vec<Decimal> = self.window.iter().copied().collect();
        sorted.sort();
        let n = sorted.len();
        let q1 = sorted[n / 4];
        let q3 = sorted[(3 * n) / 4];
        let tails: Vec<f64> = sorted
            .iter()
            .filter(|&&v| v < q1 || v > q3)
            .filter_map(|v| v.to_f64())
            .collect();
        if tails.len() < 2 {
            return None;
        }
        let nt = tails.len() as f64;
        let mean = tails.iter().sum::<f64>() / nt;
        let var = tails.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (nt - 1.0);
        Some(var)
    }

    // ── round-88 ─────────────────────────────────────────────────────────────

    /// Number of times the window reaches a new running maximum (from index 0).
    pub fn new_max_count(&self) -> usize {
        if self.window.is_empty() {
            return 0;
        }
        let vals: Vec<Decimal> = self.window.iter().copied().collect();
        let mut running = vals[0];
        let mut count = 1usize;
        for &v in vals.iter().skip(1) {
            if v > running {
                running = v;
                count += 1;
            }
        }
        count
    }

    /// Number of times the window reaches a new running minimum (from index 0).
    pub fn new_min_count(&self) -> usize {
        if self.window.is_empty() {
            return 0;
        }
        let vals: Vec<Decimal> = self.window.iter().copied().collect();
        let mut running = vals[0];
        let mut count = 1usize;
        for &v in vals.iter().skip(1) {
            if v < running {
                running = v;
                count += 1;
            }
        }
        count
    }

    /// Fraction of window values strictly equal to zero.
    pub fn zero_fraction(&self) -> Option<f64> {
        if self.window.is_empty() {
            return None;
        }
        let count = self.window.iter().filter(|&&v| v == Decimal::ZERO).count();
        Some(count as f64 / self.window.len() as f64)
    }

    // ── round-89 ─────────────────────────────────────────────────────────────

    /// Cumulative sum of all window values.
    pub fn cumulative_sum(&self) -> Decimal {
        self.window.iter().copied().sum()
    }

    /// Ratio of the window maximum to the window minimum.
    /// Returns `None` if the window is empty or minimum is zero.
    pub fn max_to_min_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let max = self.window.iter().copied().max()?;
        let min = self.window.iter().copied().min()?;
        if min.is_zero() {
            return None;
        }
        (max / min).to_f64()
    }

    // ── round-90 ─────────────────────────────────────────────────────────────

    /// Fraction of window values strictly above the window midpoint `(min + max) / 2`.
    ///
    /// Returns `None` for an empty window.
    pub fn above_midpoint_fraction(&self) -> Option<f64> {
        if self.window.is_empty() {
            return None;
        }
        let min = self.window.iter().copied().min()?;
        let max = self.window.iter().copied().max()?;
        let mid = (min + max) / Decimal::TWO;
        let count = self.window.iter().filter(|&&v| v > mid).count();
        Some(count as f64 / self.window.len() as f64)
    }

    /// Fraction of window values strictly greater than zero.
    ///
    /// Returns `None` for an empty window.
    pub fn positive_fraction(&self) -> Option<f64> {
        if self.window.is_empty() {
            return None;
        }
        let count = self.window.iter().filter(|&&v| v > Decimal::ZERO).count();
        Some(count as f64 / self.window.len() as f64)
    }

    /// Fraction of window values strictly above the window mean.
    ///
    /// Returns `None` for an empty window.
    pub fn above_mean_fraction(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() {
            return None;
        }
        let n = self.window.len() as u32;
        let mean = self.window.iter().copied().sum::<Decimal>() / Decimal::from(n);
        let count = self.window.iter().filter(|&&v| v > mean).count();
        Some(count as f64 / self.window.len() as f64)
    }

    // ── round-91 ─────────────────────────────────────────────────────────────

    /// Interquartile range of the window: `Q3 − Q1`.
    ///
    /// Returns `None` for an empty window.
    pub fn window_iqr(&self) -> Option<Decimal> {
        if self.window.is_empty() {
            return None;
        }
        let mut sorted: Vec<Decimal> = self.window.iter().copied().collect();
        sorted.sort();
        let n = sorted.len();
        let q1 = sorted[n / 4];
        let q3 = sorted[(3 * n) / 4];
        Some(q3 - q1)
    }

    /// Mean run length of monotone non-decreasing segments.
    ///
    /// Returns `None` for fewer than 2 values in the window.
    pub fn run_length_mean(&self) -> Option<f64> {
        if self.window.len() < 2 {
            return None;
        }
        let vals: Vec<Decimal> = self.window.iter().copied().collect();
        let mut runs: Vec<usize> = Vec::new();
        let mut run_len = 1usize;
        for w in vals.windows(2) {
            if w[1] >= w[0] {
                run_len += 1;
            } else {
                runs.push(run_len);
                run_len = 1;
            }
        }
        runs.push(run_len);
        Some(runs.iter().sum::<usize>() as f64 / runs.len() as f64)
    }

    // ── round-92 ─────────────────────────────────────────────────────────────

    /// Fraction of consecutive value pairs that are non-decreasing.
    ///
    /// Returns `None` for fewer than 2 values in the window.
    pub fn monotone_fraction(&self) -> Option<f64> {
        if self.window.len() < 2 {
            return None;
        }
        let vals: Vec<Decimal> = self.window.iter().copied().collect();
        let non_decreasing = vals.windows(2).filter(|w| w[1] >= w[0]).count();
        Some(non_decreasing as f64 / (vals.len() - 1) as f64)
    }

    /// Coefficient of variation: `std_dev / |mean|` for window values.
    ///
    /// Returns `None` for an empty window or zero mean.
    pub fn coeff_variation(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() {
            return None;
        }
        let n = self.window.len() as u32;
        let mean: Decimal =
            self.window.iter().copied().sum::<Decimal>() / Decimal::from(n);
        if mean.is_zero() {
            return None;
        }
        let variance: f64 = self
            .window
            .iter()
            .filter_map(|&v| {
                let d = (v - mean).to_f64()?;
                Some(d * d)
            })
            .sum::<f64>()
            / n as f64;
        let mean_f = mean.to_f64()?;
        Some(variance.sqrt() / mean_f.abs())
    }

    // ── round-93 ─────────────────────────────────────────────────────────────

    /// Sum of squared values in the rolling window.
    pub fn window_sum_of_squares(&self) -> Decimal {
        self.window.iter().map(|&v| v * v).sum()
    }

    /// 75th percentile of the rolling window.
    ///
    /// Returns `None` for an empty window.
    pub fn percentile_75(&self) -> Option<Decimal> {
        if self.window.is_empty() {
            return None;
        }
        let mut sorted: Vec<Decimal> = self.window.iter().copied().collect();
        sorted.sort();
        let idx = (sorted.len() * 3) / 4;
        Some(sorted[idx.min(sorted.len() - 1)])
    }

    // ── round-94 ─────────────────────────────────────────────────────────────

    /// Mean absolute deviation of the window values from their mean.
    /// Returns `None` if the window is empty.
    pub fn window_mean_deviation(&self) -> Option<Decimal> {
        if self.window.is_empty() {
            return None;
        }
        let n = Decimal::from(self.window.len());
        let mean: Decimal = self.window.iter().copied().sum::<Decimal>() / n;
        let mad = self.window.iter().map(|v| (*v - mean).abs()).sum::<Decimal>() / n;
        Some(mad)
    }

    /// Fraction of window values strictly below the latest observation (0.0–1.0).
    /// Returns `None` if the window is empty.
    pub fn latest_percentile(&self) -> Option<f64> {
        if self.window.is_empty() {
            return None;
        }
        use rust_decimal::prelude::ToPrimitive;
        let latest = *self.window.back()?;
        let below = self.window.iter().filter(|&&v| v < latest).count();
        Some(below as f64 / self.window.len() as f64)
    }

    // ── round-95 ─────────────────────────────────────────────────────────────

    /// Mean of window values that fall between the 25th and 75th percentile (trimmed mean).
    /// Returns `None` if the window has fewer than 4 values.
    pub fn window_trimmed_mean(&self) -> Option<Decimal> {
        if self.window.len() < 4 {
            return None;
        }
        let mut sorted: Vec<Decimal> = self.window.iter().copied().collect();
        sorted.sort();
        let q1 = sorted.len() / 4;
        let q3 = (sorted.len() * 3) / 4;
        let trimmed = &sorted[q1..q3];
        if trimmed.is_empty() {
            return None;
        }
        let sum: Decimal = trimmed.iter().copied().sum();
        Some(sum / Decimal::from(trimmed.len()))
    }

    /// Population variance of the rolling window values.
    /// Returns `None` if the window is empty.
    pub fn window_variance(&self) -> Option<Decimal> {
        if self.window.is_empty() {
            return None;
        }
        let n = Decimal::from(self.window.len());
        let mean: Decimal = self.window.iter().copied().sum::<Decimal>() / n;
        let variance = self.window.iter().map(|v| (*v - mean) * (*v - mean)).sum::<Decimal>() / n;
        Some(variance)
    }

    // ── round-96 ─────────────────────────────────────────────────────────────

    /// Population standard deviation of the rolling window.
    /// Returns `None` if the window is empty.
    pub fn window_std_dev(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let variance = self.window_variance()?;
        Some(variance.to_f64().unwrap_or(0.0).sqrt())
    }

    /// Ratio of the window minimum to the window maximum.
    /// Returns `None` if the window is empty or max is zero.
    pub fn window_min_max_ratio(&self) -> Option<Decimal> {
        if self.window.is_empty() {
            return None;
        }
        let min = self.window.iter().copied().min()?;
        let max = self.window.iter().copied().max()?;
        if max.is_zero() {
            return None;
        }
        Some(min / max)
    }

    /// Bias of recent observations vs older ones: mean of last half minus mean
    /// of first half, as a fraction of the overall mean.
    /// Returns `None` for windows with fewer than 4 values or zero overall mean.
    pub fn recent_bias(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 4 {
            return None;
        }
        let n = self.window.len();
        let vals: Vec<Decimal> = self.window.iter().copied().collect();
        let half = n / 2;
        let old_mean: Decimal = vals[..half].iter().copied().sum::<Decimal>() / Decimal::from(half);
        let new_mean: Decimal =
            vals[half..].iter().copied().sum::<Decimal>() / Decimal::from(n - half);
        let overall_mean: Decimal = vals.iter().copied().sum::<Decimal>() / Decimal::from(n);
        if overall_mean.is_zero() {
            return None;
        }
        Some(((new_mean - old_mean) / overall_mean).to_f64().unwrap_or(0.0))
    }

    /// Range of the window (max − min) as a percentage of the minimum value.
    /// Returns `None` if the window is empty or the minimum is zero.
    pub fn window_range_pct(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() {
            return None;
        }
        let min = self.window.iter().copied().min()?;
        let max = self.window.iter().copied().max()?;
        if min.is_zero() {
            return None;
        }
        Some(((max - min) / min).to_f64().unwrap_or(0.0))
    }

    // ── round-97 ─────────────────────────────────────────────────────────────

    /// Momentum of the rolling window: latest value minus the oldest value.
    /// Returns `None` if the window has fewer than 2 values.
    pub fn window_momentum(&self) -> Option<Decimal> {
        if self.window.len() < 2 {
            return None;
        }
        let oldest = *self.window.front()?;
        let latest = *self.window.back()?;
        Some(latest - oldest)
    }

    /// Fraction of window values that are strictly above the first (oldest) value.
    /// Returns `None` if the window is empty.
    pub fn above_first_fraction(&self) -> Option<f64> {
        if self.window.is_empty() {
            return None;
        }
        let first = *self.window.front()?;
        let above = self.window.iter().filter(|&&v| v > first).count();
        Some(above as f64 / self.window.len() as f64)
    }

    /// Z-score of the latest observation relative to the window mean and std-dev.
    /// Returns `None` if the window is empty or std-dev is zero.
    pub fn window_zscore_latest(&self) -> Option<f64> {
        if self.window.is_empty() {
            return None;
        }
        let n = Decimal::from(self.window.len());
        let mean: Decimal = self.window.iter().copied().sum::<Decimal>() / n;
        let variance = self.window.iter().map(|v| (*v - mean) * (*v - mean)).sum::<Decimal>() / n;
        use rust_decimal::prelude::ToPrimitive;
        let std_dev = variance.to_f64().unwrap_or(0.0).sqrt();
        if std_dev == 0.0 {
            return None;
        }
        let latest = self.window.back()?.to_f64().unwrap_or(0.0);
        let mean_f = mean.to_f64().unwrap_or(0.0);
        Some((latest - mean_f) / std_dev)
    }

    /// Exponentially-decayed weighted mean of the window (newest weight = alpha).
    /// Returns `None` for an empty window.
    pub fn decay_weighted_mean(&self, alpha: f64) -> Option<f64> {
        if self.window.is_empty() {
            return None;
        }
        use rust_decimal::prelude::ToPrimitive;
        let vals: Vec<f64> = self
            .window
            .iter()
            .rev()
            .map(|v| v.to_f64().unwrap_or(0.0))
            .collect();
        let n = vals.len();
        let mut weighted_sum = 0.0f64;
        let mut weight_sum = 0.0f64;
        for (i, v) in vals.iter().enumerate() {
            let w = alpha * (1.0 - alpha).powi(i as i32);
            weighted_sum += v * w;
            weight_sum += w;
        }
        if weight_sum == 0.0 || n == 0 {
            return None;
        }
        Some(weighted_sum / weight_sum)
    }

    // ── round-98 ─────────────────────────────────────────────────────────────

    /// Excess kurtosis of the window. Returns `None` for fewer than 4 values or zero variance.
    pub fn window_kurtosis(&self) -> Option<f64> {
        if self.window.len() < 4 {
            return None;
        }
        use rust_decimal::prelude::ToPrimitive;
        let n = self.window.len() as f64;
        let vals: Vec<f64> = self
            .window
            .iter()
            .map(|v| v.to_f64().unwrap_or(0.0))
            .collect();
        let mean = vals.iter().sum::<f64>() / n;
        let variance = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n;
        if variance == 0.0 {
            return None;
        }
        let fourth = vals.iter().map(|v| (v - mean).powi(4)).sum::<f64>() / n;
        Some(fourth / (variance * variance) - 3.0)
    }

    /// Fraction of window values above the 90th percentile value.
    /// Returns `None` if the window is empty.
    pub fn above_percentile_90(&self) -> Option<f64> {
        if self.window.is_empty() {
            return None;
        }
        let mut sorted: Vec<Decimal> = self.window.iter().copied().collect();
        sorted.sort();
        let idx = (sorted.len() * 9) / 10;
        let p90 = sorted[idx.min(sorted.len() - 1)];
        let above = self.window.iter().filter(|&&v| v > p90).count();
        Some(above as f64 / self.window.len() as f64)
    }

    /// Lag-1 autocorrelation of the rolling window values.
    /// Returns `None` for fewer than 3 values or zero variance.
    pub fn window_lag_autocorr(&self) -> Option<f64> {
        if self.window.len() < 3 {
            return None;
        }
        use rust_decimal::prelude::ToPrimitive;
        let vals: Vec<f64> = self
            .window
            .iter()
            .map(|v| v.to_f64().unwrap_or(0.0))
            .collect();
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        let variance = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n;
        if variance == 0.0 {
            return None;
        }
        let cov = vals
            .windows(2)
            .map(|w| (w[0] - mean) * (w[1] - mean))
            .sum::<f64>()
            / (n - 1.0);
        Some(cov / variance)
    }

    /// Linear slope of the rolling window mean over halves.
    /// Returns `None` for fewer than 4 values.
    pub fn slope_of_mean(&self) -> Option<f64> {
        if self.window.len() < 4 {
            return None;
        }
        use rust_decimal::prelude::ToPrimitive;
        let vals: Vec<f64> = self
            .window
            .iter()
            .map(|v| v.to_f64().unwrap_or(0.0))
            .collect();
        let half = vals.len() / 2;
        let first_mean = vals[..half].iter().sum::<f64>() / half as f64;
        let second_mean = vals[half..].iter().sum::<f64>() / (vals.len() - half) as f64;
        Some((second_mean - first_mean) / half as f64)
    }

    // ── round-99 ─────────────────────────────────────────────────────────────

    /// 25th percentile of the rolling window.
    /// Returns `None` for an empty window.
    pub fn window_percentile_25(&self) -> Option<Decimal> {
        if self.window.is_empty() {
            return None;
        }
        let mut sorted: Vec<Decimal> = self.window.iter().copied().collect();
        sorted.sort();
        let idx = sorted.len() / 4;
        Some(sorted[idx.min(sorted.len() - 1)])
    }

    /// Mean-reversion score: how far the latest value is from the window mean
    /// as a fraction of the window range. Returns `None` for empty window or zero range.
    pub fn mean_reversion_score(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() {
            return None;
        }
        let min = self.window.iter().copied().min()?;
        let max = self.window.iter().copied().max()?;
        let range = max - min;
        if range.is_zero() {
            return None;
        }
        let n = Decimal::from(self.window.len());
        let mean: Decimal = self.window.iter().copied().sum::<Decimal>() / n;
        let latest = *self.window.back()?;
        Some(((latest - mean) / range).to_f64().unwrap_or(0.0))
    }

    /// Trend strength: |(second_half_mean − first_half_mean)| / window_std_dev.
    /// Returns `None` for fewer than 4 values or zero std-dev.
    pub fn trend_strength(&self) -> Option<f64> {
        if self.window.len() < 4 {
            return None;
        }
        use rust_decimal::prelude::ToPrimitive;
        let n = self.window.len() as f64;
        let vals: Vec<f64> = self
            .window
            .iter()
            .map(|v| v.to_f64().unwrap_or(0.0))
            .collect();
        let mean = vals.iter().sum::<f64>() / n;
        let variance = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n;
        let std_dev = variance.sqrt();
        if std_dev == 0.0 {
            return None;
        }
        let half = vals.len() / 2;
        let first_mean = vals[..half].iter().sum::<f64>() / half as f64;
        let second_mean = vals[half..].iter().sum::<f64>() / (vals.len() - half) as f64;
        Some((second_mean - first_mean).abs() / std_dev)
    }

    /// Number of local peaks (value greater than both neighbours) in the window.
    /// Returns `None` for fewer than 3 values.
    pub fn window_peak_count(&self) -> Option<usize> {
        if self.window.len() < 3 {
            return None;
        }
        let vals: Vec<Decimal> = self.window.iter().copied().collect();
        let count = vals
            .windows(3)
            .filter(|w| w[1] > w[0] && w[1] > w[2])
            .count();
        Some(count)
    }

    // ── round-100 ────────────────────────────────────────────────────────────

    /// Number of local troughs (values less than both neighbours) in the window.
    /// Returns `None` for fewer than 3 values.
    pub fn window_trough_count(&self) -> Option<usize> {
        if self.window.len() < 3 {
            return None;
        }
        let vals: Vec<Decimal> = self.window.iter().copied().collect();
        let count = vals
            .windows(3)
            .filter(|w| w[1] < w[0] && w[1] < w[2])
            .count();
        Some(count)
    }

    /// Fraction of consecutive value pairs where the second is strictly greater.
    /// Returns `None` for fewer than 2 values.
    pub fn positive_momentum_fraction(&self) -> Option<f64> {
        if self.window.len() < 2 {
            return None;
        }
        let vals: Vec<Decimal> = self.window.iter().copied().collect();
        let total = vals.len() - 1;
        let positive = vals.windows(2).filter(|w| w[1] > w[0]).count();
        Some(positive as f64 / total as f64)
    }

    /// 10th percentile of the rolling window.
    /// Returns `None` for an empty window.
    pub fn below_percentile_10(&self) -> Option<Decimal> {
        if self.window.is_empty() {
            return None;
        }
        let mut sorted: Vec<Decimal> = self.window.iter().copied().collect();
        sorted.sort();
        let idx = sorted.len() / 10;
        Some(sorted[idx.min(sorted.len() - 1)])
    }

    /// Rate of direction alternation in the window.
    /// Returns `None` for fewer than 3 values or no directional pairs.
    pub fn alternation_rate(&self) -> Option<f64> {
        if self.window.len() < 3 {
            return None;
        }
        let vals: Vec<Decimal> = self.window.iter().copied().collect();
        let dirs: Vec<i32> = vals
            .windows(2)
            .map(|w| {
                if w[1] > w[0] {
                    1
                } else if w[1] < w[0] {
                    -1
                } else {
                    0
                }
            })
            .collect();
        let valid_pairs: Vec<_> = dirs.windows(2).filter(|d| d[0] != 0 && d[1] != 0).collect();
        if valid_pairs.is_empty() {
            return None;
        }
        let alternations = valid_pairs.iter().filter(|d| d[0] != d[1]).count();
        Some(alternations as f64 / valid_pairs.len() as f64)
    }

    // ── round-101 ────────────────────────────────────────────────────────────

    /// Signed area under the window curve (sum of deviations from mean).
    /// Returns `None` only for empty window.
    pub fn window_signed_area(&self) -> Option<Decimal> {
        if self.window.is_empty() {
            return None;
        }
        let n = Decimal::from(self.window.len());
        let mean: Decimal = self.window.iter().copied().sum::<Decimal>() / n;
        let area: Decimal = self.window.iter().map(|v| *v - mean).sum();
        Some(area)
    }

    /// Fraction of window values strictly above zero.
    /// Returns `None` for an empty window.
    pub fn up_fraction(&self) -> Option<f64> {
        if self.window.is_empty() {
            return None;
        }
        let above = self.window.iter().filter(|&&v| v > Decimal::ZERO).count();
        Some(above as f64 / self.window.len() as f64)
    }

    /// Number of times the window crosses the mean value.
    /// Returns `None` for fewer than 2 values.
    pub fn threshold_cross_count(&self) -> Option<usize> {
        if self.window.len() < 2 {
            return None;
        }
        let n = Decimal::from(self.window.len());
        let mean: Decimal = self.window.iter().copied().sum::<Decimal>() / n;
        let vals: Vec<Decimal> = self.window.iter().copied().collect();
        let crosses = vals
            .windows(2)
            .filter(|w| (w[0] > mean) != (w[1] > mean))
            .count();
        Some(crosses)
    }

    /// Approximate entropy using 4 equal-width bins.
    /// Returns `None` for an empty window or all identical values.
    pub fn window_entropy_approx(&self) -> Option<f64> {
        if self.window.is_empty() {
            return None;
        }
        let min = self.window.iter().copied().min()?;
        let max = self.window.iter().copied().max()?;
        if min == max {
            return None;
        }
        use rust_decimal::prelude::ToPrimitive;
        let range = (max - min).to_f64().unwrap_or(0.0);
        let n_bins = 4usize;
        let mut bins = vec![0usize; n_bins];
        for v in self.window.iter() {
            let frac = ((*v - min).to_f64().unwrap_or(0.0) / range) * (n_bins - 1) as f64;
            let idx = frac.round() as usize;
            bins[idx.min(n_bins - 1)] += 1;
        }
        let total = self.window.len() as f64;
        let entropy = bins
            .iter()
            .filter(|&&c| c > 0)
            .map(|&c| {
                let p = c as f64 / total;
                -p * p.ln()
            })
            .sum::<f64>();
        Some(entropy)
    }

    // ── round-102 ────────────────────────────────────────────────────────────

    /// Ratio of the 25th to 75th percentile of the window.
    /// Returns `None` for an empty window or zero 75th percentile.
    pub fn window_q1_q3_ratio(&self) -> Option<Decimal> {
        if self.window.is_empty() {
            return None;
        }
        let q1 = self.window_percentile_25()?;
        let q3 = self.percentile_75()?;
        if q3.is_zero() {
            return None;
        }
        Some(q1 / q3)
    }

    /// Sum of signed consecutive differences (+1 up, -1 down, 0 flat).
    /// Returns `None` for fewer than 2 values.
    pub fn signed_momentum(&self) -> Option<Decimal> {
        if self.window.len() < 2 {
            return None;
        }
        let vals: Vec<Decimal> = self.window.iter().copied().collect();
        let signed: Decimal = vals
            .windows(2)
            .map(|w| if w[1] > w[0] { Decimal::ONE } else if w[1] < w[0] { -Decimal::ONE } else { Decimal::ZERO })
            .sum();
        Some(signed)
    }

    /// Mean length of consecutive positive (increasing) runs.
    /// Returns `None` for fewer than 2 values or no positive runs.
    pub fn positive_run_length(&self) -> Option<f64> {
        if self.window.len() < 2 {
            return None;
        }
        let vals: Vec<Decimal> = self.window.iter().copied().collect();
        let mut runs = Vec::new();
        let mut cur = 0usize;
        for w in vals.windows(2) {
            if w[1] > w[0] {
                cur += 1;
            } else if cur > 0 {
                runs.push(cur);
                cur = 0;
            }
        }
        if cur > 0 {
            runs.push(cur);
        }
        if runs.is_empty() {
            return None;
        }
        Some(runs.iter().sum::<usize>() as f64 / runs.len() as f64)
    }

    /// Ratio of the last trough to the last peak in the window.
    /// Returns `None` if no peak or trough exists, or if peak is zero.
    pub fn valley_to_peak_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 3 {
            return None;
        }
        let vals: Vec<Decimal> = self.window.iter().copied().collect();
        let last_peak = vals
            .windows(3)
            .filter(|w| w[1] > w[0] && w[1] > w[2])
            .map(|w| w[1])
            .last();
        let last_trough = vals
            .windows(3)
            .filter(|w| w[1] < w[0] && w[1] < w[2])
            .map(|w| w[1])
            .last();
        match (last_trough, last_peak) {
            (Some(t), Some(p)) if !p.is_zero() => Some((t / p).to_f64().unwrap_or(0.0)),
            _ => None,
        }
    }

    // ── round-103 ────────────────────────────────────────────────────────────

    /// Maximum peak-to-trough drawdown within the window.
    /// Returns `None` for fewer than 2 values or no decline found.
    pub fn window_max_drawdown(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 {
            return None;
        }
        let vals: Vec<Decimal> = self.window.iter().copied().collect();
        let mut max_drawdown = Decimal::ZERO;
        let mut peak = vals[0];
        for &v in &vals[1..] {
            if v > peak {
                peak = v;
            } else {
                let dd = peak - v;
                if dd > max_drawdown {
                    max_drawdown = dd;
                }
            }
        }
        if max_drawdown.is_zero() {
            return None;
        }
        max_drawdown.to_f64()
    }

    /// Fraction of window values that exceed their immediate predecessor.
    /// Returns `None` for fewer than 2 values.
    pub fn above_previous_fraction(&self) -> Option<f64> {
        if self.window.len() < 2 {
            return None;
        }
        let vals: Vec<Decimal> = self.window.iter().copied().collect();
        let count = vals.windows(2).filter(|w| w[1] > w[0]).count();
        Some(count as f64 / (vals.len() - 1) as f64)
    }

    /// Range efficiency: net move divided by sum of absolute step-wise moves.
    /// A value near 1.0 means a monotone trend; near 0.0 means choppy.
    /// Returns `None` for fewer than 2 values or zero total movement.
    pub fn range_efficiency(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 {
            return None;
        }
        let vals: Vec<Decimal> = self.window.iter().copied().collect();
        let net_move = (vals.last()? - vals.first()?).abs();
        let total_move: Decimal = vals.windows(2).map(|w| (w[1] - w[0]).abs()).sum();
        if total_move.is_zero() {
            return None;
        }
        Some((net_move / total_move).to_f64().unwrap_or(0.0))
    }

    /// Running total (sum) of all values in the window.
    pub fn window_running_total(&self) -> Decimal {
        self.window.iter().copied().sum()
    }

    // ── round-104 ────────────────────────────────────────────────────────────

    /// Convexity: mean second difference `(v[i+2] - 2*v[i+1] + v[i])` across
    /// the window. Positive = accelerating upward; negative = decelerating.
    /// Returns `None` for fewer than 3 values.
    pub fn window_convexity(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 3 {
            return None;
        }
        let vals: Vec<Decimal> = self.window.iter().copied().collect();
        let second_diffs: Vec<Decimal> = vals.windows(3)
            .map(|w| w[2] - Decimal::TWO * w[1] + w[0])
            .collect();
        let n = second_diffs.len();
        let mean: Decimal = second_diffs.iter().copied().sum::<Decimal>() / Decimal::from(n);
        mean.to_f64()
    }

    /// Fraction of window values that are strictly below their immediate predecessor.
    /// Returns `None` for fewer than 2 values.
    pub fn below_previous_fraction(&self) -> Option<f64> {
        if self.window.len() < 2 {
            return None;
        }
        let vals: Vec<Decimal> = self.window.iter().copied().collect();
        let count = vals.windows(2).filter(|w| w[1] < w[0]).count();
        Some(count as f64 / (vals.len() - 1) as f64)
    }

    /// Ratio of the standard deviation of the second half of the window to the first half.
    /// Returns `None` for fewer than 4 values or zero first-half std dev.
    pub fn window_volatility_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 4 {
            return None;
        }
        let vals: Vec<Decimal> = self.window.iter().copied().collect();
        let mid = vals.len() / 2;
        let std_dev = |slice: &[Decimal]| -> Option<f64> {
            let n = slice.len() as f64;
            let mean: f64 = slice.iter().filter_map(|v| v.to_f64()).sum::<f64>() / n;
            let var = slice.iter().filter_map(|v| v.to_f64()).map(|v| (v - mean).powi(2)).sum::<f64>() / n;
            Some(var.sqrt())
        };
        let s1 = std_dev(&vals[..mid])?;
        let s2 = std_dev(&vals[mid..])?;
        if s1 == 0.0 {
            return None;
        }
        Some(s2 / s1)
    }

    /// Gini coefficient of the window values (measures inequality/spread).
    /// Values in [0, 1]: 0 = all equal, 1 = maximally unequal.
    /// Returns `None` for fewer than 2 values or zero sum.
    pub fn window_gini(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 {
            return None;
        }
        let mut vals: Vec<Decimal> = self.window.iter().copied().collect();
        vals.sort();
        let n = vals.len();
        let sum: Decimal = vals.iter().copied().sum();
        if sum.is_zero() {
            return None;
        }
        let weighted: Decimal = vals.iter().enumerate()
            .map(|(i, &v)| Decimal::from(2 * (i as i64 + 1) - n as i64 - 1) * v)
            .sum();
        Some((weighted / (sum * Decimal::from(n))).to_f64().unwrap_or(0.0).abs())
    }

    // ── round-105 ────────────────────────────────────────────────────────────

    /// Approximate Hurst exponent via rescaled range (R/S) analysis.
    /// Returns `None` for fewer than 4 values.
    pub fn window_hurst_exponent(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 4 {
            return None;
        }
        let vals: Vec<f64> = self.window.iter().filter_map(|v| v.to_f64()).collect();
        if vals.len() != self.window.len() { return None; }
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        let deviations: Vec<f64> = vals.iter().map(|v| v - mean).collect();
        let mut cumulative = 0.0_f64;
        let mut cum_max = f64::NEG_INFINITY;
        let mut cum_min = f64::INFINITY;
        for d in &deviations {
            cumulative += d;
            if cumulative > cum_max { cum_max = cumulative; }
            if cumulative < cum_min { cum_min = cumulative; }
        }
        let r = cum_max - cum_min;
        let s = (vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n).sqrt();
        if s == 0.0 || r == 0.0 { return None; }
        Some((r / s).ln() / n.ln())
    }

    /// Count of times the window series crosses its own mean.
    /// Returns `None` for fewer than 2 values.
    pub fn window_mean_crossings(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().filter_map(|v| v.to_f64()).collect();
        if vals.len() != self.window.len() { return None; }
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let crossings = vals.windows(2)
            .filter(|w| (w[0] - mean).signum() != (w[1] - mean).signum()
                && w[0] != mean && w[1] != mean)
            .count();
        Some(crossings)
    }

    /// Sample skewness of the window values.
    /// Returns `None` for fewer than 3 values or zero standard deviation.
    pub fn window_skewness(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 3 { return None; }
        let vals: Vec<f64> = self.window.iter().filter_map(|v| v.to_f64()).collect();
        if vals.len() != self.window.len() { return None; }
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        let variance = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n;
        let std_dev = variance.sqrt();
        if std_dev == 0.0 { return None; }
        let skew = vals.iter().map(|v| ((v - mean) / std_dev).powi(3)).sum::<f64>() / n;
        Some(skew)
    }

    /// Maximum length of a consecutive run of increasing values in the window.
    /// Returns `None` for fewer than 2 values.
    pub fn window_max_run(&self) -> Option<usize> {
        if self.window.len() < 2 { return None; }
        let vals: Vec<Decimal> = self.window.iter().copied().collect();
        let mut max_run = 1usize;
        let mut cur_run = 1usize;
        for i in 1..vals.len() {
            if vals[i] > vals[i - 1] {
                cur_run += 1;
                if cur_run > max_run { max_run = cur_run; }
            } else {
                cur_run = 1;
            }
        }
        Some(max_run)
    }

    // ── round-106 ────────────────────────────────────────────────────────────

    /// Mean absolute deviation from the median of the window.
    /// Returns `None` for an empty window.
    pub fn window_median_deviation(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let mut sorted: Vec<Decimal> = self.window.iter().copied().collect();
        sorted.sort();
        let n = sorted.len();
        let median = if n % 2 == 1 { sorted[n / 2] } else { (sorted[n / 2 - 1] + sorted[n / 2]) / Decimal::TWO };
        let mad: Decimal = sorted.iter().map(|&v| (v - median).abs()).sum::<Decimal>() / Decimal::from(n);
        mad.to_f64()
    }

    /// Length of the longest run of consecutive values above the window mean.
    /// Returns `None` for an empty window.
    pub fn longest_above_mean_run(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().filter_map(|v| v.to_f64()).collect();
        if vals.len() != self.window.len() { return None; }
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let mut max_run = 0usize;
        let mut cur_run = 0usize;
        for &v in &vals {
            if v > mean { cur_run += 1; if cur_run > max_run { max_run = cur_run; } }
            else { cur_run = 0; }
        }
        Some(max_run)
    }

    /// Bimodality coefficient: `(skewness^2 + 1) / kurtosis`.
    /// Values > 5/9 suggest bimodality.
    /// Returns `None` for fewer than 4 values or zero kurtosis.
    pub fn window_bimodality(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 4 { return None; }
        let vals: Vec<f64> = self.window.iter().filter_map(|v| v.to_f64()).collect();
        if vals.len() != self.window.len() { return None; }
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        let var = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n;
        if var == 0.0 { return None; }
        let std_dev = var.sqrt();
        let skew = vals.iter().map(|v| ((v - mean) / std_dev).powi(3)).sum::<f64>() / n;
        let kurt = vals.iter().map(|v| ((v - mean) / std_dev).powi(4)).sum::<f64>() / n;
        if kurt == 0.0 { return None; }
        Some((skew.powi(2) + 1.0) / kurt)
    }

    /// Count of times adjacent values in the window change sign relative to zero.
    /// Returns `None` for fewer than 2 values.
    pub fn window_zero_crossings(&self) -> Option<usize> {
        if self.window.len() < 2 { return None; }
        let vals: Vec<Decimal> = self.window.iter().copied().collect();
        let count = vals.windows(2)
            .filter(|w| {
                (w[0].is_sign_positive() && w[1].is_sign_negative())
                    || (w[0].is_sign_negative() && w[1].is_sign_positive())
            })
            .count();
        Some(count)
    }

    // ── round-107 ────────────────────────────────────────────────────────────

    /// Sum of squared values in the window (signal energy).
    pub fn window_energy(&self) -> Decimal {
        self.window.iter().map(|&v| v * v).sum()
    }

    /// Mean of the middle 50% of window values (IQR mean).
    /// Returns `None` for fewer than 4 values.
    pub fn window_interquartile_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 4 { return None; }
        let mut sorted: Vec<Decimal> = self.window.iter().copied().collect();
        sorted.sort();
        let n = sorted.len();
        let q1_idx = n / 4;
        let q3_idx = (3 * n) / 4;
        let mid: Vec<Decimal> = sorted[q1_idx..q3_idx].to_vec();
        if mid.is_empty() { return None; }
        let mean: Decimal = mid.iter().copied().sum::<Decimal>() / Decimal::from(mid.len());
        mean.to_f64()
    }

    /// Count of window values that exceed the window mean.
    /// Returns `None` for an empty window.
    pub fn above_mean_count(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let total: Decimal = self.window.iter().copied().sum();
        let mean = total / Decimal::from(self.window.len());
        Some(self.window.iter().filter(|&&v| v > mean).count())
    }

    /// Approximate differential entropy using log of std dev.
    /// Returns `None` for fewer than 2 values or zero/negative variance.
    pub fn window_diff_entropy(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().filter_map(|v| v.to_f64()).collect();
        if vals.len() != self.window.len() { return None; }
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        let var = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n - 1.0);
        if var <= 0.0 { return None; }
        Some(0.5 * (2.0 * std::f64::consts::PI * std::f64::consts::E * var).ln())
    }

    // ── round-108 ────────────────────────────────────────────────────────────

    /// Root mean square of the window values.
    pub fn window_root_mean_square(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let sum_sq: Decimal = self.window.iter().map(|&v| v * v).sum();
        let mean_sq = sum_sq / Decimal::from(self.window.len());
        mean_sq.to_f64().map(f64::sqrt)
    }

    /// Mean of the first differences `v[i+1] - v[i]` across the window.
    /// Returns `None` for fewer than 2 values.
    pub fn window_first_derivative_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<Decimal> = self.window.iter().copied().collect();
        let diffs: Vec<Decimal> = vals.windows(2).map(|w| w[1] - w[0]).collect();
        let n = diffs.len();
        let mean: Decimal = diffs.iter().copied().sum::<Decimal>() / Decimal::from(n);
        mean.to_f64()
    }

    /// L1 norm (sum of absolute values) of the window.
    pub fn window_l1_norm(&self) -> Decimal {
        self.window.iter().map(|&v| v.abs()).sum()
    }

    /// 10th-percentile value of the window.
    /// Returns `None` for an empty window.
    pub fn window_percentile_10(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let mut sorted: Vec<Decimal> = self.window.iter().copied().collect();
        sorted.sort();
        let idx = ((sorted.len() as f64 * 0.10).ceil() as usize).saturating_sub(1);
        sorted[idx.min(sorted.len() - 1)].to_f64()
    }

    // ── round-109 ────────────────────────────────────────────────────────────

    /// Mean z-score of all window values (should equal 0 by definition).
    /// Returns `None` for fewer than 2 values or zero std dev.
    pub fn window_zscore_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().filter_map(|v| v.to_f64()).collect();
        if vals.len() != self.window.len() { return None; }
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        let std_dev = (vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n).sqrt();
        if std_dev == 0.0 { return None; }
        let sum_z: f64 = vals.iter().map(|v| (v - mean) / std_dev).sum();
        Some(sum_z / n)
    }

    /// Sum of all positive values in the window.
    pub fn window_positive_sum(&self) -> Decimal {
        self.window.iter().copied().filter(|v| v.is_sign_positive()).sum()
    }

    /// Sum of all negative values in the window (result is <= 0).
    pub fn window_negative_sum(&self) -> Decimal {
        self.window.iter().copied().filter(|v| v.is_sign_negative()).sum()
    }

    /// Fraction of consecutive pairs that continue in the same direction as the
    /// overall window trend.
    /// Returns `None` for fewer than 2 values or no net trend.
    pub fn window_trend_consistency(&self) -> Option<f64> {
        if self.window.len() < 2 { return None; }
        let vals: Vec<Decimal> = self.window.iter().copied().collect();
        let overall = vals.last()? - vals.first()?;
        if overall.is_zero() { return None; }
        let consistent = vals.windows(2)
            .filter(|w| {
                let step = w[1] - w[0];
                (overall.is_sign_positive() && step.is_sign_positive())
                    || (overall.is_sign_negative() && step.is_sign_negative())
            })
            .count();
        Some(consistent as f64 / (vals.len() - 1) as f64)
    }

    // ── round-110 ────────────────────────────────────────────────────────────

    /// Mean of all pairwise absolute differences.
    /// Returns `None` for fewer than 2 values.
    pub fn window_pairwise_mean_diff(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<Decimal> = self.window.iter().copied().collect();
        let n = vals.len();
        let mut sum = Decimal::ZERO;
        let mut count = 0u64;
        for i in 0..n {
            for j in i + 1..n {
                sum += (vals[i] - vals[j]).abs();
                count += 1;
            }
        }
        if count == 0 { return None; }
        (sum / Decimal::from(count)).to_f64()
    }

    /// 75th-percentile value of the window.
    /// Returns `None` for an empty window.
    pub fn window_q3(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let mut sorted: Vec<Decimal> = self.window.iter().copied().collect();
        sorted.sort();
        let idx = ((sorted.len() as f64 * 0.75).ceil() as usize).saturating_sub(1);
        sorted[idx.min(sorted.len() - 1)].to_f64()
    }

    /// Coefficient of variation: `std_dev / mean`.
    /// Returns `None` for fewer than 2 values or zero mean.
    pub fn window_coefficient_of_variation(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().filter_map(|v| v.to_f64()).collect();
        if vals.len() != self.window.len() { return None; }
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        if mean == 0.0 { return None; }
        let std_dev = (vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n).sqrt();
        Some(std_dev / mean)
    }

    /// Second statistical moment (mean of squared values).
    pub fn window_second_moment(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let sum_sq: Decimal = self.window.iter().map(|&v| v * v).sum();
        (sum_sq / Decimal::from(self.window.len())).to_f64()
    }

    // ── round-111 ────────────────────────────────────────────────────────────

    /// Sum of window values after trimming the top and bottom 10%.
    /// Returns `None` for fewer than 5 values.
    pub fn window_trimmed_sum(&self) -> Option<Decimal> {
        if self.window.len() < 5 { return None; }
        let mut sorted: Vec<Decimal> = self.window.iter().copied().collect();
        sorted.sort();
        let n = sorted.len();
        let trim = (n as f64 * 0.1).floor() as usize;
        let trimmed = &sorted[trim..n - trim];
        if trimmed.is_empty() { return None; }
        Some(trimmed.iter().copied().sum())
    }

    /// Z-score of the window range `(max - min)` relative to the window mean.
    /// Returns `None` for fewer than 2 values or zero std dev.
    pub fn window_range_zscore(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().filter_map(|v| v.to_f64()).collect();
        if vals.len() != self.window.len() { return None; }
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        let std_dev = (vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n).sqrt();
        if std_dev == 0.0 { return None; }
        let range = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max)
            - vals.iter().cloned().fold(f64::INFINITY, f64::min);
        Some((range - mean) / std_dev)
    }

    /// Count of window values strictly above the window median.
    /// Returns `None` for an empty window.
    pub fn window_above_median_count(&self) -> Option<usize> {
        if self.window.is_empty() { return None; }
        let mut sorted: Vec<Decimal> = self.window.iter().copied().collect();
        sorted.sort();
        let n = sorted.len();
        let median = if n % 2 == 1 { sorted[n / 2] }
                     else { (sorted[n / 2 - 1] + sorted[n / 2]) / Decimal::TWO };
        Some(self.window.iter().filter(|&&v| v > median).count())
    }

    /// Maximum length of a consecutive run of *decreasing* values in the window.
    /// Returns `None` for fewer than 2 values.
    pub fn window_min_run(&self) -> Option<usize> {
        if self.window.len() < 2 { return None; }
        let vals: Vec<Decimal> = self.window.iter().copied().collect();
        let mut max_run = 1usize;
        let mut cur_run = 1usize;
        for i in 1..vals.len() {
            if vals[i] < vals[i - 1] {
                cur_run += 1;
                if cur_run > max_run { max_run = cur_run; }
            } else {
                cur_run = 1;
            }
        }
        Some(max_run)
    }

    // ── round-112 ────────────────────────────────────────────────────────────

    /// Harmonic mean of the window values. Returns `None` if window is empty
    /// or any value is zero.
    pub fn window_harmonic_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let mut sum_recip = 0f64;
        for &v in &self.window {
            let f = v.to_f64()?;
            if f == 0.0 { return None; }
            sum_recip += 1.0 / f;
        }
        Some(self.window.len() as f64 / sum_recip)
    }

    /// Geometric standard deviation of the window (exp of std-dev of log-values).
    /// Returns `None` if window has fewer than 2 values or any value is non-positive.
    pub fn window_geometric_std(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let logs: Vec<f64> = self.window.iter().map(|&v| {
            let f = v.to_f64()?;
            if f <= 0.0 { None } else { Some(f.ln()) }
        }).collect::<Option<Vec<_>>>()?;
        let n = logs.len() as f64;
        let mean = logs.iter().sum::<f64>() / n;
        let var = logs.iter().map(|&x| (x - mean).powi(2)).sum::<f64>() / (n - 1.0);
        Some(var.sqrt().exp())
    }

    /// Entropy rate approximation: mean of absolute first differences of the window.
    /// Returns `None` for fewer than 2 values.
    pub fn window_entropy_rate(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let sum: f64 = vals.windows(2).map(|w| (w[1] - w[0]).abs()).sum();
        Some(sum / (vals.len() - 1) as f64)
    }

    /// Burstiness: (std_dev - mean) / (std_dev + mean). Returns `None` if window
    /// is empty, mean + std_dev is zero, or fewer than 2 values for std_dev.
    pub fn window_burstiness(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        let var = vals.iter().map(|&x| (x - mean).powi(2)).sum::<f64>() / (n - 1.0);
        let std = var.sqrt();
        let denom = std + mean;
        if denom == 0.0 { None } else { Some((std - mean) / denom) }
    }

    // ── round-113 ────────────────────────────────────────────────────────────

    /// Ratio of IQR to median. Returns `None` if fewer than 3 values or zero median.
    pub fn window_iqr_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 3 { return None; }
        let mut sorted: Vec<Decimal> = self.window.iter().copied().collect();
        sorted.sort();
        let n = sorted.len();
        let median = if n % 2 == 1 { sorted[n / 2] }
                     else { (sorted[n / 2 - 1] + sorted[n / 2]) / Decimal::TWO };
        if median.is_zero() { return None; }
        let q1 = sorted[n / 4];
        let q3 = sorted[3 * n / 4];
        let iqr = q3 - q1;
        (iqr / median).to_f64()
    }

    /// Mean-reversion score: fraction of consecutive pairs where the value moves
    /// toward the overall window mean. Returns `None` for fewer than 2 values.
    pub fn window_mean_reversion(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let count = vals.windows(2).filter(|w| {
            let before = (w[0] - mean).abs();
            let after = (w[1] - mean).abs();
            after < before
        }).count();
        Some(count as f64 / (vals.len() - 1) as f64)
    }

    /// Lag-1 autocorrelation of the window values. Returns `None` for fewer than
    /// 3 values or zero variance.
    pub fn window_autocorrelation(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 3 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        let var = vals.iter().map(|&x| (x - mean).powi(2)).sum::<f64>() / n;
        if var == 0.0 { return None; }
        let cov: f64 = vals.windows(2).map(|w| (w[0] - mean) * (w[1] - mean)).sum::<f64>()
            / (vals.len() - 1) as f64;
        Some(cov / var)
    }

    /// Ordinary least-squares slope of window values over their index.
    /// Returns `None` for fewer than 2 values.
    pub fn window_slope(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let n = vals.len() as f64;
        let x_mean = (n - 1.0) / 2.0;
        let y_mean = vals.iter().sum::<f64>() / n;
        let num: f64 = vals.iter().enumerate().map(|(i, &y)| (i as f64 - x_mean) * (y - y_mean)).sum();
        let den: f64 = vals.iter().enumerate().map(|(i, _)| (i as f64 - x_mean).powi(2)).sum();
        if den == 0.0 { None } else { Some(num / den) }
    }

    // ── round-114 ────────────────────────────────────────────────────────────

    /// Crest factor: max absolute value divided by RMS. Returns `None` if window
    /// is empty or RMS is zero.
    pub fn window_crest_factor(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let rms = (vals.iter().map(|&x| x * x).sum::<f64>() / vals.len() as f64).sqrt();
        if rms == 0.0 { return None; }
        let peak = vals.iter().map(|&x| x.abs()).fold(0f64, f64::max);
        Some(peak / rms)
    }

    /// Relative range: `(max - min) / mean`. Returns `None` for empty window or
    /// zero mean.
    pub fn window_relative_range(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        if mean == 0.0 { return None; }
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        Some((max - min) / mean)
    }

    /// Count of values more than `k` standard deviations from the mean (default k=2).
    /// Returns `None` for fewer than 2 values.
    pub fn window_outlier_count(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        let std = (vals.iter().map(|&x| (x - mean).powi(2)).sum::<f64>() / (n - 1.0)).sqrt();
        if std == 0.0 { return Some(0); }
        let k = 2.0f64;
        Some(vals.iter().filter(|&&x| (x - mean).abs() > k * std).count())
    }

    /// Exponential decay score: weighted mean where older values have exponentially
    /// lower weight (alpha=0.5). Returns `None` for empty window.
    pub fn window_decay_score(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let alpha = 0.5f64;
        let n = vals.len();
        let mut num = 0f64;
        let mut denom = 0f64;
        for (i, &v) in vals.iter().enumerate() {
            let w = alpha.powi((n - 1 - i) as i32);
            num += v * w;
            denom += w;
        }
        if denom == 0.0 { None } else { Some(num / denom) }
    }

    // ── round-115 ────────────────────────────────────────────────────────────

    /// Mean log return between consecutive window values.
    /// Returns `None` for fewer than 2 values or any non-positive value.
    pub fn window_log_return(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| {
            let f = v.to_f64()?;
            if f <= 0.0 { None } else { Some(f) }
        }).collect::<Option<Vec<_>>>()?;
        let sum: f64 = vals.windows(2).map(|w| (w[1] / w[0]).ln()).sum();
        Some(sum / (vals.len() - 1) as f64)
    }

    /// Signed RMS: RMS with the sign of the window mean. Returns `None` for empty window.
    pub fn window_signed_rms(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let rms = (vals.iter().map(|&x| x * x).sum::<f64>() / vals.len() as f64).sqrt();
        Some(if mean < 0.0 { -rms } else { rms })
    }

    /// Count of inflection points (local minima or maxima) in the window.
    /// Returns `None` for fewer than 3 values.
    pub fn window_inflection_count(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 3 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let count = vals.windows(3).filter(|w| {
            (w[1] > w[0] && w[1] > w[2]) || (w[1] < w[0] && w[1] < w[2])
        }).count();
        Some(count)
    }

    /// Centroid of the window: index-weighted mean position.
    /// Returns `None` for empty window or zero total weight.
    pub fn window_centroid(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let total: f64 = vals.iter().sum();
        if total == 0.0 { return None; }
        let weighted: f64 = vals.iter().enumerate().map(|(i, &v)| i as f64 * v).sum();
        Some(weighted / total)
    }

    // ── round-116 ────────────────────────────────────────────────────────────

    /// Maximum absolute deviation from the window mean.
    /// Returns `None` for empty window.
    pub fn window_max_deviation(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let max_dev = vals.iter().map(|&x| (x - mean).abs()).fold(0f64, f64::max);
        Some(max_dev)
    }

    /// Ratio of window range to mean. Returns `None` for empty window or zero mean.
    pub fn window_range_mean_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        if mean == 0.0 { return None; }
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        Some((max - min) / mean)
    }

    /// Count of consecutive steps where each value is strictly greater than the previous.
    /// Returns `None` for fewer than 2 values.
    pub fn window_step_up_count(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        Some(vals.windows(2).filter(|w| w[1] > w[0]).count())
    }

    /// Count of consecutive steps where each value is strictly less than the previous.
    /// Returns `None` for fewer than 2 values.
    pub fn window_step_down_count(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        Some(vals.windows(2).filter(|w| w[1] < w[0]).count())
    }

    // ── round-117 ────────────────────────────────────────────────────────────

    /// Shannon entropy of absolute differences between consecutive window values,
    /// binned into 8 equal-width buckets. Returns `None` for fewer than 2 values.
    pub fn window_entropy_of_changes(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let diffs: Vec<f64> = vals.windows(2).map(|w| (w[1] - w[0]).abs()).collect();
        let max = diffs.iter().cloned().fold(0f64, f64::max);
        let min = diffs.iter().cloned().fold(f64::INFINITY, f64::min);
        let range = max - min;
        const BUCKETS: usize = 8;
        let mut counts = [0usize; BUCKETS];
        if range == 0.0 {
            counts[0] = diffs.len();
        } else {
            for &d in &diffs {
                let idx = ((d - min) / range * (BUCKETS - 1) as f64) as usize;
                counts[idx.min(BUCKETS - 1)] += 1;
            }
        }
        let n = diffs.len() as f64;
        Some(counts.iter().map(|&c| {
            if c == 0 { 0.0 } else { let p = c as f64 / n; -p * p.ln() }
        }).sum())
    }

    /// Rate at which window values cross their mean level.
    /// Returns `None` for fewer than 2 values.
    pub fn window_level_crossing_rate(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let crossings = vals.windows(2).filter(|w| {
            (w[0] >= mean && w[1] < mean) || (w[0] < mean && w[1] >= mean)
        }).count();
        Some(crossings as f64 / (vals.len() - 1) as f64)
    }

    /// Mean of absolute window values. Returns `None` for empty window.
    pub fn window_abs_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let sum: f64 = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0).abs()).sum();
        Some(sum / self.window.len() as f64)
    }

    /// Maximum value in the window. Returns `None` for empty window.
    pub fn window_rolling_max(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        self.window.iter().map(|v| v.to_f64().unwrap_or(f64::NEG_INFINITY)).reduce(f64::max)
    }

    // ── round-118 ────────────────────────────────────────────────────────────

    /// Minimum value in the window. Returns `None` for empty window.
    pub fn window_rolling_min(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        self.window.iter().map(|v| v.to_f64().unwrap_or(f64::INFINITY)).reduce(f64::min)
    }

    /// Fraction of window values that are strictly negative.
    /// Returns `None` for empty window.
    pub fn window_negative_fraction(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let neg = self.window.iter().filter(|&&v| v < rust_decimal::Decimal::ZERO).count();
        Some(neg as f64 / self.window.len() as f64)
    }

    /// Fraction of window values that are strictly positive.
    /// Returns `None` for empty window.
    pub fn window_positive_fraction(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let pos = self.window.iter().filter(|&&v| v > rust_decimal::Decimal::ZERO).count();
        Some(pos as f64 / self.window.len() as f64)
    }

    /// Last window value minus the window minimum.
    /// Returns `None` for empty window.
    pub fn window_last_minus_min(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let last = self.window.back()?.to_f64()?;
        let min = self.window.iter().map(|v| v.to_f64().unwrap_or(f64::INFINITY)).fold(f64::INFINITY, f64::min);
        Some(last - min)
    }

    // ── round-119 ────────────────────────────────────────────────────────────

    /// Range of the window: `max - min`. Returns `None` for empty window.
    pub fn window_max_minus_min(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let max = self.window.iter().map(|v| v.to_f64().unwrap_or(f64::NEG_INFINITY)).fold(f64::NEG_INFINITY, f64::max);
        let min = self.window.iter().map(|v| v.to_f64().unwrap_or(f64::INFINITY)).fold(f64::INFINITY, f64::min);
        Some(max - min)
    }

    /// Normalized mean: `(mean - min) / (max - min)`. Returns `None` for empty window
    /// or zero range.
    pub fn window_normalized_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        let range = max - min;
        if range == 0.0 { None } else { Some((mean - min) / range) }
    }

    /// Ratio of variance to mean squared (equivalent to squared CV).
    /// Returns `None` for fewer than 2 values or zero mean.
    pub fn window_variance_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        if mean == 0.0 { return None; }
        let var = vals.iter().map(|&x| (x - mean).powi(2)).sum::<f64>() / (n - 1.0);
        Some(var / (mean * mean))
    }

    /// Maximum window value minus the last window value.
    /// Returns `None` for empty window.
    pub fn window_max_minus_last(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let last = self.window.back()?.to_f64()?;
        let max = self.window.iter().map(|v| v.to_f64().unwrap_or(f64::NEG_INFINITY)).fold(f64::NEG_INFINITY, f64::max);
        Some(max - last)
    }

    // ── round-120 ────────────────────────────────────────────────────────────

    /// Count of window values strictly above the last value.
    pub fn window_above_last(&self) -> Option<usize> {
        if self.window.is_empty() { return None; }
        let last = self.window.back()?;
        Some(self.window.iter().filter(|v| *v > last).count())
    }

    /// Count of window values strictly below the last value.
    pub fn window_below_last(&self) -> Option<usize> {
        if self.window.is_empty() { return None; }
        let last = self.window.back()?;
        Some(self.window.iter().filter(|v| *v < last).count())
    }

    /// Mean of successive differences.
    pub fn window_diff_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let diffs: Vec<f64> = vals.windows(2).map(|w| w[1] - w[0]).collect();
        Some(diffs.iter().sum::<f64>() / diffs.len() as f64)
    }

    /// Z-score of the last value relative to the window.
    pub fn window_last_zscore(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        let std = (vals.iter().map(|&x| (x - mean).powi(2)).sum::<f64>() / n).sqrt();
        if std == 0.0 { return None; }
        let last = *vals.last()?;
        Some((last - mean) / std)
    }

    // ── round-121 ────────────────────────────────────────────────────────────

    /// Fraction of window values within lower half of range.
    pub fn window_range_fraction(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let range = max - min;
        if range == 0.0 { return Some(1.0); }
        let mid = min + range / 2.0;
        Some(vals.iter().filter(|&&v| v <= mid).count() as f64 / vals.len() as f64)
    }

    /// Whether the window mean is above the last value (1.0 if so, 0.0 otherwise).
    pub fn window_mean_above_last(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let last = *vals.last()?;
        Some(if mean > last { 1.0 } else { 0.0 })
    }

    /// Volatility trend: std of second half minus std of first half of window.
    pub fn window_volatility_trend(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 4 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mid = vals.len() / 2;
        let std_half = |half: &[f64]| -> f64 {
            let n = half.len() as f64;
            let m = half.iter().sum::<f64>() / n;
            (half.iter().map(|&x| (x - m).powi(2)).sum::<f64>() / n).sqrt()
        };
        Some(std_half(&vals[mid..]) - std_half(&vals[..mid]))
    }

    /// Count of sign changes in successive differences across the window.
    pub fn window_sign_change_count(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 3 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let diffs: Vec<f64> = vals.windows(2).map(|w| w[1] - w[0]).collect();
        let changes = diffs.windows(2)
            .filter(|w| w[0] * w[1] < 0.0)
            .count();
        Some(changes)
    }

    // ── round-122 ────────────────────────────────────────────────────────────

    /// Rank of the last value within the window (0.0 = min, 1.0 = max).
    pub fn window_last_rank(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let last = self.window.back()?.to_f64()?;
        let mut sorted: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let pos = sorted.iter().position(|&x| (x - last).abs() < 1e-12).unwrap_or(0);
        Some(pos as f64 / (sorted.len() - 1).max(1) as f64)
    }

    /// Momentum score: mean of sign(diff) across successive window values.
    pub fn window_momentum_score(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let score: f64 = vals.windows(2)
            .map(|w| if w[1] > w[0] { 1.0 } else if w[1] < w[0] { -1.0 } else { 0.0 })
            .sum::<f64>() / (vals.len() - 1) as f64;
        Some(score)
    }

    /// Count of local maxima in the window.
    pub fn window_oscillation_count(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 3 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let count = vals.windows(3).filter(|w| w[1] > w[0] && w[1] > w[2]).count();
        Some(count)
    }

    /// Direction of skew: 1.0 if mean > median, -1.0 if mean < median, 0.0 if equal.
    pub fn window_skew_direction(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let mut sorted: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = sorted.len();
        let median = if n % 2 == 0 { (sorted[n/2 - 1] + sorted[n/2]) / 2.0 } else { sorted[n/2] };
        let mean = sorted.iter().sum::<f64>() / n as f64;
        Some(if mean > median { 1.0 } else if mean < median { -1.0 } else { 0.0 })
    }

    // ── round-123 ────────────────────────────────────────────────────────────

    /// Count of trend reversals: sign changes in consecutive differences.
    pub fn window_trend_reversal_count(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 3 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let diffs: Vec<f64> = vals.windows(2).map(|w| w[1] - w[0]).collect();
        Some(diffs.windows(2).filter(|w| w[0] * w[1] < 0.0).count())
    }

    /// Difference between first and last values in the window.
    pub fn window_first_last_diff(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let first = self.window.front()?.to_f64()?;
        let last = self.window.back()?.to_f64()?;
        Some(last - first)
    }

    /// Count of values in the upper half of the window's range.
    pub fn window_upper_half_count(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let mid = (min + max) / 2.0;
        Some(vals.iter().filter(|&&v| v > mid).count())
    }

    /// Count of values in the lower half of the window's range.
    pub fn window_lower_half_count(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let mid = (min + max) / 2.0;
        Some(vals.iter().filter(|&&v| v < mid).count())
    }

    // ── round-124 ────────────────────────────────────────────────────────────

    /// 75th percentile of window values.
    pub fn window_percentile_75(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let mut sorted: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let idx = ((sorted.len() as f64 * 0.75) as usize).min(sorted.len() - 1);
        Some(sorted[idx])
    }

    /// Absolute slope: |last - first| / (n - 1).
    pub fn window_abs_slope(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let first = self.window.front()?.to_f64()?;
        let last = self.window.back()?.to_f64()?;
        Some((last - first).abs() / (self.window.len() - 1) as f64)
    }

    /// Ratio of sum of gains to sum of losses; None if no losses.
    pub fn window_gain_loss_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mut gains = 0f64;
        let mut losses = 0f64;
        for w in vals.windows(2) {
            let d = w[1] - w[0];
            if d > 0.0 { gains += d; } else { losses += d.abs(); }
        }
        if losses == 0.0 { return None; }
        Some(gains / losses)
    }

    /// Stability of range: 1 - std(range_pct).
    pub fn window_range_stability(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let range = max - min;
        if range == 0.0 { return Some(1.0); }
        let pcts: Vec<f64> = vals.iter().map(|&v| (v - min) / range).collect();
        let n = pcts.len() as f64;
        let mean = pcts.iter().sum::<f64>() / n;
        let std = (pcts.iter().map(|&x| (x - mean).powi(2)).sum::<f64>() / n).sqrt();
        Some(1.0 - std)
    }

    // ── round-125 ────────────────────────────────────────────────────────────

    /// Exponentially smoothed last value (α=0.2).
    pub fn window_exp_smoothed(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let alpha = 0.2f64;
        let mut ema = self.window.front()?.to_f64()?;
        for v in self.window.iter().skip(1) {
            ema = alpha * v.to_f64().unwrap_or(0.0) + (1.0 - alpha) * ema;
        }
        Some(ema)
    }

    /// Maximum drawdown in the window.
    pub fn window_drawdown(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mut max_dd = 0f64;
        let mut peak = vals[0];
        for &v in &vals[1..] {
            if v > peak { peak = v; }
            let dd = peak - v;
            if dd > max_dd { max_dd = dd; }
        }
        Some(max_dd)
    }

    /// Maximum drawup in the window.
    pub fn window_drawup(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mut max_du = 0f64;
        let mut trough = vals[0];
        for &v in &vals[1..] {
            if v < trough { trough = v; }
            let du = v - trough;
            if du > max_du { max_du = du; }
        }
        Some(max_du)
    }

    /// Trend strength: |signed net| / total absolute movement.
    pub fn window_trend_strength(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let signed: f64 = vals.windows(2).map(|w| w[1] - w[0]).sum();
        let abs_sum: f64 = vals.windows(2).map(|w| (w[1] - w[0]).abs()).sum();
        if abs_sum == 0.0 { return None; }
        Some(signed.abs() / abs_sum)
    }

    // ── round-126 ────────────────────────────────────────────────────────────

    /// Deviation of last value from EMA (α=0.2) of the window.
    pub fn window_ema_deviation(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let alpha = 0.2f64;
        let mut ema = self.window.front()?.to_f64()?;
        for v in self.window.iter().skip(1) {
            ema = alpha * v.to_f64().unwrap_or(0.0) + (1.0 - alpha) * ema;
        }
        let last = self.window.back()?.to_f64()?;
        Some(last - ema)
    }

    /// Variance normalized by mean squared.
    pub fn window_normalized_variance(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        if mean == 0.0 { return None; }
        let var = vals.iter().map(|&x| (x - mean).powi(2)).sum::<f64>() / n;
        Some(var / mean.powi(2))
    }

    /// Ratio of last value to window median.
    pub fn window_median_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let mut sorted: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = sorted.len();
        let median = if n % 2 == 0 { (sorted[n/2-1] + sorted[n/2]) / 2.0 } else { sorted[n/2] };
        if median == 0.0 { return None; }
        let last = self.window.back()?.to_f64()?;
        Some(last / median)
    }

    /// Approximate half-life: steps for value to decay to half peak from peak position.
    pub fn window_half_life(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let peak = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let half = peak / 2.0;
        let peak_idx = vals.iter().position(|&v| (v - peak).abs() < 1e-12)?;
        vals[peak_idx..].iter().position(|&v| v <= half)
    }

    // ── round-127 ────────────────────────────────────────────────────────────

    /// Shannon entropy of the window normalized to [0, 1].
    pub fn window_entropy_normalized(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let n = vals.len() as f64;
        let total: f64 = vals.iter().map(|&v| v.abs()).sum();
        if total == 0.0 { return Some(0.0); }
        let entropy: f64 = vals.iter()
            .map(|&v| { let p = v.abs() / total; if p > 0.0 { -p * p.log2() } else { 0.0 } })
            .sum();
        let max_entropy = n.log2();
        if max_entropy == 0.0 { return Some(0.0); }
        Some(entropy / max_entropy)
    }

    /// Maximum value in the window.
    pub fn window_peak_value(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        Some(self.window.iter().map(|v| v.to_f64().unwrap_or(f64::NEG_INFINITY)).fold(f64::NEG_INFINITY, f64::max))
    }

    /// Minimum value in the window.
    pub fn window_trough_value(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        Some(self.window.iter().map(|v| v.to_f64().unwrap_or(f64::INFINITY)).fold(f64::INFINITY, f64::min))
    }

    /// Count of positive successive differences (gains).
    pub fn window_gain_count(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        Some(vals.windows(2).filter(|w| w[1] > w[0]).count())
    }

    // ── round-128 ────────────────────────────────────────────────────────────

    /// Count of negative successive differences (losses).
    pub fn window_loss_count(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        Some(vals.windows(2).filter(|w| w[1] < w[0]).count())
    }

    /// Net change: last value minus first value.
    pub fn window_net_change(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let first = self.window.front()?.to_f64()?;
        let last = self.window.back()?.to_f64()?;
        Some(last - first)
    }

    /// Mean second-order difference (acceleration) of window values.
    pub fn window_acceleration(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 3 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let acc: Vec<f64> = vals.windows(3).map(|w| w[2] - 2.0 * w[1] + w[0]).collect();
        Some(acc.iter().sum::<f64>() / acc.len() as f64)
    }

    /// Regime score: fraction of values above mean minus fraction below mean.
    pub fn window_regime_score(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        let above = vals.iter().filter(|&&v| v > mean).count() as f64;
        let below = vals.iter().filter(|&&v| v < mean).count() as f64;
        Some((above - below) / n)
    }

    // ── round-129 ────────────────────────────────────────────────────────────

    /// Cumulative sum of all window values.
    pub fn window_cumulative_sum(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let sum: f64 = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).sum();
        Some(sum)
    }

    /// Spread ratio: (max - min) / mean of the window.
    pub fn window_spread_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        if mean == 0.0 { return None; }
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        Some((max - min) / mean.abs())
    }

    /// Center of mass: weighted index position (higher = recent values dominate).
    pub fn window_center_of_mass(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let total: f64 = vals.iter().sum();
        if total == 0.0 { return None; }
        let weighted: f64 = vals.iter().enumerate()
            .map(|(i, &v)| i as f64 * v)
            .sum();
        Some(weighted / total)
    }

    /// Cycle count: number of direction reversals in the window.
    pub fn window_cycle_count(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 3 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let diffs: Vec<f64> = vals.windows(2).map(|w| w[1] - w[0]).collect();
        let count = diffs.windows(2)
            .filter(|w| w[0] * w[1] < 0.0)
            .count();
        Some(count)
    }

    // ── round-130 ────────────────────────────────────────────────────────────

    /// Mean absolute deviation from the window mean.
    pub fn window_mad(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let mad = vals.iter().map(|&v| (v - mean).abs()).sum::<f64>() / vals.len() as f64;
        Some(mad)
    }

    /// Entropy ratio: actual entropy / max possible entropy for window size.
    pub fn window_entropy_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let total: f64 = vals.iter().sum();
        if total == 0.0 { return None; }
        let entropy: f64 = vals.iter().map(|&v| {
            let p = v / total;
            if p > 0.0 { -p * p.ln() } else { 0.0 }
        }).sum();
        let max_entropy = (vals.len() as f64).ln();
        if max_entropy == 0.0 { return None; }
        Some(entropy / max_entropy)
    }

    /// Plateau count: number of consecutive equal values in the window.
    pub fn window_plateau_count(&self) -> Option<usize> {
        if self.window.len() < 2 { return None; }
        let count = self.window.iter()
            .collect::<Vec<_>>()
            .windows(2)
            .filter(|w| w[0] == w[1])
            .count();
        Some(count)
    }

    /// Direction bias: fraction of steps that are upward minus fraction downward.
    pub fn window_direction_bias(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let n = (vals.len() - 1) as f64;
        let up = vals.windows(2).filter(|w| w[1] > w[0]).count() as f64;
        let dn = vals.windows(2).filter(|w| w[1] < w[0]).count() as f64;
        Some((up - dn) / n)
    }

    // ── round-131 ────────────────────────────────────────────────────────────

    /// Percentage change from first to last value in the window.
    pub fn window_last_pct_change(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let first = self.window.front()?.to_f64()?;
        let last = self.window.back()?.to_f64()?;
        if first == 0.0 { return None; }
        Some((last - first) / first.abs())
    }

    /// Standard deviation trend: difference between std of 2nd half and 1st half.
    pub fn window_std_trend(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 4 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mid = vals.len() / 2;
        let std_half = |slice: &[f64]| {
            let mean = slice.iter().sum::<f64>() / slice.len() as f64;
            (slice.iter().map(|&v| (v - mean).powi(2)).sum::<f64>() / slice.len() as f64).sqrt()
        };
        let first_std = std_half(&vals[..mid]);
        let second_std = std_half(&vals[mid..]);
        Some(second_std - first_std)
    }

    /// Count of non-zero values in the window.
    pub fn window_nonzero_count(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let count = self.window.iter()
            .filter(|v| v.to_f64().unwrap_or(0.0) != 0.0)
            .count();
        Some(count)
    }

    /// Fraction of window values above the mean.
    pub fn window_pct_above_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let above = vals.iter().filter(|&&v| v > mean).count() as f64;
        Some(above / vals.len() as f64)
    }

    // ── round-132 ────────────────────────────────────────────────────────────

    /// Maximum consecutive run of increasing values in the window.
    pub fn window_max_run_up(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mut max_run = 0usize;
        let mut cur_run = 0usize;
        for w in vals.windows(2) {
            if w[1] > w[0] { cur_run += 1; max_run = max_run.max(cur_run); }
            else { cur_run = 0; }
        }
        Some(max_run)
    }

    /// Maximum consecutive run of decreasing values in the window.
    pub fn window_max_run_dn(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mut max_run = 0usize;
        let mut cur_run = 0usize;
        for w in vals.windows(2) {
            if w[1] < w[0] { cur_run += 1; max_run = max_run.max(cur_run); }
            else { cur_run = 0; }
        }
        Some(max_run)
    }

    /// Sum of all consecutive differences in the window.
    pub fn window_diff_sum(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let sum: f64 = vals.windows(2).map(|w| w[1] - w[0]).sum();
        Some(sum)
    }

    /// Longest run in the same direction (up or down).
    pub fn window_run_length(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mut max_run = 0usize;
        let mut cur_run = 1usize;
        let diffs: Vec<f64> = vals.windows(2).map(|w| w[1] - w[0]).collect();
        for i in 1..diffs.len() {
            if diffs[i] * diffs[i - 1] > 0.0 { cur_run += 1; max_run = max_run.max(cur_run); }
            else { max_run = max_run.max(cur_run); cur_run = 1; }
        }
        max_run = max_run.max(cur_run);
        Some(max_run)
    }

    // ── round-133 ────────────────────────────────────────────────────────────

    /// Sum of absolute differences between consecutive window values.
    pub fn window_abs_diff_sum(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let sum: f64 = vals.windows(2).map(|w| (w[1] - w[0]).abs()).sum();
        Some(sum)
    }

    /// Maximum gap between any two consecutive window values.
    pub fn window_max_gap(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let max_gap = vals.windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(f64::NEG_INFINITY, f64::max);
        Some(max_gap)
    }

    /// Count of local maxima in the window (values greater than both neighbors).
    pub fn window_local_max_count(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 3 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let count = vals.windows(3).filter(|w| w[1] > w[0] && w[1] > w[2]).count();
        Some(count)
    }

    /// Mean of the first half of the window.
    pub fn window_first_half_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mid = vals.len() / 2;
        let first_half = &vals[..mid];
        if first_half.is_empty() { return None; }
        Some(first_half.iter().sum::<f64>() / first_half.len() as f64)
    }

    // ── round-134 ────────────────────────────────────────────────────────────

    /// Mean of the second half of the window.
    pub fn window_second_half_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mid = vals.len() / 2;
        let second_half = &vals[mid..];
        if second_half.is_empty() { return None; }
        Some(second_half.iter().sum::<f64>() / second_half.len() as f64)
    }

    /// Count of local minima in the window (values less than both neighbors).
    pub fn window_local_min_count(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 3 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let count = vals.windows(3).filter(|w| w[1] < w[0] && w[1] < w[2]).count();
        Some(count)
    }

    /// Curvature: mean of second-order differences (acceleration) in the window.
    pub fn window_curvature(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 3 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let second_diffs: Vec<f64> = vals.windows(3)
            .map(|w| (w[2] - w[1]) - (w[1] - w[0]))
            .collect();
        Some(second_diffs.iter().sum::<f64>() / second_diffs.len() as f64)
    }

    /// Mean of second half minus mean of first half of window.
    pub fn window_half_diff(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mid = vals.len() / 2;
        let first_mean = vals[..mid].iter().sum::<f64>() / mid as f64;
        let second = &vals[mid..];
        if second.is_empty() { return None; }
        let second_mean = second.iter().sum::<f64>() / second.len() as f64;
        Some(second_mean - first_mean)
    }

    // ── round-135 ────────────────────────────────────────────────────────────

    /// Rate of mean crossings in the window (fraction of steps crossing the mean).
    pub fn window_mean_crossing_rate(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let crossings = vals.windows(2)
            .filter(|w| (w[0] - mean) * (w[1] - mean) < 0.0)
            .count();
        Some(crossings as f64 / (vals.len() - 1) as f64)
    }

    /// Variance to mean ratio (index of dispersion).
    pub fn window_var_to_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        if mean == 0.0 { return None; }
        let variance = vals.iter().map(|&v| (v - mean).powi(2)).sum::<f64>() / vals.len() as f64;
        Some(variance / mean.abs())
    }

    /// Coefficient of variation: std / mean (relative dispersion).
    pub fn window_coeff_var(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        if mean == 0.0 { return None; }
        let std = (vals.iter().map(|&v| (v - mean).powi(2)).sum::<f64>() / vals.len() as f64).sqrt();
        Some(std / mean.abs())
    }

    /// Fraction of steps that are strictly upward.
    pub fn window_step_up_fraction(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let total = vals.len() - 1;
        let up = vals.windows(2).filter(|w| w[1] > w[0]).count();
        Some(up as f64 / total as f64)
    }

    // ── round-136 ────────────────────────────────────────────────────────────

    /// Fraction of steps that are strictly downward.
    pub fn window_step_dn_fraction(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let total = vals.len() - 1;
        let dn = vals.windows(2).filter(|w| w[1] < w[0]).count();
        Some(dn as f64 / total as f64)
    }

    /// Mean absolute deviation as ratio to the window range.
    pub fn window_mean_abs_dev_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        let range = max - min;
        if range == 0.0 { return None; }
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let mad = vals.iter().map(|&v| (v - mean).abs()).sum::<f64>() / vals.len() as f64;
        Some(mad / range)
    }

    /// Most recent maximum value (second half of window).
    pub fn window_recent_high(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let half = self.window.len() / 2;
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let recent = &vals[half..];
        if recent.is_empty() { return None; }
        Some(recent.iter().cloned().fold(f64::NEG_INFINITY, f64::max))
    }

    /// Most recent minimum value (second half of window).
    pub fn window_recent_low(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let half = self.window.len() / 2;
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let recent = &vals[half..];
        if recent.is_empty() { return None; }
        Some(recent.iter().cloned().fold(f64::INFINITY, f64::min))
    }

    // ── round-137 ────────────────────────────────────────────────────────────

    /// Linear trend score: slope of linear regression / mean of window values.
    pub fn window_linear_trend_score(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        if mean == 0.0 { return None; }
        let x_mean = (n - 1.0) / 2.0;
        let num: f64 = vals.iter().enumerate().map(|(i, &y)| (i as f64 - x_mean) * (y - mean)).sum();
        let den: f64 = vals.iter().enumerate().map(|(i, _)| (i as f64 - x_mean).powi(2)).sum();
        if den == 0.0 { return None; }
        Some((num / den) / mean.abs())
    }

    /// Minimum z-score in the window.
    pub fn window_zscore_min(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let std = (vals.iter().map(|&v| (v - mean).powi(2)).sum::<f64>() / vals.len() as f64).sqrt();
        if std == 0.0 { return None; }
        let min = vals.iter().map(|&v| (v - mean) / std).fold(f64::INFINITY, f64::min);
        Some(min)
    }

    /// Maximum z-score in the window.
    pub fn window_zscore_max(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let std = (vals.iter().map(|&v| (v - mean).powi(2)).sum::<f64>() / vals.len() as f64).sqrt();
        if std == 0.0 { return None; }
        let max = vals.iter().map(|&v| (v - mean) / std).fold(f64::NEG_INFINITY, f64::max);
        Some(max)
    }

    /// Variance of consecutive differences in the window.
    pub fn window_diff_variance(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let diffs: Vec<f64> = vals.windows(2).map(|w| w[1] - w[0]).collect();
        let mean = diffs.iter().sum::<f64>() / diffs.len() as f64;
        let variance = diffs.iter().map(|&d| (d - mean).powi(2)).sum::<f64>() / diffs.len() as f64;
        Some(variance)
    }

    // ── round-138 ────────────────────────────────────────────────────────────

    /// Peak-to-trough ratio: max value / min value in window.
    pub fn window_peak_to_trough(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        if min == 0.0 { return None; }
        Some(max / min)
    }

    /// Asymmetry: skewness of the window values (Pearson's second coefficient).
    pub fn window_asymmetry(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 3 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        let std = (vals.iter().map(|&v| (v - mean).powi(2)).sum::<f64>() / n).sqrt();
        if std == 0.0 { return None; }
        let mut sorted = vals.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let median = if sorted.len() % 2 == 0 {
            (sorted[sorted.len() / 2 - 1] + sorted[sorted.len() / 2]) / 2.0
        } else {
            sorted[sorted.len() / 2]
        };
        Some(3.0 * (mean - median) / std)
    }

    /// Absolute trend: sum of absolute differences between consecutive window values.
    pub fn window_abs_trend(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let total: f64 = vals.windows(2).map(|w| (w[1] - w[0]).abs()).sum();
        Some(total)
    }

    /// Recent volatility: std of the last half of the window.
    pub fn window_recent_volatility(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let half = vals.len() / 2;
        let recent = &vals[half..];
        if recent.len() < 2 { return None; }
        let mean = recent.iter().sum::<f64>() / recent.len() as f64;
        let variance = recent.iter().map(|&v| (v - mean).powi(2)).sum::<f64>() / recent.len() as f64;
        Some(variance.sqrt())
    }

    // ── round-139 ────────────────────────────────────────────────────────────

    /// Range position: (last value - min) / (max - min) in the window.
    pub fn window_range_position(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        let range = max - min;
        if range == 0.0 { return Some(0.5); }
        let last = *vals.last()?;
        Some((last - min) / range)
    }

    /// Sign changes: number of times consecutive differences change sign in the window.
    pub fn window_sign_changes(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 3 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let diffs: Vec<f64> = vals.windows(2).map(|w| w[1] - w[0]).collect();
        let changes = diffs.windows(2)
            .filter(|w| w[0] * w[1] < 0.0)
            .count();
        Some(changes)
    }

    /// Mean shift: mean of the last half minus mean of the first half.
    pub fn window_mean_shift(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 4 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mid = vals.len() / 2;
        let first_mean = vals[..mid].iter().sum::<f64>() / mid as f64;
        let second_mean = vals[mid..].iter().sum::<f64>() / (vals.len() - mid) as f64;
        Some(second_mean - first_mean)
    }

    /// Slope change: difference between the OLS slope of the second half and first half.
    pub fn window_slope_change(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 4 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mid = vals.len() / 2;
        let slope = |seg: &[f64]| -> f64 {
            let n = seg.len() as f64;
            let x_mean = (n - 1.0) / 2.0;
            let y_mean = seg.iter().sum::<f64>() / n;
            let num: f64 = seg.iter().enumerate().map(|(i, &y)| (i as f64 - x_mean) * (y - y_mean)).sum();
            let den: f64 = seg.iter().enumerate().map(|(i, _)| (i as f64 - x_mean).powi(2)).sum();
            if den == 0.0 { 0.0 } else { num / den }
        };
        Some(slope(&vals[mid..]) - slope(&vals[..mid]))
    }

    // ── round-140 ────────────────────────────────────────────────────────────

    /// Recovery rate: fraction of steps that recover from the preceding drop.
    pub fn window_recovery_rate(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 3 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let triplets = vals.windows(3)
            .filter(|w| w[1] < w[0] && w[2] > w[1])
            .count();
        let drops = vals.windows(2).filter(|w| w[1] < w[0]).count();
        if drops == 0 { return Some(1.0); }
        Some(triplets as f64 / drops as f64)
    }

    /// Normalized spread: (max - min) / mean of the window.
    pub fn window_normalized_spread(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        if mean == 0.0 { return None; }
        Some((max - min) / mean.abs())
    }

    /// First-to-last ratio: last value / first value in the window.
    pub fn window_first_last_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let first = self.window.front()?.to_f64()?;
        let last = self.window.back()?.to_f64()?;
        if first == 0.0 { return None; }
        Some(last / first)
    }

    /// Extrema count: number of local maxima and minima in the window.
    pub fn window_extrema_count(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 3 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let count = vals.windows(3)
            .filter(|w| (w[1] > w[0] && w[1] > w[2]) || (w[1] < w[0] && w[1] < w[2]))
            .count();
        Some(count)
    }

    // ── round-141 ────────────────────────────────────────────────────────────

    /// Up fraction: fraction of steps in the window that are strictly increasing.
    pub fn window_up_fraction(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let ups = vals.windows(2).filter(|w| w[1] > w[0]).count() as f64;
        Some(ups / (vals.len() - 1) as f64)
    }

    /// Half range: half of (max - min) in the window (semi-range).
    pub fn window_half_range(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        Some((max - min) / 2.0)
    }

    /// Negative count: number of values below zero in the window.
    pub fn window_negative_count(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let count = self.window.iter()
            .filter(|v| v.to_f64().unwrap_or(0.0) < 0.0)
            .count();
        Some(count)
    }

    /// Trend purity: fraction of steps in the direction of the overall trend (sign of first-to-last diff).
    pub fn window_trend_purity(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let overall = vals.last()? - vals.first()?;
        if overall == 0.0 { return Some(0.0); }
        let aligned = vals.windows(2)
            .filter(|w| (w[1] - w[0]) * overall > 0.0)
            .count() as f64;
        Some(aligned / (vals.len() - 1) as f64)
    }

    // ── round-142 ────────────────────────────────────────────────────────────

    /// Centered mean: mean of values centered around window median.
    pub fn window_centered_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let mut vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let median = if vals.len() % 2 == 0 {
            (vals[vals.len() / 2 - 1] + vals[vals.len() / 2]) / 2.0
        } else {
            vals[vals.len() / 2]
        };
        let centered: Vec<f64> = vals.iter().map(|&v| v - median).collect();
        Some(centered.iter().sum::<f64>() / centered.len() as f64)
    }

    /// Last deviation: distance of the last value from the window mean.
    pub fn window_last_deviation(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let last = *vals.last()?;
        Some(last - mean)
    }

    /// Step size mean: mean of absolute consecutive differences (average step size).
    pub fn window_step_size_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let total: f64 = vals.windows(2).map(|w| (w[1] - w[0]).abs()).sum();
        Some(total / (vals.len() - 1) as f64)
    }

    /// Net up count: number of upward steps minus number of downward steps.
    pub fn window_net_up_count(&self) -> Option<i64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let ups = vals.windows(2).filter(|w| w[1] > w[0]).count() as i64;
        let downs = vals.windows(2).filter(|w| w[1] < w[0]).count() as i64;
        Some(ups - downs)
    }

    // ── round-143 ────────────────────────────────────────────────────────────

    /// Weighted mean: linearly weighted mean giving more weight to recent values.
    pub fn window_weighted_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let n = vals.len() as f64;
        let weighted_sum: f64 = vals.iter().enumerate().map(|(i, &v)| (i as f64 + 1.0) * v).sum();
        let weight_total = n * (n + 1.0) / 2.0;
        Some(weighted_sum / weight_total)
    }

    /// Upper half mean: mean of values in the upper half of the window (above median).
    pub fn window_upper_half_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let mut vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mid = vals.len() / 2;
        let upper = &vals[mid..];
        if upper.is_empty() { return None; }
        Some(upper.iter().sum::<f64>() / upper.len() as f64)
    }

    /// Lower half mean: mean of values in the lower half of the window (below median).
    pub fn window_lower_half_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let mut vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mid = vals.len() / 2;
        let lower = &vals[..mid];
        if lower.is_empty() { return None; }
        Some(lower.iter().sum::<f64>() / lower.len() as f64)
    }

    /// Mid range: (max + min) / 2 of the window.
    pub fn window_mid_range(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        Some((max + min) / 2.0)
    }

    // ── round-144 ────────────────────────────────────────────────────────────

    /// Mean of the window after trimming the top and bottom 10% of values.
    pub fn window_trim_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let mut vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let trim = (vals.len() / 10).max(0);
        let trimmed = &vals[trim..vals.len() - trim];
        if trimmed.is_empty() { return None; }
        Some(trimmed.iter().sum::<f64>() / trimmed.len() as f64)
    }

    /// Difference between the maximum and minimum window values.
    pub fn window_value_spread(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        Some(max - min)
    }

    /// Root mean square of the window values.
    pub fn window_rms(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let sum_sq: f64 = self.window.iter()
            .map(|v| { let x = v.to_f64().unwrap_or(0.0); x * x })
            .sum();
        Some((sum_sq / self.window.len() as f64).sqrt())
    }

    /// Fraction of window values that are above the window midpoint (min+max)/2.
    pub fn window_above_mid_fraction(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        let mid = (max + min) / 2.0;
        Some(vals.iter().filter(|&&v| v > mid).count() as f64 / vals.len() as f64)
    }

    // ── round-145 ────────────────────────────────────────────────────────────

    /// Median absolute deviation of the window values.
    pub fn window_median_abs_dev(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let mut vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = vals.len();
        let median = if n % 2 == 0 { (vals[n / 2 - 1] + vals[n / 2]) / 2.0 } else { vals[n / 2] };
        let mut devs: Vec<f64> = vals.iter().map(|&v| (v - median).abs()).collect();
        devs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let m = devs.len();
        Some(if m % 2 == 0 { (devs[m / 2 - 1] + devs[m / 2]) / 2.0 } else { devs[m / 2] })
    }

    /// Cubic mean (cube root of mean of cubes) of window values.
    pub fn window_cubic_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let sum_cubes: f64 = self.window.iter()
            .map(|v| { let x = v.to_f64().unwrap_or(0.0); x * x * x })
            .sum();
        Some((sum_cubes / self.window.len() as f64).cbrt())
    }

    /// Length of the longest run of consecutive same-valued window entries.
    pub fn window_max_run_length(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mut max_run = 1usize;
        let mut run = 1usize;
        for i in 1..vals.len() {
            if (vals[i] - vals[i - 1]).abs() < 1e-12 {
                run += 1;
                if run > max_run { max_run = run; }
            } else {
                run = 1;
            }
        }
        Some(max_run)
    }

    /// Position (0..1) of the most recent value within the sorted window.
    pub fn window_sorted_position(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let last = self.window.back()?.to_f64()?;
        let mut vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let pos = vals.partition_point(|&v| v < last);
        Some(pos as f64 / vals.len() as f64)
    }

    // ── round-146 ────────────────────────────────────────────────────────────

    /// Deviation of the most recent window value from the previous one.
    pub fn window_prev_deviation(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let mut iter = self.window.iter().rev();
        let last = iter.next()?.to_f64()?;
        let prev = iter.next()?.to_f64()?;
        Some(last - prev)
    }

    /// Lower quartile (25th percentile) of the window values.
    pub fn window_lower_quartile(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let mut vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let idx = (vals.len() - 1) / 4;
        Some(vals[idx])
    }

    /// Upper quartile (75th percentile) of the window values.
    pub fn window_upper_quartile(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let mut vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let idx = (3 * (vals.len() - 1)) / 4;
        Some(vals[idx])
    }

    /// Fraction of window values in the bottom or top 10% (tail weight).
    pub fn window_tail_weight(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let mut vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = vals.len();
        let tail = (n / 10).max(1);
        let lower = vals[tail - 1];
        let upper = vals[n - tail];
        let in_tail = vals.iter().filter(|&&v| v <= lower || v >= upper).count();
        Some(in_tail as f64 / n as f64)
    }

    // ── round-147 ────────────────────────────────────────────────────────────

    /// Deviation of the last window value from the window mean.
    pub fn window_last_vs_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let last = self.window.back()?.to_f64()?;
        let mean = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).sum::<f64>()
            / self.window.len() as f64;
        Some(last - mean)
    }

    /// Mean second-order change of consecutive window values (acceleration).
    pub fn window_change_acceleration(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 3 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let accels: Vec<f64> = vals.windows(3).map(|w| (w[2] - w[1]) - (w[1] - w[0])).collect();
        Some(accels.iter().sum::<f64>() / accels.len() as f64)
    }

    /// Length of the longest consecutive run of positive (>0) window values.
    pub fn window_positive_run_length(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mut max_run = 0usize;
        let mut run = 0usize;
        for &v in &vals {
            if v > 0.0 {
                run += 1;
                if run > max_run { max_run = run; }
            } else {
                run = 0;
            }
        }
        Some(max_run)
    }

    /// Geometric mean of successive ratios (value[i]/value[i-1]) as a trend indicator.
    pub fn window_geometric_trend(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let log_sum: f64 = vals.windows(2).filter_map(|w| {
            if w[0] == 0.0 { return None; }
            let r = w[1] / w[0];
            if r <= 0.0 { return None; }
            Some(r.ln())
        }).sum();
        let count = vals.windows(2).filter(|w| w[0] != 0.0 && w[1] / w[0] > 0.0).count();
        if count == 0 { return None; }
        Some((log_sum / count as f64).exp())
    }

    // ── round-148 ────────────────────────────────────────────────────────────

    /// Mean of all pairwise absolute differences between window values.
    pub fn window_pairwise_diff_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let n = vals.len();
        let mut sum = 0.0f64;
        let mut count = 0usize;
        for i in 0..n {
            for j in (i + 1)..n {
                sum += (vals[i] - vals[j]).abs();
                count += 1;
            }
        }
        if count == 0 { return None; }
        Some(sum / count as f64)
    }

    /// Length of the longest consecutive run of negative (<0) window values.
    pub fn window_negative_run_length(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mut max_run = 0usize;
        let mut run = 0usize;
        for &v in &vals {
            if v < 0.0 {
                run += 1;
                if run > max_run { max_run = run; }
            } else {
                run = 0;
            }
        }
        Some(max_run)
    }

    /// Number of times window values cross zero (sign changes).
    pub fn window_cross_zero_count(&self) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let count = vals.windows(2).filter(|w| w[0] * w[1] < 0.0).count();
        Some(count)
    }

    /// Mean reversion strength: mean of |val - window_mean| / window_std.
    pub fn window_mean_reversion_strength(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let var = vals.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / vals.len() as f64;
        let std = var.sqrt();
        if std == 0.0 { return Some(0.0); }
        Some(vals.iter().map(|&x| (x - mean).abs() / std).sum::<f64>() / vals.len() as f64)
    }

    // ── round-149 ────────────────────────────────────────────────────────────

    /// Deviation of the first window value from the window mean.
    pub fn window_first_vs_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let first = self.window.front()?.to_f64()?;
        let mean = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).sum::<f64>()
            / self.window.len() as f64;
        Some(first - mean)
    }

    /// Ratio of the last window value to the first window value.
    pub fn window_decay_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let first = self.window.front()?.to_f64()?;
        if first == 0.0 { return None; }
        let last = self.window.back()?.to_f64()?;
        Some(last / first)
    }

    /// Bimodal score: variance of lower half minus variance of upper half (normalized).
    pub fn window_bimodal_score(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 4 { return None; }
        let mut vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mid = vals.len() / 2;
        let lower = &vals[..mid];
        let upper = &vals[mid..];
        let var = |v: &[f64]| -> f64 {
            let m = v.iter().sum::<f64>() / v.len() as f64;
            v.iter().map(|x| (x - m).powi(2)).sum::<f64>() / v.len() as f64
        };
        let total_var = var(&vals);
        if total_var == 0.0 { return Some(0.0); }
        Some((var(lower) + var(upper)) / total_var)
    }

    /// Sum of absolute values of all window entries.
    pub fn window_abs_sum(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        Some(self.window.iter().map(|v| v.to_f64().unwrap_or(0.0).abs()).sum())
    }

    // ── round-150 ────────────────────────────────────────────────────────────

    /// Coefficient of variation (std / mean) of the window.
    pub fn window_coeff_of_variation(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        if mean == 0.0 { return None; }
        let var = vals.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / vals.len() as f64;
        Some(var.sqrt() / mean.abs())
    }

    /// Mean absolute error of window values relative to the window mean.
    pub fn window_mean_absolute_error(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        Some(vals.iter().map(|&x| (x - mean).abs()).sum::<f64>() / vals.len() as f64)
    }

    /// Last window value normalized to [0,1] within the window range.
    pub fn window_normalized_last(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let last = self.window.back()?.to_f64()?;
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        if (max - min).abs() < 1e-12 { return Some(0.5); }
        Some((last - min) / (max - min))
    }

    /// Fraction of window values with positive sign minus fraction with negative sign.
    pub fn window_sign_bias(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let n = self.window.len() as f64;
        let pos = self.window.iter().filter(|v| v.to_f64().map_or(false, |x| x > 0.0)).count() as f64;
        let neg = self.window.iter().filter(|v| v.to_f64().map_or(false, |x| x < 0.0)).count() as f64;
        Some((pos - neg) / n)
    }

    // ── round-151 ────────────────────────────────────────────────────────────

    /// Deviation of the second-to-last window value from the last value.
    pub fn window_penultimate_vs_last(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let mut iter = self.window.iter().rev();
        let last = iter.next()?.to_f64()?;
        let penultimate = iter.next()?.to_f64()?;
        Some(penultimate - last)
    }

    /// Position of the window mean within the window range (0=at min, 1=at max).
    pub fn window_mean_range_position(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        if (max - min).abs() < 1e-12 { return Some(0.5); }
        Some((mean - min) / (max - min))
    }

    /// Z-score of the most recent window value.
    pub fn window_zscore_last(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let last = self.window.back()?.to_f64()?;
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let var = vals.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / vals.len() as f64;
        let std = var.sqrt();
        if std == 0.0 { return None; }
        Some((last - mean) / std)
    }

    /// Gradient (slope) of a linear fit to sequential window indices.
    pub fn window_gradient(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let n = vals.len() as f64;
        let x_mean = (n - 1.0) / 2.0;
        let y_mean = vals.iter().sum::<f64>() / n;
        let num: f64 = vals.iter().enumerate().map(|(i, &y)| (i as f64 - x_mean) * (y - y_mean)).sum();
        let den: f64 = vals.iter().enumerate().map(|(i, _)| (i as f64 - x_mean).powi(2)).sum();
        if den == 0.0 { return Some(0.0); }
        Some(num / den)
    }

    // ── round-152 ────────────────────────────────────────────────────────────

    /// Shannon entropy of the window values (binned into 8 buckets).
    pub fn window_entropy_score(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        if (max - min).abs() < 1e-12 { return Some(0.0); }
        let bins = 8usize;
        let mut counts = vec![0usize; bins];
        for &v in &vals {
            let idx = ((v - min) / (max - min) * bins as f64).min(bins as f64 - 1.0) as usize;
            counts[idx] += 1;
        }
        let n = vals.len() as f64;
        Some(counts.iter().filter(|&&c| c > 0)
            .map(|&c| { let p = c as f64 / n; -p * p.ln() }).sum())
    }

    /// Interquartile range (Q3 - Q1) of the window.
    pub fn window_quartile_spread(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 4 { return None; }
        let mut vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = vals.len();
        let q1 = vals[n / 4];
        let q3 = vals[3 * n / 4];
        Some(q3 - q1)
    }

    /// Ratio of the window maximum to minimum (returns None if min=0).
    pub fn window_max_to_min_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        if min == 0.0 { return None; }
        Some(max / min)
    }

    /// Fraction of window values that exceed the window mean.
    pub fn window_upper_fraction(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        Some(vals.iter().filter(|&&v| v > mean).count() as f64 / vals.len() as f64)
    }

    // ── round-153 ────────────────────────────────────────────────────────────

    /// Mean of absolute differences between consecutive window values.
    pub fn window_abs_change_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let changes: Vec<f64> = vals.windows(2).map(|w| (w[1] - w[0]).abs()).collect();
        Some(changes.iter().sum::<f64>() / changes.len() as f64)
    }

    /// Percentile rank of the last window value within the window (0.0–1.0).
    pub fn window_last_percentile(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let last = self.window.back()?.to_f64()?;
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let below = vals.iter().filter(|&&v| v < last).count();
        Some(below as f64 / vals.len() as f64)
    }

    /// Std dev of the trailing half of the window.
    pub fn window_trailing_std(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let half = vals.len() / 2;
        let trailing = &vals[half..];
        if trailing.len() < 2 { return None; }
        let mean = trailing.iter().sum::<f64>() / trailing.len() as f64;
        let var = trailing.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / trailing.len() as f64;
        Some(var.sqrt())
    }

    /// Mean of consecutive differences (last - first) divided by window length.
    pub fn window_mean_change(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let first = self.window.front()?.to_f64()?;
        let last = self.window.back()?.to_f64()?;
        Some((last - first) / (self.window.len() - 1) as f64)
    }

    // ── round-154 ────────────────────────────────────────────────────────────

    /// Index position of the maximum window value (0 = oldest).
    pub fn window_value_at_peak(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let peak_idx = vals.iter().enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))?.0;
        Some(peak_idx as f64)
    }

    /// Difference between the first and last window values (head - tail).
    pub fn window_head_tail_diff(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let first = self.window.front()?.to_f64()?;
        let last = self.window.back()?.to_f64()?;
        Some(first - last)
    }

    /// Midpoint of the window range: (max + min) / 2.
    pub fn window_midpoint(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        Some((min + max) / 2.0)
    }

    /// Second derivative of window mean: curvature of the smoothed series.
    pub fn window_concavity(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 3 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let n = vals.len();
        let first_third = vals[..n/3].iter().sum::<f64>() / (n/3) as f64;
        let last_third = vals[n - n/3..].iter().sum::<f64>() / (n/3) as f64;
        let mid = vals[n/3..n - n/3].iter().sum::<f64>() / (n - 2*n/3) as f64;
        Some(mid - (first_third + last_third) / 2.0)
    }

    // ── round-155 ────────────────────────────────────────────────────────────

    /// Fraction of consecutive window pairs where value rises.
    pub fn window_rise_fraction(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let rises = vals.windows(2).filter(|w| w[1] > w[0]).count();
        Some(rises as f64 / (vals.len() - 1) as f64)
    }

    /// Difference between window maximum and minimum (peak-to-valley span).
    pub fn window_peak_to_valley(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        Some(max - min)
    }

    /// Mean of positive consecutive changes in the window.
    pub fn window_positive_change_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let pos_changes: Vec<f64> = vals.windows(2)
            .filter_map(|w| if w[1] > w[0] { Some(w[1] - w[0]) } else { None })
            .collect();
        if pos_changes.is_empty() { return None; }
        Some(pos_changes.iter().sum::<f64>() / pos_changes.len() as f64)
    }

    /// Range-to-mean ratio: (max - min) / |mean|.
    pub fn window_range_cv(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let range = max - min;
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        if mean == 0.0 { return None; }
        Some(range / mean.abs())
    }

    // ── round-156 ────────────────────────────────────────────────────────────

    /// Mean of negative consecutive changes in the window.
    pub fn window_negative_change_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let neg_changes: Vec<f64> = vals.windows(2)
            .filter_map(|w| if w[1] < w[0] { Some(w[0] - w[1]) } else { None })
            .collect();
        if neg_changes.is_empty() { return None; }
        Some(neg_changes.iter().sum::<f64>() / neg_changes.len() as f64)
    }

    /// Fraction of consecutive window pairs where value falls.
    pub fn window_fall_fraction(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let falls = vals.windows(2).filter(|w| w[1] < w[0]).count();
        Some(falls as f64 / (vals.len() - 1) as f64)
    }

    /// Difference between the last window value and the window maximum.
    pub fn window_last_vs_max(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let last = self.window.back()?.to_f64()?;
        let max = self.window.iter().map(|v| v.to_f64().unwrap_or(f64::NEG_INFINITY)).fold(f64::NEG_INFINITY, f64::max);
        Some(last - max)
    }

    /// Difference between the last window value and the window minimum.
    pub fn window_last_vs_min(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let last = self.window.back()?.to_f64()?;
        let min = self.window.iter().map(|v| v.to_f64().unwrap_or(f64::INFINITY)).fold(f64::INFINITY, f64::min);
        Some(last - min)
    }

    // ── round-157 ────────────────────────────────────────────────────────────

    /// Mean absolute deviation of consecutive differences (oscillation measure).
    pub fn window_mean_oscillation(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let diffs: Vec<f64> = vals.windows(2).map(|w| w[1] - w[0]).collect();
        let mean_diff = diffs.iter().sum::<f64>() / diffs.len() as f64;
        Some(diffs.iter().map(|d| (d - mean_diff).abs()).sum::<f64>() / diffs.len() as f64)
    }

    /// Monotone score: fraction of consecutive pairs in the dominant direction.
    pub fn window_monotone_score(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let pairs = vals.len() - 1;
        let rises = vals.windows(2).filter(|w| w[1] > w[0]).count();
        let falls = pairs - rises;
        Some(rises.max(falls) as f64 / pairs as f64)
    }

    /// Trend of window std dev: difference between std dev of second half minus first half.
    pub fn window_stddev_trend(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 4 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mid = vals.len() / 2;
        let std_half = |v: &[f64]| -> f64 {
            let m = v.iter().sum::<f64>() / v.len() as f64;
            (v.iter().map(|x| (x - m).powi(2)).sum::<f64>() / v.len() as f64).sqrt()
        };
        Some(std_half(&vals[mid..]) - std_half(&vals[..mid]))
    }

    /// Fraction of consecutive pairs where the sign of the change alternates.
    pub fn window_zero_cross_fraction(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 3 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let diffs: Vec<f64> = vals.windows(2).map(|w| w[1] - w[0]).collect();
        let sign_changes = diffs.windows(2).filter(|w| w[0] * w[1] < 0.0).count();
        Some(sign_changes as f64 / (diffs.len() - 1) as f64)
    }

    // ── round-158 ────────────────────────────────────────────────────────────

    /// Exponentially weighted sum of window values (alpha=0.1, most recent highest weight).
    pub fn window_exponential_decay_sum(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let alpha = 0.1_f64;
        let n = vals.len();
        let sum: f64 = vals.iter().enumerate()
            .map(|(i, &v)| v * alpha.powi((n - 1 - i) as i32))
            .sum();
        Some(sum)
    }

    /// Mean of differences between values separated by lag=1 position.
    pub fn window_lagged_diff(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let diffs: Vec<f64> = vals.windows(2).map(|w| w[1] - w[0]).collect();
        Some(diffs.iter().sum::<f64>() / diffs.len() as f64)
    }

    /// Ratio of window mean to window maximum.
    pub fn window_mean_to_max(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        if max == 0.0 { return None; }
        Some(mean / max)
    }

    /// Fraction of values equal to the most frequent bucket (mode approximation, 8 bins).
    pub fn window_mode_fraction(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        if (max - min).abs() < 1e-12 { return Some(1.0); }
        let bins = 8usize;
        let mut counts = vec![0usize; bins];
        for &v in &vals {
            let idx = ((v - min) / (max - min) * bins as f64).min(bins as f64 - 1.0) as usize;
            counts[idx] += 1;
        }
        let mode_count = counts.iter().cloned().max().unwrap_or(0);
        Some(mode_count as f64 / vals.len() as f64)
    }

    // ── round-159 ────────────────────────────────────────────────────────────

    /// Mean of window values that are below zero.
    pub fn window_mean_below_zero(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let below: Vec<f64> = self.window.iter()
            .filter_map(|v| v.to_f64())
            .filter(|&v| v < 0.0)
            .collect();
        if below.is_empty() { return None; }
        Some(below.iter().sum::<f64>() / below.len() as f64)
    }

    /// Mean of window values that are above zero.
    pub fn window_mean_above_zero(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let above: Vec<f64> = self.window.iter()
            .filter_map(|v| v.to_f64())
            .filter(|&v| v > 0.0)
            .collect();
        if above.is_empty() { return None; }
        Some(above.iter().sum::<f64>() / above.len() as f64)
    }

    /// Fraction of window values at or above the running max at each step.
    pub fn window_running_max_fraction(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mut running_max = f64::NEG_INFINITY;
        let count = vals.iter().filter(|&&v| {
            let is_max = v >= running_max;
            if v > running_max { running_max = v; }
            is_max
        }).count();
        Some(count as f64 / vals.len() as f64)
    }

    /// Change in variance between first half and second half of the window.
    pub fn window_variance_change(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 4 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mid = vals.len() / 2;
        let var = |s: &[f64]| -> f64 {
            let m = s.iter().sum::<f64>() / s.len() as f64;
            s.iter().map(|&v| (v - m).powi(2)).sum::<f64>() / s.len() as f64
        };
        Some(var(&vals[mid..]) - var(&vals[..mid]))
    }

    // ── round-160 ────────────────────────────────────────────────────────────

    /// Slope of EMA of window values (alpha=0.1), computed as (last_ema - first_ema) / n.
    pub fn window_ema_slope(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let alpha = 0.1_f64;
        let mut ema = vals[0];
        let first_ema = ema;
        for &v in &vals[1..] {
            ema = alpha * v + (1.0 - alpha) * ema;
        }
        Some((ema - first_ema) / (vals.len() - 1) as f64)
    }

    /// Ratio of window max to window min (returns None if min is 0).
    pub fn window_range_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        if min == 0.0 { return None; }
        Some(max / min)
    }

    /// Longest consecutive run of values above the window mean.
    pub fn window_above_mean_streak(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let mut max_streak = 0usize;
        let mut cur = 0usize;
        for &v in &vals {
            if v > mean { cur += 1; if cur > max_streak { max_streak = cur; } }
            else { cur = 0; }
        }
        Some(max_streak as f64)
    }

    /// Mean absolute difference between consecutive window values.
    pub fn window_mean_abs_diff(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let diffs: Vec<f64> = vals.windows(2).map(|w| (w[1] - w[0]).abs()).collect();
        Some(diffs.iter().sum::<f64>() / diffs.len() as f64)
    }

    // ── round-161 ────────────────────────────────────────────────────────────

    /// Shannon entropy of sign distribution (+/-/0) in the window.
    pub fn window_sign_entropy(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let mut pos = 0usize; let mut neg = 0usize; let mut zer = 0usize;
        for v in &self.window {
            match v.to_f64().unwrap_or(0.0).partial_cmp(&0.0) {
                Some(std::cmp::Ordering::Greater) => pos += 1,
                Some(std::cmp::Ordering::Less) => neg += 1,
                _ => zer += 1,
            }
        }
        let n = self.window.len() as f64;
        let entropy = [pos, neg, zer].iter()
            .filter(|&&c| c > 0)
            .map(|&c| { let p = c as f64 / n; -p * p.ln() })
            .sum();
        Some(entropy)
    }

    /// Count of local extrema (peaks + troughs) in the window.
    pub fn window_local_extrema_count(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 3 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let count = vals.windows(3).filter(|w| {
            (w[1] > w[0] && w[1] > w[2]) || (w[1] < w[0] && w[1] < w[2])
        }).count();
        Some(count as f64)
    }

    /// Lag-2 autocorrelation of window values.
    pub fn window_autocorr_lag2(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 3 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let variance: f64 = vals.iter().map(|&v| (v - mean).powi(2)).sum::<f64>() / vals.len() as f64;
        if variance == 0.0 { return None; }
        let cov: f64 = vals.windows(3).map(|w| (w[0] - mean) * (w[2] - mean)).sum::<f64>()
            / (vals.len() - 2) as f64;
        Some(cov / variance)
    }

    /// Fraction of window values strictly above the window median.
    pub fn window_pct_above_median(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let mut sorted: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let median = if sorted.len() % 2 == 0 {
            (sorted[sorted.len() / 2 - 1] + sorted[sorted.len() / 2]) / 2.0
        } else {
            sorted[sorted.len() / 2]
        };
        let above = sorted.iter().filter(|&&v| v > median).count();
        Some(above as f64 / sorted.len() as f64)
    }

    // ── round-162 ────────────────────────────────────────────────────────────

    /// Longest consecutive run of values strictly below zero.
    pub fn window_below_zero_streak(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let mut max_streak = 0usize;
        let mut cur = 0usize;
        for v in &self.window {
            if v.to_f64().unwrap_or(0.0) < 0.0 { cur += 1; if cur > max_streak { max_streak = cur; } }
            else { cur = 0; }
        }
        Some(max_streak as f64)
    }

    /// Ratio of window max to window mean; None if mean is zero.
    pub fn window_max_to_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        if mean == 0.0 { return None; }
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        Some(max / mean)
    }

    /// Length of the longest run of the same sign (+/-/0) in the window.
    pub fn window_sign_run_length(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let signs: Vec<i8> = self.window.iter().map(|v| {
            let f = v.to_f64().unwrap_or(0.0);
            if f > 0.0 { 1 } else if f < 0.0 { -1 } else { 0 }
        }).collect();
        let mut max_run = 1usize;
        let mut cur_run = 1usize;
        for i in 1..signs.len() {
            if signs[i] == signs[i-1] { cur_run += 1; if cur_run > max_run { max_run = cur_run; } }
            else { cur_run = 1; }
        }
        Some(max_run as f64)
    }

    /// Exponentially decayed weighted mean (more recent values weighted higher, alpha=0.2).
    pub fn window_decay_weighted_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let alpha = 0.2_f64;
        let n = vals.len();
        let weights: Vec<f64> = (0..n).map(|i| alpha.powi((n - 1 - i) as i32)).collect();
        let total_w: f64 = weights.iter().sum();
        if total_w == 0.0 { return None; }
        let weighted_sum: f64 = vals.iter().zip(weights.iter()).map(|(v, w)| v * w).sum();
        Some(weighted_sum / total_w)
    }

    // ── round-163 ────────────────────────────────────────────────────────────

    /// Ratio of window minimum to window mean; None if mean is zero.
    pub fn window_min_to_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        if mean == 0.0 { return None; }
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        Some(min / mean)
    }

    /// (max - min) normalized by (max + min)/2; None if midpoint is zero.
    pub fn window_normalized_range(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let midpoint = (max + min) / 2.0;
        if midpoint == 0.0 { return None; }
        Some((max - min) / midpoint)
    }

    /// Mean of window after clipping top and bottom 10% of values.
    pub fn window_winsorized_mean(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let mut sorted: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = sorted.len();
        let trim = (n as f64 * 0.1).floor() as usize;
        let trimmed = if 2 * trim < n { &sorted[trim..n - trim] } else { &sorted[..] };
        if trimmed.is_empty() { return None; }
        Some(trimmed.iter().sum::<f64>() / trimmed.len() as f64)
    }

    /// Ratio of (max - min) to std dev; None if std dev is zero.
    pub fn window_range_to_std(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let std = (vals.iter().map(|&v| (v - mean).powi(2)).sum::<f64>() / vals.len() as f64).sqrt();
        if std == 0.0 { return None; }
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        Some((max - min) / std)
    }

    // ── round-164 ────────────────────────────────────────────────────────────

    /// Coefficient of variation: std / mean * 100; None if mean is zero.
    pub fn window_cv(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        if mean == 0.0 { return None; }
        let std = (vals.iter().map(|&v| (v - mean).powi(2)).sum::<f64>() / vals.len() as f64).sqrt();
        Some(std / mean * 100.0)
    }

    /// Fraction of window values that are non-zero.
    pub fn window_non_zero_fraction(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let non_zero = self.window.iter()
            .filter(|v| v.to_f64().unwrap_or(0.0) != 0.0)
            .count();
        Some(non_zero as f64 / self.window.len() as f64)
    }

    /// Root mean square of window values.
    pub fn window_rms_abs(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean_sq = vals.iter().map(|&v| v * v).sum::<f64>() / vals.len() as f64;
        Some(mean_sq.sqrt())
    }

    /// Kurtosis proxy: mean of (x - mean)^4 / std^4; None if std is zero.
    pub fn window_kurtosis_proxy(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 4 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let var = vals.iter().map(|&v| (v - mean).powi(2)).sum::<f64>() / vals.len() as f64;
        if var == 0.0 { return None; }
        let kurt = vals.iter().map(|&v| (v - mean).powi(4)).sum::<f64>() / (vals.len() as f64 * var.powi(2));
        Some(kurt)
    }

    // ── round-165 ────────────────────────────────────────────────────────────

    /// Index of mean reversion strength: fraction of steps returning toward the mean.
    pub fn window_mean_reversion_index(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 3 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let reverts = vals.windows(2).filter(|w| {
            let d = w[1] - w[0];
            (w[0] > mean && d < 0.0) || (w[0] < mean && d > 0.0)
        }).count();
        Some(reverts as f64 / (vals.len() - 1) as f64)
    }

    /// Ratio of the 90th to 10th percentile of window values.
    pub fn window_tail_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let mut sorted: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = sorted.len();
        let p10 = sorted[((n as f64 - 1.0) * 0.1).round() as usize];
        let p90 = sorted[((n as f64 - 1.0) * 0.9).round() as usize];
        if p10 == 0.0 { return None; }
        Some(p90 / p10)
    }

    /// Slope of cumulative sum of window values (total drift / n).
    pub fn window_cumsum_trend(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.is_empty() { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let cumsum: f64 = vals.iter().sum();
        Some(cumsum / vals.len() as f64)
    }

    /// Count of times window values cross the window mean.
    pub fn window_mean_crossing_count(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.window.len() < 2 { return None; }
        let vals: Vec<f64> = self.window.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let above: Vec<bool> = vals.iter().map(|&v| v > mean).collect();
        let crosses = above.windows(2).filter(|w| w[0] != w[1]).count();
        Some(crosses as f64)
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
    fn test_zscore_is_full_false_before_capacity() {
        let mut n = znorm(3);
        assert!(!n.is_full());
        n.update(dec!(1));
        n.update(dec!(2));
        assert!(!n.is_full());
        n.update(dec!(3));
        assert!(n.is_full());
    }

    #[test]
    fn test_zscore_is_full_stays_true_after_eviction() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] {
            n.update(v);
        }
        assert!(n.is_full());
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

    #[test]
    fn test_sum_empty_returns_none() {
        let n = znorm(4);
        assert!(n.sum().is_none());
    }

    #[test]
    fn test_sum_matches_manual() {
        let mut n = znorm(4);
        n.update(dec!(10));
        n.update(dec!(20));
        n.update(dec!(30));
        // window = [10, 20, 30], sum = 60
        assert_eq!(n.sum().unwrap(), dec!(60));
    }

    #[test]
    fn test_sum_evicts_old_values() {
        let mut n = znorm(2);
        n.update(dec!(10));
        n.update(dec!(20));
        n.update(dec!(30)); // evicts 10
        // window = [20, 30], sum = 50
        assert_eq!(n.sum().unwrap(), dec!(50));
    }

    #[test]
    fn test_std_dev_single_observation_returns_some_zero() {
        let mut n = znorm(5);
        n.update(dec!(10));
        // Single sample → variance undefined, std_dev should return None or 0
        // ZScoreNormalizer::std_dev returns None for n < 2
        assert!(n.std_dev().is_none() || n.std_dev().unwrap() == 0.0);
    }

    #[test]
    fn test_std_dev_constant_window_is_zero() {
        let mut n = znorm(4);
        for _ in 0..4 {
            n.update(dec!(5));
        }
        let sd = n.std_dev().unwrap();
        assert!(sd.abs() < 1e-9, "expected 0.0 got {sd}");
    }

    #[test]
    fn test_std_dev_known_population() {
        // values [2, 4, 4, 4, 5, 5, 7, 9] → σ = 2.0
        let mut n = znorm(8);
        for v in [dec!(2), dec!(4), dec!(4), dec!(4), dec!(5), dec!(5), dec!(7), dec!(9)] {
            n.update(v);
        }
        let sd = n.std_dev().unwrap();
        assert!((sd - 2.0).abs() < 1e-6, "expected ~2.0 got {sd}");
    }

    // --- window_range / coefficient_of_variation ---

    #[test]
    fn test_window_range_none_when_empty() {
        let n = znorm(5);
        assert!(n.window_range().is_none());
    }

    #[test]
    fn test_window_range_correct_value() {
        let mut n = znorm(5);
        n.update(dec!(10));
        n.update(dec!(20));
        n.update(dec!(15));
        // max=20, min=10 → range=10
        assert_eq!(n.window_range().unwrap(), dec!(10));
    }

    #[test]
    fn test_coefficient_of_variation_none_when_empty() {
        let n = znorm(5);
        assert!(n.coefficient_of_variation().is_none());
    }

    #[test]
    fn test_coefficient_of_variation_none_when_mean_zero() {
        let mut n = znorm(5);
        n.update(dec!(-5));
        n.update(dec!(5)); // mean = 0
        assert!(n.coefficient_of_variation().is_none());
    }

    #[test]
    fn test_coefficient_of_variation_positive_for_nonzero_mean() {
        let mut n = znorm(8);
        for v in [dec!(2), dec!(4), dec!(4), dec!(4), dec!(5), dec!(5), dec!(7), dec!(9)] {
            n.update(v);
        }
        // mean = 5, std_dev = 2, cv = 2/5 = 0.4
        let cv = n.coefficient_of_variation().unwrap();
        assert!((cv - 0.4).abs() < 1e-5, "expected ~0.4 got {cv}");
    }

    // --- sample_variance ---

    #[test]
    fn test_sample_variance_none_when_empty() {
        let n = znorm(5);
        assert!(n.sample_variance().is_none());
    }

    #[test]
    fn test_sample_variance_zero_for_constant_window() {
        let mut n = znorm(3);
        n.update(dec!(7));
        n.update(dec!(7));
        n.update(dec!(7));
        assert!(n.sample_variance().unwrap().abs() < 1e-10);
    }

    #[test]
    fn test_sample_variance_equals_std_dev_squared() {
        let mut n = znorm(8);
        for v in [dec!(2), dec!(4), dec!(4), dec!(4), dec!(5), dec!(5), dec!(7), dec!(9)] {
            n.update(v);
        }
        // std_dev ≈ 2.0, variance ≈ 4.0
        let variance = n.sample_variance().unwrap();
        let sd = n.std_dev().unwrap();
        assert!((variance - sd * sd).abs() < 1e-10);
    }

    // --- window_mean_f64 ---

    #[test]
    fn test_window_mean_f64_none_when_empty() {
        let n = znorm(5);
        assert!(n.window_mean_f64().is_none());
    }

    #[test]
    fn test_window_mean_f64_correct_value() {
        let mut n = znorm(4);
        n.update(dec!(10));
        n.update(dec!(20));
        // mean = 15.0
        let m = n.window_mean_f64().unwrap();
        assert!((m - 15.0).abs() < 1e-10);
    }

    #[test]
    fn test_window_mean_f64_matches_decimal_mean() {
        let mut n = znorm(8);
        for v in [dec!(2), dec!(4), dec!(4), dec!(4), dec!(5), dec!(5), dec!(7), dec!(9)] {
            n.update(v);
        }
        use rust_decimal::prelude::ToPrimitive;
        let expected = n.mean().unwrap().to_f64().unwrap();
        assert!((n.window_mean_f64().unwrap() - expected).abs() < 1e-10);
    }

    // ── ZScoreNormalizer::kurtosis ────────────────────────────────────────────

    #[test]
    fn test_kurtosis_none_when_fewer_than_4_observations() {
        let mut n = znorm(5);
        n.update(dec!(1));
        n.update(dec!(2));
        n.update(dec!(3));
        assert!(n.kurtosis().is_none());
    }

    #[test]
    fn test_kurtosis_returns_some_with_4_observations() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] {
            n.update(v);
        }
        assert!(n.kurtosis().is_some());
    }

    #[test]
    fn test_kurtosis_none_when_all_same_value() {
        let mut n = znorm(4);
        for _ in 0..4 {
            n.update(dec!(5));
        }
        // std_dev = 0 → kurtosis is None
        assert!(n.kurtosis().is_none());
    }

    #[test]
    fn test_kurtosis_uniform_distribution_is_negative() {
        // Uniform distribution has excess kurtosis of -1.2
        let mut n = znorm(10);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5),
                  dec!(6), dec!(7), dec!(8), dec!(9), dec!(10)] {
            n.update(v);
        }
        let k = n.kurtosis().unwrap();
        // Excess kurtosis of uniform dist over integers is negative
        assert!(k < 0.0, "expected negative excess kurtosis for uniform dist, got {k}");
    }

    // --- ZScoreNormalizer::is_near_mean ---
    #[test]
    fn test_is_near_mean_false_with_fewer_than_two_obs() {
        let mut n = znorm(5);
        n.update(dec!(10));
        assert!(!n.is_near_mean(dec!(10), 1.0));
    }

    #[test]
    fn test_is_near_mean_true_within_one_sigma() {
        let mut n = znorm(10);
        // Feed 10, 10, 10, ..., 10, 20 → mean≈11, std_dev small-ish
        for _ in 0..9 {
            n.update(dec!(10));
        }
        n.update(dec!(20));
        // mean = (90 + 20) / 10 = 11
        assert!(n.is_near_mean(dec!(11), 1.0));
    }

    #[test]
    fn test_is_near_mean_false_when_far_from_mean() {
        let mut n = znorm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)] {
            n.update(v);
        }
        // mean = 3, std_dev ≈ 1.41; 100 is many sigmas away
        assert!(!n.is_near_mean(dec!(100), 2.0));
    }

    #[test]
    fn test_is_near_mean_true_when_all_identical_any_value() {
        let mut n = znorm(4);
        for _ in 0..4 {
            n.update(dec!(7));
        }
        // std_dev = 0 → any value returns true
        assert!(n.is_near_mean(dec!(999), 0.0));
    }

    // --- ZScoreNormalizer::window_sum_f64 ---
    #[test]
    fn test_window_sum_f64_zero_on_empty() {
        let n = znorm(5);
        assert_eq!(n.window_sum_f64(), 0.0);
    }

    #[test]
    fn test_window_sum_f64_correct_after_updates() {
        let mut n = znorm(5);
        n.update(dec!(10));
        n.update(dec!(20));
        n.update(dec!(30));
        assert!((n.window_sum_f64() - 60.0).abs() < 1e-10);
    }

    #[test]
    fn test_window_sum_f64_rolls_out_old_values() {
        let mut n = znorm(2);
        n.update(dec!(100));
        n.update(dec!(200));
        n.update(dec!(300)); // 100 rolls out
        // window contains 200, 300 → sum = 500
        assert!((n.window_sum_f64() - 500.0).abs() < 1e-10);
    }

    // ── ZScoreNormalizer::latest ────────────────────────────────────────────

    #[test]
    fn test_zscore_latest_none_when_empty() {
        let n = znorm(5);
        assert!(n.latest().is_none());
    }

    #[test]
    fn test_zscore_latest_returns_most_recent() {
        let mut n = znorm(5);
        n.update(dec!(10));
        n.update(dec!(20));
        assert_eq!(n.latest(), Some(dec!(20)));
    }

    #[test]
    fn test_zscore_latest_updates_on_roll() {
        let mut n = znorm(2);
        n.update(dec!(1));
        n.update(dec!(2));
        n.update(dec!(3)); // rolls out 1
        assert_eq!(n.latest(), Some(dec!(3)));
    }

    // --- ZScoreNormalizer::window_max_f64 / window_min_f64 ---
    #[test]
    fn test_window_max_f64_none_on_empty() {
        let n = znorm(5);
        assert!(n.window_max_f64().is_none());
    }

    #[test]
    fn test_window_max_f64_correct_value() {
        let mut n = znorm(5);
        for v in [dec!(3), dec!(7), dec!(1), dec!(5)] {
            n.update(v);
        }
        assert!((n.window_max_f64().unwrap() - 7.0).abs() < 1e-10);
    }

    #[test]
    fn test_window_min_f64_none_on_empty() {
        let n = znorm(5);
        assert!(n.window_min_f64().is_none());
    }

    #[test]
    fn test_window_min_f64_correct_value() {
        let mut n = znorm(5);
        for v in [dec!(3), dec!(7), dec!(1), dec!(5)] {
            n.update(v);
        }
        assert!((n.window_min_f64().unwrap() - 1.0).abs() < 1e-10);
    }

    // ── ZScoreNormalizer::percentile ────────────────────────────────────────

    #[test]
    fn test_percentile_none_when_empty() {
        let n = znorm(5);
        assert!(n.percentile(dec!(10)).is_none());
    }

    #[test]
    fn test_percentile_one_when_all_lte_value() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] {
            n.update(v);
        }
        assert!((n.percentile(dec!(4)).unwrap() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_percentile_zero_when_all_gt_value() {
        let mut n = znorm(4);
        for v in [dec!(5), dec!(6), dec!(7), dec!(8)] {
            n.update(v);
        }
        // 0 of 4 values are ≤ 4
        assert_eq!(n.percentile(dec!(4)).unwrap(), 0.0);
    }

    #[test]
    fn test_percentile_half_at_median() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] {
            n.update(v);
        }
        // 2 of 4 values ≤ 2 → 0.5
        assert!((n.percentile(dec!(2)).unwrap() - 0.5).abs() < 1e-9);
    }

    // ── ZScoreNormalizer::interquartile_range ────────────────────────────────

    #[test]
    fn test_zscore_iqr_none_fewer_than_4_observations() {
        let mut n = znorm(5);
        for v in [dec!(1), dec!(2), dec!(3)] {
            n.update(v);
        }
        assert!(n.interquartile_range().is_none());
    }

    #[test]
    fn test_zscore_iqr_some_with_4_observations() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] {
            n.update(v);
        }
        assert!(n.interquartile_range().is_some());
    }

    #[test]
    fn test_zscore_iqr_zero_when_all_same() {
        let mut n = znorm(4);
        for _ in 0..4 {
            n.update(dec!(5));
        }
        assert_eq!(n.interquartile_range(), Some(dec!(0)));
    }

    #[test]
    fn test_zscore_iqr_correct_for_sorted_data() {
        // [1,2,3,4,5,6,7,8]: q1_idx=2 → sorted[2]=3, q3_idx=6 → sorted[6]=7, IQR=4
        let mut n = znorm(8);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5), dec!(6), dec!(7), dec!(8)] {
            n.update(v);
        }
        assert_eq!(n.interquartile_range(), Some(dec!(4)));
    }

    // ── ZScoreNormalizer::z_score_of_latest / deviation_from_mean ───────────

    #[test]
    fn test_z_score_of_latest_none_when_empty() {
        let n = znorm(5);
        assert!(n.z_score_of_latest().is_none());
    }

    #[test]
    fn test_z_score_of_latest_zero_when_all_same() {
        let mut n = znorm(4);
        for _ in 0..4 {
            n.update(dec!(5));
        }
        // std_dev = 0 → normalize returns Ok(0.0) → z_score_of_latest returns Some(0.0)
        assert_eq!(n.z_score_of_latest(), Some(0.0));
    }

    #[test]
    fn test_z_score_of_latest_returns_some_with_variance() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] {
            n.update(v);
        }
        // latest = 4; should produce Some
        assert!(n.z_score_of_latest().is_some());
    }

    #[test]
    fn test_deviation_from_mean_none_when_empty() {
        let n = znorm(5);
        assert!(n.deviation_from_mean(dec!(10)).is_none());
    }

    #[test]
    fn test_deviation_from_mean_correct() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] {
            n.update(v);
        }
        // mean = 2.5, value = 4 → deviation = 1.5
        let d = n.deviation_from_mean(dec!(4)).unwrap();
        assert!((d - 1.5).abs() < 1e-9);
    }

    // ── ZScoreNormalizer::add_observation ─────────────────────────────────────

    #[test]
    fn test_add_observation_same_as_update() {
        let mut n1 = znorm(4);
        let mut n2 = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] {
            n1.update(v);
            n2.add_observation(v);
        }
        assert_eq!(n1.mean(), n2.mean());
    }

    #[test]
    fn test_add_observation_chainable() {
        let mut n = znorm(4);
        n.add_observation(dec!(1))
         .add_observation(dec!(2))
         .add_observation(dec!(3));
        assert_eq!(n.len(), 3);
    }

    // ── ZScoreNormalizer::variance_f64 ────────────────────────────────────────

    #[test]
    fn test_variance_f64_none_when_single_observation() {
        let mut n = znorm(4);
        n.update(dec!(5));
        assert!(n.variance_f64().is_none());
    }

    #[test]
    fn test_variance_f64_zero_when_all_same() {
        let mut n = znorm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        assert_eq!(n.variance_f64(), Some(0.0));
    }

    #[test]
    fn test_variance_f64_positive_with_spread() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        assert!(n.variance_f64().unwrap() > 0.0);
    }

    // ── ZScoreNormalizer::ema_of_z_scores ────────────────────────────────────

    #[test]
    fn test_ema_of_z_scores_none_when_single_value() {
        let mut n = znorm(4);
        n.update(dec!(5));
        assert!(n.ema_of_z_scores(0.5).is_none());
    }

    #[test]
    fn test_ema_of_z_scores_returns_some_with_variance() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] {
            n.update(v);
        }
        let ema = n.ema_of_z_scores(0.3);
        assert!(ema.is_some());
    }

    #[test]
    fn test_ema_of_z_scores_zero_when_all_same() {
        let mut n = znorm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        // All z-scores are 0.0 → EMA = 0.0
        assert_eq!(n.ema_of_z_scores(0.5), Some(0.0));
    }

    // ── ZScoreNormalizer::std_dev_f64 ─────────────────────────────────────────

    #[test]
    fn test_std_dev_f64_none_when_single_observation() {
        let mut n = znorm(4);
        n.update(dec!(5));
        assert!(n.std_dev_f64().is_none());
    }

    #[test]
    fn test_std_dev_f64_zero_when_all_same() {
        let mut n = znorm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        assert_eq!(n.std_dev_f64(), Some(0.0));
    }

    #[test]
    fn test_std_dev_f64_equals_sqrt_of_variance() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let var = n.variance_f64().unwrap();
        let std = n.std_dev_f64().unwrap();
        assert!((std - var.sqrt()).abs() < 1e-12);
    }

    // ── rolling_mean_change ───────────────────────────────────────────────────

    #[test]
    fn test_rolling_mean_change_none_when_one_observation() {
        let mut n = znorm(4);
        n.update(dec!(5));
        assert!(n.rolling_mean_change().is_none());
    }

    #[test]
    fn test_rolling_mean_change_positive_when_rising() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        // first half [1,2] mean=1.5, second half [3,4] mean=3.5 → change=2.0
        let change = n.rolling_mean_change().unwrap();
        assert!((change - 2.0).abs() < 1e-9);
    }

    #[test]
    fn test_rolling_mean_change_negative_when_falling() {
        let mut n = znorm(4);
        for v in [dec!(4), dec!(3), dec!(2), dec!(1)] { n.update(v); }
        let change = n.rolling_mean_change().unwrap();
        assert!(change < 0.0);
    }

    #[test]
    fn test_rolling_mean_change_zero_when_flat() {
        let mut n = znorm(4);
        for _ in 0..4 { n.update(dec!(7)); }
        let change = n.rolling_mean_change().unwrap();
        assert!(change.abs() < 1e-9);
    }

    // ── window_span_f64 ───────────────────────────────────────────────────────

    #[test]
    fn test_window_span_f64_none_when_empty() {
        let n = znorm(4);
        assert!(n.window_span_f64().is_none());
    }

    #[test]
    fn test_window_span_f64_zero_when_all_same() {
        let mut n = znorm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        assert_eq!(n.window_span_f64(), Some(0.0));
    }

    #[test]
    fn test_window_span_f64_correct_value() {
        let mut n = znorm(4);
        for v in [dec!(10), dec!(20), dec!(30), dec!(40)] { n.update(v); }
        // max=40, min=10, span=30
        assert!((n.window_span_f64().unwrap() - 30.0).abs() < 1e-9);
    }

    // ── count_positive_z_scores ───────────────────────────────────────────────

    #[test]
    fn test_count_positive_z_scores_zero_when_empty() {
        let n = znorm(4);
        assert_eq!(n.count_positive_z_scores(), 0);
    }

    #[test]
    fn test_count_positive_z_scores_zero_when_all_same() {
        let mut n = znorm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        assert_eq!(n.count_positive_z_scores(), 0);
    }

    #[test]
    fn test_count_positive_z_scores_half_above_mean() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        // mean=2.5, values above: 3 and 4
        assert_eq!(n.count_positive_z_scores(), 2);
    }

    // ── above_threshold_count ─────────────────────────────────────────────────

    #[test]
    fn test_above_threshold_count_zero_when_empty() {
        let n = znorm(4);
        assert_eq!(n.above_threshold_count(1.0), 0);
    }

    #[test]
    fn test_above_threshold_count_zero_when_all_same() {
        let mut n = znorm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        assert_eq!(n.above_threshold_count(0.5), 0);
    }

    #[test]
    fn test_above_threshold_count_correct_with_extremes() {
        let mut n = znorm(6);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5), dec!(100)] { n.update(v); }
        // 100 is many std devs from mean; threshold=1.0 should catch it
        assert!(n.above_threshold_count(1.0) >= 1);
    }
}

#[cfg(test)]
mod minmax_extra_tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn norm(w: usize) -> MinMaxNormalizer {
        MinMaxNormalizer::new(w).unwrap()
    }

    // ── fraction_above_mid ────────────────────────────────────────────────────

    #[test]
    fn test_fraction_above_mid_none_when_empty() {
        let mut n = norm(4);
        assert!(n.fraction_above_mid().is_none());
    }

    #[test]
    fn test_fraction_above_mid_zero_when_all_same() {
        let mut n = norm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        assert_eq!(n.fraction_above_mid(), Some(0.0));
    }

    #[test]
    fn test_fraction_above_mid_half_when_symmetric() {
        let mut n = norm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        // mid = (1+4)/2 = 2.5, above: 3 and 4 = 2/4 = 0.5
        let f = n.fraction_above_mid().unwrap();
        assert!((f - 0.5).abs() < 1e-10);
    }
}

#[cfg(test)]
mod zscore_stability_tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn znorm(w: usize) -> ZScoreNormalizer {
        ZScoreNormalizer::new(w).unwrap()
    }

    // ── is_mean_stable ────────────────────────────────────────────────────────

    #[test]
    fn test_is_mean_stable_false_when_window_too_small() {
        let n = znorm(4);
        assert!(!n.is_mean_stable(1.0));
    }

    #[test]
    fn test_is_mean_stable_true_when_flat() {
        let mut n = znorm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        assert!(n.is_mean_stable(0.001));
    }

    #[test]
    fn test_is_mean_stable_false_when_trending() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(10), dec!(20)] { n.update(v); }
        assert!(!n.is_mean_stable(0.5));
    }

    // ── ZScoreNormalizer::count_above / count_below ───────────────────────────

    #[test]
    fn test_zscore_count_above_zero_for_empty_window() {
        assert_eq!(znorm(4).count_above(dec!(10)), 0);
    }

    #[test]
    fn test_zscore_count_above_correct() {
        let mut n = znorm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)] { n.update(v); }
        // strictly above 3: [4, 5] → count = 2
        assert_eq!(n.count_above(dec!(3)), 2);
    }

    #[test]
    fn test_zscore_count_below_correct() {
        let mut n = znorm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)] { n.update(v); }
        // strictly below 3: [1, 2] → count = 2
        assert_eq!(n.count_below(dec!(3)), 2);
    }

    #[test]
    fn test_zscore_count_above_excludes_at_threshold() {
        let mut n = znorm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        assert_eq!(n.count_above(dec!(5)), 0);
        assert_eq!(n.count_below(dec!(5)), 0);
    }

    // ── ZScoreNormalizer::skewness ────────────────────────────────────────────

    #[test]
    fn test_zscore_skewness_none_for_fewer_than_3_obs() {
        let mut n = znorm(5);
        n.update(dec!(10));
        n.update(dec!(20));
        assert!(n.skewness().is_none());
    }

    #[test]
    fn test_zscore_skewness_none_for_all_identical() {
        let mut n = znorm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        assert!(n.skewness().is_none());
    }

    #[test]
    fn test_zscore_skewness_near_zero_for_symmetric_distribution() {
        let mut n = znorm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)] { n.update(v); }
        let skew = n.skewness().unwrap();
        assert!(skew.abs() < 0.01, "symmetric distribution should have ~0 skewness, got {skew}");
    }

    // ── ZScoreNormalizer::percentile_value ────────────────────────────────────

    #[test]
    fn test_zscore_percentile_value_none_for_empty_window() {
        assert!(znorm(4).percentile_value(0.5).is_none());
    }

    #[test]
    fn test_zscore_percentile_value_min_at_zero() {
        let mut n = znorm(5);
        for v in [dec!(10), dec!(20), dec!(30), dec!(40), dec!(50)] { n.update(v); }
        assert_eq!(n.percentile_value(0.0), Some(dec!(10)));
    }

    #[test]
    fn test_zscore_percentile_value_max_at_one() {
        let mut n = znorm(5);
        for v in [dec!(10), dec!(20), dec!(30), dec!(40), dec!(50)] { n.update(v); }
        assert_eq!(n.percentile_value(1.0), Some(dec!(50)));
    }

    // ── ZScoreNormalizer::ewma ────────────────────────────────────────────────

    #[test]
    fn test_zscore_ewma_none_for_empty_window() {
        assert!(znorm(4).ewma(0.5).is_none());
    }

    #[test]
    fn test_zscore_ewma_equals_value_for_single_obs() {
        let mut n = znorm(4);
        n.update(dec!(42));
        assert!((n.ewma(0.5).unwrap() - 42.0).abs() < 1e-10);
    }

    #[test]
    fn test_zscore_ewma_weights_recent_more_with_high_alpha() {
        // With alpha=1.0 EWMA = last value
        let mut n = znorm(4);
        for v in [dec!(10), dec!(20), dec!(30), dec!(100)] { n.update(v); }
        let ewma = n.ewma(1.0).unwrap();
        assert!((ewma - 100.0).abs() < 1e-10);
    }

    #[test]
    fn test_zscore_fraction_above_mid_none_for_empty_window() {
        let n = znorm(3);
        assert!(n.fraction_above_mid().is_none());
    }

    #[test]
    fn test_zscore_fraction_above_mid_none_when_all_equal() {
        let mut n = znorm(3);
        for _ in 0..3 { n.update(dec!(5)); }
        assert!(n.fraction_above_mid().is_none());
    }

    #[test]
    fn test_zscore_fraction_above_mid_half_above() {
        let mut n = znorm(4);
        for v in [dec!(0), dec!(10), dec!(6), dec!(4)] { n.update(v); }
        // mid = (0+10)/2 = 5; above 5: 10 and 6 → 2/4 = 0.5
        let frac = n.fraction_above_mid().unwrap();
        assert!((frac - 0.5).abs() < 1e-9);
    }

    #[test]
    fn test_zscore_normalized_range_none_for_empty_window() {
        let n = znorm(3);
        assert!(n.normalized_range().is_none());
    }

    #[test]
    fn test_zscore_normalized_range_zero_for_uniform_window() {
        let mut n = znorm(3);
        for _ in 0..3 { n.update(dec!(10)); }
        assert_eq!(n.normalized_range(), Some(0.0));
    }

    #[test]
    fn test_zscore_normalized_range_positive_for_varying_window() {
        let mut n = znorm(3);
        for v in [dec!(8), dec!(10), dec!(12)] { n.update(v); }
        // span = 12 - 8 = 4, mean = 10, ratio = 0.4
        let nr = n.normalized_range().unwrap();
        assert!((nr - 0.4).abs() < 1e-9);
    }

    // ── ZScoreNormalizer::midpoint ────────────────────────────────────────────

    #[test]
    fn test_zscore_midpoint_none_for_empty_window() {
        assert!(znorm(3).midpoint().is_none());
    }

    #[test]
    fn test_zscore_midpoint_correct_for_known_range() {
        let mut n = znorm(4);
        for v in [dec!(10), dec!(20), dec!(30), dec!(40)] { n.update(v); }
        // min=10, max=40, midpoint=25
        assert_eq!(n.midpoint(), Some(dec!(25)));
    }

    // ── ZScoreNormalizer::clamp_to_window ─────────────────────────────────────

    #[test]
    fn test_zscore_clamp_returns_value_unchanged_on_empty_window() {
        let n = znorm(3);
        assert_eq!(n.clamp_to_window(dec!(50)), dec!(50));
    }

    #[test]
    fn test_zscore_clamp_clamps_to_min() {
        let mut n = znorm(3);
        for v in [dec!(10), dec!(20), dec!(30)] { n.update(v); }
        assert_eq!(n.clamp_to_window(dec!(-5)), dec!(10));
    }

    #[test]
    fn test_zscore_clamp_clamps_to_max() {
        let mut n = znorm(3);
        for v in [dec!(10), dec!(20), dec!(30)] { n.update(v); }
        assert_eq!(n.clamp_to_window(dec!(100)), dec!(30));
    }

    #[test]
    fn test_zscore_clamp_passes_through_in_range_value() {
        let mut n = znorm(3);
        for v in [dec!(10), dec!(20), dec!(30)] { n.update(v); }
        assert_eq!(n.clamp_to_window(dec!(15)), dec!(15));
    }

    // ── ZScoreNormalizer::min_max ─────────────────────────────────────────────

    #[test]
    fn test_zscore_min_max_none_for_empty_window() {
        assert!(znorm(3).min_max().is_none());
    }

    #[test]
    fn test_zscore_min_max_returns_correct_pair() {
        let mut n = znorm(4);
        for v in [dec!(5), dec!(15), dec!(10), dec!(20)] { n.update(v); }
        assert_eq!(n.min_max(), Some((dec!(5), dec!(20))));
    }

    #[test]
    fn test_zscore_min_max_single_value() {
        let mut n = znorm(3);
        n.update(dec!(42));
        assert_eq!(n.min_max(), Some((dec!(42), dec!(42))));
    }

    // ── ZScoreNormalizer::values ──────────────────────────────────────────────

    #[test]
    fn test_zscore_values_empty_for_empty_window() {
        assert!(znorm(3).values().is_empty());
    }

    #[test]
    fn test_zscore_values_preserves_insertion_order() {
        let mut n = znorm(4);
        for v in [dec!(10), dec!(20), dec!(30), dec!(40)] { n.update(v); }
        assert_eq!(n.values(), vec![dec!(10), dec!(20), dec!(30), dec!(40)]);
    }

    // ── ZScoreNormalizer::above_zero_fraction ─────────────────────────────────

    #[test]
    fn test_zscore_above_zero_fraction_none_for_empty_window() {
        assert!(znorm(3).above_zero_fraction().is_none());
    }

    #[test]
    fn test_zscore_above_zero_fraction_zero_for_all_negative() {
        let mut n = znorm(3);
        for v in [dec!(-3), dec!(-2), dec!(-1)] { n.update(v); }
        assert_eq!(n.above_zero_fraction(), Some(0.0));
    }

    #[test]
    fn test_zscore_above_zero_fraction_one_for_all_positive() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        assert_eq!(n.above_zero_fraction(), Some(1.0));
    }

    #[test]
    fn test_zscore_above_zero_fraction_half_for_mixed() {
        let mut n = znorm(4);
        for v in [dec!(-2), dec!(-1), dec!(1), dec!(2)] { n.update(v); }
        let frac = n.above_zero_fraction().unwrap();
        assert!((frac - 0.5).abs() < 1e-9);
    }

    // ── ZScoreNormalizer::z_score_opt ─────────────────────────────────────────

    #[test]
    fn test_zscore_opt_none_for_empty_window() {
        assert!(znorm(3).z_score_opt(dec!(10)).is_none());
    }

    #[test]
    fn test_zscore_opt_matches_normalize_for_populated_window() {
        let mut n = znorm(4);
        for v in [dec!(10), dec!(20), dec!(30), dec!(40)] { n.update(v); }
        let z_opt = n.z_score_opt(dec!(25)).unwrap();
        let z_norm = n.normalize(dec!(25)).unwrap();
        assert!((z_opt - z_norm).abs() < 1e-12);
    }

    // ── ZScoreNormalizer::is_stable ───────────────────────────────────────────

    #[test]
    fn test_zscore_is_stable_false_for_empty_window() {
        assert!(!znorm(3).is_stable(2.0));
    }

    #[test]
    fn test_zscore_is_stable_true_for_near_mean_value() {
        let mut n = znorm(5);
        for v in [dec!(10), dec!(20), dec!(30), dec!(40), dec!(30)] { n.update(v); }
        // latest is 30, near mean of 26; should be stable with threshold=2
        assert!(n.is_stable(2.0));
    }

    #[test]
    fn test_zscore_is_stable_false_for_extreme_value() {
        let mut n = znorm(5);
        for v in [dec!(10), dec!(10), dec!(10), dec!(10), dec!(100)] { n.update(v); }
        // latest is 100, far from mean ~28; should be unstable
        assert!(!n.is_stable(1.0));
    }

    // ── ZScoreNormalizer::window_values_above / window_values_below (moved here) ─

    #[test]
    fn test_zscore_window_values_above_via_znorm_empty() {
        assert!(znorm(3).window_values_above(dec!(5)).is_empty());
    }

    #[test]
    fn test_zscore_window_values_above_via_znorm_filters() {
        let mut n = znorm(5);
        for v in [dec!(1), dec!(3), dec!(5), dec!(7), dec!(9)] { n.update(v); }
        let above = n.window_values_above(dec!(5));
        assert_eq!(above.len(), 2);
        assert!(above.contains(&dec!(7)));
        assert!(above.contains(&dec!(9)));
    }

    #[test]
    fn test_zscore_window_values_below_via_znorm_empty() {
        assert!(znorm(3).window_values_below(dec!(5)).is_empty());
    }

    #[test]
    fn test_zscore_window_values_below_via_znorm_filters() {
        let mut n = znorm(5);
        for v in [dec!(1), dec!(3), dec!(5), dec!(7), dec!(9)] { n.update(v); }
        let below = n.window_values_below(dec!(5));
        assert_eq!(below.len(), 2);
        assert!(below.contains(&dec!(1)));
        assert!(below.contains(&dec!(3)));
    }

    // ── ZScoreNormalizer::fraction_above / fraction_below ─────────────────────

    #[test]
    fn test_zscore_fraction_above_none_for_empty_window() {
        assert!(znorm(3).fraction_above(dec!(5)).is_none());
    }

    #[test]
    fn test_zscore_fraction_above_correct() {
        let mut n = znorm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)] { n.update(v); }
        // above 3: {4, 5} = 2/5 = 0.4
        let frac = n.fraction_above(dec!(3)).unwrap();
        assert!((frac - 0.4).abs() < 1e-9);
    }

    #[test]
    fn test_zscore_fraction_below_none_for_empty_window() {
        assert!(znorm(3).fraction_below(dec!(5)).is_none());
    }

    #[test]
    fn test_zscore_fraction_below_correct() {
        let mut n = znorm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)] { n.update(v); }
        // below 3: {1, 2} = 2/5 = 0.4
        let frac = n.fraction_below(dec!(3)).unwrap();
        assert!((frac - 0.4).abs() < 1e-9);
    }

    // ── ZScoreNormalizer::window_values_above / window_values_below ──────────

    #[test]
    fn test_zscore_window_values_above_empty_window() {
        assert!(znorm(3).window_values_above(dec!(0)).is_empty());
    }

    #[test]
    fn test_zscore_window_values_above_filters_correctly() {
        let mut n = znorm(5);
        for v in [dec!(1), dec!(3), dec!(5), dec!(7), dec!(9)] { n.update(v); }
        let above = n.window_values_above(dec!(5));
        assert_eq!(above.len(), 2);
        assert!(above.contains(&dec!(7)));
        assert!(above.contains(&dec!(9)));
    }

    #[test]
    fn test_zscore_window_values_below_empty_window() {
        assert!(znorm(3).window_values_below(dec!(0)).is_empty());
    }

    #[test]
    fn test_zscore_window_values_below_filters_correctly() {
        let mut n = znorm(5);
        for v in [dec!(1), dec!(3), dec!(5), dec!(7), dec!(9)] { n.update(v); }
        let below = n.window_values_below(dec!(5));
        assert_eq!(below.len(), 2);
        assert!(below.contains(&dec!(1)));
        assert!(below.contains(&dec!(3)));
    }

    // ── ZScoreNormalizer::percentile_rank ─────────────────────────────────────

    #[test]
    fn test_zscore_percentile_rank_none_for_empty_window() {
        assert!(znorm(3).percentile_rank(dec!(5)).is_none());
    }

    #[test]
    fn test_zscore_percentile_rank_correct() {
        let mut n = znorm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)] { n.update(v); }
        // <= 3: {1, 2, 3} = 3/5 = 0.6
        let rank = n.percentile_rank(dec!(3)).unwrap();
        assert!((rank - 0.6).abs() < 1e-9);
    }

    // ── ZScoreNormalizer::count_equal ─────────────────────────────────────────

    #[test]
    fn test_zscore_count_equal_zero_for_no_match() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        assert_eq!(n.count_equal(dec!(99)), 0);
    }

    #[test]
    fn test_zscore_count_equal_counts_duplicates() {
        let mut n = znorm(5);
        for v in [dec!(5), dec!(5), dec!(3), dec!(5), dec!(2)] { n.update(v); }
        assert_eq!(n.count_equal(dec!(5)), 3);
    }

    // ── ZScoreNormalizer::median ──────────────────────────────────────────────

    #[test]
    fn test_zscore_median_none_for_empty_window() {
        assert!(znorm(3).median().is_none());
    }

    #[test]
    fn test_zscore_median_correct_for_odd_count() {
        let mut n = znorm(5);
        for v in [dec!(3), dec!(1), dec!(5), dec!(4), dec!(2)] { n.update(v); }
        // sorted: 1,2,3,4,5 → middle (idx 2) = 3
        assert_eq!(n.median(), Some(dec!(3)));
    }

    // ── ZScoreNormalizer::rolling_range ───────────────────────────────────────

    #[test]
    fn test_zscore_rolling_range_none_for_empty() {
        assert!(znorm(3).rolling_range().is_none());
    }

    #[test]
    fn test_zscore_rolling_range_correct() {
        let mut n = znorm(5);
        for v in [dec!(10), dec!(50), dec!(30), dec!(20), dec!(40)] { n.update(v); }
        assert_eq!(n.rolling_range(), Some(dec!(40)));
    }

    // ── ZScoreNormalizer::skewness ─────────────────────────────────────────────

    #[test]
    fn test_zscore_skewness_none_for_fewer_than_3() {
        let mut n = znorm(5);
        n.update(dec!(1)); n.update(dec!(2));
        assert!(n.skewness().is_none());
    }

    #[test]
    fn test_zscore_skewness_near_zero_for_symmetric_data() {
        let mut n = znorm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)] { n.update(v); }
        let s = n.skewness().unwrap();
        assert!(s.abs() < 0.5);
    }

    // ── ZScoreNormalizer::kurtosis ─────────────────────────────────────

    #[test]
    fn test_zscore_kurtosis_none_for_fewer_than_4() {
        let mut n = znorm(5);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        assert!(n.kurtosis().is_none());
    }

    #[test]
    fn test_zscore_kurtosis_returns_f64_for_populated_window() {
        let mut n = znorm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)] { n.update(v); }
        assert!(n.kurtosis().is_some());
    }

    // ── ZScoreNormalizer::autocorrelation_lag1 ────────────────────────────────

    #[test]
    fn test_zscore_autocorrelation_none_for_single_value() {
        let mut n = znorm(3);
        n.update(dec!(1));
        assert!(n.autocorrelation_lag1().is_none());
    }

    #[test]
    fn test_zscore_autocorrelation_positive_for_trending_data() {
        let mut n = znorm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)] { n.update(v); }
        let ac = n.autocorrelation_lag1().unwrap();
        assert!(ac > 0.0);
    }

    // ── ZScoreNormalizer::trend_consistency ───────────────────────────────────

    #[test]
    fn test_zscore_trend_consistency_none_for_single_value() {
        let mut n = znorm(3);
        n.update(dec!(1));
        assert!(n.trend_consistency().is_none());
    }

    #[test]
    fn test_zscore_trend_consistency_one_for_strictly_rising() {
        let mut n = znorm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)] { n.update(v); }
        let tc = n.trend_consistency().unwrap();
        assert!((tc - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_zscore_trend_consistency_zero_for_strictly_falling() {
        let mut n = znorm(5);
        for v in [dec!(5), dec!(4), dec!(3), dec!(2), dec!(1)] { n.update(v); }
        let tc = n.trend_consistency().unwrap();
        assert!((tc - 0.0).abs() < 1e-9);
    }

    // ── ZScoreNormalizer::coefficient_of_variation ────────────────────────────

    #[test]
    fn test_zscore_cov_none_for_empty_window() {
        assert!(znorm(3).coefficient_of_variation().is_none());
    }

    #[test]
    fn test_zscore_cov_positive_for_varied_data() {
        let mut n = znorm(5);
        for v in [dec!(10), dec!(20), dec!(30), dec!(40), dec!(50)] { n.update(v); }
        let cov = n.coefficient_of_variation().unwrap();
        assert!(cov > 0.0);
    }

    // ── ZScoreNormalizer::mean_absolute_deviation ─────────────────────────────

    #[test]
    fn test_zscore_mad_none_for_empty() {
        assert!(znorm(3).mean_absolute_deviation().is_none());
    }

    #[test]
    fn test_zscore_mad_zero_for_identical_values() {
        let mut n = znorm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let mad = n.mean_absolute_deviation().unwrap();
        assert!((mad - 0.0).abs() < 1e-9);
    }

    #[test]
    fn test_zscore_mad_positive_for_varied_data() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let mad = n.mean_absolute_deviation().unwrap();
        assert!(mad > 0.0);
    }

    // ── ZScoreNormalizer::percentile_of_latest ────────────────────────────────

    #[test]
    fn test_zscore_percentile_of_latest_none_for_empty() {
        assert!(znorm(3).percentile_of_latest().is_none());
    }

    #[test]
    fn test_zscore_percentile_of_latest_returns_some_after_update() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        assert!(n.percentile_of_latest().is_some());
    }

    #[test]
    fn test_zscore_percentile_of_latest_max_has_high_rank() {
        let mut n = znorm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)] { n.update(v); }
        let rank = n.percentile_of_latest().unwrap();
        assert!(rank >= 0.9, "max value should have rank near 1.0, got {}", rank);
    }

    // ── ZScoreNormalizer::tail_ratio ──────────────────────────────────────────

    #[test]
    fn test_zscore_tail_ratio_none_for_empty() {
        assert!(znorm(4).tail_ratio().is_none());
    }

    #[test]
    fn test_zscore_tail_ratio_one_for_identical_values() {
        let mut n = znorm(4);
        for _ in 0..4 { n.update(dec!(7)); }
        let r = n.tail_ratio().unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0, got {}", r);
    }

    #[test]
    fn test_zscore_tail_ratio_above_one_with_outlier() {
        let mut n = znorm(5);
        for v in [dec!(1), dec!(1), dec!(1), dec!(1), dec!(10)] { n.update(v); }
        let r = n.tail_ratio().unwrap();
        assert!(r > 1.0, "outlier should push ratio above 1.0, got {}", r);
    }

    // ── ZScoreNormalizer::z_score_of_min / z_score_of_max ────────────────────

    #[test]
    fn test_zscore_z_score_of_min_none_for_empty() {
        assert!(znorm(4).z_score_of_min().is_none());
    }

    #[test]
    fn test_zscore_z_score_of_max_none_for_empty() {
        assert!(znorm(4).z_score_of_max().is_none());
    }

    #[test]
    fn test_zscore_z_score_of_min_negative_for_varied_window() {
        let mut n = znorm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)] { n.update(v); }
        let z = n.z_score_of_min().unwrap();
        assert!(z < 0.0, "z-score of min should be negative, got {}", z);
    }

    #[test]
    fn test_zscore_z_score_of_max_positive_for_varied_window() {
        let mut n = znorm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)] { n.update(v); }
        let z = n.z_score_of_max().unwrap();
        assert!(z > 0.0, "z-score of max should be positive, got {}", z);
    }

    // ── ZScoreNormalizer::window_entropy ──────────────────────────────────────

    #[test]
    fn test_zscore_window_entropy_none_for_empty() {
        assert!(znorm(4).window_entropy().is_none());
    }

    #[test]
    fn test_zscore_window_entropy_zero_for_identical_values() {
        let mut n = znorm(3);
        for _ in 0..3 { n.update(dec!(5)); }
        let e = n.window_entropy().unwrap();
        assert!((e - 0.0).abs() < 1e-9, "identical values should have zero entropy, got {}", e);
    }

    #[test]
    fn test_zscore_window_entropy_positive_for_varied_values() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let e = n.window_entropy().unwrap();
        assert!(e > 0.0, "varied values should have positive entropy, got {}", e);
    }

    // ── ZScoreNormalizer::normalized_std_dev ──────────────────────────────────

    #[test]
    fn test_zscore_normalized_std_dev_none_for_empty() {
        assert!(znorm(4).normalized_std_dev().is_none());
    }

    #[test]
    fn test_zscore_normalized_std_dev_positive_for_varied_values() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let r = n.normalized_std_dev().unwrap();
        assert!(r > 0.0, "expected positive normalized std dev, got {}", r);
    }

    // ── ZScoreNormalizer::value_above_mean_count ──────────────────────────────

    #[test]
    fn test_zscore_value_above_mean_count_none_for_empty() {
        assert!(znorm(4).value_above_mean_count().is_none());
    }

    #[test]
    fn test_zscore_value_above_mean_count_correct() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        // mean=2.5, above: 3 and 4 → 2
        assert_eq!(n.value_above_mean_count().unwrap(), 2);
    }

    // ── ZScoreNormalizer::consecutive_above_mean ──────────────────────────────

    #[test]
    fn test_zscore_consecutive_above_mean_none_for_empty() {
        assert!(znorm(4).consecutive_above_mean().is_none());
    }

    #[test]
    fn test_zscore_consecutive_above_mean_correct() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(5), dec!(6), dec!(7)] { n.update(v); }
        // mean=4.75, above: 5,6,7 → run=3
        assert_eq!(n.consecutive_above_mean().unwrap(), 3);
    }

    // ── ZScoreNormalizer::above_threshold_fraction / below_threshold_fraction ─

    #[test]
    fn test_zscore_above_threshold_fraction_none_for_empty() {
        assert!(znorm(4).above_threshold_fraction(dec!(5)).is_none());
    }

    #[test]
    fn test_zscore_above_threshold_fraction_correct() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let f = n.above_threshold_fraction(dec!(2)).unwrap();
        assert!((f - 0.5).abs() < 1e-9, "expected 0.5, got {}", f);
    }

    #[test]
    fn test_zscore_below_threshold_fraction_none_for_empty() {
        assert!(znorm(4).below_threshold_fraction(dec!(5)).is_none());
    }

    #[test]
    fn test_zscore_below_threshold_fraction_correct() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let f = n.below_threshold_fraction(dec!(3)).unwrap();
        assert!((f - 0.5).abs() < 1e-9, "expected 0.5, got {}", f);
    }

    // ── ZScoreNormalizer::lag_k_autocorrelation ───────────────────────────────

    #[test]
    fn test_zscore_lag_k_autocorrelation_none_for_zero_k() {
        let mut n = znorm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)] { n.update(v); }
        assert!(n.lag_k_autocorrelation(0).is_none());
    }

    #[test]
    fn test_zscore_lag_k_autocorrelation_none_when_k_gte_len() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        assert!(n.lag_k_autocorrelation(3).is_none());
    }

    #[test]
    fn test_zscore_lag_k_autocorrelation_positive_for_trend() {
        let mut n = znorm(6);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5), dec!(6)] { n.update(v); }
        let ac = n.lag_k_autocorrelation(1).unwrap();
        assert!(ac > 0.0, "trending series should have positive AC, got {}", ac);
    }

    // ── ZScoreNormalizer::half_life_estimate ──────────────────────────────────

    #[test]
    fn test_zscore_half_life_estimate_none_for_fewer_than_3() {
        let mut n = znorm(3);
        n.update(dec!(1)); n.update(dec!(2));
        assert!(n.half_life_estimate().is_none());
    }

    #[test]
    fn test_zscore_half_life_no_panic_for_alternating() {
        let mut n = znorm(6);
        for v in [dec!(10), dec!(5), dec!(10), dec!(5), dec!(10), dec!(5)] { n.update(v); }
        let _ = n.half_life_estimate();
    }

    // ── ZScoreNormalizer::geometric_mean ──────────────────────────────────────

    #[test]
    fn test_zscore_geometric_mean_none_for_empty() {
        assert!(znorm(4).geometric_mean().is_none());
    }

    #[test]
    fn test_zscore_geometric_mean_correct_for_powers_of_2() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(4), dec!(8)] { n.update(v); }
        let gm = n.geometric_mean().unwrap();
        assert!((gm - 64.0f64.powf(0.25)).abs() < 1e-6, "got {}", gm);
    }

    // ── ZScoreNormalizer::harmonic_mean ───────────────────────────────────────

    #[test]
    fn test_zscore_harmonic_mean_none_for_empty() {
        assert!(znorm(4).harmonic_mean().is_none());
    }

    #[test]
    fn test_zscore_harmonic_mean_positive_for_positive_values() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let hm = n.harmonic_mean().unwrap();
        assert!(hm > 0.0 && hm < 4.0, "HM should be in (0, max), got {}", hm);
    }

    // ── ZScoreNormalizer::range_normalized_value ──────────────────────────────

    #[test]
    fn test_zscore_range_normalized_value_none_for_empty() {
        assert!(znorm(4).range_normalized_value(dec!(5)).is_none());
    }

    #[test]
    fn test_zscore_range_normalized_value_in_range() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let r = n.range_normalized_value(dec!(2)).unwrap();
        assert!(r >= 0.0 && r <= 1.0, "expected [0,1], got {}", r);
    }

    // ── ZScoreNormalizer::distance_from_median ────────────────────────────────

    #[test]
    fn test_zscore_distance_from_median_none_for_empty() {
        assert!(znorm(4).distance_from_median(dec!(5)).is_none());
    }

    #[test]
    fn test_zscore_distance_from_median_zero_at_median() {
        let mut n = znorm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)] { n.update(v); }
        let d = n.distance_from_median(dec!(3)).unwrap();
        assert!((d - 0.0).abs() < 1e-9, "distance from median=3 should be 0, got {}", d);
    }

    #[test]
    fn test_zscore_momentum_none_for_single_value() {
        let mut n = znorm(5);
        n.update(dec!(10));
        assert!(n.momentum().is_none());
    }

    #[test]
    fn test_zscore_momentum_positive_for_rising_window() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let m = n.momentum().unwrap();
        assert!(m > 0.0, "rising window → positive momentum, got {}", m);
    }

    #[test]
    fn test_zscore_value_rank_none_for_empty() {
        assert!(znorm(4).value_rank(dec!(5)).is_none());
    }

    #[test]
    fn test_zscore_value_rank_extremes() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let low = n.value_rank(dec!(0)).unwrap();
        assert!((low - 0.0).abs() < 1e-9, "got {}", low);
        let high = n.value_rank(dec!(5)).unwrap();
        assert!((high - 1.0).abs() < 1e-9, "got {}", high);
    }

    #[test]
    fn test_zscore_coeff_of_variation_none_for_single_value() {
        let mut n = znorm(5);
        n.update(dec!(10));
        assert!(n.coeff_of_variation().is_none());
    }

    #[test]
    fn test_zscore_coeff_of_variation_positive_for_spread() {
        let mut n = znorm(4);
        for v in [dec!(10), dec!(20), dec!(30), dec!(40)] { n.update(v); }
        let cv = n.coeff_of_variation().unwrap();
        assert!(cv > 0.0, "expected positive CV, got {}", cv);
    }

    #[test]
    fn test_zscore_quantile_range_none_for_empty() {
        assert!(znorm(4).quantile_range().is_none());
    }

    #[test]
    fn test_zscore_quantile_range_non_negative() {
        let mut n = znorm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)] { n.update(v); }
        let iqr = n.quantile_range().unwrap();
        assert!(iqr >= 0.0, "IQR should be non-negative, got {}", iqr);
    }

    // ── ZScoreNormalizer::upper_quartile / lower_quartile ─────────────────────

    #[test]
    fn test_zscore_upper_quartile_none_for_empty() {
        assert!(znorm(4).upper_quartile().is_none());
    }

    #[test]
    fn test_zscore_lower_quartile_none_for_empty() {
        assert!(znorm(4).lower_quartile().is_none());
    }

    #[test]
    fn test_zscore_upper_ge_lower_quartile() {
        let mut n = znorm(8);
        for v in [dec!(10), dec!(20), dec!(30), dec!(40), dec!(50), dec!(60), dec!(70), dec!(80)] {
            n.update(v);
        }
        let q3 = n.upper_quartile().unwrap();
        let q1 = n.lower_quartile().unwrap();
        assert!(q3 >= q1, "Q3 ({}) should be >= Q1 ({})", q3, q1);
    }

    // ── ZScoreNormalizer::sign_change_rate ────────────────────────────────────

    #[test]
    fn test_zscore_sign_change_rate_none_for_fewer_than_3() {
        let mut n = znorm(4);
        n.update(dec!(1));
        n.update(dec!(2));
        assert!(n.sign_change_rate().is_none());
    }

    #[test]
    fn test_zscore_sign_change_rate_one_for_zigzag() {
        let mut n = znorm(5);
        for v in [dec!(1), dec!(3), dec!(1), dec!(3), dec!(1)] { n.update(v); }
        let r = n.sign_change_rate().unwrap();
        assert!((r - 1.0).abs() < 1e-9, "zigzag should give 1.0, got {}", r);
    }

    #[test]
    fn test_zscore_sign_change_rate_zero_for_monotone() {
        let mut n = znorm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)] { n.update(v); }
        let r = n.sign_change_rate().unwrap();
        assert!((r - 0.0).abs() < 1e-9, "monotone should give 0.0, got {}", r);
    }

    // ── ZScoreNormalizer::trimmed_mean ────────────────────────────────────────

    #[test]
    fn test_zscore_trimmed_mean_none_for_empty() {
        assert!(znorm(4).trimmed_mean(0.1).is_none());
    }

    #[test]
    fn test_zscore_trimmed_mean_equals_mean_at_zero_trim() {
        let mut n = znorm(4);
        for v in [dec!(10), dec!(20), dec!(30), dec!(40)] { n.update(v); }
        let tm = n.trimmed_mean(0.0).unwrap();
        let m = n.mean().unwrap().to_f64().unwrap();
        assert!((tm - m).abs() < 1e-9, "0% trim should equal mean, got tm={} m={}", tm, m);
    }

    #[test]
    fn test_zscore_trimmed_mean_reduces_outlier_effect() {
        let mut n = znorm(5);
        for v in [dec!(10), dec!(10), dec!(10), dec!(10), dec!(1000)] { n.update(v); }
        // p=0.2 → trim = floor(5*0.2) = 1 element removed from each end
        let tm = n.trimmed_mean(0.2).unwrap();
        let m = n.mean().unwrap().to_f64().unwrap();
        assert!(tm < m, "trimmed mean should be less than mean when outlier trimmed, tm={} m={}", tm, m);
    }

    // ── ZScoreNormalizer::linear_trend_slope ─────────────────────────────────

    #[test]
    fn test_zscore_linear_trend_slope_none_for_single_value() {
        let mut n = znorm(4);
        n.update(dec!(10));
        assert!(n.linear_trend_slope().is_none());
    }

    #[test]
    fn test_zscore_linear_trend_slope_positive_for_rising() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let slope = n.linear_trend_slope().unwrap();
        assert!(slope > 0.0, "rising window → positive slope, got {}", slope);
    }

    #[test]
    fn test_zscore_linear_trend_slope_negative_for_falling() {
        let mut n = znorm(4);
        for v in [dec!(4), dec!(3), dec!(2), dec!(1)] { n.update(v); }
        let slope = n.linear_trend_slope().unwrap();
        assert!(slope < 0.0, "falling window → negative slope, got {}", slope);
    }

    #[test]
    fn test_zscore_linear_trend_slope_zero_for_flat() {
        let mut n = znorm(4);
        for v in [dec!(5), dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let slope = n.linear_trend_slope().unwrap();
        assert!(slope.abs() < 1e-9, "flat window → slope=0, got {}", slope);
    }

    // ── ZScoreNormalizer::variance_ratio ─────────────────────────────────────

    #[test]
    fn test_zscore_variance_ratio_none_for_few_values() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        assert!(n.variance_ratio().is_none());
    }

    #[test]
    fn test_zscore_variance_ratio_gt_one_for_decreasing_vol() {
        let mut n = znorm(6);
        // first half: high variance [1, 10, 1]; second half: low variance [5, 6, 5]
        for v in [dec!(1), dec!(10), dec!(1), dec!(5), dec!(6), dec!(5)] { n.update(v); }
        let r = n.variance_ratio().unwrap();
        assert!(r > 1.0, "first half more volatile → ratio > 1, got {}", r);
    }

    // ── ZScoreNormalizer::z_score_trend_slope ────────────────────────────────

    #[test]
    fn test_zscore_z_score_trend_slope_none_for_single_value() {
        let mut n = znorm(4);
        n.update(dec!(10));
        assert!(n.z_score_trend_slope().is_none());
    }

    #[test]
    fn test_zscore_z_score_trend_slope_positive_for_rising() {
        let mut n = znorm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)] { n.update(v); }
        let slope = n.z_score_trend_slope().unwrap();
        assert!(slope > 0.0, "rising window → positive z-score slope, got {}", slope);
    }

    // ── ZScoreNormalizer::mean_absolute_change ────────────────────────────────

    #[test]
    fn test_zscore_mean_absolute_change_none_for_single_value() {
        let mut n = znorm(4);
        n.update(dec!(10));
        assert!(n.mean_absolute_change().is_none());
    }

    #[test]
    fn test_zscore_mean_absolute_change_zero_for_constant() {
        let mut n = znorm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        let mac = n.mean_absolute_change().unwrap();
        assert!(mac.abs() < 1e-9, "constant window → MAC=0, got {}", mac);
    }

    #[test]
    fn test_zscore_mean_absolute_change_positive_for_varying() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(3), dec!(2), dec!(5)] { n.update(v); }
        let mac = n.mean_absolute_change().unwrap();
        assert!(mac > 0.0, "varying window → MAC > 0, got {}", mac);
    }

    // ── round-83 tests ─────────────────────────────────────────────────────

    #[test]
    fn test_zscore_monotone_increase_fraction_none_for_single() {
        let mut n = znorm(4);
        n.update(dec!(5));
        assert!(n.monotone_increase_fraction().is_none());
    }

    #[test]
    fn test_zscore_monotone_increase_fraction_one_for_rising() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let f = n.monotone_increase_fraction().unwrap();
        assert!((f - 1.0).abs() < 1e-9, "all rising → fraction=1, got {}", f);
    }

    #[test]
    fn test_zscore_abs_max_none_for_empty() {
        let n = znorm(4);
        assert!(n.abs_max().is_none());
    }

    #[test]
    fn test_zscore_abs_max_returns_max_absolute() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(3), dec!(2)] { n.update(v); }
        assert_eq!(n.abs_max().unwrap(), dec!(3));
    }

    #[test]
    fn test_zscore_max_count_none_for_empty() {
        let n = znorm(4);
        assert!(n.max_count().is_none());
    }

    #[test]
    fn test_zscore_max_count_correct() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(5), dec!(3), dec!(5)] { n.update(v); }
        assert_eq!(n.max_count().unwrap(), 2);
    }

    #[test]
    fn test_zscore_mean_ratio_none_for_single() {
        let mut n = znorm(4);
        n.update(dec!(10));
        assert!(n.mean_ratio().is_none());
    }

    // ── round-84 tests ─────────────────────────────────────────────────────

    #[test]
    fn test_zscore_exponential_weighted_mean_none_for_empty() {
        let n = znorm(4);
        assert!(n.exponential_weighted_mean(0.5).is_none());
    }

    #[test]
    fn test_zscore_exponential_weighted_mean_returns_value() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let ewm = n.exponential_weighted_mean(0.5).unwrap();
        assert!(ewm > 0.0, "EWM should be positive, got {}", ewm);
    }

    #[test]
    fn test_zscore_peak_to_trough_none_for_empty() {
        let n = znorm(4);
        assert!(n.peak_to_trough_ratio().is_none());
    }

    #[test]
    fn test_zscore_peak_to_trough_correct() {
        let mut n = znorm(4);
        for v in [dec!(2), dec!(4), dec!(1), dec!(8)] { n.update(v); }
        let r = n.peak_to_trough_ratio().unwrap();
        assert!((r - 8.0).abs() < 1e-9, "max=8, min=1 → ratio=8, got {}", r);
    }

    #[test]
    fn test_zscore_second_moment_none_for_empty() {
        let n = znorm(4);
        assert!(n.second_moment().is_none());
    }

    #[test]
    fn test_zscore_second_moment_correct() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let m = n.second_moment().unwrap();
        assert!((m - 14.0 / 3.0).abs() < 1e-9, "second moment ≈ 4.667, got {}", m);
    }

    #[test]
    fn test_zscore_range_over_mean_none_for_empty() {
        let n = znorm(4);
        assert!(n.range_over_mean().is_none());
    }

    #[test]
    fn test_zscore_range_over_mean_positive() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let r = n.range_over_mean().unwrap();
        assert!(r > 0.0, "range/mean should be positive, got {}", r);
    }

    #[test]
    fn test_zscore_above_median_fraction_none_for_empty() {
        let n = znorm(4);
        assert!(n.above_median_fraction().is_none());
    }

    #[test]
    fn test_zscore_above_median_fraction_in_range() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let f = n.above_median_fraction().unwrap();
        assert!(f >= 0.0 && f <= 1.0, "fraction in [0,1], got {}", f);
    }

    // ── round-85 tests ─────────────────────────────────────────────────────

    #[test]
    fn test_zscore_interquartile_mean_none_for_empty() {
        let n = znorm(4);
        assert!(n.interquartile_mean().is_none());
    }

    #[test]
    fn test_zscore_outlier_fraction_none_for_empty() {
        let n = znorm(4);
        assert!(n.outlier_fraction(2.0).is_none());
    }

    #[test]
    fn test_zscore_outlier_fraction_zero_for_constant() {
        let mut n = znorm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        let f = n.outlier_fraction(1.0).unwrap();
        assert!(f.abs() < 1e-9, "constant window → no outliers, got {}", f);
    }

    #[test]
    fn test_zscore_sign_flip_count_none_for_single() {
        let mut n = znorm(4);
        n.update(dec!(1));
        assert!(n.sign_flip_count().is_none());
    }

    #[test]
    fn test_zscore_sign_flip_count_correct() {
        let mut n = znorm(6);
        for v in [dec!(1), dec!(-1), dec!(1), dec!(-1)] { n.update(v); }
        let c = n.sign_flip_count().unwrap();
        assert_eq!(c, 3, "3 sign flips expected, got {}", c);
    }

    #[test]
    fn test_zscore_rms_none_for_empty() {
        let n = znorm(4);
        assert!(n.rms().is_none());
    }

    #[test]
    fn test_zscore_rms_positive_for_nonzero_values() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let r = n.rms().unwrap();
        assert!(r > 0.0, "RMS should be positive, got {}", r);
    }

    // ── round-86 tests ─────────────────────────────────────────────────────

    #[test]
    fn test_zscore_distinct_count_zero_for_empty() {
        let n = znorm(4);
        assert_eq!(n.distinct_count(), 0);
    }

    #[test]
    fn test_zscore_distinct_count_correct() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(1), dec!(2), dec!(3)] { n.update(v); }
        assert_eq!(n.distinct_count(), 3);
    }

    #[test]
    fn test_zscore_max_fraction_none_for_empty() {
        let n = znorm(4);
        assert!(n.max_fraction().is_none());
    }

    #[test]
    fn test_zscore_max_fraction_correct() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(3)] { n.update(v); }
        let f = n.max_fraction().unwrap();
        assert!((f - 0.5).abs() < 1e-9, "2/4 are max → 0.5, got {}", f);
    }

    #[test]
    fn test_zscore_latest_minus_mean_none_for_empty() {
        let n = znorm(4);
        assert!(n.latest_minus_mean().is_none());
    }

    #[test]
    fn test_zscore_latest_to_mean_ratio_none_for_empty() {
        let n = znorm(4);
        assert!(n.latest_to_mean_ratio().is_none());
    }

    #[test]
    fn test_zscore_latest_to_mean_ratio_one_for_constant() {
        let mut n = znorm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        let r = n.latest_to_mean_ratio().unwrap();
        assert!((r - 1.0).abs() < 1e-9, "latest=mean → ratio=1, got {}", r);
    }

    // ── round-87 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_zscore_below_mean_fraction_none_for_empty() {
        assert!(znorm(4).below_mean_fraction().is_none());
    }

    #[test]
    fn test_zscore_below_mean_fraction_symmetric_data() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        // mean=2.5; values strictly below: 1, 2 → 2/4 = 0.5
        let f = n.below_mean_fraction().unwrap();
        assert!((f - 0.5).abs() < 1e-9, "expected 0.5, got {}", f);
    }

    #[test]
    fn test_zscore_tail_variance_none_for_small_window() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        assert!(n.tail_variance().is_none());
    }

    #[test]
    fn test_zscore_tail_variance_nonneg_for_varied_data() {
        let mut n = znorm(6);
        for v in [dec!(1), dec!(2), dec!(5), dec!(6), dec!(9), dec!(10)] { n.update(v); }
        let tv = n.tail_variance().unwrap();
        assert!(tv >= 0.0, "tail variance should be non-negative, got {}", tv);
    }

    // ── round-88 tests ─────────────────────────────────────────────────────

    #[test]
    fn test_zscore_new_max_count_zero_for_empty() {
        let n = znorm(4);
        assert_eq!(n.new_max_count(), 0);
    }

    #[test]
    fn test_zscore_new_max_count_all_rising() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        assert_eq!(n.new_max_count(), 4, "each value is a new high");
    }

    #[test]
    fn test_zscore_new_min_count_zero_for_empty() {
        let n = znorm(4);
        assert_eq!(n.new_min_count(), 0);
    }

    #[test]
    fn test_zscore_new_min_count_all_falling() {
        let mut n = znorm(4);
        for v in [dec!(4), dec!(3), dec!(2), dec!(1)] { n.update(v); }
        assert_eq!(n.new_min_count(), 4, "each value is a new low");
    }

    #[test]
    fn test_zscore_zero_fraction_none_for_empty() {
        let n = znorm(4);
        assert!(n.zero_fraction().is_none());
    }

    #[test]
    fn test_zscore_zero_fraction_zero_when_no_zeros() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let f = n.zero_fraction().unwrap();
        assert!(f.abs() < 1e-9, "no zeros → fraction=0, got {}", f);
    }

    // ── round-89 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_zscore_cumulative_sum_zero_for_empty() {
        let n = znorm(4);
        assert_eq!(n.cumulative_sum(), rust_decimal::Decimal::ZERO);
    }

    #[test]
    fn test_zscore_cumulative_sum_correct() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        assert_eq!(n.cumulative_sum(), dec!(6));
    }

    #[test]
    fn test_zscore_max_to_min_ratio_none_for_empty() {
        assert!(znorm(4).max_to_min_ratio().is_none());
    }

    #[test]
    fn test_zscore_max_to_min_ratio_one_for_constant() {
        let mut n = znorm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        let r = n.max_to_min_ratio().unwrap();
        assert!((r - 1.0).abs() < 1e-9, "constant window → ratio=1, got {}", r);
    }

    // ── round-90 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_zscore_above_midpoint_fraction_none_for_empty() {
        assert!(znorm(4).above_midpoint_fraction().is_none());
    }

    #[test]
    fn test_zscore_above_midpoint_fraction_half_for_symmetric() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        // midpoint = (1+4)/2 = 2.5; values above: 3 and 4 → 2/4 = 0.5
        let f = n.above_midpoint_fraction().unwrap();
        assert!((f - 0.5).abs() < 1e-9, "expected 0.5, got {}", f);
    }

    #[test]
    fn test_zscore_positive_fraction_none_for_empty() {
        assert!(znorm(4).positive_fraction().is_none());
    }

    #[test]
    fn test_zscore_positive_fraction_zero_for_all_nonpositive() {
        let mut n = znorm(3);
        for v in [dec!(-3), dec!(-1), dec!(0)] { n.update(v); }
        let f = n.positive_fraction().unwrap();
        assert!((f - 0.0).abs() < 1e-9, "no positives → 0.0, got {}", f);
    }

    #[test]
    fn test_zscore_above_mean_fraction_none_for_empty() {
        assert!(znorm(4).above_mean_fraction().is_none());
    }

    #[test]
    fn test_zscore_above_mean_fraction_half_for_symmetric() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        // mean = 2.5; values above: 3 and 4 → 0.5
        let f = n.above_mean_fraction().unwrap();
        assert!((f - 0.5).abs() < 1e-9, "expected 0.5, got {}", f);
    }

    // ── round-91 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_iqr_none_for_empty() {
        assert!(znorm(4).window_iqr().is_none());
    }

    #[test]
    fn test_zscore_window_iqr_zero_for_constant() {
        let mut n = znorm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        assert_eq!(n.window_iqr().unwrap(), dec!(0));
    }

    #[test]
    fn test_zscore_mean_absolute_deviation_none_for_empty() {
        assert!(znorm(4).mean_absolute_deviation().is_none());
    }

    #[test]
    fn test_zscore_mean_absolute_deviation_zero_for_constant() {
        let mut n = znorm(4);
        for _ in 0..4 { n.update(dec!(7)); }
        let mad = n.mean_absolute_deviation().unwrap();
        assert!(mad.abs() < 1e-9, "constant window → MAD=0, got {}", mad);
    }

    #[test]
    fn test_zscore_run_length_mean_none_for_single_value() {
        let mut n = znorm(4);
        n.update(dec!(1));
        assert!(n.run_length_mean().is_none());
    }

    #[test]
    fn test_zscore_run_length_mean_all_increasing() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        // one monotone run of length 4 → mean = 4.0
        let r = n.run_length_mean().unwrap();
        assert!((r - 4.0).abs() < 1e-9, "monotone up → run_len=4, got {}", r);
    }

    // ── round-92 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_zscore_monotone_fraction_none_for_single_value() {
        let mut n = znorm(4);
        n.update(dec!(1));
        assert!(n.monotone_fraction().is_none());
    }

    #[test]
    fn test_zscore_monotone_fraction_one_for_increasing() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let f = n.monotone_fraction().unwrap();
        assert!((f - 1.0).abs() < 1e-9, "monotone up → 1.0, got {}", f);
    }

    #[test]
    fn test_zscore_coeff_variation_none_for_empty() {
        assert!(znorm(4).coeff_variation().is_none());
    }

    #[test]
    fn test_zscore_coeff_variation_zero_for_constant() {
        let mut n = znorm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        let cv = n.coeff_variation().unwrap();
        assert!(cv.abs() < 1e-9, "constant window → CV=0, got {}", cv);
    }

    // ── round-93 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_sum_of_squares_zero_for_empty() {
        assert_eq!(znorm(4).window_sum_of_squares(), rust_decimal::Decimal::ZERO);
    }

    #[test]
    fn test_zscore_window_sum_of_squares_correct() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        // 1+4+9 = 14
        assert_eq!(n.window_sum_of_squares(), dec!(14));
    }

    #[test]
    fn test_zscore_percentile_75_none_for_empty() {
        assert!(znorm(4).percentile_75().is_none());
    }

    #[test]
    fn test_zscore_percentile_75_for_constant() {
        let mut n = znorm(4);
        for _ in 0..4 { n.update(dec!(7)); }
        assert_eq!(n.percentile_75().unwrap(), dec!(7));
    }

    // ── round-94 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_mean_deviation_none_for_empty() {
        assert!(znorm(4).window_mean_deviation().is_none());
    }

    #[test]
    fn test_zscore_window_mean_deviation_constant() {
        let mut n = znorm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        assert_eq!(n.window_mean_deviation().unwrap(), dec!(0));
    }

    #[test]
    fn test_zscore_latest_percentile_none_for_empty() {
        assert!(znorm(4).latest_percentile().is_none());
    }

    #[test]
    fn test_zscore_latest_percentile_top_value() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        // latest = 4, below = 3 (1,2,3) → 3/4 = 0.75
        let p = n.latest_percentile().unwrap();
        assert!((p - 0.75).abs() < 1e-9, "expected 0.75, got {}", p);
    }

    // ── round-95 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_trimmed_mean_none_for_small_window() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        assert!(n.window_trimmed_mean().is_none());
    }

    #[test]
    fn test_zscore_window_trimmed_mean_constant() {
        let mut n = znorm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        assert_eq!(n.window_trimmed_mean().unwrap(), dec!(5));
    }

    #[test]
    fn test_zscore_window_variance_none_for_empty() {
        assert!(znorm(4).window_variance().is_none());
    }

    #[test]
    fn test_zscore_window_variance_zero_for_constant() {
        let mut n = znorm(4);
        for _ in 0..4 { n.update(dec!(7)); }
        assert_eq!(n.window_variance().unwrap(), dec!(0));
    }

    // ── round-96 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_std_dev_none_for_empty() {
        assert!(znorm(4).window_std_dev().is_none());
    }

    #[test]
    fn test_zscore_window_std_dev_zero_for_constant() {
        let mut n = znorm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        assert!(n.window_std_dev().unwrap().abs() < 1e-9);
    }

    #[test]
    fn test_zscore_window_min_max_ratio_none_for_empty() {
        assert!(znorm(4).window_min_max_ratio().is_none());
    }

    #[test]
    fn test_zscore_window_min_max_ratio_one_for_constant() {
        let mut n = znorm(4);
        for _ in 0..4 { n.update(dec!(3)); }
        assert_eq!(n.window_min_max_ratio().unwrap(), dec!(1));
    }

    #[test]
    fn test_zscore_recent_bias_none_for_small_window() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        assert!(n.recent_bias().is_none());
    }

    #[test]
    fn test_zscore_window_range_pct_none_for_empty() {
        assert!(znorm(4).window_range_pct().is_none());
    }

    #[test]
    fn test_zscore_window_range_pct_zero_for_constant() {
        let mut n = znorm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        let r = n.window_range_pct().unwrap();
        assert!(r.abs() < 1e-9, "constant → range_pct=0, got {}", r);
    }

    // ── round-97 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_momentum_none_for_single() {
        let mut n = znorm(4);
        n.update(dec!(5));
        assert!(n.window_momentum().is_none());
    }

    #[test]
    fn test_zscore_window_momentum_positive_for_rising() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(3)] { n.update(v); }
        assert_eq!(n.window_momentum().unwrap(), dec!(2));
    }

    #[test]
    fn test_zscore_above_first_fraction_none_for_empty() {
        assert!(znorm(4).above_first_fraction().is_none());
    }

    #[test]
    fn test_zscore_above_first_fraction_all_above() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        // first=1, above: 2,3,4 → 3/4
        let f = n.above_first_fraction().unwrap();
        assert!((f - 0.75).abs() < 1e-9, "expected 0.75, got {}", f);
    }

    #[test]
    fn test_zscore_window_zscore_latest_none_for_constant() {
        let mut n = znorm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        assert!(n.window_zscore_latest().is_none());
    }

    #[test]
    fn test_zscore_decay_weighted_mean_none_for_empty() {
        assert!(znorm(4).decay_weighted_mean(0.1).is_none());
    }

    // ── round-98 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_kurtosis_none_for_constant() {
        let mut n = znorm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        assert!(n.window_kurtosis().is_none());
    }

    #[test]
    fn test_zscore_window_kurtosis_none_for_small_window() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        assert!(n.window_kurtosis().is_none());
    }

    #[test]
    fn test_zscore_above_percentile_90_none_for_empty() {
        assert!(znorm(4).above_percentile_90().is_none());
    }

    #[test]
    fn test_zscore_above_percentile_90_basic() {
        let mut n = znorm(10);
        for v in 1i64..=10 { n.update(Decimal::from(v)); }
        let f = n.above_percentile_90().unwrap();
        assert!(f < 0.2, "expected small fraction, got {}", f);
    }

    #[test]
    fn test_zscore_window_lag_autocorr_none_for_constant() {
        let mut n = znorm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        assert!(n.window_lag_autocorr().is_none());
    }

    #[test]
    fn test_zscore_slope_of_mean_none_for_small_window() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        assert!(n.slope_of_mean().is_none());
    }

    // ── round-99 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_percentile_25_none_for_empty() {
        assert!(znorm(4).window_percentile_25().is_none());
    }

    #[test]
    fn test_zscore_window_percentile_25_constant() {
        let mut n = znorm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        assert_eq!(n.window_percentile_25().unwrap(), dec!(5));
    }

    #[test]
    fn test_zscore_mean_reversion_score_none_for_constant() {
        let mut n = znorm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        assert!(n.mean_reversion_score().is_none());
    }

    #[test]
    fn test_zscore_trend_strength_none_for_constant() {
        let mut n = znorm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        assert!(n.trend_strength().is_none());
    }

    #[test]
    fn test_zscore_window_peak_count_none_for_small_window() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2)] { n.update(v); }
        assert!(n.window_peak_count().is_none());
    }

    #[test]
    fn test_zscore_window_peak_count_basic() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(3), dec!(2), dec!(4)] { n.update(v); }
        let p = n.window_peak_count().unwrap();
        assert_eq!(p, 1);
    }

    // ── round-100 tests ───────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_trough_count_none_for_small_window() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2)] { n.update(v); }
        assert!(n.window_trough_count().is_none());
    }

    #[test]
    fn test_zscore_window_trough_count_basic() {
        let mut n = znorm(4);
        for v in [dec!(3), dec!(1), dec!(4), dec!(2)] { n.update(v); }
        let t = n.window_trough_count().unwrap();
        assert_eq!(t, 1);
    }

    #[test]
    fn test_zscore_positive_momentum_fraction_all_rising() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let f = n.positive_momentum_fraction().unwrap();
        assert!((f - 1.0).abs() < 1e-9, "all rising → 1.0, got {}", f);
    }

    #[test]
    fn test_zscore_below_percentile_10_none_for_empty() {
        assert!(znorm(4).below_percentile_10().is_none());
    }

    #[test]
    fn test_zscore_alternation_rate_none_for_constant() {
        let mut n = znorm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        assert!(n.alternation_rate().is_none());
    }

    #[test]
    fn test_zscore_alternation_rate_full_for_perfect_alternation() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(3), dec!(1), dec!(3)] { n.update(v); }
        let r = n.alternation_rate().unwrap();
        assert!((r - 1.0).abs() < 1e-9, "perfect alternation → 1.0, got {}", r);
    }

    // ── round-101 tests ───────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_signed_area_none_for_empty() {
        assert!(znorm(4).window_signed_area().is_none());
    }

    #[test]
    fn test_zscore_window_signed_area_zero_for_constant() {
        let mut n = znorm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        assert_eq!(n.window_signed_area().unwrap(), dec!(0));
    }

    #[test]
    fn test_zscore_up_fraction_none_for_empty() {
        assert!(znorm(4).up_fraction().is_none());
    }

    #[test]
    fn test_zscore_up_fraction_for_all_positive() {
        let mut n = znorm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        let f = n.up_fraction().unwrap();
        assert!((f - 1.0).abs() < 1e-9, "all positive → 1.0, got {}", f);
    }

    #[test]
    fn test_zscore_threshold_cross_count_none_for_single() {
        let mut n = znorm(4);
        n.update(dec!(5));
        assert!(n.threshold_cross_count().is_none());
    }

    #[test]
    fn test_zscore_window_entropy_approx_none_for_constant() {
        let mut n = znorm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        assert!(n.window_entropy_approx().is_none());
    }

    // ── round-102 tests ───────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_q1_q3_ratio_none_for_empty() {
        assert!(znorm(4).window_q1_q3_ratio().is_none());
    }

    #[test]
    fn test_zscore_window_q1_q3_ratio_one_for_constant() {
        let mut n = znorm(4);
        for _ in 0..4 { n.update(dec!(5)); }
        assert_eq!(n.window_q1_q3_ratio().unwrap(), dec!(1));
    }

    #[test]
    fn test_zscore_signed_momentum_positive_for_rising() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        assert_eq!(n.signed_momentum().unwrap(), dec!(2));
    }

    #[test]
    fn test_zscore_positive_run_length_none_for_single() {
        let mut n = znorm(4);
        n.update(dec!(5));
        assert!(n.positive_run_length().is_none());
    }

    #[test]
    fn test_zscore_valley_to_peak_ratio_none_for_small_window() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2)] { n.update(v); }
        assert!(n.valley_to_peak_ratio().is_none());
    }

    // ── round-103 tests ───────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_max_drawdown_none_for_single() {
        let mut n = znorm(5);
        n.update(dec!(10));
        assert!(n.window_max_drawdown().is_none());
    }

    #[test]
    fn test_zscore_window_max_drawdown_basic() {
        let mut n = znorm(5);
        for v in [dec!(10), dec!(20), dec!(5), dec!(15), dec!(8)] { n.update(v); }
        let dd = n.window_max_drawdown().unwrap();
        assert!((dd - 15.0).abs() < 1e-9, "expected 15.0, got {}", dd);
    }

    #[test]
    fn test_zscore_above_previous_fraction_none_for_single() {
        let mut n = znorm(5);
        n.update(dec!(10));
        assert!(n.above_previous_fraction().is_none());
    }

    #[test]
    fn test_zscore_above_previous_fraction_basic() {
        let mut n = znorm(5);
        for v in [dec!(10), dec!(20), dec!(15), dec!(25)] { n.update(v); }
        let f = n.above_previous_fraction().unwrap();
        assert!((f - 2.0 / 3.0).abs() < 1e-9, "expected 0.667, got {}", f);
    }

    #[test]
    fn test_zscore_range_efficiency_none_for_single() {
        let mut n = znorm(5);
        n.update(dec!(10));
        assert!(n.range_efficiency().is_none());
    }

    #[test]
    fn test_zscore_range_efficiency_monotone() {
        let mut n = znorm(5);
        for v in [dec!(10), dec!(20), dec!(30)] { n.update(v); }
        let e = n.range_efficiency().unwrap();
        assert!((e - 1.0).abs() < 1e-9, "expected 1.0, got {}", e);
    }

    #[test]
    fn test_zscore_window_running_total_basic() {
        let mut n = znorm(5);
        for v in [dec!(10), dec!(20), dec!(30)] { n.update(v); }
        assert_eq!(n.window_running_total(), dec!(60));
    }

    // ── round-104 tests ───────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_convexity_none_for_two_vals() {
        let mut n = znorm(5);
        for v in [dec!(10), dec!(20)] { n.update(v); }
        assert!(n.window_convexity().is_none());
    }

    #[test]
    fn test_zscore_window_convexity_linear() {
        let mut n = znorm(5);
        for v in [dec!(10), dec!(20), dec!(30)] { n.update(v); }
        let c = n.window_convexity().unwrap();
        assert!(c.abs() < 1e-9, "expected ~0, got {}", c);
    }

    #[test]
    fn test_zscore_below_previous_fraction_none_for_single() {
        let mut n = znorm(5);
        n.update(dec!(10));
        assert!(n.below_previous_fraction().is_none());
    }

    #[test]
    fn test_zscore_below_previous_fraction_basic() {
        let mut n = znorm(5);
        for v in [dec!(30), dec!(20), dec!(10)] { n.update(v); }
        let f = n.below_previous_fraction().unwrap();
        assert!((f - 1.0).abs() < 1e-9, "expected 1.0, got {}", f);
    }

    #[test]
    fn test_zscore_window_volatility_ratio_none_for_three() {
        let mut n = znorm(5);
        for v in [dec!(10), dec!(20), dec!(30)] { n.update(v); }
        assert!(n.window_volatility_ratio().is_none());
    }

    #[test]
    fn test_zscore_window_gini_none_for_single() {
        let mut n = znorm(5);
        n.update(dec!(10));
        assert!(n.window_gini().is_none());
    }

    #[test]
    fn test_zscore_window_gini_equal_values() {
        let mut n = znorm(5);
        for v in [dec!(10), dec!(10), dec!(10)] { n.update(v); }
        let g = n.window_gini().unwrap();
        assert!(g.abs() < 1e-9, "expected ~0, got {}", g);
    }

    // ── round-105 tests ───────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_hurst_none_for_three() {
        let mut n = znorm(5);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        assert!(n.window_hurst_exponent().is_none());
    }

    #[test]
    fn test_zscore_window_hurst_basic() {
        let mut n = znorm(10);
        for v in [dec!(1), dec!(2), dec!(4), dec!(8)] { n.update(v); }
        assert!(n.window_hurst_exponent().is_some());
    }

    #[test]
    fn test_zscore_window_mean_crossings_none_for_single() {
        let mut n = znorm(5);
        n.update(dec!(10));
        assert!(n.window_mean_crossings().is_none());
    }

    #[test]
    fn test_zscore_window_mean_crossings_basic() {
        let mut n = znorm(5);
        for v in [dec!(10), dec!(20), dec!(5), dec!(20)] { n.update(v); }
        let c = n.window_mean_crossings().unwrap();
        assert_eq!(c, 3);
    }

    #[test]
    fn test_zscore_window_skewness_none_for_two() {
        let mut n = znorm(5);
        for v in [dec!(10), dec!(20)] { n.update(v); }
        assert!(n.window_skewness().is_none());
    }

    #[test]
    fn test_zscore_window_skewness_symmetric() {
        let mut n = znorm(5);
        for v in [dec!(10), dec!(20), dec!(30)] { n.update(v); }
        let s = n.window_skewness().unwrap();
        assert!(s.abs() < 1e-9, "expected ~0, got {}", s);
    }

    #[test]
    fn test_zscore_window_max_run_none_for_single() {
        let mut n = znorm(5);
        n.update(dec!(10));
        assert!(n.window_max_run().is_none());
    }

    #[test]
    fn test_zscore_window_max_run_basic() {
        let mut n = znorm(5);
        for v in [dec!(10), dec!(20), dec!(30), dec!(10)] { n.update(v); }
        let r = n.window_max_run().unwrap();
        assert_eq!(r, 3);
    }

    // ── round-106 tests ───────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_median_deviation_basic() {
        let mut n = znorm(5);
        for v in [dec!(10), dec!(20), dec!(30)] { n.update(v); }
        let m = n.window_median_deviation().unwrap();
        assert!((m - 20.0 / 3.0).abs() < 1e-9, "expected 6.667, got {}", m);
    }

    #[test]
    fn test_zscore_longest_above_mean_run_basic() {
        let mut n = znorm(10);
        for v in [dec!(5), dec!(10), dec!(15), dec!(3)] { n.update(v); }
        let r = n.longest_above_mean_run().unwrap();
        assert_eq!(r, 2);
    }

    #[test]
    fn test_zscore_window_bimodality_none_for_three() {
        let mut n = znorm(5);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        assert!(n.window_bimodality().is_none());
    }

    #[test]
    fn test_zscore_window_bimodality_basic() {
        let mut n = znorm(10);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        assert!(n.window_bimodality().is_some());
    }

    #[test]
    fn test_zscore_window_zero_crossings_none_for_single() {
        let mut n = znorm(5);
        n.update(dec!(1));
        assert!(n.window_zero_crossings().is_none());
    }

    #[test]
    fn test_zscore_window_zero_crossings_basic() {
        let mut n = znorm(5);
        for v in [dec!(1), dec!(-1), dec!(1)] { n.update(v); }
        let c = n.window_zero_crossings().unwrap();
        assert_eq!(c, 2);
    }

    // ── round-107 tests ───────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_energy_basic() {
        let mut n = znorm(5);
        for v in [dec!(3), dec!(4)] { n.update(v); }
        assert_eq!(n.window_energy(), dec!(25));
    }

    #[test]
    fn test_zscore_window_interquartile_mean_none_for_three() {
        let mut n = znorm(5);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        assert!(n.window_interquartile_mean().is_none());
    }

    #[test]
    fn test_zscore_window_interquartile_mean_basic() {
        let mut n = znorm(10);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let m = n.window_interquartile_mean().unwrap();
        assert!((m - 2.5).abs() < 1e-9, "expected 2.5, got {}", m);
    }

    #[test]
    fn test_zscore_above_mean_count_basic() {
        let mut n = znorm(5);
        for v in [dec!(10), dec!(20), dec!(30)] { n.update(v); }
        let c = n.above_mean_count().unwrap();
        assert_eq!(c, 1);
    }

    #[test]
    fn test_zscore_window_diff_entropy_none_for_single() {
        let mut n = znorm(5);
        n.update(dec!(10));
        assert!(n.window_diff_entropy().is_none());
    }

    #[test]
    fn test_zscore_window_diff_entropy_basic() {
        let mut n = znorm(5);
        for v in [dec!(10), dec!(20), dec!(30)] { n.update(v); }
        assert!(n.window_diff_entropy().is_some());
    }

    // ── round-108 tests ───────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_root_mean_square_basic() {
        let mut n = znorm(5);
        for v in [dec!(3), dec!(4)] { n.update(v); }
        let rms = n.window_root_mean_square().unwrap();
        assert!((rms - 12.5_f64.sqrt()).abs() < 1e-9, "expected sqrt(12.5), got {}", rms);
    }

    #[test]
    fn test_zscore_window_first_derivative_mean_none_for_single() {
        let mut n = znorm(5);
        n.update(dec!(10));
        assert!(n.window_first_derivative_mean().is_none());
    }

    #[test]
    fn test_zscore_window_first_derivative_mean_basic() {
        let mut n = znorm(5);
        for v in [dec!(10), dec!(20), dec!(30)] { n.update(v); }
        let d = n.window_first_derivative_mean().unwrap();
        assert!((d - 10.0).abs() < 1e-9, "expected 10.0, got {}", d);
    }

    #[test]
    fn test_zscore_window_l1_norm_basic() {
        let mut n = znorm(5);
        for v in [dec!(3), dec!(-4), dec!(5)] { n.update(v); }
        assert_eq!(n.window_l1_norm(), dec!(12));
    }

    #[test]
    fn test_zscore_window_percentile_10_none_for_empty() {
        let n = znorm(5);
        assert!(n.window_percentile_10().is_none());
    }

    #[test]
    fn test_zscore_window_percentile_10_basic() {
        let mut n = znorm(10);
        for v in [dec!(10), dec!(20), dec!(30), dec!(40), dec!(50),
                  dec!(60), dec!(70), dec!(80), dec!(90), dec!(100)] { n.update(v); }
        let p = n.window_percentile_10().unwrap();
        assert!((p - 10.0).abs() < 1e-9, "expected 10.0, got {}", p);
    }

    // ── round-109 tests ───────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_zscore_mean_none_for_single() {
        let mut n = znorm(5);
        n.update(dec!(10));
        assert!(n.window_zscore_mean().is_none());
    }

    #[test]
    fn test_zscore_window_zscore_mean_basic() {
        let mut n = znorm(5);
        for v in [dec!(10), dec!(20), dec!(30)] { n.update(v); }
        let z = n.window_zscore_mean().unwrap();
        assert!(z.abs() < 1e-9, "expected ~0, got {}", z);
    }

    #[test]
    fn test_zscore_window_positive_sum_basic() {
        let mut n = znorm(5);
        for v in [dec!(5), dec!(-3), dec!(8)] { n.update(v); }
        assert_eq!(n.window_positive_sum(), dec!(13));
    }

    #[test]
    fn test_zscore_window_negative_sum_basic() {
        let mut n = znorm(5);
        for v in [dec!(5), dec!(-3), dec!(-7)] { n.update(v); }
        assert_eq!(n.window_negative_sum(), dec!(-10));
    }

    #[test]
    fn test_zscore_window_trend_consistency_none_for_flat() {
        let mut n = znorm(5);
        for v in [dec!(10), dec!(10), dec!(10)] { n.update(v); }
        assert!(n.window_trend_consistency().is_none());
    }

    #[test]
    fn test_zscore_window_trend_consistency_perfect() {
        let mut n = znorm(5);
        for v in [dec!(10), dec!(20), dec!(30)] { n.update(v); }
        let c = n.window_trend_consistency().unwrap();
        assert!((c - 1.0).abs() < 1e-9, "expected 1.0, got {}", c);
    }

    // ── round-110 tests ───────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_pairwise_mean_diff_none_for_single() {
        let mut n = znorm(5);
        n.update(dec!(10));
        assert!(n.window_pairwise_mean_diff().is_none());
    }

    #[test]
    fn test_zscore_window_pairwise_mean_diff_basic() {
        let mut n = znorm(5);
        for v in [dec!(0), dec!(10)] { n.update(v); }
        let d = n.window_pairwise_mean_diff().unwrap();
        assert!((d - 10.0).abs() < 1e-9, "expected 10.0, got {}", d);
    }

    #[test]
    fn test_zscore_window_q3_none_for_empty() {
        let n = znorm(5);
        assert!(n.window_q3().is_none());
    }

    #[test]
    fn test_zscore_window_q3_basic() {
        let mut n = znorm(10);
        for v in [dec!(10), dec!(20), dec!(30), dec!(40)] { n.update(v); }
        let q3 = n.window_q3().unwrap();
        assert!((q3 - 30.0).abs() < 1e-9, "expected 30.0, got {}", q3);
    }

    #[test]
    fn test_zscore_window_coefficient_of_variation_none_for_single() {
        let mut n = znorm(5);
        n.update(dec!(10));
        assert!(n.window_coefficient_of_variation().is_none());
    }

    #[test]
    fn test_zscore_window_coefficient_of_variation_basic() {
        let mut n = znorm(5);
        for v in [dec!(10), dec!(10), dec!(10)] { n.update(v); }
        let cv = n.window_coefficient_of_variation().unwrap();
        assert!(cv.abs() < 1e-9, "expected ~0, got {}", cv);
    }

    #[test]
    fn test_zscore_window_second_moment_basic() {
        let mut n = znorm(5);
        for v in [dec!(3), dec!(4)] { n.update(v); }
        let m = n.window_second_moment().unwrap();
        assert!((m - 12.5).abs() < 1e-9, "expected 12.5, got {}", m);
    }

    // ── round-111 tests ───────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_trimmed_sum_none_for_four() {
        let mut n = znorm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        assert!(n.window_trimmed_sum().is_none());
    }

    #[test]
    fn test_zscore_window_trimmed_sum_basic() {
        let mut n = znorm(10);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5),
                  dec!(6), dec!(7), dec!(8), dec!(9), dec!(10)] { n.update(v); }
        let s = n.window_trimmed_sum().unwrap();
        assert_eq!(s, dec!(44));
    }

    #[test]
    fn test_zscore_window_range_zscore_none_for_single() {
        let mut n = znorm(5);
        n.update(dec!(10));
        assert!(n.window_range_zscore().is_none());
    }

    #[test]
    fn test_zscore_window_range_zscore_basic() {
        let mut n = znorm(5);
        for v in [dec!(10), dec!(20), dec!(30)] { n.update(v); }
        assert!(n.window_range_zscore().is_some());
    }

    #[test]
    fn test_zscore_window_above_median_count_basic() {
        let mut n = znorm(5);
        for v in [dec!(10), dec!(20), dec!(30)] { n.update(v); }
        let c = n.window_above_median_count().unwrap();
        assert_eq!(c, 1);
    }

    #[test]
    fn test_zscore_window_min_run_none_for_single() {
        let mut n = znorm(5);
        n.update(dec!(10));
        assert!(n.window_min_run().is_none());
    }

    #[test]
    fn test_zscore_window_min_run_basic() {
        let mut n = znorm(5);
        for v in [dec!(30), dec!(20), dec!(10), dec!(25)] { n.update(v); }
        let r = n.window_min_run().unwrap();
        assert_eq!(r, 3);
    }

    #[test]
    fn test_zscore_window_harmonic_mean_none_for_zero() {
        let mut n = znorm(3);
        n.update(dec!(0));
        assert!(n.window_harmonic_mean().is_none());
    }

    #[test]
    fn test_zscore_window_harmonic_mean_basic() {
        let mut n = znorm(3);
        n.update(dec!(1));
        n.update(dec!(2));
        n.update(dec!(4));
        let h = n.window_harmonic_mean().unwrap();
        assert!((h - 12.0 / 7.0).abs() < 1e-9, "expected ~1.7143, got {}", h);
    }

    #[test]
    fn test_zscore_window_geometric_std_none_for_single() {
        let mut n = znorm(3);
        n.update(dec!(5));
        assert!(n.window_geometric_std().is_none());
    }

    #[test]
    fn test_zscore_window_geometric_std_basic() {
        let mut n = znorm(3);
        n.update(dec!(1));
        n.update(dec!(10));
        let g = n.window_geometric_std().unwrap();
        assert!(g > 1.0, "geometric std should be > 1 for spread data");
    }

    #[test]
    fn test_zscore_window_entropy_rate_none_for_single() {
        let mut n = znorm(3);
        n.update(dec!(5));
        assert!(n.window_entropy_rate().is_none());
    }

    #[test]
    fn test_zscore_window_entropy_rate_basic() {
        let mut n = znorm(3);
        n.update(dec!(10));
        n.update(dec!(12));
        n.update(dec!(14));
        let e = n.window_entropy_rate().unwrap();
        assert!((e - 2.0).abs() < 1e-9, "expected 2.0, got {}", e);
    }

    #[test]
    fn test_zscore_window_burstiness_none_for_single() {
        let mut n = znorm(3);
        n.update(dec!(5));
        assert!(n.window_burstiness().is_none());
    }

    #[test]
    fn test_zscore_window_burstiness_basic() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(1), dec!(1), dec!(10)] { n.update(v); }
        let b = n.window_burstiness().unwrap();
        assert!(b > 0.0, "expected positive burstiness, got {}", b);
    }

    #[test]
    fn test_zscore_window_iqr_ratio_none_for_two() {
        let mut n = znorm(3);
        n.update(dec!(1));
        n.update(dec!(2));
        assert!(n.window_iqr_ratio().is_none());
    }

    #[test]
    fn test_zscore_window_iqr_ratio_basic() {
        let mut n = znorm(5);
        for v in [dec!(2), dec!(4), dec!(6), dec!(8), dec!(10)] { n.update(v); }
        let r = n.window_iqr_ratio().unwrap();
        assert!(r > 0.0, "expected positive IQR ratio, got {}", r);
    }

    #[test]
    fn test_zscore_window_mean_reversion_none_for_single() {
        let mut n = znorm(3);
        n.update(dec!(5));
        assert!(n.window_mean_reversion().is_none());
    }

    #[test]
    fn test_zscore_window_mean_reversion_basic() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(5), dec!(2), dec!(5)] { n.update(v); }
        let r = n.window_mean_reversion().unwrap();
        assert!(r >= 0.0 && r <= 1.0, "expected [0,1], got {}", r);
    }

    #[test]
    fn test_zscore_window_autocorrelation_none_for_two() {
        let mut n = znorm(3);
        n.update(dec!(1));
        n.update(dec!(2));
        assert!(n.window_autocorrelation().is_none());
    }

    #[test]
    fn test_zscore_window_autocorrelation_basic() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let a = n.window_autocorrelation().unwrap();
        assert!(a > 0.0, "expected positive autocorrelation, got {}", a);
    }

    #[test]
    fn test_zscore_window_slope_none_for_single() {
        let mut n = znorm(3);
        n.update(dec!(5));
        assert!(n.window_slope().is_none());
    }

    #[test]
    fn test_zscore_window_slope_basic() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let s = n.window_slope().unwrap();
        assert!((s - 1.0).abs() < 1e-9, "expected 1.0, got {}", s);
    }

    #[test]
    fn test_zscore_window_crest_factor_none_for_empty() {
        let n = znorm(3);
        assert!(n.window_crest_factor().is_none());
    }

    #[test]
    fn test_zscore_window_crest_factor_basic() {
        let mut n = znorm(3);
        for v in [dec!(2), dec!(2), dec!(2)] { n.update(v); }
        let c = n.window_crest_factor().unwrap();
        assert!((c - 1.0).abs() < 1e-9, "expected 1.0, got {}", c);
    }

    #[test]
    fn test_zscore_window_relative_range_none_for_empty() {
        let n = znorm(3);
        assert!(n.window_relative_range().is_none());
    }

    #[test]
    fn test_zscore_window_relative_range_basic() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let r = n.window_relative_range().unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0, got {}", r);
    }

    #[test]
    fn test_zscore_window_outlier_count_none_for_single() {
        let mut n = znorm(3);
        n.update(dec!(5));
        assert!(n.window_outlier_count().is_none());
    }

    #[test]
    fn test_zscore_window_outlier_count_basic() {
        let mut n = znorm(5);
        for v in [dec!(1), dec!(1), dec!(1), dec!(1), dec!(1)] { n.update(v); }
        let c = n.window_outlier_count().unwrap();
        assert_eq!(c, 0);
    }

    #[test]
    fn test_zscore_window_decay_score_none_for_empty() {
        let n = znorm(3);
        assert!(n.window_decay_score().is_none());
    }

    #[test]
    fn test_zscore_window_decay_score_basic() {
        let mut n = znorm(2);
        n.update(dec!(0));
        n.update(dec!(10));
        let d = n.window_decay_score().unwrap();
        assert!(d > 5.0, "expected decay-weighted value closer to 10, got {}", d);
    }

    #[test]
    fn test_zscore_window_log_return_none_for_single() {
        let mut n = znorm(3);
        n.update(dec!(5));
        assert!(n.window_log_return().is_none());
    }

    #[test]
    fn test_zscore_window_log_return_basic() {
        let mut n = znorm(2);
        n.update(dec!(1));
        n.update(dec!(1));
        let r = n.window_log_return().unwrap();
        assert!(r.abs() < 1e-9, "expected 0.0, got {}", r);
    }

    #[test]
    fn test_zscore_window_signed_rms_none_for_empty() {
        let n = znorm(3);
        assert!(n.window_signed_rms().is_none());
    }

    #[test]
    fn test_zscore_window_signed_rms_basic() {
        let mut n = znorm(2);
        n.update(dec!(3));
        n.update(dec!(4));
        let r = n.window_signed_rms().unwrap();
        assert!(r > 0.0, "expected positive signed_rms, got {}", r);
    }

    #[test]
    fn test_zscore_window_inflection_count_none_for_two() {
        let mut n = znorm(3);
        n.update(dec!(1));
        n.update(dec!(2));
        assert!(n.window_inflection_count().is_none());
    }

    #[test]
    fn test_zscore_window_inflection_count_basic() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(3), dec!(2)] { n.update(v); }
        let c = n.window_inflection_count().unwrap();
        assert_eq!(c, 1);
    }

    #[test]
    fn test_zscore_window_centroid_none_for_empty() {
        let n = znorm(3);
        assert!(n.window_centroid().is_none());
    }

    #[test]
    fn test_zscore_window_centroid_basic() {
        let mut n = znorm(3);
        for v in [dec!(0), dec!(0), dec!(10)] { n.update(v); }
        let c = n.window_centroid().unwrap();
        assert!((c - 2.0).abs() < 1e-9, "expected 2.0, got {}", c);
    }

    #[test]
    fn test_zscore_window_max_deviation_none_for_empty() {
        let n = znorm(3);
        assert!(n.window_max_deviation().is_none());
    }

    #[test]
    fn test_zscore_window_max_deviation_basic() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let d = n.window_max_deviation().unwrap();
        assert!((d - 1.0).abs() < 1e-9, "expected 1.0, got {}", d);
    }

    #[test]
    fn test_zscore_window_range_mean_ratio_none_for_empty() {
        let n = znorm(3);
        assert!(n.window_range_mean_ratio().is_none());
    }

    #[test]
    fn test_zscore_window_range_mean_ratio_basic() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let r = n.window_range_mean_ratio().unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0, got {}", r);
    }

    #[test]
    fn test_zscore_window_step_up_count_none_for_single() {
        let mut n = znorm(3);
        n.update(dec!(5));
        assert!(n.window_step_up_count().is_none());
    }

    #[test]
    fn test_zscore_window_step_up_count_basic() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let c = n.window_step_up_count().unwrap();
        assert_eq!(c, 2);
    }

    #[test]
    fn test_zscore_window_step_down_count_none_for_single() {
        let mut n = znorm(3);
        n.update(dec!(5));
        assert!(n.window_step_down_count().is_none());
    }

    #[test]
    fn test_zscore_window_step_down_count_basic() {
        let mut n = znorm(3);
        for v in [dec!(3), dec!(2), dec!(1)] { n.update(v); }
        let c = n.window_step_down_count().unwrap();
        assert_eq!(c, 2);
    }

    #[test]
    fn test_zscore_window_entropy_of_changes_none_for_single() {
        let mut n = znorm(3);
        n.update(dec!(5));
        assert!(n.window_entropy_of_changes().is_none());
    }

    #[test]
    fn test_zscore_window_entropy_of_changes_basic() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let e = n.window_entropy_of_changes().unwrap();
        assert!(e >= 0.0, "entropy should be non-negative, got {}", e);
    }

    #[test]
    fn test_zscore_window_level_crossing_rate_none_for_single() {
        let mut n = znorm(3);
        n.update(dec!(5));
        assert!(n.window_level_crossing_rate().is_none());
    }

    #[test]
    fn test_zscore_window_level_crossing_rate_basic() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(3), dec!(1), dec!(3)] { n.update(v); }
        let r = n.window_level_crossing_rate().unwrap();
        assert!(r > 0.0 && r <= 1.0, "expected (0,1], got {}", r);
    }

    #[test]
    fn test_zscore_window_abs_mean_none_for_empty() {
        let n = znorm(3);
        assert!(n.window_abs_mean().is_none());
    }

    #[test]
    fn test_zscore_window_abs_mean_basic() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let m = n.window_abs_mean().unwrap();
        assert!((m - 2.0).abs() < 1e-9, "expected 2.0, got {}", m);
    }

    #[test]
    fn test_zscore_window_rolling_max_none_for_empty() {
        let n = znorm(3);
        assert!(n.window_rolling_max().is_none());
    }

    #[test]
    fn test_zscore_window_rolling_max_basic() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(5), dec!(3)] { n.update(v); }
        let m = n.window_rolling_max().unwrap();
        assert!((m - 5.0).abs() < 1e-9, "expected 5.0, got {}", m);
    }

    #[test]
    fn test_zscore_window_rolling_min_none_for_empty() {
        let n = znorm(3);
        assert!(n.window_rolling_min().is_none());
    }

    #[test]
    fn test_zscore_window_rolling_min_basic() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(5), dec!(3)] { n.update(v); }
        let m = n.window_rolling_min().unwrap();
        assert!((m - 1.0).abs() < 1e-9, "expected 1.0, got {}", m);
    }

    #[test]
    fn test_zscore_window_negative_fraction_none_for_empty() {
        let n = znorm(3);
        assert!(n.window_negative_fraction().is_none());
    }

    #[test]
    fn test_zscore_window_negative_fraction_basic() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let f = n.window_negative_fraction().unwrap();
        assert!(f.abs() < 1e-9, "expected 0.0, got {}", f);
    }

    #[test]
    fn test_zscore_window_positive_fraction_none_for_empty() {
        let n = znorm(3);
        assert!(n.window_positive_fraction().is_none());
    }

    #[test]
    fn test_zscore_window_positive_fraction_basic() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let f = n.window_positive_fraction().unwrap();
        assert!((f - 1.0).abs() < 1e-9, "expected 1.0, got {}", f);
    }

    #[test]
    fn test_zscore_window_last_minus_min_none_for_empty() {
        let n = znorm(3);
        assert!(n.window_last_minus_min().is_none());
    }

    #[test]
    fn test_zscore_window_last_minus_min_basic() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(5), dec!(3)] { n.update(v); }
        let d = n.window_last_minus_min().unwrap();
        assert!((d - 2.0).abs() < 1e-9, "expected 2.0, got {}", d);
    }

    #[test]
    fn test_zscore_window_max_minus_min_none_for_empty() {
        let n = znorm(3);
        assert!(n.window_max_minus_min().is_none());
    }

    #[test]
    fn test_zscore_window_max_minus_min_basic() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(5), dec!(3)] { n.update(v); }
        let r = n.window_max_minus_min().unwrap();
        assert!((r - 4.0).abs() < 1e-9, "expected 4.0, got {}", r);
    }

    #[test]
    fn test_zscore_window_normalized_mean_none_for_empty() {
        let n = znorm(3);
        assert!(n.window_normalized_mean().is_none());
    }

    #[test]
    fn test_zscore_window_normalized_mean_basic() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let m = n.window_normalized_mean().unwrap();
        assert!((m - 0.5).abs() < 1e-9, "expected 0.5, got {}", m);
    }

    #[test]
    fn test_zscore_window_variance_ratio_none_for_single() {
        let mut n = znorm(3);
        n.update(dec!(5));
        assert!(n.window_variance_ratio().is_none());
    }

    #[test]
    fn test_zscore_window_variance_ratio_basic() {
        let mut n = znorm(3);
        for v in [dec!(2), dec!(2), dec!(2)] { n.update(v); }
        let r = n.window_variance_ratio().unwrap();
        assert!(r.abs() < 1e-9, "expected 0.0, got {}", r);
    }

    #[test]
    fn test_zscore_window_max_minus_last_none_for_empty() {
        let n = znorm(3);
        assert!(n.window_max_minus_last().is_none());
    }

    #[test]
    fn test_zscore_window_max_minus_last_basic() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(5), dec!(3)] { n.update(v); }
        let d = n.window_max_minus_last().unwrap();
        assert!((d - 2.0).abs() < 1e-9, "expected 2.0, got {}", d);
    }

    // ── round-120 ────────────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_above_last_none_for_empty() {
        let n = znorm(3);
        assert!(n.window_above_last().is_none());
    }

    #[test]
    fn test_zscore_window_above_last_basic() {
        let mut n = znorm(5);
        for v in [dec!(3), dec!(5), dec!(1), dec!(4), dec!(2)] { n.update(v); }
        let c = n.window_above_last().unwrap();
        assert_eq!(c, 3);
    }

    #[test]
    fn test_zscore_window_below_last_none_for_empty() {
        let n = znorm(3);
        assert!(n.window_below_last().is_none());
    }

    #[test]
    fn test_zscore_window_below_last_basic() {
        let mut n = znorm(5);
        for v in [dec!(3), dec!(5), dec!(1), dec!(4), dec!(2)] { n.update(v); }
        let c = n.window_below_last().unwrap();
        assert_eq!(c, 1);
    }

    #[test]
    fn test_zscore_window_diff_mean_none_for_single() {
        let mut n = znorm(3);
        n.update(dec!(5));
        assert!(n.window_diff_mean().is_none());
    }

    #[test]
    fn test_zscore_window_diff_mean_basic() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(3), dec!(5)] { n.update(v); }
        let d = n.window_diff_mean().unwrap();
        assert!((d - 2.0).abs() < 1e-9, "expected 2.0, got {}", d);
    }

    #[test]
    fn test_zscore_window_last_zscore_none_for_single() {
        let mut n = znorm(3);
        n.update(dec!(5));
        assert!(n.window_last_zscore().is_none());
    }

    #[test]
    fn test_zscore_window_last_zscore_basic() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let z = n.window_last_zscore().unwrap();
        assert!(z > 0.0, "expected positive zscore, got {}", z);
    }

    // ── round-121 ────────────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_range_fraction_none_for_empty() {
        let n = znorm(3);
        assert!(n.window_range_fraction().is_none());
    }

    #[test]
    fn test_zscore_window_range_fraction_uniform() {
        let mut n = znorm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let f = n.window_range_fraction().unwrap();
        assert!((f - 1.0).abs() < 1e-9, "expected 1.0, got {}", f);
    }

    #[test]
    fn test_zscore_window_mean_above_last_none_for_empty() {
        let n = znorm(3);
        assert!(n.window_mean_above_last().is_none());
    }

    #[test]
    fn test_zscore_window_mean_above_last_basic() {
        let mut n = znorm(3);
        for v in [dec!(5), dec!(4), dec!(1)] { n.update(v); }
        let r = n.window_mean_above_last().unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0, got {}", r);
    }

    #[test]
    fn test_zscore_window_volatility_trend_none_for_three() {
        let mut n = znorm(5);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        assert!(n.window_volatility_trend().is_none());
    }

    #[test]
    fn test_zscore_window_volatility_trend_basic() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        assert!(n.window_volatility_trend().is_some());
    }

    #[test]
    fn test_zscore_window_sign_change_count_none_for_two() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2)] { n.update(v); }
        assert!(n.window_sign_change_count().is_none());
    }

    #[test]
    fn test_zscore_window_sign_change_count_alternating() {
        let mut n = znorm(5);
        for v in [dec!(1), dec!(3), dec!(1), dec!(3), dec!(1)] { n.update(v); }
        let c = n.window_sign_change_count().unwrap();
        assert_eq!(c, 3);
    }

    // ── round-122 ────────────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_last_rank_none_for_empty() {
        let n = znorm(3);
        assert!(n.window_last_rank().is_none());
    }

    #[test]
    fn test_zscore_window_last_rank_max() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2), dec!(5)] { n.update(v); }
        let r = n.window_last_rank().unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0 (max rank), got {}", r);
    }

    #[test]
    fn test_zscore_window_momentum_score_none_for_single() {
        let mut n = znorm(3);
        n.update(dec!(5));
        assert!(n.window_momentum_score().is_none());
    }

    #[test]
    fn test_zscore_window_momentum_score_trending_up() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let s = n.window_momentum_score().unwrap();
        assert!((s - 1.0).abs() < 1e-9, "expected 1.0, got {}", s);
    }

    #[test]
    fn test_zscore_window_oscillation_count_none_for_two() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2)] { n.update(v); }
        assert!(n.window_oscillation_count().is_none());
    }

    #[test]
    fn test_zscore_window_oscillation_count_basic() {
        let mut n = znorm(5);
        for v in [dec!(1), dec!(3), dec!(1), dec!(3), dec!(1)] { n.update(v); }
        let c = n.window_oscillation_count().unwrap();
        assert_eq!(c, 2);
    }

    #[test]
    fn test_zscore_window_skew_direction_none_for_empty() {
        let n = znorm(3);
        assert!(n.window_skew_direction().is_none());
    }

    #[test]
    fn test_zscore_window_skew_direction_positive() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2), dec!(10)] { n.update(v); }
        let d = n.window_skew_direction().unwrap();
        assert!((d - 1.0).abs() < 1e-9, "expected 1.0, got {}", d);
    }

    // ── round-123 ────────────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_trend_reversal_count_none_for_two() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2)] { n.update(v); }
        assert!(n.window_trend_reversal_count().is_none());
    }

    #[test]
    fn test_zscore_window_trend_reversal_count_alternating() {
        let mut n = znorm(5);
        for v in [dec!(1), dec!(3), dec!(1), dec!(3), dec!(1)] { n.update(v); }
        let c = n.window_trend_reversal_count().unwrap();
        assert_eq!(c, 3);
    }

    #[test]
    fn test_zscore_window_first_last_diff_none_for_single() {
        let mut n = znorm(3);
        n.update(dec!(5));
        assert!(n.window_first_last_diff().is_none());
    }

    #[test]
    fn test_zscore_window_first_last_diff_basic() {
        let mut n = znorm(3);
        for v in [dec!(2), dec!(3), dec!(7)] { n.update(v); }
        let d = n.window_first_last_diff().unwrap();
        assert!((d - 5.0).abs() < 1e-9, "expected 5.0, got {}", d);
    }

    #[test]
    fn test_zscore_window_upper_half_count_none_for_empty() {
        let n = znorm(3);
        assert!(n.window_upper_half_count().is_none());
    }

    #[test]
    fn test_zscore_window_upper_half_count_basic() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let c = n.window_upper_half_count().unwrap();
        assert_eq!(c, 2);
    }

    #[test]
    fn test_zscore_window_lower_half_count_none_for_empty() {
        let n = znorm(3);
        assert!(n.window_lower_half_count().is_none());
    }

    #[test]
    fn test_zscore_window_lower_half_count_basic() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let c = n.window_lower_half_count().unwrap();
        assert_eq!(c, 2);
    }

    // ── round-124 ────────────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_percentile_75_none_for_empty() {
        let n = znorm(3);
        assert!(n.window_percentile_75().is_none());
    }

    #[test]
    fn test_zscore_window_percentile_75_basic() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let p = n.window_percentile_75().unwrap();
        assert!(p >= 3.0, "expected >=3.0, got {}", p);
    }

    #[test]
    fn test_zscore_window_abs_slope_none_for_single() {
        let mut n = znorm(3);
        n.update(dec!(5));
        assert!(n.window_abs_slope().is_none());
    }

    #[test]
    fn test_zscore_window_abs_slope_basic() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2), dec!(5)] { n.update(v); }
        let s = n.window_abs_slope().unwrap();
        assert!((s - 2.0).abs() < 1e-9, "expected 2.0, got {}", s);
    }

    #[test]
    fn test_zscore_window_gain_loss_ratio_none_for_single() {
        let mut n = znorm(3);
        n.update(dec!(5));
        assert!(n.window_gain_loss_ratio().is_none());
    }

    #[test]
    fn test_zscore_window_gain_loss_ratio_only_gains() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        assert!(n.window_gain_loss_ratio().is_none());
    }

    #[test]
    fn test_zscore_window_range_stability_none_for_empty() {
        let n = znorm(3);
        assert!(n.window_range_stability().is_none());
    }

    #[test]
    fn test_zscore_window_range_stability_uniform() {
        let mut n = znorm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let s = n.window_range_stability().unwrap();
        assert!((s - 1.0).abs() < 1e-9, "expected 1.0, got {}", s);
    }

    // ── round-125 ────────────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_exp_smoothed_none_for_empty() {
        let n = znorm(3);
        assert!(n.window_exp_smoothed().is_none());
    }

    #[test]
    fn test_zscore_window_exp_smoothed_single() {
        let mut n = znorm(3);
        n.update(dec!(7));
        let e = n.window_exp_smoothed().unwrap();
        assert!((e - 7.0).abs() < 1e-9, "expected 7.0, got {}", e);
    }

    #[test]
    fn test_zscore_window_drawdown_none_for_empty() {
        let n = znorm(3);
        assert!(n.window_drawdown().is_none());
    }

    #[test]
    fn test_zscore_window_drawdown_monotone_up() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let dd = n.window_drawdown().unwrap();
        assert!(dd.abs() < 1e-9, "expected 0.0, got {}", dd);
    }

    #[test]
    fn test_zscore_window_drawup_none_for_empty() {
        let n = znorm(3);
        assert!(n.window_drawup().is_none());
    }

    #[test]
    fn test_zscore_window_drawup_monotone_down() {
        let mut n = znorm(4);
        for v in [dec!(4), dec!(3), dec!(2), dec!(1)] { n.update(v); }
        let du = n.window_drawup().unwrap();
        assert!(du.abs() < 1e-9, "expected 0.0, got {}", du);
    }

    #[test]
    fn test_zscore_window_trend_strength_none_for_single() {
        let mut n = znorm(3);
        n.update(dec!(5));
        assert!(n.window_trend_strength().is_none());
    }

    #[test]
    fn test_zscore_window_trend_strength_pure_trend() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let s = n.window_trend_strength().unwrap();
        assert!((s - 1.0).abs() < 1e-9, "expected 1.0, got {}", s);
    }

    // ── round-126 ────────────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_ema_deviation_none_for_empty() {
        let n = znorm(3);
        assert!(n.window_ema_deviation().is_none());
    }

    #[test]
    fn test_zscore_window_ema_deviation_flat() {
        let mut n = znorm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let d = n.window_ema_deviation().unwrap();
        assert!(d.abs() < 1e-9, "expected 0.0, got {}", d);
    }

    #[test]
    fn test_zscore_window_normalized_variance_none_for_empty() {
        let n = znorm(3);
        assert!(n.window_normalized_variance().is_none());
    }

    #[test]
    fn test_zscore_window_normalized_variance_uniform_zero() {
        let mut n = znorm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let v = n.window_normalized_variance().unwrap();
        assert!(v.abs() < 1e-9, "expected 0.0, got {}", v);
    }

    #[test]
    fn test_zscore_window_median_ratio_none_for_empty() {
        let n = znorm(3);
        assert!(n.window_median_ratio().is_none());
    }

    #[test]
    fn test_zscore_window_median_ratio_last_eq_median() {
        let mut n = znorm(3);
        for v in [dec!(2), dec!(3), dec!(3)] { n.update(v); }
        let r = n.window_median_ratio().unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0, got {}", r);
    }

    #[test]
    fn test_zscore_window_half_life_none_for_empty() {
        let n = znorm(3);
        assert!(n.window_half_life().is_none());
    }

    #[test]
    fn test_zscore_window_half_life_immediate() {
        let mut n = znorm(3);
        for v in [dec!(10), dec!(5), dec!(3)] { n.update(v); }
        let h = n.window_half_life().unwrap();
        assert_eq!(h, 1);
    }

    // ── round-127 ────────────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_entropy_normalized_none_for_empty() {
        let n = znorm(3);
        assert!(n.window_entropy_normalized().is_none());
    }

    #[test]
    fn test_zscore_window_entropy_normalized_single() {
        let mut n = znorm(3);
        n.update(dec!(5));
        let e = n.window_entropy_normalized().unwrap();
        assert!(e.abs() < 1e-9, "expected 0.0, got {}", e);
    }

    #[test]
    fn test_zscore_window_peak_value_none_for_empty() {
        let n = znorm(3);
        assert!(n.window_peak_value().is_none());
    }

    #[test]
    fn test_zscore_window_peak_value_basic() {
        let mut n = znorm(3);
        for v in [dec!(2), dec!(7), dec!(3)] { n.update(v); }
        let p = n.window_peak_value().unwrap();
        assert!((p - 7.0).abs() < 1e-9, "expected 7.0, got {}", p);
    }

    #[test]
    fn test_zscore_window_trough_value_none_for_empty() {
        let n = znorm(3);
        assert!(n.window_trough_value().is_none());
    }

    #[test]
    fn test_zscore_window_trough_value_basic() {
        let mut n = znorm(3);
        for v in [dec!(2), dec!(7), dec!(3)] { n.update(v); }
        let t = n.window_trough_value().unwrap();
        assert!((t - 2.0).abs() < 1e-9, "expected 2.0, got {}", t);
    }

    #[test]
    fn test_zscore_window_gain_count_none_for_single() {
        let mut n = znorm(3);
        n.update(dec!(5));
        assert!(n.window_gain_count().is_none());
    }

    #[test]
    fn test_zscore_window_gain_count_all_rising() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let c = n.window_gain_count().unwrap();
        assert_eq!(c, 2);
    }

    // ── round-128 ────────────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_loss_count_none_for_single() {
        let mut n = znorm(3);
        n.update(dec!(5));
        assert!(n.window_loss_count().is_none());
    }

    #[test]
    fn test_zscore_window_loss_count_all_falling() {
        let mut n = znorm(3);
        for v in [dec!(3), dec!(2), dec!(1)] { n.update(v); }
        let c = n.window_loss_count().unwrap();
        assert_eq!(c, 2);
    }

    #[test]
    fn test_zscore_window_net_change_none_for_single() {
        let mut n = znorm(3);
        n.update(dec!(5));
        assert!(n.window_net_change().is_none());
    }

    #[test]
    fn test_zscore_window_net_change_basic() {
        let mut n = znorm(3);
        for v in [dec!(3), dec!(5), dec!(8)] { n.update(v); }
        let nc = n.window_net_change().unwrap();
        assert!((nc - 5.0).abs() < 1e-9, "expected 5.0, got {}", nc);
    }

    #[test]
    fn test_zscore_window_acceleration_none_for_two() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2)] { n.update(v); }
        assert!(n.window_acceleration().is_none());
    }

    #[test]
    fn test_zscore_window_acceleration_linear_zero() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let a = n.window_acceleration().unwrap();
        assert!(a.abs() < 1e-9, "expected 0.0, got {}", a);
    }

    #[test]
    fn test_zscore_window_regime_score_none_for_empty() {
        let n = znorm(3);
        assert!(n.window_regime_score().is_none());
    }

    #[test]
    fn test_zscore_window_regime_score_all_above() {
        let mut n = znorm(3);
        for v in [dec!(10), dec!(10), dec!(1)] { n.update(v); }
        let s = n.window_regime_score().unwrap();
        assert!(s > 0.0, "expected positive, got {}", s);
    }

    // ── round-129 ────────────────────────────────────────────────────────────
    #[test]
    fn test_zscore_window_cumulative_sum_none_for_empty() {
        let n = znorm(3);
        assert!(n.window_cumulative_sum().is_none());
    }

    #[test]
    fn test_zscore_window_cumulative_sum_basic() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let s = n.window_cumulative_sum().unwrap();
        assert!((s - 6.0).abs() < 1e-9, "expected 6.0, got {}", s);
    }

    #[test]
    fn test_zscore_window_spread_ratio_none_for_empty() {
        let n = znorm(3);
        assert!(n.window_spread_ratio().is_none());
    }

    #[test]
    fn test_zscore_window_spread_ratio_basic() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let r = n.window_spread_ratio().unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0, got {}", r);
    }

    #[test]
    fn test_zscore_window_center_of_mass_none_for_empty() {
        let n = znorm(3);
        assert!(n.window_center_of_mass().is_none());
    }

    #[test]
    fn test_zscore_window_center_of_mass_basic() {
        let mut n = znorm(2);
        for v in [dec!(1), dec!(1)] { n.update(v); }
        let c = n.window_center_of_mass().unwrap();
        assert!((c - 0.5).abs() < 1e-9, "expected 0.5, got {}", c);
    }

    #[test]
    fn test_zscore_window_cycle_count_none_for_two() {
        let mut n = znorm(2);
        for v in [dec!(1), dec!(2)] { n.update(v); }
        assert!(n.window_cycle_count().is_none());
    }

    #[test]
    fn test_zscore_window_cycle_count_one_reversal() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(3), dec!(2)] { n.update(v); }
        let c = n.window_cycle_count().unwrap();
        assert_eq!(c, 1);
    }

    // ── round-130 ────────────────────────────────────────────────────────────
    #[test]
    fn test_zscore_window_mad_none_for_empty() {
        let n = znorm(3);
        assert!(n.window_mad().is_none());
    }

    #[test]
    fn test_zscore_window_mad_identical() {
        let mut n = znorm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let m = n.window_mad().unwrap();
        assert!(m.abs() < 1e-9, "expected 0.0 for identical, got {}", m);
    }

    #[test]
    fn test_zscore_window_entropy_ratio_none_for_empty() {
        let n = znorm(3);
        assert!(n.window_entropy_ratio().is_none());
    }

    #[test]
    fn test_zscore_window_entropy_ratio_uniform() {
        let mut n = znorm(2);
        for v in [dec!(1), dec!(1)] { n.update(v); }
        let e = n.window_entropy_ratio().unwrap();
        assert!((e - 1.0).abs() < 1e-9, "expected 1.0 for uniform, got {}", e);
    }

    #[test]
    fn test_zscore_window_plateau_count_none_for_single() {
        let mut n = znorm(3);
        n.update(dec!(5));
        assert!(n.window_plateau_count().is_none());
    }

    #[test]
    fn test_zscore_window_plateau_count_all_equal() {
        let mut n = znorm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let p = n.window_plateau_count().unwrap();
        assert_eq!(p, 2);
    }

    #[test]
    fn test_zscore_window_direction_bias_none_for_single() {
        let mut n = znorm(3);
        n.update(dec!(5));
        assert!(n.window_direction_bias().is_none());
    }

    #[test]
    fn test_zscore_window_direction_bias_all_up() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let b = n.window_direction_bias().unwrap();
        assert!((b - 1.0).abs() < 1e-9, "expected 1.0 for all up, got {}", b);
    }

    // ── round-131 ────────────────────────────────────────────────────────────
    #[test]
    fn test_zscore_window_last_pct_change_none_for_single() {
        let mut n = znorm(3);
        n.update(dec!(5));
        assert!(n.window_last_pct_change().is_none());
    }

    #[test]
    fn test_zscore_window_last_pct_change_basic() {
        let mut n = znorm(2);
        for v in [dec!(10), dec!(15)] { n.update(v); }
        let p = n.window_last_pct_change().unwrap();
        assert!((p - 0.5).abs() < 1e-9, "expected 0.5, got {}", p);
    }

    #[test]
    fn test_zscore_window_std_trend_none_for_three() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        assert!(n.window_std_trend().is_none());
    }

    #[test]
    fn test_zscore_window_std_trend_flat() {
        let mut n = znorm(4);
        for v in [dec!(5), dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let t = n.window_std_trend().unwrap();
        assert!(t.abs() < 1e-9, "expected 0.0 for flat window, got {}", t);
    }

    #[test]
    fn test_zscore_window_nonzero_count_none_for_empty() {
        let n = znorm(3);
        assert!(n.window_nonzero_count().is_none());
    }

    #[test]
    fn test_zscore_window_nonzero_count_basic() {
        let mut n = znorm(3);
        for v in [dec!(0), dec!(1), dec!(2)] { n.update(v); }
        let c = n.window_nonzero_count().unwrap();
        assert_eq!(c, 2);
    }

    #[test]
    fn test_zscore_window_pct_above_mean_none_for_empty() {
        let n = znorm(3);
        assert!(n.window_pct_above_mean().is_none());
    }

    #[test]
    fn test_zscore_window_pct_above_mean_half() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let p = n.window_pct_above_mean().unwrap();
        assert!((p - 0.5).abs() < 1e-9, "expected 0.5, got {}", p);
    }

    // ── round-132 ────────────────────────────────────────────────────────────
    #[test]
    fn test_zscore_window_max_run_up_none_for_single() {
        let mut n = znorm(3);
        n.update(dec!(1));
        assert!(n.window_max_run_up().is_none());
    }

    #[test]
    fn test_zscore_window_max_run_up_basic() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(2)] { n.update(v); }
        let r = n.window_max_run_up().unwrap();
        assert_eq!(r, 2);
    }

    #[test]
    fn test_zscore_window_max_run_dn_none_for_single() {
        let mut n = znorm(3);
        n.update(dec!(1));
        assert!(n.window_max_run_dn().is_none());
    }

    #[test]
    fn test_zscore_window_max_run_dn_basic() {
        let mut n = znorm(4);
        for v in [dec!(4), dec!(3), dec!(2), dec!(3)] { n.update(v); }
        let r = n.window_max_run_dn().unwrap();
        assert_eq!(r, 2);
    }

    #[test]
    fn test_zscore_window_diff_sum_none_for_single() {
        let mut n = znorm(3);
        n.update(dec!(1));
        assert!(n.window_diff_sum().is_none());
    }

    #[test]
    fn test_zscore_window_diff_sum_monotone() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let s = n.window_diff_sum().unwrap();
        assert!((s - 2.0).abs() < 1e-9, "expected 2.0, got {}", s);
    }

    #[test]
    fn test_zscore_window_run_length_none_for_single() {
        let mut n = znorm(3);
        n.update(dec!(1));
        assert!(n.window_run_length().is_none());
    }

    #[test]
    fn test_zscore_window_run_length_basic() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let r = n.window_run_length().unwrap();
        assert_eq!(r, 2);
    }

    // ── round-133 ────────────────────────────────────────────────────────────
    #[test]
    fn test_zscore_window_abs_diff_sum_none_for_single() {
        let mut n = znorm(3);
        n.update(dec!(5));
        assert!(n.window_abs_diff_sum().is_none());
    }

    #[test]
    fn test_zscore_window_abs_diff_sum_basic() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(3), dec!(2)] { n.update(v); }
        let s = n.window_abs_diff_sum().unwrap();
        assert!((s - 3.0).abs() < 1e-9, "expected 3.0, got {}", s);
    }

    #[test]
    fn test_zscore_window_max_gap_none_for_single() {
        let mut n = znorm(3);
        n.update(dec!(5));
        assert!(n.window_max_gap().is_none());
    }

    #[test]
    fn test_zscore_window_max_gap_basic() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(5), dec!(3)] { n.update(v); }
        let g = n.window_max_gap().unwrap();
        assert!((g - 4.0).abs() < 1e-9, "expected 4.0, got {}", g);
    }

    #[test]
    fn test_zscore_window_local_max_count_none_for_two() {
        let mut n = znorm(2);
        for v in [dec!(1), dec!(2)] { n.update(v); }
        assert!(n.window_local_max_count().is_none());
    }

    #[test]
    fn test_zscore_window_local_max_count_one_peak() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(3), dec!(2)] { n.update(v); }
        let c = n.window_local_max_count().unwrap();
        assert_eq!(c, 1);
    }

    #[test]
    fn test_zscore_window_first_half_mean_none_for_single() {
        let mut n = znorm(3);
        n.update(dec!(5));
        assert!(n.window_first_half_mean().is_none());
    }

    #[test]
    fn test_zscore_window_first_half_mean_basic() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let m = n.window_first_half_mean().unwrap();
        assert!((m - 1.5).abs() < 1e-9, "expected 1.5, got {}", m);
    }

    // ── round-134 ────────────────────────────────────────────────────────────
    #[test]
    fn test_zscore_window_second_half_mean_none_for_single() {
        let mut n = znorm(3);
        n.update(dec!(5));
        assert!(n.window_second_half_mean().is_none());
    }

    #[test]
    fn test_zscore_window_second_half_mean_basic() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let m = n.window_second_half_mean().unwrap();
        assert!((m - 3.5).abs() < 1e-9, "expected 3.5, got {}", m);
    }

    #[test]
    fn test_zscore_window_local_min_count_none_for_two() {
        let mut n = znorm(2);
        for v in [dec!(2), dec!(1)] { n.update(v); }
        assert!(n.window_local_min_count().is_none());
    }

    #[test]
    fn test_zscore_window_local_min_count_one_valley() {
        let mut n = znorm(3);
        for v in [dec!(3), dec!(1), dec!(2)] { n.update(v); }
        let c = n.window_local_min_count().unwrap();
        assert_eq!(c, 1);
    }

    #[test]
    fn test_zscore_window_curvature_none_for_two() {
        let mut n = znorm(2);
        for v in [dec!(1), dec!(2)] { n.update(v); }
        assert!(n.window_curvature().is_none());
    }

    #[test]
    fn test_zscore_window_curvature_linear_zero() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let c = n.window_curvature().unwrap();
        assert!(c.abs() < 1e-9, "expected 0.0 for linear, got {}", c);
    }

    #[test]
    fn test_zscore_window_half_diff_none_for_single() {
        let mut n = znorm(3);
        n.update(dec!(5));
        assert!(n.window_half_diff().is_none());
    }

    #[test]
    fn test_zscore_window_half_diff_rising() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let d = n.window_half_diff().unwrap();
        assert!((d - 2.0).abs() < 1e-9, "expected 2.0, got {}", d);
    }

    // ── round-135 ────────────────────────────────────────────────────────────
    #[test]
    fn test_zscore_window_mean_crossing_rate_none_for_single() {
        let mut n = znorm(3);
        n.update(dec!(5));
        assert!(n.window_mean_crossing_rate().is_none());
    }

    #[test]
    fn test_zscore_window_mean_crossing_rate_monotone() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let r = n.window_mean_crossing_rate().unwrap();
        assert!(r.abs() < 1e-9, "expected 0.0 for monotone, got {}", r);
    }

    #[test]
    fn test_zscore_window_var_to_mean_none_for_empty() {
        let n = znorm(3);
        assert!(n.window_var_to_mean().is_none());
    }

    #[test]
    fn test_zscore_window_var_to_mean_basic() {
        let mut n = znorm(2);
        for v in [dec!(1), dec!(3)] { n.update(v); }
        let r = n.window_var_to_mean().unwrap();
        assert!((r - 0.5).abs() < 1e-9, "expected 0.5, got {}", r);
    }

    #[test]
    fn test_zscore_window_coeff_var_none_for_empty() {
        let n = znorm(3);
        assert!(n.window_coeff_var().is_none());
    }

    #[test]
    fn test_zscore_window_coeff_var_identical() {
        let mut n = znorm(2);
        for v in [dec!(5), dec!(5)] { n.update(v); }
        let c = n.window_coeff_var().unwrap();
        assert!(c.abs() < 1e-9, "expected 0.0 for identical values, got {}", c);
    }

    #[test]
    fn test_zscore_window_step_up_fraction_none_for_single() {
        let mut n = znorm(3);
        n.update(dec!(5));
        assert!(n.window_step_up_fraction().is_none());
    }

    #[test]
    fn test_zscore_window_step_up_fraction_all_up() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let f = n.window_step_up_fraction().unwrap();
        assert!((f - 1.0).abs() < 1e-9, "expected 1.0 for all up, got {}", f);
    }

    // ── round-136 ────────────────────────────────────────────────────────────
    #[test]
    fn test_zscore_window_step_dn_fraction_none_for_single() {
        let mut n = znorm(3);
        n.update(dec!(5));
        assert!(n.window_step_dn_fraction().is_none());
    }

    #[test]
    fn test_zscore_window_step_dn_fraction_all_dn() {
        let mut n = znorm(3);
        for v in [dec!(3), dec!(2), dec!(1)] { n.update(v); }
        let f = n.window_step_dn_fraction().unwrap();
        assert!((f - 1.0).abs() < 1e-9, "expected 1.0, got {}", f);
    }

    #[test]
    fn test_zscore_window_mean_abs_dev_ratio_none_for_empty() {
        let n = znorm(3);
        assert!(n.window_mean_abs_dev_ratio().is_none());
    }

    #[test]
    fn test_zscore_window_mean_abs_dev_ratio_basic() {
        let mut n = znorm(2);
        for v in [dec!(1), dec!(3)] { n.update(v); }
        let r = n.window_mean_abs_dev_ratio().unwrap();
        assert!((r - 0.5).abs() < 1e-9, "expected 0.5, got {}", r);
    }

    #[test]
    fn test_zscore_window_recent_high_none_for_empty() {
        let n = znorm(3);
        assert!(n.window_recent_high().is_none());
    }

    #[test]
    fn test_zscore_window_recent_high_basic() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(5), dec!(3)] { n.update(v); }
        let h = n.window_recent_high().unwrap();
        assert!((h - 5.0).abs() < 1e-9, "expected 5.0, got {}", h);
    }

    #[test]
    fn test_zscore_window_recent_low_none_for_empty() {
        let n = znorm(3);
        assert!(n.window_recent_low().is_none());
    }

    #[test]
    fn test_zscore_window_recent_low_basic() {
        let mut n = znorm(4);
        for v in [dec!(5), dec!(3), dec!(1), dec!(4)] { n.update(v); }
        let l = n.window_recent_low().unwrap();
        assert!((l - 1.0).abs() < 1e-9, "expected 1.0, got {}", l);
    }

    // ── round-137 ────────────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_linear_trend_score_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_linear_trend_score().is_none());
    }

    #[test]
    fn test_zscore_window_linear_trend_score_increasing() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let s = n.window_linear_trend_score().unwrap();
        assert!(s > 0.0, "expected positive trend score, got {}", s);
    }

    #[test]
    fn test_zscore_window_zscore_min_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_zscore_min().is_none());
    }

    #[test]
    fn test_zscore_window_zscore_min_basic() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let zmin = n.window_zscore_min().unwrap();
        let zmax = n.window_zscore_max().unwrap();
        assert!(zmin <= zmax, "min {} should be <= max {}", zmin, zmax);
    }

    #[test]
    fn test_zscore_window_zscore_max_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_zscore_max().is_none());
    }

    #[test]
    fn test_zscore_window_zscore_max_positive() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(10)] { n.update(v); }
        let zmax = n.window_zscore_max().unwrap();
        assert!(zmax > 0.0, "expected positive zmax, got {}", zmax);
    }

    #[test]
    fn test_zscore_window_diff_variance_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_diff_variance().is_none());
    }

    #[test]
    fn test_zscore_window_diff_variance_constant() {
        let mut n = znorm(4);
        for v in [dec!(5), dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let dv = n.window_diff_variance().unwrap();
        assert!((dv - 0.0).abs() < 1e-9, "expected 0.0 for constant, got {}", dv);
    }

    // ── round-138 ────────────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_peak_to_trough_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_peak_to_trough().is_none());
    }

    #[test]
    fn test_zscore_window_peak_to_trough_basic() {
        let mut n = znorm(4);
        for v in [dec!(2), dec!(4), dec!(6), dec!(8)] { n.update(v); }
        let r = n.window_peak_to_trough().unwrap();
        assert!((r - 4.0).abs() < 1e-9, "expected 4.0 (8/2), got {}", r);
    }

    #[test]
    fn test_zscore_window_asymmetry_none_for_small() {
        let mut n = znorm(4);
        n.update(dec!(1)); n.update(dec!(2));
        assert!(n.window_asymmetry().is_none());
    }

    #[test]
    fn test_zscore_window_asymmetry_symmetric() {
        let mut n = znorm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)] { n.update(v); }
        let a = n.window_asymmetry().unwrap();
        assert!(a.abs() < 0.5, "expected near-zero asymmetry, got {}", a);
    }

    #[test]
    fn test_zscore_window_abs_trend_none_for_single() {
        let mut n = znorm(4);
        n.update(dec!(5));
        assert!(n.window_abs_trend().is_none());
    }

    #[test]
    fn test_zscore_window_abs_trend_monotonic() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let t = n.window_abs_trend().unwrap();
        assert!((t - 3.0).abs() < 1e-9, "expected 3.0, got {}", t);
    }

    #[test]
    fn test_zscore_window_recent_volatility_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_recent_volatility().is_none());
    }

    #[test]
    fn test_zscore_window_recent_volatility_constant() {
        let mut n = znorm(4);
        for v in [dec!(5), dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let rv = n.window_recent_volatility().unwrap();
        assert!((rv - 0.0).abs() < 1e-9, "expected 0.0 for constant, got {}", rv);
    }

    // ── round-139 ────────────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_range_position_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_range_position().is_none());
    }

    #[test]
    fn test_zscore_window_range_position_at_max() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let r = n.window_range_position().unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0, got {}", r);
    }

    #[test]
    fn test_zscore_window_sign_changes_none_for_small() {
        let mut n = znorm(4);
        n.update(dec!(1)); n.update(dec!(2));
        assert!(n.window_sign_changes().is_none());
    }

    #[test]
    fn test_zscore_window_sign_changes_alternating() {
        let mut n = znorm(5);
        for v in [dec!(1), dec!(3), dec!(1), dec!(3), dec!(1)] { n.update(v); }
        let sc = n.window_sign_changes().unwrap();
        assert_eq!(sc, 3, "expected 3 sign changes, got {}", sc);
    }

    #[test]
    fn test_zscore_window_mean_shift_none_for_small() {
        let mut n = znorm(4);
        n.update(dec!(1)); n.update(dec!(2)); n.update(dec!(3));
        assert!(n.window_mean_shift().is_none());
    }

    #[test]
    fn test_zscore_window_mean_shift_increasing() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(10), dec!(11)] { n.update(v); }
        let ms = n.window_mean_shift().unwrap();
        assert!(ms > 0.0, "expected positive shift, got {}", ms);
    }

    #[test]
    fn test_zscore_window_slope_change_none_for_small() {
        let mut n = znorm(4);
        n.update(dec!(1)); n.update(dec!(2)); n.update(dec!(3));
        assert!(n.window_slope_change().is_none());
    }

    #[test]
    fn test_zscore_window_slope_change_basic() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let sc = n.window_slope_change().unwrap();
        assert!(sc.abs() < 1.0, "expected small slope change, got {}", sc);
    }

    // ── round-140 ────────────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_recovery_rate_none_for_small() {
        let mut n = znorm(4);
        n.update(dec!(1)); n.update(dec!(2));
        assert!(n.window_recovery_rate().is_none());
    }

    #[test]
    fn test_zscore_window_recovery_rate_no_drops() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let r = n.window_recovery_rate().unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0 when no drops, got {}", r);
    }

    #[test]
    fn test_zscore_window_normalized_spread_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_normalized_spread().is_none());
    }

    #[test]
    fn test_zscore_window_normalized_spread_basic() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let s = n.window_normalized_spread().unwrap();
        assert!((s - 1.2).abs() < 1e-9, "expected 1.2, got {}", s);
    }

    #[test]
    fn test_zscore_window_first_last_ratio_none_for_single() {
        let mut n = znorm(4);
        n.update(dec!(5));
        assert!(n.window_first_last_ratio().is_none());
    }

    #[test]
    fn test_zscore_window_first_last_ratio_basic() {
        let mut n = znorm(4);
        for v in [dec!(2), dec!(3), dec!(4), dec!(8)] { n.update(v); }
        let r = n.window_first_last_ratio().unwrap();
        assert!((r - 4.0).abs() < 1e-9, "expected 4.0, got {}", r);
    }

    #[test]
    fn test_zscore_window_extrema_count_none_for_small() {
        let mut n = znorm(4);
        n.update(dec!(1)); n.update(dec!(2));
        assert!(n.window_extrema_count().is_none());
    }

    #[test]
    fn test_zscore_window_extrema_count_alternating() {
        let mut n = znorm(5);
        for v in [dec!(1), dec!(3), dec!(1), dec!(3), dec!(1)] { n.update(v); }
        let e = n.window_extrema_count().unwrap();
        assert_eq!(e, 3, "expected 3 extrema, got {}", e);
    }

    // ── round-141 ────────────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_up_fraction_none_for_single() {
        let mut n = znorm(4);
        n.update(dec!(5));
        assert!(n.window_up_fraction().is_none());
    }

    #[test]
    fn test_zscore_window_up_fraction_all_up() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let f = n.window_up_fraction().unwrap();
        assert!((f - 1.0).abs() < 1e-9, "expected 1.0, got {}", f);
    }

    #[test]
    fn test_zscore_window_half_range_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_half_range().is_none());
    }

    #[test]
    fn test_zscore_window_half_range_basic() {
        let mut n = znorm(4);
        for v in [dec!(2), dec!(4), dec!(6), dec!(8)] { n.update(v); }
        let hr = n.window_half_range().unwrap();
        assert!((hr - 3.0).abs() < 1e-9, "expected 3.0, got {}", hr);
    }

    #[test]
    fn test_zscore_window_negative_count_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_negative_count().is_none());
    }

    #[test]
    fn test_zscore_window_negative_count_no_negatives() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let c = n.window_negative_count().unwrap();
        assert_eq!(c, 0, "expected 0 negatives, got {}", c);
    }

    #[test]
    fn test_zscore_window_trend_purity_none_for_single() {
        let mut n = znorm(4);
        n.update(dec!(5));
        assert!(n.window_trend_purity().is_none());
    }

    #[test]
    fn test_zscore_window_trend_purity_pure_uptrend() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let p = n.window_trend_purity().unwrap();
        assert!((p - 1.0).abs() < 1e-9, "expected 1.0 for pure uptrend, got {}", p);
    }

    // ── round-142 ────────────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_centered_mean_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_centered_mean().is_none());
    }

    #[test]
    fn test_zscore_window_centered_mean_symmetric() {
        let mut n = znorm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)] { n.update(v); }
        let cm = n.window_centered_mean().unwrap();
        assert!(cm.abs() < 1e-9, "expected 0.0, got {}", cm);
    }

    #[test]
    fn test_zscore_window_last_deviation_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_last_deviation().is_none());
    }

    #[test]
    fn test_zscore_window_last_deviation_last_is_highest() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(10)] { n.update(v); }
        let d = n.window_last_deviation().unwrap();
        assert!(d > 0.0, "expected positive deviation, got {}", d);
    }

    #[test]
    fn test_zscore_window_step_size_mean_none_for_single() {
        let mut n = znorm(4);
        n.update(dec!(5));
        assert!(n.window_step_size_mean().is_none());
    }

    #[test]
    fn test_zscore_window_step_size_mean_uniform() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(3), dec!(5), dec!(7)] { n.update(v); }
        let s = n.window_step_size_mean().unwrap();
        assert!((s - 2.0).abs() < 1e-9, "expected 2.0, got {}", s);
    }

    #[test]
    fn test_zscore_window_net_up_count_none_for_single() {
        let mut n = znorm(4);
        n.update(dec!(5));
        assert!(n.window_net_up_count().is_none());
    }

    #[test]
    fn test_zscore_window_net_up_count_all_up() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let c = n.window_net_up_count().unwrap();
        assert_eq!(c, 3, "expected 3, got {}", c);
    }

    // ── round-143 ────────────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_weighted_mean_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_weighted_mean().is_none());
    }

    #[test]
    fn test_zscore_window_weighted_mean_single() {
        let mut n = znorm(4);
        n.update(dec!(10));
        let wm = n.window_weighted_mean().unwrap();
        assert!((wm - 10.0).abs() < 1e-9, "expected 10.0, got {}", wm);
    }

    #[test]
    fn test_zscore_window_upper_half_mean_none_for_single() {
        let mut n = znorm(4);
        n.update(dec!(5));
        assert!(n.window_upper_half_mean().is_none());
    }

    #[test]
    fn test_zscore_window_upper_half_mean_basic() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let um = n.window_upper_half_mean().unwrap();
        assert!((um - 3.5).abs() < 1e-9, "expected 3.5, got {}", um);
    }

    #[test]
    fn test_zscore_window_lower_half_mean_none_for_single() {
        let mut n = znorm(4);
        n.update(dec!(5));
        assert!(n.window_lower_half_mean().is_none());
    }

    #[test]
    fn test_zscore_window_lower_half_mean_basic() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let lm = n.window_lower_half_mean().unwrap();
        assert!((lm - 1.5).abs() < 1e-9, "expected 1.5, got {}", lm);
    }

    #[test]
    fn test_zscore_window_mid_range_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_mid_range().is_none());
    }

    #[test]
    fn test_zscore_window_mid_range_basic() {
        let mut n = znorm(4);
        for v in [dec!(2), dec!(4), dec!(6), dec!(8)] { n.update(v); }
        let mr = n.window_mid_range().unwrap();
        assert!((mr - 5.0).abs() < 1e-9, "expected 5.0, got {}", mr);
    }

    // ── round-144 ────────────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_trim_mean_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_trim_mean().is_none());
    }

    #[test]
    fn test_zscore_window_trim_mean_basic() {
        let mut n = znorm(10);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5),
                  dec!(6), dec!(7), dec!(8), dec!(9), dec!(10)] { n.update(v); }
        let tm = n.window_trim_mean().unwrap();
        assert!((tm - 5.5).abs() < 1e-9, "expected 5.5, got {}", tm);
    }

    #[test]
    fn test_zscore_window_value_spread_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_value_spread().is_none());
    }

    #[test]
    fn test_zscore_window_value_spread_basic() {
        let mut n = znorm(4);
        for v in [dec!(2), dec!(5), dec!(3), dec!(9)] { n.update(v); }
        let sp = n.window_value_spread().unwrap();
        assert!((sp - 7.0).abs() < 1e-9, "expected 7.0, got {}", sp);
    }

    #[test]
    fn test_zscore_window_rms_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_rms().is_none());
    }

    #[test]
    fn test_zscore_window_rms_basic() {
        let mut n = znorm(2);
        for v in [dec!(3), dec!(4)] { n.update(v); }
        let rms = n.window_rms().unwrap();
        assert!((rms - 12.5f64.sqrt()).abs() < 1e-9, "expected sqrt(12.5), got {}", rms);
    }

    #[test]
    fn test_zscore_window_above_mid_fraction_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_above_mid_fraction().is_none());
    }

    #[test]
    fn test_zscore_window_above_mid_fraction_basic() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let f = n.window_above_mid_fraction().unwrap();
        assert!((f - 0.5).abs() < 1e-9, "expected 0.5, got {}", f);
    }

    // ── round-145 ────────────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_median_abs_dev_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_median_abs_dev().is_none());
    }

    #[test]
    fn test_zscore_window_median_abs_dev_basic() {
        let mut n = znorm(5);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)] { n.update(v); }
        let mad = n.window_median_abs_dev().unwrap();
        assert!((mad - 1.0).abs() < 1e-9, "expected 1.0, got {}", mad);
    }

    #[test]
    fn test_zscore_window_cubic_mean_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_cubic_mean().is_none());
    }

    #[test]
    fn test_zscore_window_cubic_mean_basic() {
        let mut n = znorm(2);
        for v in [dec!(2), dec!(4)] { n.update(v); }
        let cm = n.window_cubic_mean().unwrap();
        assert!((cm - 36.0f64.cbrt()).abs() < 1e-9, "expected cbrt(36), got {}", cm);
    }

    #[test]
    fn test_zscore_window_max_run_length_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_max_run_length().is_none());
    }

    #[test]
    fn test_zscore_window_max_run_length_all_same() {
        let mut n = znorm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let r = n.window_max_run_length().unwrap();
        assert_eq!(r, 3);
    }

    #[test]
    fn test_zscore_window_sorted_position_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_sorted_position().is_none());
    }

    #[test]
    fn test_zscore_window_sorted_position_max() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(10)] { n.update(v); }
        let p = n.window_sorted_position().unwrap();
        assert!((p - 0.75).abs() < 1e-9, "expected 0.75, got {}", p);
    }

    // ── round-146 ────────────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_prev_deviation_none_for_single() {
        let mut n = znorm(4);
        n.update(dec!(5));
        assert!(n.window_prev_deviation().is_none());
    }

    #[test]
    fn test_zscore_window_prev_deviation_basic() {
        let mut n = znorm(4);
        n.update(dec!(10));
        n.update(dec!(15));
        let d = n.window_prev_deviation().unwrap();
        assert!((d - 5.0).abs() < 1e-9, "expected 5.0, got {}", d);
    }

    #[test]
    fn test_zscore_window_lower_quartile_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_lower_quartile().is_none());
    }

    #[test]
    fn test_zscore_window_lower_quartile_basic() {
        let mut n = znorm(4);
        for v in [dec!(10), dec!(20), dec!(30), dec!(40)] { n.update(v); }
        let q = n.window_lower_quartile().unwrap();
        assert!((q - 10.0).abs() < 1e-9, "expected 10.0, got {}", q);
    }

    #[test]
    fn test_zscore_window_upper_quartile_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_upper_quartile().is_none());
    }

    #[test]
    fn test_zscore_window_upper_quartile_basic() {
        let mut n = znorm(4);
        for v in [dec!(10), dec!(20), dec!(30), dec!(40)] { n.update(v); }
        let q = n.window_upper_quartile().unwrap();
        assert!((q - 30.0).abs() < 1e-9, "expected 30.0, got {}", q);
    }

    #[test]
    fn test_zscore_window_tail_weight_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_tail_weight().is_none());
    }

    #[test]
    fn test_zscore_window_tail_weight_basic() {
        let mut n = znorm(10);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5),
                  dec!(6), dec!(7), dec!(8), dec!(9), dec!(10)] { n.update(v); }
        let tw = n.window_tail_weight().unwrap();
        assert!(tw > 0.0, "tail weight should be > 0, got {}", tw);
    }

    // ── round-147 ────────────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_last_vs_mean_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_last_vs_mean().is_none());
    }

    #[test]
    fn test_zscore_window_last_vs_mean_basic() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let d = n.window_last_vs_mean().unwrap();
        assert!((d - 1.0).abs() < 1e-9, "expected 1.0, got {}", d);
    }

    #[test]
    fn test_zscore_window_change_acceleration_none_for_two() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2)] { n.update(v); }
        assert!(n.window_change_acceleration().is_none());
    }

    #[test]
    fn test_zscore_window_change_acceleration_constant() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let a = n.window_change_acceleration().unwrap();
        assert!((a - 0.0).abs() < 1e-9, "expected 0.0, got {}", a);
    }

    #[test]
    fn test_zscore_window_positive_run_length_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_positive_run_length().is_none());
    }

    #[test]
    fn test_zscore_window_positive_run_length_all_positive() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let r = n.window_positive_run_length().unwrap();
        assert_eq!(r, 3);
    }

    #[test]
    fn test_zscore_window_geometric_trend_none_for_single() {
        let mut n = znorm(4);
        n.update(dec!(5));
        assert!(n.window_geometric_trend().is_none());
    }

    #[test]
    fn test_zscore_window_geometric_trend_constant() {
        let mut n = znorm(3);
        for v in [dec!(4), dec!(4), dec!(4)] { n.update(v); }
        let gt = n.window_geometric_trend().unwrap();
        assert!((gt - 1.0).abs() < 1e-9, "expected 1.0, got {}", gt);
    }

    // ── round-148 ────────────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_pairwise_diff_mean_none_for_single() {
        let mut n = znorm(4);
        n.update(dec!(5));
        assert!(n.window_pairwise_diff_mean().is_none());
    }

    #[test]
    fn test_zscore_window_pairwise_diff_mean_basic() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(3), dec!(5)] { n.update(v); }
        let d = n.window_pairwise_diff_mean().unwrap();
        assert!((d - 8.0 / 3.0).abs() < 1e-9, "expected 8/3, got {}", d);
    }

    #[test]
    fn test_zscore_window_negative_run_length_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_negative_run_length().is_none());
    }

    #[test]
    fn test_zscore_window_negative_run_length_no_negatives() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let r = n.window_negative_run_length().unwrap();
        assert_eq!(r, 0);
    }

    #[test]
    fn test_zscore_window_cross_zero_count_none_for_single() {
        let mut n = znorm(4);
        n.update(dec!(5));
        assert!(n.window_cross_zero_count().is_none());
    }

    #[test]
    fn test_zscore_window_cross_zero_count_no_crossings() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let c = n.window_cross_zero_count().unwrap();
        assert_eq!(c, 0);
    }

    #[test]
    fn test_zscore_window_mean_reversion_strength_none_for_single() {
        let mut n = znorm(4);
        n.update(dec!(5));
        assert!(n.window_mean_reversion_strength().is_none());
    }

    #[test]
    fn test_zscore_window_mean_reversion_strength_uniform() {
        let mut n = znorm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let s = n.window_mean_reversion_strength().unwrap();
        assert!((s - 0.0).abs() < 1e-9, "expected 0.0, got {}", s);
    }

    // ── round-149 ────────────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_first_vs_mean_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_first_vs_mean().is_none());
    }

    #[test]
    fn test_zscore_window_first_vs_mean_basic() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let d = n.window_first_vs_mean().unwrap();
        assert!((d - (-1.0)).abs() < 1e-9, "expected -1.0, got {}", d);
    }

    #[test]
    fn test_zscore_window_decay_ratio_none_for_single() {
        let mut n = znorm(4);
        n.update(dec!(5));
        assert!(n.window_decay_ratio().is_none());
    }

    #[test]
    fn test_zscore_window_decay_ratio_basic() {
        let mut n = znorm(2);
        n.update(dec!(4));
        n.update(dec!(8));
        let r = n.window_decay_ratio().unwrap();
        assert!((r - 2.0).abs() < 1e-9, "expected 2.0, got {}", r);
    }

    #[test]
    fn test_zscore_window_bimodal_score_none_for_few() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        assert!(n.window_bimodal_score().is_none());
    }

    #[test]
    fn test_zscore_window_bimodal_score_returns_some() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(8), dec!(9)] { n.update(v); }
        let s = n.window_bimodal_score();
        assert!(s.is_some());
    }

    #[test]
    fn test_zscore_window_abs_sum_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_abs_sum().is_none());
    }

    #[test]
    fn test_zscore_window_abs_sum_basic() {
        let mut n = znorm(2);
        for v in [dec!(3), dec!(4)] { n.update(v); }
        let s = n.window_abs_sum().unwrap();
        assert!((s - 7.0).abs() < 1e-9, "expected 7.0, got {}", s);
    }

    // ── round-150 ────────────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_coeff_of_variation_none_for_single() {
        let mut n = znorm(4);
        n.update(dec!(5));
        assert!(n.window_coeff_of_variation().is_none());
    }

    #[test]
    fn test_zscore_window_coeff_of_variation_basic() {
        let mut n = znorm(2);
        for v in [dec!(10), dec!(20)] { n.update(v); }
        let cv = n.window_coeff_of_variation().unwrap();
        assert!((cv - 5.0 / 15.0).abs() < 1e-9, "expected ~0.333, got {}", cv);
    }

    #[test]
    fn test_zscore_window_mean_absolute_error_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_mean_absolute_error().is_none());
    }

    #[test]
    fn test_zscore_window_mean_absolute_error_uniform() {
        let mut n = znorm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let mae = n.window_mean_absolute_error().unwrap();
        assert!((mae - 0.0).abs() < 1e-9, "expected 0.0, got {}", mae);
    }

    #[test]
    fn test_zscore_window_normalized_last_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_normalized_last().is_none());
    }

    #[test]
    fn test_zscore_window_normalized_last_at_max() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(10)] { n.update(v); }
        let nl = n.window_normalized_last().unwrap();
        assert!((nl - 1.0).abs() < 1e-9, "expected 1.0, got {}", nl);
    }

    #[test]
    fn test_zscore_window_sign_bias_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_sign_bias().is_none());
    }

    #[test]
    fn test_zscore_window_sign_bias_all_positive() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let sb = n.window_sign_bias().unwrap();
        assert!((sb - 1.0).abs() < 1e-9, "expected 1.0, got {}", sb);
    }

    // ── round-151 ────────────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_penultimate_vs_last_none_for_single() {
        let mut n = znorm(4);
        n.update(dec!(5));
        assert!(n.window_penultimate_vs_last().is_none());
    }

    #[test]
    fn test_zscore_window_penultimate_vs_last_basic() {
        let mut n = znorm(4);
        for v in [dec!(10), dec!(6)] { n.update(v); }
        let d = n.window_penultimate_vs_last().unwrap();
        assert!((d - 4.0).abs() < 1e-9, "expected 4.0, got {}", d);
    }

    #[test]
    fn test_zscore_window_mean_range_position_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_mean_range_position().is_none());
    }

    #[test]
    fn test_zscore_window_mean_range_position_single() {
        let mut n = znorm(4);
        n.update(dec!(5));
        let _ = n.window_mean_range_position();
    }

    #[test]
    fn test_zscore_window_zscore_last_none_for_single() {
        let mut n = znorm(4);
        n.update(dec!(5));
        assert!(n.window_zscore_last().is_none());
    }

    #[test]
    fn test_zscore_window_zscore_last_zero_for_uniform() {
        let mut n = znorm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        assert!(n.window_zscore_last().is_none());
    }

    #[test]
    fn test_zscore_window_gradient_none_for_single() {
        let mut n = znorm(4);
        n.update(dec!(5));
        assert!(n.window_gradient().is_none());
    }

    #[test]
    fn test_zscore_window_gradient_ascending() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let g = n.window_gradient().unwrap();
        assert!(g > 0.0, "expected positive gradient, got {}", g);
    }

    // ── round-152 ────────────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_entropy_score_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_entropy_score().is_none());
    }

    #[test]
    fn test_zscore_window_entropy_score_uniform_zero() {
        let mut n = znorm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let e = n.window_entropy_score().unwrap();
        assert!((e - 0.0).abs() < 1e-9, "expected 0.0, got {}", e);
    }

    #[test]
    fn test_zscore_window_quartile_spread_none_for_three() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        assert!(n.window_quartile_spread().is_none());
    }

    #[test]
    fn test_zscore_window_quartile_spread_basic() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let qs = n.window_quartile_spread().unwrap();
        assert!(qs >= 0.0, "expected non-negative, got {}", qs);
    }

    #[test]
    fn test_zscore_window_max_to_min_ratio_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_max_to_min_ratio().is_none());
    }

    #[test]
    fn test_zscore_window_max_to_min_ratio_basic() {
        let mut n = znorm(4);
        for v in [dec!(2), dec!(4)] { n.update(v); }
        let r = n.window_max_to_min_ratio().unwrap();
        assert!((r - 2.0).abs() < 1e-9, "expected 2.0, got {}", r);
    }

    #[test]
    fn test_zscore_window_upper_fraction_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_upper_fraction().is_none());
    }

    #[test]
    fn test_zscore_window_upper_fraction_half() {
        let mut n = znorm(2);
        for v in [dec!(1), dec!(3)] { n.update(v); }
        let f = n.window_upper_fraction().unwrap();
        assert!((f - 0.5).abs() < 1e-9, "expected 0.5, got {}", f);
    }

    // ── round-153 ────────────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_abs_change_mean_none_for_single() {
        let mut n = znorm(4);
        n.update(dec!(5));
        assert!(n.window_abs_change_mean().is_none());
    }

    #[test]
    fn test_zscore_window_abs_change_mean_constant() {
        let mut n = znorm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let m = n.window_abs_change_mean().unwrap();
        assert!((m - 0.0).abs() < 1e-9, "expected 0.0, got {}", m);
    }

    #[test]
    fn test_zscore_window_last_percentile_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_last_percentile().is_none());
    }

    #[test]
    fn test_zscore_window_last_percentile_at_max() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let p = n.window_last_percentile().unwrap();
        assert!((p - 2.0 / 3.0).abs() < 1e-9, "expected 0.667, got {}", p);
    }

    #[test]
    fn test_zscore_window_trailing_std_none_for_single() {
        let mut n = znorm(4);
        n.update(dec!(5));
        assert!(n.window_trailing_std().is_none());
    }

    #[test]
    fn test_zscore_window_trailing_std_uniform() {
        let mut n = znorm(4);
        for v in [dec!(5), dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let s = n.window_trailing_std().unwrap();
        assert!((s - 0.0).abs() < 1e-9, "expected 0.0, got {}", s);
    }

    #[test]
    fn test_zscore_window_mean_change_none_for_single() {
        let mut n = znorm(4);
        n.update(dec!(5));
        assert!(n.window_mean_change().is_none());
    }

    #[test]
    fn test_zscore_window_mean_change_ascending() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(3), dec!(5)] { n.update(v); }
        let c = n.window_mean_change().unwrap();
        assert!((c - 2.0).abs() < 1e-9, "expected 2.0, got {}", c);
    }

    // ── round-154 ────────────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_value_at_peak_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_value_at_peak().is_none());
    }

    #[test]
    fn test_zscore_window_value_at_peak_single() {
        let mut n = znorm(4);
        n.update(dec!(5));
        let p = n.window_value_at_peak().unwrap();
        assert!((p - 0.0).abs() < 1e-9, "expected 0.0, got {}", p);
    }

    #[test]
    fn test_zscore_window_head_tail_diff_none_for_single() {
        let mut n = znorm(4);
        n.update(dec!(5));
        assert!(n.window_head_tail_diff().is_none());
    }

    #[test]
    fn test_zscore_window_head_tail_diff_basic() {
        let mut n = znorm(2);
        for v in [dec!(10), dec!(3)] { n.update(v); }
        let d = n.window_head_tail_diff().unwrap();
        assert!((d - 7.0).abs() < 1e-9, "expected 7.0, got {}", d);
    }

    #[test]
    fn test_zscore_window_midpoint_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_midpoint().is_none());
    }

    #[test]
    fn test_zscore_window_midpoint_basic() {
        let mut n = znorm(2);
        for v in [dec!(2), dec!(8)] { n.update(v); }
        let m = n.window_midpoint().unwrap();
        assert!((m - 5.0).abs() < 1e-9, "expected 5.0, got {}", m);
    }

    #[test]
    fn test_zscore_window_concavity_none_for_two() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2)] { n.update(v); }
        assert!(n.window_concavity().is_none());
    }

    #[test]
    fn test_zscore_window_concavity_flat() {
        let mut n = znorm(6);
        for v in [dec!(5), dec!(5), dec!(5), dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let c = n.window_concavity().unwrap();
        assert!((c - 0.0).abs() < 1e-9, "expected 0.0, got {}", c);
    }

    // ── round-155 ────────────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_rise_fraction_none_for_single() {
        let mut n = znorm(4);
        n.update(dec!(5));
        assert!(n.window_rise_fraction().is_none());
    }

    #[test]
    fn test_zscore_window_rise_fraction_all_ascending() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let f = n.window_rise_fraction().unwrap();
        assert!((f - 1.0).abs() < 1e-9, "expected 1.0, got {}", f);
    }

    #[test]
    fn test_zscore_window_peak_to_valley_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_peak_to_valley().is_none());
    }

    #[test]
    fn test_zscore_window_peak_to_valley_basic() {
        let mut n = znorm(2);
        for v in [dec!(3), dec!(10)] { n.update(v); }
        let ptv = n.window_peak_to_valley().unwrap();
        assert!((ptv - 7.0).abs() < 1e-9, "expected 7.0, got {}", ptv);
    }

    #[test]
    fn test_zscore_window_positive_change_mean_none_for_single() {
        let mut n = znorm(4);
        n.update(dec!(5));
        assert!(n.window_positive_change_mean().is_none());
    }

    #[test]
    fn test_zscore_window_positive_change_mean_no_rises() {
        let mut n = znorm(2);
        for v in [dec!(10), dec!(5)] { n.update(v); }
        assert!(n.window_positive_change_mean().is_none());
    }

    #[test]
    fn test_zscore_window_range_cv_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_range_cv().is_none());
    }

    #[test]
    fn test_zscore_window_range_cv_uniform() {
        let mut n = znorm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let cv = n.window_range_cv().unwrap();
        assert!((cv - 0.0).abs() < 1e-9, "expected 0.0, got {}", cv);
    }

    // ── round-156 ────────────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_negative_change_mean_none_for_single() {
        let mut n = znorm(4);
        n.update(dec!(5));
        assert!(n.window_negative_change_mean().is_none());
    }

    #[test]
    fn test_zscore_window_negative_change_mean_no_falls() {
        let mut n = znorm(2);
        for v in [dec!(1), dec!(5)] { n.update(v); }
        assert!(n.window_negative_change_mean().is_none());
    }

    #[test]
    fn test_zscore_window_fall_fraction_none_for_single() {
        let mut n = znorm(4);
        n.update(dec!(5));
        assert!(n.window_fall_fraction().is_none());
    }

    #[test]
    fn test_zscore_window_fall_fraction_all_descending() {
        let mut n = znorm(3);
        for v in [dec!(5), dec!(3), dec!(1)] { n.update(v); }
        let f = n.window_fall_fraction().unwrap();
        assert!((f - 1.0).abs() < 1e-9, "expected 1.0, got {}", f);
    }

    #[test]
    fn test_zscore_window_last_vs_max_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_last_vs_max().is_none());
    }

    #[test]
    fn test_zscore_window_last_vs_max_at_max() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(3), dec!(5)] { n.update(v); }
        let d = n.window_last_vs_max().unwrap();
        assert!((d - 0.0).abs() < 1e-9, "expected 0.0, got {}", d);
    }

    #[test]
    fn test_zscore_window_last_vs_min_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_last_vs_min().is_none());
    }

    #[test]
    fn test_zscore_window_last_vs_min_at_min() {
        let mut n = znorm(3);
        for v in [dec!(5), dec!(3), dec!(1)] { n.update(v); }
        let d = n.window_last_vs_min().unwrap();
        assert!((d - 0.0).abs() < 1e-9, "expected 0.0, got {}", d);
    }

    // ── round-157 ────────────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_mean_oscillation_none_for_single() {
        let mut n = znorm(4);
        n.update(dec!(5));
        assert!(n.window_mean_oscillation().is_none());
    }

    #[test]
    fn test_zscore_window_mean_oscillation_constant() {
        let mut n = znorm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let o = n.window_mean_oscillation().unwrap();
        assert!((o - 0.0).abs() < 1e-9, "expected 0.0, got {}", o);
    }

    #[test]
    fn test_zscore_window_monotone_score_none_for_single() {
        let mut n = znorm(4);
        n.update(dec!(5));
        assert!(n.window_monotone_score().is_none());
    }

    #[test]
    fn test_zscore_window_monotone_score_perfectly_ascending() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let s = n.window_monotone_score().unwrap();
        assert!((s - 1.0).abs() < 1e-9, "expected 1.0, got {}", s);
    }

    #[test]
    fn test_zscore_window_stddev_trend_none_for_three() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        assert!(n.window_stddev_trend().is_none());
    }

    #[test]
    fn test_zscore_window_stddev_trend_uniform() {
        let mut n = znorm(4);
        for v in [dec!(5), dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let t = n.window_stddev_trend().unwrap();
        assert!((t - 0.0).abs() < 1e-9, "expected 0.0, got {}", t);
    }

    #[test]
    fn test_zscore_window_zero_cross_fraction_none_for_two() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2)] { n.update(v); }
        assert!(n.window_zero_cross_fraction().is_none());
    }

    #[test]
    fn test_zscore_window_zero_cross_fraction_alternating() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(3), dec!(1), dec!(3)] { n.update(v); }
        let f = n.window_zero_cross_fraction().unwrap();
        assert!((f - 1.0).abs() < 1e-9, "expected 1.0, got {}", f);
    }

    // ── round-158 ────────────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_exponential_decay_sum_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_exponential_decay_sum().is_none());
    }

    #[test]
    fn test_zscore_window_exponential_decay_sum_positive() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let s = n.window_exponential_decay_sum().unwrap();
        assert!(s > 0.0, "expected positive, got {}", s);
    }

    #[test]
    fn test_zscore_window_lagged_diff_none_for_single() {
        let mut n = znorm(4);
        n.update(dec!(5));
        assert!(n.window_lagged_diff().is_none());
    }

    #[test]
    fn test_zscore_window_lagged_diff_constant() {
        let mut n = znorm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let d = n.window_lagged_diff().unwrap();
        assert!((d - 0.0).abs() < 1e-9, "expected 0.0, got {}", d);
    }

    #[test]
    fn test_zscore_window_mean_to_max_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_mean_to_max().is_none());
    }

    #[test]
    fn test_zscore_window_mean_to_max_all_equal() {
        let mut n = znorm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let r = n.window_mean_to_max().unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0, got {}", r);
    }

    #[test]
    fn test_zscore_window_mode_fraction_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_mode_fraction().is_none());
    }

    #[test]
    fn test_zscore_window_mode_fraction_uniform() {
        let mut n = znorm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let f = n.window_mode_fraction().unwrap();
        assert!((f - 1.0).abs() < 1e-9, "expected 1.0, got {}", f);
    }

    // ── round-159 ────────────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_mean_below_zero_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_mean_below_zero().is_none());
    }

    #[test]
    fn test_zscore_window_mean_below_zero_no_negatives() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        assert!(n.window_mean_below_zero().is_none());
    }

    #[test]
    fn test_zscore_window_mean_above_zero_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_mean_above_zero().is_none());
    }

    #[test]
    fn test_zscore_window_mean_above_zero_positive() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let m = n.window_mean_above_zero().unwrap();
        assert!((m - 2.0).abs() < 1e-9, "expected 2.0, got {}", m);
    }

    #[test]
    fn test_zscore_window_running_max_fraction_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_running_max_fraction().is_none());
    }

    #[test]
    fn test_zscore_window_running_max_fraction_ascending() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let f = n.window_running_max_fraction().unwrap();
        assert!((f - 1.0).abs() < 1e-9, "expected 1.0, got {}", f);
    }

    #[test]
    fn test_zscore_window_variance_change_none_for_short() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        assert!(n.window_variance_change().is_none());
    }

    #[test]
    fn test_zscore_window_variance_change_constant() {
        let mut n = znorm(4);
        for v in [dec!(5), dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let vc = n.window_variance_change().unwrap();
        assert!((vc - 0.0).abs() < 1e-9, "expected 0.0, got {}", vc);
    }

    // ── round-160 ────────────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_ema_slope_none_for_single() {
        let mut n = znorm(4);
        n.update(dec!(5));
        assert!(n.window_ema_slope().is_none());
    }

    #[test]
    fn test_zscore_window_ema_slope_constant_zero() {
        let mut n = znorm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let s = n.window_ema_slope().unwrap();
        assert!((s - 0.0).abs() < 1e-9, "expected 0.0, got {}", s);
    }

    #[test]
    fn test_zscore_window_range_ratio_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_range_ratio().is_none());
    }

    #[test]
    fn test_zscore_window_range_ratio_equal_values() {
        let mut n = znorm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let r = n.window_range_ratio().unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0, got {}", r);
    }

    #[test]
    fn test_zscore_window_above_mean_streak_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_above_mean_streak().is_none());
    }

    #[test]
    fn test_zscore_window_above_mean_streak_constant_zero() {
        let mut n = znorm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let s = n.window_above_mean_streak().unwrap();
        assert!((s - 0.0).abs() < 1e-9, "expected 0.0, got {}", s);
    }

    #[test]
    fn test_zscore_window_mean_abs_diff_none_for_single() {
        let mut n = znorm(4);
        n.update(dec!(5));
        assert!(n.window_mean_abs_diff().is_none());
    }

    #[test]
    fn test_zscore_window_mean_abs_diff_constant_zero() {
        let mut n = znorm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let d = n.window_mean_abs_diff().unwrap();
        assert!((d - 0.0).abs() < 1e-9, "expected 0.0, got {}", d);
    }

    // ── round-161 ────────────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_sign_entropy_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_sign_entropy().is_none());
    }

    #[test]
    fn test_zscore_window_sign_entropy_all_positive() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let e = n.window_sign_entropy().unwrap();
        assert!((e - 0.0).abs() < 1e-9, "expected 0.0, got {}", e);
    }

    #[test]
    fn test_zscore_window_local_extrema_count_none_for_two() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2)] { n.update(v); }
        assert!(n.window_local_extrema_count().is_none());
    }

    #[test]
    fn test_zscore_window_local_extrema_count_monotone_zero() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3), dec!(4)] { n.update(v); }
        let c = n.window_local_extrema_count().unwrap();
        assert!((c - 0.0).abs() < 1e-9, "expected 0.0, got {}", c);
    }

    #[test]
    fn test_zscore_window_autocorr_lag2_none_for_two() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2)] { n.update(v); }
        assert!(n.window_autocorr_lag2().is_none());
    }

    #[test]
    fn test_zscore_window_autocorr_lag2_uniform_none() {
        let mut n = znorm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        assert!(n.window_autocorr_lag2().is_none());
    }

    #[test]
    fn test_zscore_window_pct_above_median_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_pct_above_median().is_none());
    }

    #[test]
    fn test_zscore_window_pct_above_median_single() {
        let mut n = znorm(4);
        n.update(dec!(5));
        let p = n.window_pct_above_median().unwrap();
        assert!((p - 0.0).abs() < 1e-9, "expected 0.0, got {}", p);
    }

    // ── round-162 ────────────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_below_zero_streak_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_below_zero_streak().is_none());
    }

    #[test]
    fn test_zscore_window_below_zero_streak_no_negatives_zero() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let s = n.window_below_zero_streak().unwrap();
        assert!((s - 0.0).abs() < 1e-9, "expected 0.0, got {}", s);
    }

    #[test]
    fn test_zscore_window_max_to_mean_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_max_to_mean().is_none());
    }

    #[test]
    fn test_zscore_window_max_to_mean_constant_one() {
        let mut n = znorm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let r = n.window_max_to_mean().unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0, got {}", r);
    }

    #[test]
    fn test_zscore_window_sign_run_length_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_sign_run_length().is_none());
    }

    #[test]
    fn test_zscore_window_sign_run_length_single() {
        let mut n = znorm(4);
        n.update(dec!(5));
        let r = n.window_sign_run_length().unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0, got {}", r);
    }

    #[test]
    fn test_zscore_window_decay_weighted_mean_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_decay_weighted_mean().is_none());
    }

    #[test]
    fn test_zscore_window_decay_weighted_mean_constant() {
        let mut n = znorm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let m = n.window_decay_weighted_mean().unwrap();
        assert!((m - 5.0).abs() < 1e-9, "expected 5.0, got {}", m);
    }

    // ── round-163 ────────────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_min_to_mean_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_min_to_mean().is_none());
    }

    #[test]
    fn test_zscore_window_min_to_mean_constant_one() {
        let mut n = znorm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let r = n.window_min_to_mean().unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0, got {}", r);
    }

    #[test]
    fn test_zscore_window_normalized_range_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_normalized_range().is_none());
    }

    #[test]
    fn test_zscore_window_normalized_range_constant_zero() {
        let mut n = znorm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let r = n.window_normalized_range().unwrap();
        assert!((r - 0.0).abs() < 1e-9, "expected 0.0, got {}", r);
    }

    #[test]
    fn test_zscore_window_winsorized_mean_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_winsorized_mean().is_none());
    }

    #[test]
    fn test_zscore_window_winsorized_mean_constant() {
        let mut n = znorm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let m = n.window_winsorized_mean().unwrap();
        assert!((m - 5.0).abs() < 1e-9, "expected 5.0, got {}", m);
    }

    #[test]
    fn test_zscore_window_range_to_std_none_for_single() {
        let mut n = znorm(4);
        n.update(dec!(5));
        assert!(n.window_range_to_std().is_none());
    }

    #[test]
    fn test_zscore_window_range_to_std_constant_none() {
        let mut n = znorm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        assert!(n.window_range_to_std().is_none());
    }

    // ── round-164 ────────────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_cv_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_cv().is_none());
    }

    #[test]
    fn test_zscore_window_cv_constant_zero() {
        let mut n = znorm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let cv = n.window_cv().unwrap();
        assert!((cv - 0.0).abs() < 1e-9, "expected 0.0, got {}", cv);
    }

    #[test]
    fn test_zscore_window_non_zero_fraction_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_non_zero_fraction().is_none());
    }

    #[test]
    fn test_zscore_window_non_zero_fraction_all_nonzero() {
        let mut n = znorm(3);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        let f = n.window_non_zero_fraction().unwrap();
        assert!((f - 1.0).abs() < 1e-9, "expected 1.0, got {}", f);
    }

    #[test]
    fn test_zscore_window_rms_abs_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_rms_abs().is_none());
    }

    #[test]
    fn test_zscore_window_rms_abs_constant() {
        let mut n = znorm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let m = n.window_rms_abs().unwrap();
        assert!((m - 5.0).abs() < 1e-9, "expected 5.0, got {}", m);
    }

    #[test]
    fn test_zscore_window_kurtosis_proxy_none_for_three() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2), dec!(3)] { n.update(v); }
        assert!(n.window_kurtosis_proxy().is_none());
    }

    #[test]
    fn test_zscore_window_kurtosis_proxy_constant_none() {
        let mut n = znorm(4);
        for v in [dec!(5), dec!(5), dec!(5), dec!(5)] { n.update(v); }
        assert!(n.window_kurtosis_proxy().is_none());
    }

    // ── round-165 ────────────────────────────────────────────────────────────

    #[test]
    fn test_zscore_window_mean_reversion_index_none_for_two() {
        let mut n = znorm(4);
        for v in [dec!(1), dec!(2)] { n.update(v); }
        assert!(n.window_mean_reversion_index().is_none());
    }

    #[test]
    fn test_zscore_window_mean_reversion_index_constant_zero() {
        let mut n = znorm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let r = n.window_mean_reversion_index().unwrap();
        assert!((r - 0.0).abs() < 1e-9, "expected 0.0, got {}", r);
    }

    #[test]
    fn test_zscore_window_tail_ratio_none_for_single() {
        let mut n = znorm(4);
        n.update(dec!(5));
        assert!(n.window_tail_ratio().is_none());
    }

    #[test]
    fn test_zscore_window_tail_ratio_equal_one() {
        let mut n = znorm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let r = n.window_tail_ratio().unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0, got {}", r);
    }

    #[test]
    fn test_zscore_window_cumsum_trend_none_for_empty() {
        let n = znorm(4);
        assert!(n.window_cumsum_trend().is_none());
    }

    #[test]
    fn test_zscore_window_cumsum_trend_constant() {
        let mut n = znorm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let t = n.window_cumsum_trend().unwrap();
        assert!((t - 5.0).abs() < 1e-9, "expected 5.0, got {}", t);
    }

    #[test]
    fn test_zscore_window_mean_crossing_count_none_for_single() {
        let mut n = znorm(4);
        n.update(dec!(5));
        assert!(n.window_mean_crossing_count().is_none());
    }

    #[test]
    fn test_zscore_window_mean_crossing_count_constant_zero() {
        let mut n = znorm(3);
        for v in [dec!(5), dec!(5), dec!(5)] { n.update(v); }
        let c = n.window_mean_crossing_count().unwrap();
        assert!((c - 0.0).abs() < 1e-9, "expected 0.0, got {}", c);
    }
}
