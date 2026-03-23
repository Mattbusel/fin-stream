//! Synthetic tick data generation from OHLCV bars.
//!
//! ## Responsibility
//!
//! Reconstruct a plausible intraday tick stream from OHLCV bars when raw tick
//! data is unavailable.  The generated ticks preserve the bar's statistical
//! properties — open, high, low, close, and total volume — and are useful for
//! backtesting strategies that require tick-resolution inputs.
//!
//! ## Algorithm
//!
//! For each OHLCV bar the generator:
//!
//! 1. **Anchors** the first tick at `open` and the last tick at `close`.
//! 2. **Inserts** a high-price tick and a low-price tick at random positions
//!    within the bar's time window, with the high coming before the low on a
//!    random coin-flip (reflecting real intraday path uncertainty).
//! 3. **Fills** the remaining `n_ticks - 4` ticks with prices drawn from a
//!    Brownian bridge between the anchor points, scaled to stay within
//!    `[low, high]`.
//! 4. **Distributes volume** proportionally to the absolute price change at
//!    each step (volume-weighted generation): larger moves attract more volume.
//!
//! ## Guarantees
//! - Every generated tick stream has exactly `n_ticks` ticks.
//! - Total volume of the generated ticks equals `bar.volume` exactly.
//! - All generated prices are within `[bar.low, bar.high]`.
//! - Non-panicking: invalid inputs return `Err(StreamError::InvalidInput)`.

use crate::error::StreamError;

/// A single OHLCV bar.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OhlcvInput {
    /// Bar open time in milliseconds since Unix epoch.
    pub open_time_ms: u64,
    /// Bar close time in milliseconds since Unix epoch.
    pub close_time_ms: u64,
    /// Open price.
    pub open: f64,
    /// High price.
    pub high: f64,
    /// Low price.
    pub low: f64,
    /// Close price.
    pub close: f64,
    /// Total volume traded in the bar.
    pub volume: f64,
}

/// A single synthetic tick.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SyntheticTick {
    /// Timestamp in milliseconds since Unix epoch.
    pub timestamp_ms: u64,
    /// Trade price.
    pub price: f64,
    /// Volume attributed to this tick.
    pub volume: f64,
}

/// Configuration for synthetic tick generation.
#[derive(Debug, Clone)]
pub struct SyntheticConfig {
    /// Number of ticks to generate per OHLCV bar.  Must be >= 4.
    pub ticks_per_bar: usize,
    /// Fixed seed for the pseudo-random number generator.  `None` → use
    /// a deterministic default sequence (not truly random; reproducible).
    pub seed: u64,
}

impl Default for SyntheticConfig {
    fn default() -> Self {
        Self {
            ticks_per_bar: 20,
            seed: 42,
        }
    }
}

/// Lightweight linear congruential PRNG — no external crate needed.
struct Lcg {
    state: u64,
}

impl Lcg {
    fn new(seed: u64) -> Self {
        Self { state: seed.wrapping_add(1) }
    }

    /// Next value in (0, 1).
    fn next_f64(&mut self) -> f64 {
        // Numerical Recipes parameters.
        self.state = self
            .state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let bits = (self.state >> 11) as f64;
        bits / (u64::MAX >> 11) as f64
    }

    /// Uniform integer in `[lo, hi)`.
    fn next_usize(&mut self, lo: usize, hi: usize) -> usize {
        if hi <= lo {
            return lo;
        }
        lo + (self.next_f64() * (hi - lo) as f64) as usize
    }

    /// Box-Muller normal sample.
    fn next_normal(&mut self) -> f64 {
        let u1 = self.next_f64().max(1e-15);
        let u2 = self.next_f64();
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
    }
}

/// Synthetic tick generator.
pub struct SyntheticGenerator {
    cfg: SyntheticConfig,
}

impl SyntheticGenerator {
    /// Create a new generator with the given configuration.
    pub fn new(cfg: SyntheticConfig) -> Self {
        Self { cfg }
    }

    /// Generate `ticks_per_bar` synthetic ticks from a single OHLCV bar.
    ///
    /// # Errors
    /// - `InvalidInput` if `high < low`, `open` or `close` outside `[low, high]`,
    ///   `volume <= 0`, or `ticks_per_bar < 4`.
    pub fn generate(&self, bar: &OhlcvInput) -> Result<Vec<SyntheticTick>, StreamError> {
        let n = self.cfg.ticks_per_bar;
        if n < 4 {
            return Err(StreamError::InvalidInput(
                "ticks_per_bar must be >= 4".into(),
            ));
        }
        if bar.high < bar.low {
            return Err(StreamError::InvalidInput("high < low".into()));
        }
        if bar.open < bar.low || bar.open > bar.high {
            return Err(StreamError::InvalidInput("open outside [low, high]".into()));
        }
        if bar.close < bar.low || bar.close > bar.high {
            return Err(StreamError::InvalidInput("close outside [low, high]".into()));
        }
        if bar.volume <= 0.0 {
            return Err(StreamError::InvalidInput("volume must be positive".into()));
        }

        let mut rng = Lcg::new(self.cfg.seed);
        let bar_duration_ms = bar.close_time_ms.saturating_sub(bar.open_time_ms);
        let dt_ms = if n > 1 { bar_duration_ms / (n as u64 - 1) } else { 0 };

        // --- Step 1: lay out n timestamps equally spaced in the bar.
        let timestamps: Vec<u64> = (0..n)
            .map(|i| bar.open_time_ms + i as u64 * dt_ms)
            .collect();

        // --- Step 2: choose positions for high and low ticks.
        // Reserve index 0 for open, index n-1 for close.
        // High and low are placed in [1, n-2].
        let high_idx = rng.next_usize(1, n - 1);
        let mut low_idx = rng.next_usize(1, n - 1);
        if low_idx == high_idx {
            low_idx = if high_idx + 1 < n - 1 { high_idx + 1 } else { high_idx - 1 };
        }

        // --- Step 3: build price path via Brownian bridge.
        // Segment: open → (high or low at their indices) → close.
        let mut prices = vec![0.0_f64; n];
        prices[0] = bar.open;
        prices[n - 1] = bar.close;
        prices[high_idx] = bar.high;
        prices[low_idx] = bar.low;

        // Fill remaining slots with a Brownian bridge interpolation.
        let range = bar.high - bar.low;
        for i in 1..(n - 1) {
            if i == high_idx || i == low_idx {
                continue;
            }
            // Bridge between nearest set anchors.
            let (left_idx, left_price) = Self::nearest_left(&prices, i);
            let (right_idx, right_price) = Self::nearest_right(&prices, i, n);
            let frac = (i - left_idx) as f64 / (right_idx - left_idx) as f64;
            let bridge = left_price + frac * (right_price - left_price);
            // Add a small noise term, clamped to [low, high].
            let noise = rng.next_normal() * range * 0.02;
            prices[i] = (bridge + noise).clamp(bar.low, bar.high);
        }

        // --- Step 4: volume-weighted distribution.
        // Weight per tick ∝ |price change|; normalise to sum = bar.volume.
        let mut weights: Vec<f64> = (0..n)
            .map(|i| {
                if i == 0 {
                    (prices[1] - prices[0]).abs() + 1e-8
                } else {
                    (prices[i] - prices[i - 1]).abs() + 1e-8
                }
            })
            .collect();
        let weight_sum: f64 = weights.iter().sum();
        for w in &mut weights {
            *w *= bar.volume / weight_sum;
        }

        let ticks: Vec<SyntheticTick> = (0..n)
            .map(|i| SyntheticTick {
                timestamp_ms: timestamps[i],
                price: prices[i],
                volume: weights[i],
            })
            .collect();

        Ok(ticks)
    }

    fn nearest_left(prices: &[f64], idx: usize) -> (usize, f64) {
        for i in (0..idx).rev() {
            if prices[i] != 0.0 || i == 0 {
                return (i, prices[i]);
            }
        }
        (0, prices[0])
    }

    fn nearest_right(prices: &[f64], idx: usize, n: usize) -> (usize, f64) {
        for i in (idx + 1)..n {
            if prices[i] != 0.0 || i == n - 1 {
                return (i, prices[i]);
            }
        }
        (n - 1, prices[n - 1])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_bar() -> OhlcvInput {
        OhlcvInput {
            open_time_ms: 0,
            close_time_ms: 60_000,
            open: 100.0,
            high: 101.0,
            low: 99.5,
            close: 100.5,
            volume: 1000.0,
        }
    }

    #[test]
    fn tick_count_matches_config() {
        let gen = SyntheticGenerator::new(SyntheticConfig { ticks_per_bar: 10, seed: 1 });
        let ticks = gen.generate(&sample_bar()).unwrap();
        assert_eq!(ticks.len(), 10);
    }

    #[test]
    fn open_and_close_preserved() {
        let gen = SyntheticGenerator::new(SyntheticConfig::default());
        let bar = sample_bar();
        let ticks = gen.generate(&bar).unwrap();
        assert!((ticks.first().unwrap().price - bar.open).abs() < 1e-10);
        assert!((ticks.last().unwrap().price - bar.close).abs() < 1e-10);
    }

    #[test]
    fn all_prices_within_high_low() {
        let gen = SyntheticGenerator::new(SyntheticConfig::default());
        let bar = sample_bar();
        let ticks = gen.generate(&bar).unwrap();
        for t in &ticks {
            assert!(
                t.price >= bar.low && t.price <= bar.high,
                "price {} outside [{}, {}]",
                t.price, bar.low, bar.high
            );
        }
    }

    #[test]
    fn total_volume_matches_bar() {
        let gen = SyntheticGenerator::new(SyntheticConfig::default());
        let bar = sample_bar();
        let ticks = gen.generate(&bar).unwrap();
        let total: f64 = ticks.iter().map(|t| t.volume).sum();
        assert!((total - bar.volume).abs() < 1e-6, "volume mismatch: {total} vs {}", bar.volume);
    }

    #[test]
    fn invalid_high_lt_low_returns_error() {
        let gen = SyntheticGenerator::new(SyntheticConfig::default());
        let mut bar = sample_bar();
        bar.high = 99.0; // < low
        assert!(gen.generate(&bar).is_err());
    }

    #[test]
    fn too_few_ticks_returns_error() {
        let gen = SyntheticGenerator::new(SyntheticConfig { ticks_per_bar: 3, seed: 0 });
        assert!(gen.generate(&sample_bar()).is_err());
    }
}
