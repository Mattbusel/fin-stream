//! Technical analysis indicators: RSI, MACD, Bollinger Bands, ATR, Stochastic, EMA, SMA.

/// Relative Strength Index using Wilder's smoothing method.
pub struct Rsi {
    /// Lookback period (typically 14).
    pub period: usize,
}

impl Rsi {
    /// Computes RSI values for a price series.
    ///
    /// Returns a `Vec<f64>` of the same length as `prices`. Entries before the
    /// warm-up period are `f64::NAN`.
    pub fn compute(&self, prices: &[f64]) -> Vec<f64> {
        let n = prices.len();
        let mut result = vec![f64::NAN; n];

        if n <= self.period {
            return result;
        }

        // Calculate initial average gain and loss over first `period` changes.
        let mut avg_gain = 0.0_f64;
        let mut avg_loss = 0.0_f64;

        for i in 1..=self.period {
            let change = prices[i] - prices[i - 1];
            if change > 0.0 {
                avg_gain += change;
            } else {
                avg_loss += change.abs();
            }
        }
        avg_gain /= self.period as f64;
        avg_loss /= self.period as f64;

        let rs = if avg_loss == 0.0 { f64::INFINITY } else { avg_gain / avg_loss };
        result[self.period] = 100.0 - 100.0 / (1.0 + rs);

        // Wilder's smoothing for remaining bars.
        for i in (self.period + 1)..n {
            let change = prices[i] - prices[i - 1];
            let gain = change.max(0.0);
            let loss = (-change).max(0.0);
            avg_gain = (avg_gain * (self.period - 1) as f64 + gain) / self.period as f64;
            avg_loss = (avg_loss * (self.period - 1) as f64 + loss) / self.period as f64;
            let rs = if avg_loss == 0.0 { f64::INFINITY } else { avg_gain / avg_loss };
            result[i] = 100.0 - 100.0 / (1.0 + rs);
        }

        result
    }
}

/// Exponential Moving Average.
pub struct Ema {
    /// Lookback period.
    pub period: usize,
}

impl Ema {
    /// Computes EMA using multiplier `2 / (period + 1)`.
    ///
    /// Returns a `Vec<f64>` of the same length. First `period - 1` entries are `NAN`.
    pub fn compute(&self, prices: &[f64]) -> Vec<f64> {
        let n = prices.len();
        let mut result = vec![f64::NAN; n];

        if n < self.period {
            return result;
        }

        let k = 2.0 / (self.period as f64 + 1.0);

        // Seed with simple average of first `period` values.
        let seed: f64 = prices[..self.period].iter().sum::<f64>() / self.period as f64;
        result[self.period - 1] = seed;

        for i in self.period..n {
            result[i] = prices[i] * k + result[i - 1] * (1.0 - k);
        }

        result
    }
}

/// Simple Moving Average.
pub struct Sma {
    /// Lookback period.
    pub period: usize,
}

impl Sma {
    /// Computes SMA over a rolling window of `period` bars.
    ///
    /// Returns a `Vec<f64>` of the same length. First `period - 1` entries are `NAN`.
    pub fn compute(&self, prices: &[f64]) -> Vec<f64> {
        let n = prices.len();
        let mut result = vec![f64::NAN; n];

        if n < self.period {
            return result;
        }

        let mut window_sum: f64 = prices[..self.period].iter().sum();
        result[self.period - 1] = window_sum / self.period as f64;

        for i in self.period..n {
            window_sum += prices[i] - prices[i - self.period];
            result[i] = window_sum / self.period as f64;
        }

        result
    }
}

/// MACD indicator: fast EMA, slow EMA, signal EMA, and histogram.
pub struct Macd {
    /// Fast EMA period (typically 12).
    pub fast: usize,
    /// Slow EMA period (typically 26).
    pub slow: usize,
    /// Signal EMA period (typically 9).
    pub signal: usize,
}

impl Macd {
    /// Computes MACD line, signal line, and histogram.
    ///
    /// Returns `(macd_line, signal_line, histogram)` each of length `prices.len()`.
    /// Values before the warm-up are `NAN`.
    pub fn compute(&self, prices: &[f64]) -> (Vec<f64>, Vec<f64>, Vec<f64>) {
        let n = prices.len();
        let fast_ema = Ema { period: self.fast }.compute(prices);
        let slow_ema = Ema { period: self.slow }.compute(prices);

        // MACD line = fast EMA - slow EMA (valid from index slow - 1).
        let mut macd_line = vec![f64::NAN; n];
        for i in (self.slow - 1)..n {
            if !fast_ema[i].is_nan() && !slow_ema[i].is_nan() {
                macd_line[i] = fast_ema[i] - slow_ema[i];
            }
        }

        // Signal line = EMA(signal) applied to macd_line.
        let macd_start = self.slow - 1;
        let signal_start = macd_start + self.signal - 1;

        let mut signal_line = vec![f64::NAN; n];
        if signal_start < n {
            // Seed with simple mean of first `signal` MACD values.
            let seed: f64 = macd_line[macd_start..macd_start + self.signal]
                .iter()
                .filter(|v| !v.is_nan())
                .sum::<f64>()
                / self.signal as f64;
            signal_line[signal_start] = seed;

            let k = 2.0 / (self.signal as f64 + 1.0);
            for i in (signal_start + 1)..n {
                if !macd_line[i].is_nan() {
                    signal_line[i] = macd_line[i] * k + signal_line[i - 1] * (1.0 - k);
                }
            }
        }

        // Histogram = MACD line - signal line.
        let mut histogram = vec![f64::NAN; n];
        for i in 0..n {
            if !macd_line[i].is_nan() && !signal_line[i].is_nan() {
                histogram[i] = macd_line[i] - signal_line[i];
            }
        }

        (macd_line, signal_line, histogram)
    }
}

/// Bollinger Bands: upper, middle (SMA), and lower bands.
pub struct BollingerBands {
    /// Lookback period for SMA and standard deviation (typically 20).
    pub period: usize,
    /// Number of standard deviations for band width (typically 2.0).
    pub std_devs: f64,
}

impl BollingerBands {
    /// Computes Bollinger Bands.
    ///
    /// Returns `(upper, middle, lower)` each of length `prices.len()`.
    /// Values before the warm-up are `NAN`.
    pub fn compute(&self, prices: &[f64]) -> (Vec<f64>, Vec<f64>, Vec<f64>) {
        let n = prices.len();
        let middle = Sma { period: self.period }.compute(prices);

        let mut upper = vec![f64::NAN; n];
        let mut lower = vec![f64::NAN; n];

        for i in (self.period - 1)..n {
            if middle[i].is_nan() {
                continue;
            }
            let window = &prices[(i + 1 - self.period)..=i];
            let mean = middle[i];
            let variance = window.iter().map(|x| (x - mean).powi(2)).sum::<f64>()
                / self.period as f64;
            let std = variance.sqrt();
            upper[i] = mean + self.std_devs * std;
            lower[i] = mean - self.std_devs * std;
        }

        (upper, middle, lower)
    }
}

/// Average True Range indicator.
pub struct Atr {
    /// Smoothing period (typically 14).
    pub period: usize,
}

impl Atr {
    /// Computes ATR using Wilder's smoothing.
    ///
    /// Returns `Vec<f64>` of the same length as the input slices.
    /// First `period - 1` entries are `NAN`. Requires `high.len() == low.len() == close.len()`.
    pub fn compute(&self, high: &[f64], low: &[f64], close: &[f64]) -> Vec<f64> {
        let n = high.len().min(low.len()).min(close.len());
        let mut result = vec![f64::NAN; n];

        if n < self.period {
            return result;
        }

        // True ranges.
        let mut tr = vec![0.0_f64; n];
        tr[0] = high[0] - low[0];
        for i in 1..n {
            let hl = high[i] - low[i];
            let hc = (high[i] - close[i - 1]).abs();
            let lc = (low[i] - close[i - 1]).abs();
            tr[i] = hl.max(hc).max(lc);
        }

        // Initial ATR = simple mean over first `period` true ranges.
        let initial: f64 = tr[..self.period].iter().sum::<f64>() / self.period as f64;
        result[self.period - 1] = initial;

        // Wilder's smoothing.
        for i in self.period..n {
            result[i] = (result[i - 1] * (self.period - 1) as f64 + tr[i]) / self.period as f64;
        }

        result
    }
}

/// Stochastic oscillator (%K and %D lines).
pub struct Stochastic {
    /// Lookback period for %K calculation (typically 14).
    pub k_period: usize,
    /// Smoothing period for %D (typically 3).
    pub d_period: usize,
}

impl Stochastic {
    /// Computes %K and %D lines.
    ///
    /// Returns `(k_line, d_line)` each of length `high.len()`. Values before warm-up are `NAN`.
    pub fn compute(&self, high: &[f64], low: &[f64], close: &[f64]) -> (Vec<f64>, Vec<f64>) {
        let n = high.len().min(low.len()).min(close.len());
        let mut k_line = vec![f64::NAN; n];

        if n < self.k_period {
            return (k_line.clone(), k_line);
        }

        for i in (self.k_period - 1)..n {
            let window_high = high[(i + 1 - self.k_period)..=i]
                .iter()
                .cloned()
                .fold(f64::NEG_INFINITY, f64::max);
            let window_low = low[(i + 1 - self.k_period)..=i]
                .iter()
                .cloned()
                .fold(f64::INFINITY, f64::min);
            let range = window_high - window_low;
            k_line[i] = if range == 0.0 {
                50.0
            } else {
                100.0 * (close[i] - window_low) / range
            };
        }

        // %D = SMA(d_period) of %K.
        let d_line = Sma { period: self.d_period }.compute(&k_line.iter().map(|v| {
            if v.is_nan() { 0.0 } else { *v }
        }).collect::<Vec<_>>());

        // Re-NAN entries that were NAN in k_line or where d hasn't warmed up.
        let mut d_result = vec![f64::NAN; n];
        let d_start = self.k_period - 1 + self.d_period - 1;
        for i in d_start..n {
            if !k_line[i].is_nan() {
                d_result[i] = d_line[i];
            }
        }

        (k_line, d_result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_prices() -> Vec<f64> {
        vec![
            44.34, 44.09, 44.15, 43.61, 44.33, 44.83, 45.10, 45.15,
            43.61, 44.33, 44.83, 45.10, 45.15, 43.61, 44.33, 44.83,
            45.10, 45.15, 45.98, 45.41, 48.60, 47.60, 47.34, 46.99,
            46.32, 46.00, 45.43, 45.03, 44.32, 44.83,
        ]
    }

    #[test]
    fn test_ema_length() {
        let prices = sample_prices();
        let ema = Ema { period: 5 };
        let result = ema.compute(&prices);
        assert_eq!(result.len(), prices.len());
        assert!(result[3].is_nan());
        assert!(!result[4].is_nan());
    }

    #[test]
    fn test_sma_basic() {
        let prices = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        let sma = Sma { period: 3 };
        let result = sma.compute(&prices);
        assert!(result[0].is_nan());
        assert!(result[1].is_nan());
        assert!((result[2] - 2.0).abs() < 1e-9);
        assert!((result[3] - 3.0).abs() < 1e-9);
        assert!((result[4] - 4.0).abs() < 1e-9);
    }

    #[test]
    fn test_rsi_range() {
        let prices = sample_prices();
        let rsi = Rsi { period: 14 };
        let result = rsi.compute(&prices);
        assert_eq!(result.len(), prices.len());
        for v in result.iter().filter(|v| !v.is_nan()) {
            assert!(*v >= 0.0 && *v <= 100.0, "RSI out of range: {v}");
        }
    }

    #[test]
    fn test_rsi_warm_up() {
        let prices = sample_prices();
        let rsi = Rsi { period: 14 };
        let result = rsi.compute(&prices);
        for i in 0..14 {
            assert!(result[i].is_nan(), "index {i} should be NAN");
        }
        assert!(!result[14].is_nan());
    }

    #[test]
    fn test_macd_lengths() {
        let prices = sample_prices();
        let macd = Macd { fast: 3, slow: 5, signal: 3 };
        let (m, s, h) = macd.compute(&prices);
        assert_eq!(m.len(), prices.len());
        assert_eq!(s.len(), prices.len());
        assert_eq!(h.len(), prices.len());
    }

    #[test]
    fn test_macd_nan_before_warmup() {
        let prices = sample_prices();
        let macd = Macd { fast: 3, slow: 5, signal: 3 };
        let (m, s, _) = macd.compute(&prices);
        // slow period = 5, so macd starts at index 4
        assert!(m[3].is_nan());
        assert!(!m[4].is_nan());
        // signal starts at index 4 + 3 - 1 = 6
        assert!(s[5].is_nan());
        assert!(!s[6].is_nan());
    }

    #[test]
    fn test_bollinger_bands_width() {
        let prices = sample_prices();
        let bb = BollingerBands { period: 5, std_devs: 2.0 };
        let (upper, middle, lower) = bb.compute(&prices);
        for i in 4..prices.len() {
            if !upper[i].is_nan() {
                assert!(upper[i] >= middle[i], "upper >= middle");
                assert!(middle[i] >= lower[i], "middle >= lower");
            }
        }
    }

    #[test]
    fn test_atr_positive() {
        let high: Vec<f64> = sample_prices().iter().map(|p| p + 0.5).collect();
        let low: Vec<f64> = sample_prices().iter().map(|p| p - 0.5).collect();
        let close = sample_prices();
        let atr = Atr { period: 5 };
        let result = atr.compute(&high, &low, &close);
        for v in result.iter().filter(|v| !v.is_nan()) {
            assert!(*v > 0.0, "ATR should be positive, got {v}");
        }
    }

    #[test]
    fn test_atr_warm_up() {
        let n = 20;
        let high = vec![10.0_f64; n];
        let low = vec![9.0_f64; n];
        let close = vec![9.5_f64; n];
        let atr = Atr { period: 5 };
        let result = atr.compute(&high, &low, &close);
        for i in 0..4 {
            assert!(result[i].is_nan(), "index {i} should be NAN");
        }
        assert!(!result[4].is_nan());
    }

    #[test]
    fn test_stochastic_range() {
        let prices = sample_prices();
        let high: Vec<f64> = prices.iter().map(|p| p + 0.3).collect();
        let low: Vec<f64> = prices.iter().map(|p| p - 0.3).collect();
        let stoch = Stochastic { k_period: 5, d_period: 3 };
        let (k, d) = stoch.compute(&high, &low, &prices);
        for v in k.iter().filter(|v| !v.is_nan()) {
            assert!(*v >= 0.0 && *v <= 100.0, "K out of range: {v}");
        }
        for v in d.iter().filter(|v| !v.is_nan()) {
            assert!(*v >= 0.0 && *v <= 100.0, "D out of range: {v}");
        }
    }

    #[test]
    fn test_ema_monotone_up() {
        // Steadily rising prices: EMA should trend upward.
        let prices: Vec<f64> = (1..=20).map(|i| i as f64).collect();
        let ema = Ema { period: 5 };
        let result = ema.compute(&prices);
        let valid: Vec<f64> = result.iter().filter(|v| !v.is_nan()).cloned().collect();
        for i in 1..valid.len() {
            assert!(valid[i] >= valid[i - 1], "EMA should increase: {} < {}", valid[i], valid[i - 1]);
        }
    }
}
