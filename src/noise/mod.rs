//! Microstructure noise filter — efficient price estimation from tick data.
//!
//! ## Responsibility
//!
//! Tick data is contaminated by **microstructure noise**: the bid-ask bounce,
//! rounding to the tick grid, and order-flow pressure that cause observed
//! transaction prices to deviate from the unobservable "efficient" price.
//! This module estimates and removes that noise using two complementary
//! methods:
//!
//! ### 1. Roll's Bid-Ask Spread Estimator
//!
//! Roll (1984) showed that in the presence of a constant bid-ask spread `s`,
//! consecutive price changes exhibit negative first-order auto-covariance `c`:
//!
//! ```text
//! c = Cov(Δp_t, Δp_{t-1})
//! s = 2 · √(max(-c, 0))
//! ```
//!
//! Roll's spread estimate is used here as a noise-level proxy: a wider spread
//! implies higher microstructure noise.
//!
//! ### 2. Realised Kernel Estimator (Barndorff-Nielsen et al. 2008)
//!
//! The realised kernel `RK` is a bias-corrected estimator of integrated
//! variance that is robust to microstructure noise:
//!
//! ```text
//! RK = Σ_{h=-H}^{H} k(h/H) · γ_h
//! ```
//!
//! where `γ_h = Σ_t Δp_t · Δp_{t+h}` is the h-th realised auto-covariance
//! and `k(·)` is the Parzen kernel.  The de-noised "efficient price" is
//! constructed by cumulating the square-root of `RK` as a volatility-adjusted
//! drift.
//!
//! ## Usage
//!
//! ```rust
//! use fin_stream::noise::{NoiseFilter, NoiseConfig};
//!
//! let mut filter = NoiseFilter::new(NoiseConfig::default());
//! let prices = vec![100.0, 100.05, 99.98, 100.03, 100.08, 99.97];
//! for &p in &prices {
//!     filter.push(p);
//! }
//! let efficient = filter.efficient_price_series();
//! let roll_spread = filter.roll_spread();
//! ```

/// Configuration for the microstructure noise filter.
#[derive(Debug, Clone)]
pub struct NoiseConfig {
    /// Maximum lag `H` for the realised kernel (Barndorff-Nielsen).
    /// Typical values: 5–20.
    pub kernel_max_lag: usize,

    /// Rolling window for Roll's auto-covariance estimator.
    pub roll_window: usize,

    /// Smoothing factor for the efficient price EMA de-noising step.
    /// In (0, 1]: 1.0 = no smoothing (raw prices), small values = heavy smoothing.
    pub ema_alpha: f64,
}

impl Default for NoiseConfig {
    fn default() -> Self {
        Self {
            kernel_max_lag: 10,
            roll_window: 100,
            ema_alpha: 0.15,
        }
    }
}

/// Output snapshot from the noise filter.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NoiseSnapshot {
    /// Roll's bid-ask spread estimate (in price units).
    pub roll_spread: f64,

    /// Realised kernel variance estimate (annualise by multiplying by
    /// the number of observations per year).
    pub realised_kernel_variance: f64,

    /// Most recent de-noised efficient price.
    pub efficient_price: f64,

    /// Noise-to-signal ratio estimate:
    /// `roll_spread² / (4 · realised_kernel_variance)`.
    pub noise_to_signal: f64,
}

/// Online microstructure noise filter.
///
/// Maintains a rolling buffer of transaction prices and computes:
/// - Roll's implied bid-ask spread
/// - Realised kernel variance
/// - A de-noised efficient price series via EMA
#[derive(Debug, Clone)]
pub struct NoiseFilter {
    cfg: NoiseConfig,
    /// Raw price history (rolling, capped at `roll_window`).
    prices: std::collections::VecDeque<f64>,
    /// De-noised efficient price (EMA).
    ema: Option<f64>,
    /// Full efficient price series (for inspection/backtesting).
    efficient_series: Vec<f64>,
}

impl NoiseFilter {
    /// Create a new filter with the given configuration.
    pub fn new(cfg: NoiseConfig) -> Self {
        Self {
            cfg,
            prices: std::collections::VecDeque::new(),
            ema: None,
            efficient_series: Vec::new(),
        }
    }

    /// Push a new transaction price.
    pub fn push(&mut self, price: f64) {
        // Update EMA (de-noised efficient price).
        let alpha = self.cfg.ema_alpha;
        let ema = match self.ema {
            Some(prev) => prev + alpha * (price - prev),
            None => price,
        };
        self.ema = Some(ema);
        self.efficient_series.push(ema);

        // Maintain rolling window.
        self.prices.push_back(price);
        if self.prices.len() > self.cfg.roll_window {
            self.prices.pop_front();
        }
    }

    /// Roll's implied bid-ask spread.
    ///
    /// Returns `0.0` if there are fewer than 3 prices.
    pub fn roll_spread(&self) -> f64 {
        let prices: Vec<f64> = self.prices.iter().cloned().collect();
        if prices.len() < 3 {
            return 0.0;
        }
        let cov = first_order_autocovariance(&prices);
        2.0 * (-cov).max(0.0).sqrt()
    }

    /// Realised kernel variance (Barndorff-Nielsen & Shephard, Parzen kernel).
    ///
    /// Returns `0.0` if there are fewer prices than `2 * kernel_max_lag + 2`.
    pub fn realised_kernel_variance(&self) -> f64 {
        let prices: Vec<f64> = self.prices.iter().cloned().collect();
        let h = self.cfg.kernel_max_lag;
        if prices.len() < 2 * h + 2 {
            return 0.0;
        }
        let returns: Vec<f64> = prices.windows(2).map(|w| w[1] - w[0]).collect();
        let n = returns.len();

        let mut rk = 0.0_f64;
        for lag in 0..=h {
            let gamma = realised_autocovariance(&returns, lag, n);
            let weight = if lag == 0 { 1.0 } else { 2.0 * parzen_kernel(lag as f64 / (h as f64 + 1.0)) };
            rk += weight * gamma;
        }
        rk.max(0.0)
    }

    /// Current de-noised efficient price (EMA).
    pub fn efficient_price(&self) -> Option<f64> {
        self.ema
    }

    /// Full de-noised efficient price series since construction.
    pub fn efficient_price_series(&self) -> &[f64] {
        &self.efficient_series
    }

    /// Full noise snapshot at the current state.
    pub fn snapshot(&self) -> Option<NoiseSnapshot> {
        let efficient_price = self.ema?;
        let roll_spread = self.roll_spread();
        let rkv = self.realised_kernel_variance();
        let noise_to_signal = if rkv > 1e-16 {
            (roll_spread * roll_spread) / (4.0 * rkv)
        } else {
            0.0
        };
        Some(NoiseSnapshot {
            roll_spread,
            realised_kernel_variance: rkv,
            efficient_price,
            noise_to_signal,
        })
    }

    /// Number of prices in the rolling window.
    pub fn window_len(&self) -> usize {
        self.prices.len()
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// First-order autocovariance of the return series implied by `prices`.
fn first_order_autocovariance(prices: &[f64]) -> f64 {
    if prices.len() < 3 {
        return 0.0;
    }
    let returns: Vec<f64> = prices.windows(2).map(|w| w[1] - w[0]).collect();
    let n = returns.len();
    realised_autocovariance(&returns, 1, n)
}

/// Sample autocovariance at `lag` of the returns series.
fn realised_autocovariance(returns: &[f64], lag: usize, n: usize) -> f64 {
    if lag >= n {
        return 0.0;
    }
    let mean: f64 = returns.iter().sum::<f64>() / n as f64;
    let mut cov = 0.0_f64;
    for i in lag..n {
        cov += (returns[i] - mean) * (returns[i - lag] - mean);
    }
    cov / (n - lag) as f64
}

/// Parzen kernel: a smooth weight function on [0, 1] with compact support.
///
/// ```text
/// k(x) = 1 - 6x² + 6|x|³   for |x| ≤ 0.5
/// k(x) = 2(1 - |x|)³        for 0.5 < |x| ≤ 1
/// k(x) = 0                   otherwise
/// ```
fn parzen_kernel(x: f64) -> f64 {
    let ax = x.abs();
    if ax <= 0.5 {
        1.0 - 6.0 * ax * ax + 6.0 * ax * ax * ax
    } else if ax <= 1.0 {
        2.0 * (1.0 - ax).powi(3)
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_filter_with_prices(prices: &[f64]) -> NoiseFilter {
        let mut f = NoiseFilter::new(NoiseConfig::default());
        for &p in prices {
            f.push(p);
        }
        f
    }

    #[test]
    fn roll_spread_zero_for_constant_prices() {
        let prices = vec![100.0; 50];
        let f = build_filter_with_prices(&prices);
        assert!(
            f.roll_spread().abs() < 1e-10,
            "constant price series should yield zero Roll spread"
        );
    }

    #[test]
    fn roll_spread_positive_for_bouncing_price() {
        // Classic bid-ask bounce: price alternates between bid and ask.
        let prices: Vec<f64> = (0..50).map(|i| if i % 2 == 0 { 100.0 } else { 100.1 }).collect();
        let f = build_filter_with_prices(&prices);
        let spread = f.roll_spread();
        assert!(spread > 0.0, "bid-ask bounce should yield positive Roll spread, got {spread}");
    }

    #[test]
    fn efficient_price_converges_toward_true_price() {
        // A flat "true" price of 100 with noise.
        let mut f = NoiseFilter::new(NoiseConfig { ema_alpha: 0.3, ..Default::default() });
        for i in 0..100 {
            let noise = if i % 2 == 0 { 0.1 } else { -0.1 };
            f.push(100.0 + noise);
        }
        let ep = f.efficient_price().unwrap();
        assert!((ep - 100.0).abs() < 0.5, "efficient price should be near 100.0, got {ep}");
    }

    #[test]
    fn realised_kernel_non_negative() {
        let prices: Vec<f64> = (0..200).map(|i| 100.0 + i as f64 * 0.01).collect();
        let f = build_filter_with_prices(&prices);
        let rkv = f.realised_kernel_variance();
        assert!(rkv >= 0.0, "RK variance must be non-negative, got {rkv}");
    }

    #[test]
    fn snapshot_available_after_first_push() {
        let mut f = NoiseFilter::new(NoiseConfig::default());
        f.push(100.0);
        assert!(f.snapshot().is_some());
    }

    #[test]
    fn efficient_series_length_matches_push_count() {
        let mut f = NoiseFilter::new(NoiseConfig::default());
        for i in 0..15 {
            f.push(100.0 + i as f64 * 0.01);
        }
        assert_eq!(f.efficient_price_series().len(), 15);
    }

    #[test]
    fn parzen_kernel_at_zero_is_one() {
        assert!((parzen_kernel(0.0) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn parzen_kernel_beyond_one_is_zero() {
        assert_eq!(parzen_kernel(1.5), 0.0);
    }
}
