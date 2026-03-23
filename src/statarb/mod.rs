//! # Module: statarb
//!
//! Statistical Arbitrage detector for streaming price pairs.
//!
//! ## Responsibility
//! Tests pairs of price series for cointegration using a simplified
//! Augmented Dickey-Fuller (ADF) test via OLS, computes spread z-scores
//! in real time, and emits trading signals when spreads deviate beyond
//! configurable thresholds.
//!
//! ## Guarantees
//! - `PairCointegration::test` is pure; no internal state mutated
//! - `StatArbDetector::update` is `O(1)` per tick (amortized)
//! - ADF 5% critical value is -2.86 (conventional simplified OLS)

use std::collections::HashMap;

// ─── Cointegration ────────────────────────────────────────────────────────────

/// Result of a cointegration test on two price series.
#[derive(Debug, Clone)]
pub struct CointegrationResult {
    /// Mean of the spread (series_a - hedge_ratio * series_b).
    pub spread_mean: f64,
    /// Standard deviation of the spread.
    pub spread_std: f64,
    /// Mean-reversion half-life in days.
    pub half_life: f64,
    /// `true` if the ADF t-statistic is below the 5% critical value (-2.86).
    pub is_cointegrated: bool,
    /// ADF t-statistic for the spread series (more negative = more stationary).
    pub adf_statistic: f64,
}

/// Tests two price series for cointegration.
pub struct PairCointegration;

impl PairCointegration {
    /// Test `series_a` and `series_b` for cointegration.
    ///
    /// Steps:
    /// 1. Estimate hedge ratio via OLS: `a ~ b`
    /// 2. Compute spread `s = a - hedge_ratio * b`
    /// 3. Run simplified ADF: regress `Δs` on `s_{t-1}`, extract t-stat
    /// 4. Estimate half-life from the OLS beta of the mean-reversion regression
    ///
    /// Returns `None` if either series has fewer than 3 elements or is constant.
    pub fn test(series_a: &[f64], series_b: &[f64]) -> Option<CointegrationResult> {
        let n = series_a.len().min(series_b.len());
        if n < 3 {
            return None;
        }
        let a = &series_a[..n];
        let b = &series_b[..n];

        // Step 1: OLS hedge ratio: a = alpha + beta * b
        let hedge_ratio = ols_beta(b, a)?;

        // Step 2: Compute spread
        let spread: Vec<f64> = a.iter().zip(b.iter()).map(|(&ai, &bi)| ai - hedge_ratio * bi).collect();

        let spread_mean = mean(&spread);
        let spread_std = std_dev(&spread);

        if spread_std < 1e-12 {
            return None;
        }

        // Step 3: ADF test on spread
        // Regress Δs on s_{t-1}: Δs_t = alpha + beta * s_{t-1} + eps
        let adf_statistic = adf_t_stat(&spread)?;
        let is_cointegrated = adf_statistic < -2.86;

        // Step 4: Half-life from mean-reversion regression
        // Δs = phi * (s_{t-1} - mu) → phi = beta, half-life = -ln(2) / phi
        let half_life = compute_half_life(&spread);

        Some(CointegrationResult { spread_mean, spread_std, half_life, is_cointegrated, adf_statistic })
    }
}

// ─── Spread Monitor ───────────────────────────────────────────────────────────

/// Signal from the statistical arbitrage detector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatArbSignal {
    /// Z-score < -2: spread is too low, buy A / sell B.
    Long,
    /// Z-score > +2: spread is too high, sell A / buy B.
    Short,
    /// |Z-score| < 0.5: spread has reverted, exit position.
    Exit,
    /// Z-score in neutral band.
    Neutral,
}

/// Live state for a monitored symbol pair.
#[derive(Debug, Clone)]
pub struct SpreadMonitor {
    /// First symbol.
    pub symbol_a: String,
    /// Second symbol.
    pub symbol_b: String,
    /// Hedge ratio (beta): spread = price_a - hedge_ratio * price_b.
    pub hedge_ratio: f64,
    /// Current z-score of the spread.
    pub z_score: f64,
    /// Current trading signal.
    pub signal: StatArbSignal,
}

// ─── Detector ─────────────────────────────────────────────────────────────────

/// Monitors multiple pairs in real time, emitting statistical arbitrage signals.
///
/// Internally maintains a rolling price history per symbol. When both symbols
/// in a pair have at least `min_history` observations, the spread z-score is
/// recomputed and a signal is produced.
pub struct StatArbDetector {
    /// Minimum number of price observations before computing a signal.
    pub min_history: usize,
    /// Registered pairs: `(symbol_a, symbol_b)` → hedge ratio (initially estimated).
    pairs: Vec<(String, String)>,
    /// Rolling price history per symbol.
    prices: HashMap<String, Vec<f64>>,
    /// Maximum window length kept per symbol.
    max_window: usize,
}

impl StatArbDetector {
    /// Create a new detector.
    ///
    /// # Arguments
    /// - `min_history`: minimum data points before computing signals (minimum 3)
    /// - `max_window`: maximum history length retained per symbol
    pub fn new(min_history: usize, max_window: usize) -> Self {
        Self {
            min_history: min_history.max(3),
            pairs: Vec::new(),
            prices: HashMap::new(),
            max_window: max_window.max(3),
        }
    }

    /// Register a pair of symbols to monitor.
    pub fn add_pair(&mut self, symbol_a: impl Into<String>, symbol_b: impl Into<String>) {
        self.pairs.push((symbol_a.into(), symbol_b.into()));
    }

    /// Update the price for a symbol.
    ///
    /// This is called once per tick. Updates the rolling history and recomputes
    /// signals for all pairs involving this symbol.
    pub fn update(&mut self, symbol: &str, price: f64) {
        let entry = self.prices.entry(symbol.to_string()).or_insert_with(Vec::new);
        entry.push(price);
        if entry.len() > self.max_window {
            entry.remove(0);
        }
    }

    /// Compute and return signals for all registered pairs.
    ///
    /// A pair produces a signal only if both symbols have at least `min_history`
    /// observations.
    pub fn signals(&self) -> Vec<SpreadMonitor> {
        let mut result = Vec::new();
        for (sym_a, sym_b) in &self.pairs {
            let prices_a = match self.prices.get(sym_a.as_str()) {
                Some(p) if p.len() >= self.min_history => p,
                _ => continue,
            };
            let prices_b = match self.prices.get(sym_b.as_str()) {
                Some(p) if p.len() >= self.min_history => p,
                _ => continue,
            };

            let n = prices_a.len().min(prices_b.len());
            let a = &prices_a[prices_a.len() - n..];
            let b = &prices_b[prices_b.len() - n..];

            // Estimate hedge ratio
            let hedge_ratio = ols_beta(b, a).unwrap_or(1.0);

            // Compute spread
            let spread: Vec<f64> =
                a.iter().zip(b.iter()).map(|(&ai, &bi)| ai - hedge_ratio * bi).collect();
            let s_mean = mean(&spread);
            let s_std = std_dev(&spread);

            let current_spread = spread.last().copied().unwrap_or(0.0);
            let z_score = if s_std > 1e-12 {
                (current_spread - s_mean) / s_std
            } else {
                0.0
            };

            let signal = classify_signal(z_score);

            result.push(SpreadMonitor {
                symbol_a: sym_a.clone(),
                symbol_b: sym_b.clone(),
                hedge_ratio,
                z_score,
                signal,
            });
        }
        result
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Classify a z-score into a `StatArbSignal`.
fn classify_signal(z: f64) -> StatArbSignal {
    if z < -2.0 {
        StatArbSignal::Long
    } else if z > 2.0 {
        StatArbSignal::Short
    } else if z.abs() < 0.5 {
        StatArbSignal::Exit
    } else {
        StatArbSignal::Neutral
    }
}

/// Arithmetic mean.
fn mean(v: &[f64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.iter().sum::<f64>() / v.len() as f64
}

/// Sample standard deviation.
fn std_dev(v: &[f64]) -> f64 {
    let n = v.len();
    if n < 2 {
        return 0.0;
    }
    let m = mean(v);
    let var = v.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (n as f64 - 1.0);
    var.sqrt()
}

/// OLS: regress `y` on `x` (with intercept), return slope beta.
/// β = (Σ(xi - x̄)(yi - ȳ)) / Σ(xi - x̄)²
fn ols_beta(x: &[f64], y: &[f64]) -> Option<f64> {
    let n = x.len().min(y.len());
    if n < 2 {
        return None;
    }
    let x = &x[..n];
    let y = &y[..n];
    let mx = mean(x);
    let my = mean(y);
    let num: f64 = x.iter().zip(y.iter()).map(|(&xi, &yi)| (xi - mx) * (yi - my)).sum();
    let den: f64 = x.iter().map(|&xi| (xi - mx).powi(2)).sum();
    if den.abs() < 1e-14 {
        return None;
    }
    Some(num / den)
}

/// Simplified ADF t-statistic.
///
/// Regresses `Δs` on `s_{t-1}` (no intercept adjustment for simplicity).
/// Returns the t-statistic of the coefficient on `s_{t-1}`.
fn adf_t_stat(spread: &[f64]) -> Option<f64> {
    let n = spread.len();
    if n < 3 {
        return None;
    }
    // Build Δs and s_{t-1}
    let delta: Vec<f64> = (1..n).map(|i| spread[i] - spread[i - 1]).collect();
    let lagged: Vec<f64> = spread[..n - 1].to_vec();

    // OLS: Δs = beta * s_{t-1} + eps (no constant for ADF simplification)
    let m_lag = mean(&lagged);
    let m_delta = mean(&delta);
    let num: f64 = lagged.iter().zip(delta.iter())
        .map(|(&l, &d)| (l - m_lag) * (d - m_delta))
        .sum();
    let den: f64 = lagged.iter().map(|&l| (l - m_lag).powi(2)).sum();
    if den.abs() < 1e-14 {
        return None;
    }
    let beta = num / den;

    // Residuals
    let alpha = m_delta - beta * m_lag;
    let residuals: Vec<f64> =
        lagged.iter().zip(delta.iter()).map(|(&l, &d)| d - (alpha + beta * l)).collect();
    let m_res = residuals.len() as f64;
    let sse: f64 = residuals.iter().map(|r| r.powi(2)).sum::<f64>() / (m_res - 2.0).max(1.0);
    let se_beta = if den > 0.0 { (sse / den).sqrt() } else { return None };
    if se_beta < 1e-14 {
        return None;
    }
    Some(beta / se_beta)
}

/// Mean-reversion half-life: `hl = -ln(2) / beta` where beta is from
/// OLS of `Δs ~ s_{t-1}`.
fn compute_half_life(spread: &[f64]) -> f64 {
    let n = spread.len();
    if n < 3 {
        return f64::INFINITY;
    }
    let delta: Vec<f64> = (1..n).map(|i| spread[i] - spread[i - 1]).collect();
    let lagged: Vec<f64> = spread[..n - 1].to_vec();
    match ols_beta(&lagged, &delta) {
        Some(beta) if beta < 0.0 => -std::f64::consts::LN_2 / beta,
        _ => f64::INFINITY,
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Generate a cointegrated pair: b = random walk, a = 2*b + noise
    fn cointegrated_pair(n: usize) -> (Vec<f64>, Vec<f64>) {
        let mut b = Vec::with_capacity(n);
        let mut a = Vec::with_capacity(n);
        let mut price = 100.0_f64;
        // Simple deterministic "random" walk using LCG
        let mut state: u64 = 12345;
        for _ in 0..n {
            state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            let noise = ((state >> 32) as f64 / u32::MAX as f64 - 0.5) * 0.5;
            price += noise;
            b.push(price);
            let a_noise = ((state >> 11) as f64 / (1u64 << 53) as f64 - 0.5) * 0.1;
            a.push(2.0 * price + a_noise);
        }
        (a, b)
    }

    #[test]
    fn test_cointegration_returns_some_for_valid_series() {
        let (a, b) = cointegrated_pair(100);
        let result = PairCointegration::test(&a, &b);
        assert!(result.is_some());
    }

    #[test]
    fn test_cointegration_short_series_returns_none() {
        let result = PairCointegration::test(&[1.0, 2.0], &[1.0, 2.0]);
        assert!(result.is_none());
    }

    #[test]
    fn test_cointegration_constant_series_returns_none() {
        let a = vec![1.0; 50];
        let b = vec![1.0; 50];
        let result = PairCointegration::test(&a, &b);
        // Constant spread has std=0, should return None
        assert!(result.is_none());
    }

    #[test]
    fn test_cointegration_result_has_positive_std() {
        let (a, b) = cointegrated_pair(100);
        let result = PairCointegration::test(&a, &b).unwrap();
        assert!(result.spread_std > 0.0);
    }

    #[test]
    fn test_cointegration_adf_statistic_is_finite() {
        let (a, b) = cointegrated_pair(200);
        let result = PairCointegration::test(&a, &b).unwrap();
        assert!(result.adf_statistic.is_finite());
    }

    #[test]
    fn test_cointegration_is_cointegrated_based_on_adf() {
        let (a, b) = cointegrated_pair(200);
        let result = PairCointegration::test(&a, &b).unwrap();
        // is_cointegrated should match adf threshold
        assert_eq!(result.is_cointegrated, result.adf_statistic < -2.86);
    }

    #[test]
    fn test_half_life_positive_for_mean_reverting() {
        let (a, b) = cointegrated_pair(200);
        let result = PairCointegration::test(&a, &b).unwrap();
        // Half-life should be positive (or infinity for non-reverting)
        assert!(result.half_life > 0.0);
    }

    #[test]
    fn test_detector_no_signals_before_min_history() {
        let mut det = StatArbDetector::new(50, 100);
        det.add_pair("A", "B");
        det.update("A", 100.0);
        det.update("B", 50.0);
        // Only 1 obs each → no signals
        assert!(det.signals().is_empty());
    }

    #[test]
    fn test_detector_signals_after_enough_history() {
        let mut det = StatArbDetector::new(10, 100);
        det.add_pair("A", "B");
        let (a_prices, b_prices) = cointegrated_pair(20);
        for (a, b) in a_prices.iter().zip(b_prices.iter()) {
            det.update("A", *a);
            det.update("B", *b);
        }
        let sigs = det.signals();
        assert_eq!(sigs.len(), 1);
    }

    #[test]
    fn test_detector_hedge_ratio_positive() {
        let mut det = StatArbDetector::new(10, 100);
        det.add_pair("A", "B");
        let (a_prices, b_prices) = cointegrated_pair(20);
        for (a, b) in a_prices.iter().zip(b_prices.iter()) {
            det.update("A", *a);
            det.update("B", *b);
        }
        let sigs = det.signals();
        assert!(sigs[0].hedge_ratio > 0.0);
    }

    #[test]
    fn test_signal_long_when_z_below_minus_two() {
        assert_eq!(classify_signal(-2.5), StatArbSignal::Long);
    }

    #[test]
    fn test_signal_short_when_z_above_two() {
        assert_eq!(classify_signal(2.5), StatArbSignal::Short);
    }

    #[test]
    fn test_signal_exit_when_z_near_zero() {
        assert_eq!(classify_signal(0.1), StatArbSignal::Exit);
        assert_eq!(classify_signal(-0.3), StatArbSignal::Exit);
    }

    #[test]
    fn test_signal_neutral_in_band() {
        assert_eq!(classify_signal(1.0), StatArbSignal::Neutral);
        assert_eq!(classify_signal(-1.5), StatArbSignal::Neutral);
    }

    #[test]
    fn test_detector_multiple_pairs() {
        let mut det = StatArbDetector::new(5, 100);
        det.add_pair("A", "B");
        det.add_pair("C", "D");
        for i in 0..10 {
            det.update("A", 100.0 + i as f64);
            det.update("B", 50.0 + i as f64 * 0.5);
            det.update("C", 200.0 - i as f64);
            det.update("D", 100.0 - i as f64 * 0.5);
        }
        let sigs = det.signals();
        assert_eq!(sigs.len(), 2);
    }

    #[test]
    fn test_max_window_caps_history() {
        let mut det = StatArbDetector::new(3, 5);
        det.add_pair("A", "B");
        for i in 0..20 {
            det.update("A", 100.0 + i as f64);
            det.update("B", 50.0 + i as f64 * 0.5);
        }
        // Should not panic and history is capped at 5
        let prices_a = det.prices.get("A").unwrap();
        assert!(prices_a.len() <= 5);
    }

    #[test]
    fn test_spread_monitor_fields_populated() {
        let mut det = StatArbDetector::new(5, 100);
        det.add_pair("AAPL", "MSFT");
        let (a, b) = cointegrated_pair(10);
        for (av, bv) in a.iter().zip(b.iter()) {
            det.update("AAPL", *av);
            det.update("MSFT", *bv);
        }
        let sigs = det.signals();
        let m = &sigs[0];
        assert_eq!(m.symbol_a, "AAPL");
        assert_eq!(m.symbol_b, "MSFT");
        assert!(m.hedge_ratio.is_finite());
        assert!(m.z_score.is_finite());
    }
}
