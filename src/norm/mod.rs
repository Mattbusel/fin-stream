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
}
